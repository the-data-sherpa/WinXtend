//! The thread that owns the `InputCapture` portal session and its libei transport.
//!
//! Linux-only, and the only part of capture that talks to a live desktop. The
//! rules it enforces live in [`super::capture`], which is testable without one —
//! the same division [`super::driver`] and [`super::session`] keep for injection,
//! and for the same reason: CI has no compositor but does have to compile and
//! test this crate.
//!
//! # Why a second session, and a second thread
//!
//! Two portals, not one. `RemoteDesktop` cannot capture — the reasoning and the
//! measurements are at the top of [`super::capture`] — so this session is
//! `org.freedesktop.portal.InputCapture` and is entirely separate from the one
//! [`super::driver`] holds. It gets its own consent dialog, which is the cost of
//! the desktop having split the two.
//!
//! One long-lived session for the life of the process, deliberately. `InputCapture`
//! is version 1 on the alpha target, so it carries no restore token and a dialog
//! appears every time a session is created. Creating one per screen crossing would
//! prompt on every crossing; arming and releasing barriers *within* a session needs
//! no fresh consent, which was confirmed on hardware across thirteen activations
//! in one session.
//!
//! # The sequence, and what it costs
//!
//! `CreateSession` → `GetZones` → `SetPointerBarriers` → `ConnectToEIS` → `Enable`,
//! and then the compositor decides when to activate: capture starts when the user
//! pushes the pointer through an armed barrier, and not before. There is no request
//! for "start capturing now", which is why [`super::capture`] cannot conjure
//! suppression on demand and says so instead.
//!
//! # Verified by hand on Ubuntu 26.04 / GNOME Shell 50.1 / xdg-desktop-portal 1.21.1
//!
//! Against a scratch client built from this same `ashpd`/`reis` pairing:
//!
//! * `InputCapture` version 1; `CreateSession` granting `Keyboard | Pointer`;
//!   `GetZones` reporting one 3072x1728 region at the origin, matching what
//!   [`super::display`] enumerates;
//! * a barrier accepted on any of the four edges once placed the way
//!   [`barriers_for`] places them, and rejected otherwise — a fact about the
//!   *geometry* the compositor enforces, and not a reason to arm all four. Which
//!   edges are asked for is the layout's answer; see [`barriers_for`] for both;
//! * thirteen `Activated` signals across one session with no further consent, each
//!   carrying a real cursor position and the barrier that fired;
//! * `Release` handing the pointer back and the session re-arming immediately;
//! * keyboard, button and scroll events arriving as evdev codes and 120-unit
//!   detents, with no `ei_keyboard.modifiers` and no absolute motion at all.
//!
//! The consent dialog itself cannot be automated, and must not be: that click *is*
//! the security property. See the same note at the top of [`super::driver`].
//!
//! # What has *not* been verified against a live compositor
//!
//! Moving the barriers on a running session — [`rearm`], reached when the layout
//! changes which edges have a machine beyond them. Every step of it is a portal
//! call this session already makes, but the `Disable`/`SetPointerBarriers`/`Enable`
//! *sequence* has not been run against GNOME, because exercising it means arming
//! capture on a working desktop and an exclusive grab that is not released takes
//! the machine away from whoever is at it. `rearm` carries what is known — the
//! GNOME 46 bug `ashpd`'s own docs record, and which of its failure modes are made
//! safe here — and the pure half of the decision is tested in
//! [`super::capture_tests`] and in this module's own tests without a desktop:
//! [`PendingRearm`] decides *when* a re-arm happens and [`OwnDisable`] decides
//! which `Disabled` signal is ours, and both are pure. Anyone with two machines to
//! spare should confirm the portal half: move a machine in the layout editor and
//! check the cursor then crosses by the new edge and not the old one.
//!
//! Two things are therefore deliberately never asked of the compositor, because a
//! guess about how it answers would be load-bearing:
//!
//! * **`SetPointerBarriers` is never called with an empty array.** A machine the
//!   layout puts nothing beside is the state every fresh install starts in, and
//!   whether GNOME accepts an empty set is not established anywhere. So no request
//!   is made at all and the session is left not enabled — see [`place_barriers`].
//! * **`Enable` is never sent over an empty barrier set**, for the same reason and
//!   because it would have nothing to arm. [`Barriers::should_enable`] is the one
//!   place that decides.
//!
//! What that costs, and the assumption it leaves standing: because the empty plan
//! makes no request, **the barriers placed for the previous layout stay registered
//! with the compositor** when the last neighbour goes away. They are inert only
//! because the session is disabled in the same breath and nothing re-enables it —
//! so [`Barriers::should_enable`] must remain the *sole* gate on `Enable`. An
//! `Enable` added anywhere that does not consult it would arm a barrier for a
//! machine that is no longer there, which is the bug this whole layering exists to
//! fix. There are two call sites today, in [`obey`] and [`rearm`], and both go
//! through it.

use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ashpd::desktop::input_capture::{
    ActivatedBarrier, Barrier, BarrierID, Capabilities as PortalCapabilities, CreateSessionOptions,
    InputCapture, Region, ReleaseOptions,
};
use ashpd::desktop::Session;
use ashpd::enumflags2::BitFlags;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use tokio::sync::{mpsc, oneshot};
use wx_proto::Point;

use super::capture::{barriers_for, zone_at, BarrierEdge, CaptureState, Command, Zone};
use super::session::{SharedSession, INPUT_CAPTURE_CAPABILITIES};
use crate::traits::ScreenExits;

/// Name this client reports to the compositor. User-visible in the shell's
/// screen-sharing indicator, so it is the product's name and not a code name.
const EI_CLIENT_NAME: &str = "WinXtend";

/// How long teardown may take before the thread gives up and exits anyway.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Handle to the running capture session. Dropping it tears the session down.
pub struct Driver {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// Start acquiring the capture session on a thread of its own.
///
/// Returns immediately: the consent dialog can sit on screen for as long as the
/// user takes, and nothing above this may block on that.
pub fn start(shared: Arc<SharedSession>, capture: Arc<CaptureState>) -> Driver {
    shared.starting();
    let (stop_tx, stop_rx) = oneshot::channel();
    let thread_shared = Arc::clone(&shared);

    let spawned = std::thread::Builder::new()
        .name("wx-capture".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    thread_shared.failed(format!("starting the input-capture runtime: {e}"));
                    return;
                }
            };
            runtime.block_on(run(&thread_shared, &capture, stop_rx));
        });

    match spawned {
        Ok(thread) => Driver {
            stop: Some(stop_tx),
            thread: Some(thread),
        },
        Err(e) => {
            shared.failed(format!("starting the input-capture thread: {e}"));
            Driver {
                stop: None,
                thread: None,
            }
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            if let Err(e) = thread.join() {
                tracing::warn!(error = ?e, "the input-capture thread panicked");
            }
        }
    }
}

async fn run(shared: &SharedSession, capture: &CaptureState, mut stop: oneshot::Receiver<()>) {
    // Shutdown has to be able to interrupt the sequence and not just the loop that
    // follows it: `CreateSession` blocks on the consent dialog, and a dialog behind
    // a full-screen window has no upper bound at all.
    let established = tokio::select! {
        biased;
        _ = &mut stop => {
            tracing::debug!("shutting down before the input-capture session was granted");
            shared.stopped();
            return;
        }
        established = establish(capture) => established,
    };

    let (live, mut barriers, mut events) = match established {
        Ok(live) => live,
        Err(Aborted { failure, session }) => {
            failure.report(shared);
            if let Some(session) = session {
                close(session).await;
            }
            return;
        }
    };

    tracing::info!(
        capabilities = ?live.granted,
        zones = live.zones.len(),
        barriers = barriers.edges.len(),
        "the desktop portal granted an InputCapture session"
    );

    // The command hook goes in before the capability, so nothing can be told this
    // machine captures while `Enable` would still go nowhere.
    let (tx, rx) = mpsc::unbounded_channel();
    capture.attach(Box::new(move |command| {
        // A closed channel means the driver has already gone; there is nothing to
        // enable or release on a session that no longer exists.
        let _ = tx.send(command);
    }));
    shared.activate(INPUT_CAPTURE_CAPABILITIES);

    let reason = pump(&live, &mut barriers, &mut events, capture, rx, stop).await;
    capture.detach();
    teardown(shared, live, reason).await;
}

/// Bring the placed barriers, and whether the session is enabled, into line with
/// `exits`. Answers whether the session now matches what was asked for.
///
/// # Never called from the place that learns the layout changed
///
/// A re-arm ends the current activation, and ending one out from under the engine
/// is a bug of its own: the router still says a peer owns the cursor, the engine
/// never re-reads `suppresses_local`, and physical input goes to local windows
/// while the remote cursor is frozen. So a layout edit does not interrupt a drive
/// in progress — [`PendingRearm`] holds the change and this is only ever reached
/// through [`PendingRearm::settle`], which runs at the top of [`pump`]'s loop.
/// That is the one place every path that could have ended an activation comes back
/// through, so the deferral cannot be forgotten by a call site that did not know
/// about it.
///
/// # The disable/enable dance, and why it is guarded
///
/// GNOME's portal will not accept `SetPointerBarriers` on an enabled session. The
/// specification says such a request suspends the session; the implementation
/// requires it to have been disabled first, which `ashpd`'s own module docs record
/// along with the GNOME 46 bug that then prevented re-enabling it. The alpha
/// target is GNOME 50, well past that, but this sequence has **not** been
/// exercised against a live compositor — arming capture on a working desktop to
/// test it is precisely the thing that takes someone's machine away from them —
/// so it is written to fail safe rather than to be clever.
///
/// The guard that matters is `Disabled`. That signal normally means the compositor
/// has ended the session for its own reasons and the pump tears everything down
/// over it; a `Disabled` raised by our *own* `Disable` would therefore turn a
/// layout change into a lost session and a fresh consent dialog. So a disable this
/// side asked for is announced first and the signal it may produce is absorbed.
/// The same shape as `Release`, which GNOME is measured not to answer with a
/// `Deactivated` — our own decision is authoritative for our own state, and the
/// signal is kept for the case where the compositor is the one deciding. See
/// [`OwnDisable`] for why that expectation is both counted and given a deadline.
///
/// A refusal anywhere leaves the previous barriers in place, logs, and answers
/// `false` so the caller tries again — committing the new answer over a placement
/// that never happened would leave the newly live edge unarmed for good, with only
/// a `warn!` in the log. Until it does succeed the edges stay as they were, and an
/// activation on an edge that is no longer live is handed straight back by
/// [`CaptureState::activated`].
async fn rearm(
    live: &Live,
    barriers: &mut Barriers,
    capture: &CaptureState,
    own_disable: &OwnDisable,
    exits: Vec<ScreenExits>,
) -> bool {
    let wants_capture = capture.is_capturing();
    // Idempotent, whatever the command queue holds: a `Rearm` sent before the
    // session was granted arrives afterwards carrying exits already applied, and
    // obeying it would cost a whole disable/enable cycle — a window with no
    // barriers placed — for nothing.
    if barriers.exits == exits && barriers.enabled == barriers.should_enable(wants_capture) {
        return true;
    }
    if barriers.enabled {
        own_disable.expect();
        if let Err(e) = live.portal.disable(&live.session, Default::default()).await {
            own_disable.forget();
            tracing::warn!(error = %e, "could not disable the capture session to move its barriers");
            return false;
        }
        barriers.enabled = false;
        // A disabled session is not capturing, whether or not the compositor says
        // so in a signal — GNOME is measured not to answer our own `Release` with a
        // `Deactivated`, and the same may be true here. Waiting for a signal that
        // may never come would leave `suppresses_local` claiming local input is
        // still being swallowed, and any key held at the moment of the change
        // stranded on whichever machine had the cursor. A no-op in practice,
        // because `settle` only gets here with no activation in force.
        capture.deactivated(None);
    }
    let placed = place_barriers(
        &live.portal,
        &live.session,
        live.zone_set,
        &live.zones,
        exits,
    )
    .await;
    let applied = match placed {
        Ok(placed) => {
            barriers.edges = placed.edges;
            barriers.exits = placed.exits;
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "the portal refused the new pointer barriers");
            false
        }
    };
    // Re-enabled even when the placement failed: the old barriers are still the
    // session's, and leaving it disabled would silently stop this machine driving
    // anything at all.
    if barriers.should_enable(wants_capture) && !barriers.enabled {
        if let Err(e) = live.portal.enable(&live.session, Default::default()).await {
            tracing::warn!(error = %e, "could not re-arm the capture session after a layout change");
            return false;
        }
        barriers.enabled = true;
    }
    applied
}

/// A capture session that has been granted, with its transport connected.
struct Live {
    portal: InputCapture,
    session: Session<InputCapture>,
    granted: BitFlags<PortalCapabilities>,
    connection: reis::event::Connection,
    /// Every region the portal reported, kept rather than counted so an activation
    /// the compositor did not attribute to a barrier can still be clamped onto a
    /// screen that exists.
    zones: Vec<Zone>,
    /// The `GetZones` serial the barriers are placed against. Retained because
    /// re-arming has to quote it again, and the compositor refuses a set of
    /// barriers offered against a zone set it has moved on from.
    zone_set: u32,
}

/// The barriers currently placed, and what they were placed for.
///
/// Separate from [`Live`] because it is the one part of the session that changes
/// while the session runs: a layout change alters which edges have somewhere
/// beyond them, and the barriers have to follow without a new session — this
/// portal has no restore token at the version the alpha target ships, so a new
/// session means a new consent dialog.
struct Barriers {
    /// Which edge each barrier id sits on and which zone it bounds, so an
    /// activation knows which way the pointer came in — and therefore which way is
    /// back out, and which screen it is really on.
    edges: Vec<(BarrierID, BarrierEdge, Zone)>,
    /// The answer these were planned from, so an unchanged push costs no round
    /// trip and a changed one is noticed. Advanced only when a placement actually
    /// succeeded, so a refusal stays visible as a difference from what the layout
    /// asked for and can be tried again.
    exits: Vec<ScreenExits>,
    /// Whether `Enable` is in force on the session.
    ///
    /// Tracked rather than inferred so a `Disable` is only sent — and therefore
    /// only expected back — when there is something to disable, and so a session
    /// with nothing placed is never enabled over an empty barrier set.
    enabled: bool,
}

impl Barriers {
    fn barrier(&self, id: BarrierID) -> Option<(BarrierEdge, Zone)> {
        self.edges
            .iter()
            .find(|(b, _, _)| *b == id)
            .map(|(_, e, z)| (*e, *z))
    }

    /// Whether the session should be enabled: the engine has asked for capture
    /// *and* there is a barrier to arm.
    ///
    /// The second half is the whole answer for a machine the layout puts nothing
    /// beside, which is the state every fresh install starts in. Nothing is placed,
    /// so nothing is enabled, so no edge can take the cursor — and none of that
    /// rests on how the compositor answers a request this build never makes. The
    /// session itself stays up and capable, and arms the moment a peer is placed.
    fn should_enable(&self, wants_capture: bool) -> bool {
        wants_capture && !self.edges.is_empty()
    }
}

/// How long a `Disabled` signal may still be attributed to a `Disable` this side
/// sent.
///
/// GNOME is measured not to answer a client-initiated `Release` with a
/// `Deactivated`, so it may well not answer a client-initiated `Disable` with a
/// `Disabled` either. An expectation that is never met must not stand for the rest
/// of the session: the next `Disabled` would be absorbed by it, and that one is a
/// screen lock or a revocation — a session that has really died, reported to the
/// agent as nothing at all. A signal the portal does send follows the round trip
/// that asked for it, so the window only has to cover that.
const OWN_DISABLE_WINDOW: Duration = Duration::from_secs(2);

/// The `Disabled` signals still owed to a `Disable` this side sent.
///
/// A queue of deadlines and not a flag, because both mismatches are reachable and
/// both are silent. Two disables answered by one signal leaves an expectation
/// standing that swallows a genuine revocation, so they are counted; a disable the
/// compositor never answers would leave that expectation standing for ever, so
/// each one expires. One signal answered by no expectation is the compositor
/// deciding, and is still believed.
///
/// A `RefCell` and not a lock: the capture driver is one thread running a
/// current-thread runtime, and everything that touches this is on it.
#[derive(Default)]
struct OwnDisable(std::cell::RefCell<VecDeque<Instant>>);

impl OwnDisable {
    /// A `Disable` is about to be sent.
    fn expect(&self) {
        self.0
            .borrow_mut()
            .push_back(Instant::now() + OWN_DISABLE_WINDOW);
    }

    /// It was refused, so no signal is coming.
    fn forget(&self) {
        self.0.borrow_mut().pop_back();
    }

    /// Answer whether an arriving `Disabled` is ours, and stop expecting that one.
    fn claim(&self) -> bool {
        self.claim_at(Instant::now())
    }

    fn claim_at(&self, now: Instant) -> bool {
        let mut owed = self.0.borrow_mut();
        // Pushed in order, so the front is always the oldest deadline.
        while owed.front().is_some_and(|deadline| *deadline <= now) {
            owed.pop_front();
        }
        owed.pop_front().is_some()
    }
}

/// How long to wait before offering a refused set of barriers again.
///
/// Bounded rather than endless: a portal that has refused this five times is not
/// going to be talked round, and a warning every thirty seconds for the life of
/// the process is noise rather than information. A later layout change starts the
/// budget over, which is the case that matters — the acceptance criterion is that
/// changing the layout re-arms without a restart.
const REARM_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];

/// The barrier change the layout has asked for and the session has not applied.
///
/// The one owner of "a re-arm is owed", and the reason [`rearm`] is never called
/// from the branch that learns about a layout change. Two things it exists to stop,
/// both of which are an action deferred on an event that may never arrive:
///
/// * a re-arm run *during* an activation, which ends the drive the user is in the
///   middle of and leaves the engine believing local input is still suppressed;
/// * a re-arm the portal refused and nobody tried again, which leaves a newly live
///   edge unarmed for good because every layer above has already committed the new
///   answer and will never push it a second time.
///
/// [`PendingRearm::due`] is the whole decision and is pure, so both are tested with
/// no desktop.
#[derive(Default)]
struct PendingRearm {
    /// What the layout last asked for, until it has been applied.
    wanted: Option<Vec<ScreenExits>>,
    /// When a refused attempt may be made again.
    retry_at: Option<Instant>,
    /// How many refusals have been answered with a retry so far.
    attempts: usize,
}

impl PendingRearm {
    /// The layout wants these edges live. Supersedes anything still owed, and
    /// starts the retry budget over.
    fn want(&mut self, exits: Vec<ScreenExits>) {
        self.wanted = Some(exits);
        self.retry_at = None;
        self.attempts = 0;
    }

    /// The re-arm to apply now, if there is one and this is the moment for it.
    ///
    /// Deferred, never dropped, while an activation is in force: the user is
    /// driving a peer and a layout edit must not take that away from them. It is
    /// applied at the next pass of the pump loop after the activation ends, by
    /// whatever ended it — a clean release, the compositor deactivating, a paused
    /// or removed device, the engine giving up suppression, a `stop`. None of those
    /// call sites has to know this exists.
    fn due(&self, now: Instant, activation_in_force: bool) -> Option<&[ScreenExits]> {
        let wanted = self.wanted.as_deref()?;
        if activation_in_force {
            return None;
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return None;
        }
        Some(wanted)
    }

    /// When the pump loop should come back of its own accord, if it should.
    ///
    /// Nothing at all while an activation is in force, and that is not an
    /// optimisation. [`PendingRearm::due`] refuses during a drive whatever the
    /// deadline says, so a deadline surfaced then is one the loop wakes for and can
    /// do nothing about — and a refused deadline already in the past makes that
    /// wake instantaneous, spinning the driver thread at full tilt, taking the
    /// [`CaptureState`] lock against the events it is delivering, for exactly as
    /// long as the user keeps driving. Nothing is lost by staying asleep: whatever
    /// ends the activation is itself a branch of the loop, and `due` re-reads the
    /// deadline on the way back through.
    fn wake_at(&self, activation_in_force: bool) -> Option<Instant> {
        if activation_in_force {
            return None;
        }
        self.retry_at
    }

    /// The session now matches what the layout asked for.
    fn applied(&mut self) {
        self.wanted = None;
        self.retry_at = None;
        self.attempts = 0;
    }

    /// The portal refused. Try again later, until the budget is out.
    fn refused(&mut self, now: Instant) {
        match REARM_RETRY_DELAYS.get(self.attempts) {
            Some(delay) => {
                self.attempts += 1;
                self.retry_at = Some(now + *delay);
            }
            None => {
                tracing::warn!(
                    "the portal would not move the pointer barriers; the screen edges stay as \
                     they were until the layout changes again"
                );
                self.applied();
            }
        }
    }

    /// Apply what is owed, if anything is and this is the moment for it, and
    /// answer when the loop should come back for what is left.
    ///
    /// The wake instant comes from here rather than being read off afterwards so
    /// that the answer and the condition it was computed under cannot disagree, and
    /// so the [`CaptureState`] lock is taken once a pass.
    async fn settle(
        &mut self,
        live: &Live,
        barriers: &mut Barriers,
        capture: &CaptureState,
        own_disable: &OwnDisable,
    ) -> Option<Instant> {
        let activation_in_force = capture.suppresses_local();
        let Some(exits) = self.due(Instant::now(), activation_in_force) else {
            return self.wake_at(activation_in_force);
        };
        let exits = exits.to_vec();
        if rearm(live, barriers, capture, own_disable, exits).await {
            self.applied();
        } else {
            self.refused(Instant::now());
        }
        self.wake_at(capture.suppresses_local())
    }
}

/// Wait for a deadline, or for ever when there is nothing to wait for.
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep(at.saturating_duration_since(Instant::now())).await,
        None => std::future::pending().await,
    }
}

/// Why the event loop stopped.
enum Ended {
    Stopped,
    Revoked(String),
    Broken(String),
}

struct Aborted {
    failure: Failure,
    session: Option<Session<InputCapture>>,
}

impl Aborted {
    fn before_session(e: ashpd::Error) -> Self {
        Self {
            failure: Failure::from_ashpd(e, Stage::BeforeConsent),
            session: None,
        }
    }
}

/// Run the whole sequence and bring the `ei` connection up.
async fn establish(
    capture: &CaptureState,
) -> Result<(Live, Barriers, reis::tokio::EiConvertEventStream), Aborted> {
    let portal = InputCapture::new().await.map_err(Aborted::before_session)?;
    tracing::debug!(version = portal.version(), "portal InputCapture interface");

    // Version 1 only, on purpose. `CreateSession2` and its restore token arrived in
    // version 2, which the alpha target does not have; asking for it and falling
    // back would put two round trips and an error in the log on every launch for a
    // feature the desktop cannot offer. Worth revisiting when the target moves,
    // because a restore token is what would remove the per-launch dialog.
    //
    // `SupportedCapabilities` is deliberately not consulted: GNOME 50 publishes a
    // value with bits this `ashpd` does not know, and reading it fails outright.
    // What the session was actually granted is the honest answer anyway, and it
    // comes back from `CreateSession`.
    let (session, granted) = portal
        .create_session(
            None,
            CreateSessionOptions::default()
                .set_capabilities(PortalCapabilities::Keyboard | PortalCapabilities::Pointer),
        )
        .await
        .map_err(|e| Aborted {
            failure: Failure::from_ashpd(e, Stage::Consent),
            session: None,
        })?;

    match negotiate(&portal, &session, granted, capture).await {
        Ok(negotiated) => Ok((
            Live {
                portal,
                session,
                granted,
                connection: negotiated.connection,
                zones: negotiated.zones,
                zone_set: negotiated.zone_set,
            },
            negotiated.barriers,
            negotiated.events,
        )),
        Err(failure) => Err(Aborted {
            failure,
            session: Some(session),
        }),
    }
}

struct Negotiated {
    connection: reis::event::Connection,
    events: reis::tokio::EiConvertEventStream,
    barriers: Barriers,
    zones: Vec<Zone>,
    zone_set: u32,
}

async fn negotiate(
    portal: &InputCapture,
    session: &Session<InputCapture>,
    granted: BitFlags<PortalCapabilities>,
    capture: &CaptureState,
) -> Result<Negotiated, Failure> {
    accept_granted(granted)?;

    let zones = portal
        .zones(session, Default::default())
        .await
        .map_err(|e| Failure::from_ashpd(e, Stage::AfterGrant))?
        .response()
        .map_err(|e| Failure::from_ashpd(e, Stage::AfterGrant))?;

    let regions: Vec<Zone> = zones.regions().iter().copied().map(zone_of).collect();
    let zone_set = zones.zone_set();
    // Whatever the agent has said by now. The consent dialog is answered by a
    // human, so a layout has usually arrived long before this point; if none has,
    // nothing is armed and the first `Rearm` places the barriers instead.
    let exits = capture.exits();
    let barriers = place_barriers(portal, session, zone_set, &regions, exits)
        .await
        .map_err(|e| Failure::from_ashpd(e, Stage::AfterGrant))?;

    let fd = portal
        .connect_to_eis(session, Default::default())
        .await
        .map_err(|e| Failure::from_ashpd(e, Stage::AfterGrant))?;
    let context = ei::Context::new(UnixStream::from(fd))
        .map_err(|e| Failure::broken(format!("opening the libei transport: {e}")))?;
    // Receiver, not Sender. This is the whole difference between the two sessions:
    // a receiver context is handed the compositor's *captured* devices, where a
    // sender's are client-owned emulation devices with nothing to report.
    let (connection, events) = context
        .handshake_tokio(EI_CLIENT_NAME, ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| Failure::broken(format!("the libei handshake failed: {e}")))?;
    tracing::debug!("libei capture transport connected");

    Ok(Negotiated {
        connection,
        events,
        zones: regions,
        zone_set,
        barriers,
    })
}

/// Ask the compositor for exactly the barriers `exits` calls for.
///
/// Replaces whatever is currently placed: `SetPointerBarriers` is the whole set,
/// not an addition to it, so an edge that has stopped being live is disarmed by
/// being left out.
///
/// Placing none is a legitimate outcome and not an error. A machine the layout
/// puts nothing next to has nowhere to send the cursor from any edge, and the
/// right behaviour is a pointer that stops on all four sides — the session stays
/// up, capable of driving nothing, and arms itself the moment a peer is placed
/// beside it.
///
/// **An empty plan is not sent.** Whether GNOME accepts a `SetPointerBarriers`
/// with an empty array is not established by this change, by `ashpd`'s docs, or by
/// anything that has been run against the alpha target — and a refusal at
/// [`negotiate`] aborts the whole session, which would leave every fresh
/// single-machine install reporting no capture capability at all. That is the
/// common case, not an edge one, so it is not made to rest on a guess: no request
/// is made, and [`Barriers::should_enable`] then keeps the session out of `Enable`.
/// Barriers already placed are left with the compositor rather than cleared, which
/// is safe because [`rearm`] disables the session in the same breath and a session
/// that is not enabled cannot be activated — the same property teardown relies on
/// — and because the next non-empty placement replaces the whole set.
async fn place_barriers(
    portal: &InputCapture,
    session: &Session<InputCapture>,
    zone_set: u32,
    zones: &[Zone],
    exits: Vec<ScreenExits>,
) -> ashpd::Result<Barriers> {
    let placed = barriers_for(zones, &exits);
    if placed.is_empty() {
        tracing::debug!("the layout has no machine beyond any screen edge; nothing to arm");
        return Ok(Barriers {
            edges: Vec::new(),
            exits,
            enabled: false,
        });
    }
    // A plan whose id the portal cannot express is one this build asked for and
    // cannot name; dropping it loses an edge, which is far better than shifting
    // every later id and mis-attributing a refusal.
    let barriers: Vec<Barrier> = placed
        .iter()
        .filter_map(|p| Some(Barrier::new(BarrierID::new(p.id)?, p.position)))
        .collect();
    let response = portal
        .set_pointer_barriers(session, &barriers, zone_set, Default::default())
        .await?
        .response()?;

    // A barrier the compositor would not place is one edge the cursor cannot
    // leave by. Not fatal — the others still work, and a node with one usable
    // edge is far better than none — but it is exactly the kind of thing that
    // reads as "the cursor won't go left" and is otherwise unattributable.
    let refused = response.failed_barriers();
    if !refused.is_empty() {
        tracing::warn!(
            ?refused,
            "the compositor would not place some pointer barriers; the cursor cannot leave by \
             those edges"
        );
    }
    let edges: Vec<(BarrierID, BarrierEdge, Zone)> = placed
        .into_iter()
        .filter_map(|p| Some((BarrierID::new(p.id)?, p.edge, p.zone)))
        .filter(|(id, _, _)| !refused.contains(id))
        .collect();
    tracing::debug!(
        armed = edges.len(),
        asked = barriers.len(),
        "pointer barriers placed"
    );
    Ok(Barriers {
        edges,
        exits,
        enabled: false,
    })
}

/// Refuse a grant that is missing either capability.
///
/// [`wx_proto::Capabilities`] has no per-device granularity: `CAPTURE_INPUT` is a
/// promise to capture the keyboard *and* the pointer. A pointer-only grant would
/// have peers route keystrokes to a machine that cannot see any.
fn accept_granted(granted: BitFlags<PortalCapabilities>) -> Result<(), Failure> {
    let missing: Vec<&str> = [
        (PortalCapabilities::Keyboard, "keyboard"),
        (PortalCapabilities::Pointer, "pointer"),
    ]
    .into_iter()
    .filter(|(c, _)| !granted.contains(*c))
    .map(|(_, name)| name)
    .collect();

    if missing.is_empty() {
        return Ok(());
    }
    Err(Failure::denied(format!(
        "the desktop portal withheld {} capture; this machine needs keyboard and pointer \
         together, so the session was given back",
        missing.join(" and ")
    )))
}

/// The portal's own zone, in the shape [`barriers_for`] plans against.
///
/// A `GetZones` region carries its size unsigned and its offset signed, which is
/// exactly the mix that gets arithmetic on a monitor at a negative offset wrong.
fn zone_of(region: Region) -> Zone {
    Zone {
        x: region.x_offset(),
        y: region.y_offset(),
        w: i32::try_from(region.width()).unwrap_or(i32::MAX),
        h: i32::try_from(region.height()).unwrap_or(i32::MAX),
    }
}

/// Drive the session until it ends.
async fn pump(
    live: &Live,
    barriers: &mut Barriers,
    events: &mut reis::tokio::EiConvertEventStream,
    capture: &CaptureState,
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut stop: oneshot::Receiver<()>,
) -> Ended {
    // Every signal is subscribed before anything is enabled, so no activation can
    // slip past unseen. Losing one only costs part of the story, so a failure to
    // subscribe is a warning rather than a refusal — except that losing `Activated`
    // would mean capture that never reports where the pointer is, which is worth
    // giving up over.
    let activated = match live.portal.receive_activated().await {
        Ok(stream) => stream,
        Err(e) => {
            return Ended::Broken(format!(
                "cannot watch the input-capture session for activation: {e}"
            ))
        }
    };
    let deactivated = live.portal.receive_deactivated().await.ok();
    let disabled = live.portal.receive_disabled().await.ok();
    let closed = live.session.receive_closed().await.ok();

    let mut activated = std::pin::pin!(activated);
    let mut deactivated = std::pin::pin!(OptionStream(deactivated));
    let mut disabled = std::pin::pin!(OptionStream(disabled));
    let mut closed = std::pin::pin!(OptionStream(closed));

    let own_disable = OwnDisable::default();

    // The layout may have changed while the consent dialog was on screen, which is
    // a window measured in however long a human takes. Any `Rearm` sent then went
    // nowhere — there was no command hook yet — so the answer is re-read here
    // rather than waiting for the next layout event, which may never come.
    //
    // Here and not before `pump`, deliberately. A `Disable` sent before `Disabled`
    // is being watched leaves an expectation standing for a signal that can no
    // longer be delivered, and the next genuine revocation — a screen lock — is
    // then absorbed by it and the session dies with nobody told.
    //
    // Only on a real change. Nothing owed on the ordinary path, where the answer is
    // still the one `negotiate` planned from: asking for it again would re-issue
    // `SetPointerBarriers` with the identical plan on every session establishment.
    // Arming is not this call's business — the `Enable` `attach` queued does that,
    // and `obey` handles it idempotently.
    let mut pending = PendingRearm::default();
    let current = capture.exits();
    if current != barriers.exits {
        tracing::debug!("the layout changed while the session was being granted; re-arming");
        pending.want(current);
    }

    loop {
        // The single owner of "the barriers owe the layout a change". Every path
        // that ends an activation arrives as one of the branches below and comes
        // straight back here, so a deferred re-arm is applied by whatever ended the
        // activation without that path having to know it exists. An unchanged
        // answer costs nothing — `rearm` returns at once.
        let retry_at = pending.settle(live, barriers, capture, &own_disable).await;

        tokio::select! {
            biased;

            _ = &mut stop => return Ended::Stopped,

            // `Some(..)` rather than a match on the option: a closed channel yields
            // `None` immediately and forever, and a branch that accepted it would
            // spin this thread at 100% for the rest of the session. Not matching
            // disables the branch instead, which is what "there is nobody left to
            // send commands" should mean.
            Some(command) = commands.recv() => {
                if let Err(e) = obey(live, barriers, &own_disable, &mut pending, command).await {
                    // Not fatal: a refused `Release` leaves the pointer pinned,
                    // which the user can still clear by crossing back, and a
                    // refused `Enable` leaves capture idle. Tearing the session
                    // down over either would be worse.
                    tracing::warn!(error = %e, "the input-capture portal refused a request");
                }
            },

            Some(signal) = activated.next() => {
                let position = signal.cursor_position()
                    .map(|(x, y)| Point::new(f64::from(x), f64::from(y)));
                let barrier = match signal.barrier_id() {
                    Some(ActivatedBarrier::Barrier(id)) => barriers.barrier(id),
                    _ => None,
                };
                match position {
                    // The zone falls back to whichever region the position lands
                    // on, so an activation the compositor did not attribute to a
                    // barrier is still clamped onto a screen that exists. Without
                    // it the boundary coordinate reaches the engine unclamped and
                    // the resynchronisation is silently dropped — the precise
                    // failure `Zone::clamp` was added for.
                    Some(position) => capture.activated(
                        signal.activation_id(),
                        position,
                        barrier.map(|(edge, _)| edge),
                        barrier
                            .map(|(_, zone)| zone)
                            .or_else(|| zone_at(&live.zones, position)),
                    ),
                    // Every activation observed on the alpha target carried one.
                    // Without it there is no honest absolute position to report, and
                    // inventing one would be exactly the fiction `CapturedEvent`
                    // forbids — so the crossing is refused rather than faked.
                    None => {
                        tracing::warn!(
                            "the portal activated capture without saying where the pointer is; \
                             handing it straight back"
                        );
                        let _ = live.portal.release(
                            &live.session,
                            ReleaseOptions::default().set_activation_id(signal.activation_id()),
                        ).await;
                    }
                }
            },

            Some(signal) = deactivated.next() => capture.deactivated(signal.activation_id()),

            Some(_) = disabled.next() => {
                // A disable this side asked for — moving the barriers after a layout
                // change, or `stop` — is not the compositor revoking anything, and
                // tearing the session down over it would cost a consent dialog to get
                // back. See `OwnDisable`.
                if own_disable.claim() {
                    tracing::debug!("the portal acknowledged a disable this side asked for");
                    capture.deactivated(None);
                    continue;
                }
                // The compositor has stopped capturing for this session and will not
                // start again without a fresh `Enable`. Handled defensively rather
                // than tested, because the case that produces it is a screen lock and
                // testing that means locking the machine with no way back.
                capture.deactivated(None);
                return Ended::Revoked(
                    "the desktop portal disabled the input-capture session".into(),
                );
            },

            Some(_) = closed.next() => {
                return Ended::Revoked(
                    "the desktop portal closed the input-capture session".into(),
                );
            },

            event = events.next() => match event {
                Some(Ok(event)) => {
                    if let Some(ended) = on_ei_event(&live.connection, capture, event) {
                        return ended;
                    }
                }
                Some(Err(e)) => return Ended::Broken(format!("the libei transport failed: {e}")),
                None => return Ended::Revoked(
                    "the compositor closed the libei capture transport".into(),
                ),
            },

            // Nothing happened, but a refused re-arm is owed another attempt. Only
            // ever armed while one is outstanding; otherwise this branch waits for
            // ever and the loop is driven entirely by the ones above.
            () = wait_until(retry_at) => {},
        }
    }
}

/// Carry out one [`Command`] from the trait side.
async fn obey(
    live: &Live,
    barriers: &mut Barriers,
    own_disable: &OwnDisable,
    pending: &mut PendingRearm,
    command: Command,
) -> ashpd::Result<()> {
    match command {
        Command::Enable => {
            // Nothing placed is nothing to arm. The session is enabled by [`rearm`]
            // the moment the layout puts a machine beyond an edge; see
            // [`Barriers::should_enable`].
            if !barriers.should_enable(true) {
                tracing::debug!("no screen edge has a machine beyond it; nothing to arm");
                return Ok(());
            }
            if barriers.enabled {
                return Ok(());
            }
            tracing::debug!("arming the pointer barriers");
            live.portal
                .enable(&live.session, Default::default())
                .await?;
            barriers.enabled = true;
            Ok(())
        }
        Command::Disable => {
            if !barriers.enabled {
                return Ok(());
            }
            tracing::debug!("disarming the pointer barriers");
            own_disable.expect();
            match live.portal.disable(&live.session, Default::default()).await {
                Ok(()) => {
                    barriers.enabled = false;
                    Ok(())
                }
                Err(e) => {
                    own_disable.forget();
                    Err(e)
                }
            }
        }
        // Recorded rather than obeyed. A re-arm ends the activation, so it waits
        // for one to end rather than taking a drive away from the user mid-gesture;
        // [`PendingRearm`] owns when that happens.
        Command::Rearm(exits) => {
            tracing::debug!("the layout changed which edges have a machine beyond them");
            pending.want(exits);
            Ok(())
        }
        Command::Release {
            activation_id,
            position,
        } => {
            live.portal
                .release(
                    &live.session,
                    ReleaseOptions::default()
                        .set_activation_id(activation_id)
                        .set_cursor_position(position),
                )
                .await
        }
    }
}

/// Handle one `ei` event, returning `Some` if it ended the session.
fn on_ei_event(
    connection: &reis::event::Connection,
    capture: &CaptureState,
    event: EiEvent,
) -> Option<Ended> {
    match event {
        EiEvent::SeatAdded(added) => {
            // Nothing is captured until the capabilities are bound. Text is asked
            // for although no EIS implements it yet, for the same reason injection
            // asks: the day one does, keys arrive already resolved.
            added.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll
                    | DeviceCapability::Keyboard,
            );
            if let Err(e) = connection.flush() {
                return Some(Ended::Broken(format!("flushing the libei transport: {e}")));
            }
            tracing::debug!(seat = ?added.seat.name(), "libei capture seat bound");
            None
        }
        EiEvent::DeviceAdded(added) => {
            tracing::debug!(
                device = ?added.device.name(),
                keyboard = added.device.has_capability(DeviceCapability::Keyboard),
                pointer = added.device.has_capability(DeviceCapability::Pointer),
                keymap = added.device.keymap().is_some(),
                "libei capture device offered"
            );
            if let Some(source) = read_keymap(&added.device) {
                capture.set_keymap(source);
            }
            None
        }
        EiEvent::PointerMotion(m) => {
            capture.motion(f64::from(m.dx), f64::from(m.dy));
            None
        }
        // Not offered by the alpha target's capture session — the seat has no
        // absolute device — but handled rather than ignored, because a compositor
        // that does send it is reporting the real pointer position, which is
        // strictly better than the pinned point from `Activated`.
        EiEvent::PointerMotionAbsolute(m) => {
            capture.absolute(Point::new(
                f64::from(m.dx_absolute),
                f64::from(m.dy_absolute),
            ));
            None
        }
        EiEvent::Button(b) => {
            capture.button(b.button, b.state == ei::button::ButtonState::Press);
            None
        }
        EiEvent::ScrollDelta(s) => {
            capture.scroll(f64::from(s.dx), f64::from(s.dy));
            None
        }
        EiEvent::ScrollDiscrete(s) => {
            capture.scroll_discrete(s.discrete_dx, s.discrete_dy);
            None
        }
        EiEvent::KeyboardKey(k) => {
            capture.key(k.key, k.state == ei::keyboard::KeyState::Press);
            None
        }
        // Never observed from a capturing session on the alpha target, which is
        // why modifier state is tracked from the key stream instead. Taken when it
        // does arrive, because the compositor's answer beats ours.
        EiEvent::KeyboardModifiers(m) => {
            capture.modifiers(m.depressed | m.latched, m.locked);
            None
        }
        // A pause stops events without ending the activation, and a resume starts
        // them again. Neither is a decision about who owns the cursor, but a key
        // held across a pause would be stranded, so a pause ends the activation
        // the same way a deactivation does.
        EiEvent::DevicePaused(_) => {
            capture.deactivated(None);
            None
        }
        EiEvent::DeviceRemoved(_) => {
            capture.deactivated(None);
            None
        }
        EiEvent::Disconnected(gone) => {
            let detail = format!(
                "the compositor disconnected the libei capture transport ({:?})",
                gone.reason
            );
            match gone.reason {
                ei::connection::DisconnectReason::Disconnected => Some(Ended::Revoked(detail)),
                _ => Some(Ended::Broken(detail)),
            }
        }
        _ => None,
    }
}

/// Read the keymap the compositor attached to the captured keyboard.
fn read_keymap(device: &reis::event::Device) -> Option<String> {
    use std::io::Read;

    let keymap = device.keymap()?;
    if keymap.type_ != ei::keyboard::KeymapType::Xkb {
        tracing::warn!(
            keymap = ?keymap.type_,
            "the captured keyboard's keymap is not xkb; keys will cross the wire as raw codes"
        );
        return None;
    }
    let fd = keymap.fd.try_clone().ok()?;
    let mut source = String::new();
    if let Err(e) = std::fs::File::from(fd)
        .take(u64::from(keymap.size))
        .read_to_string(&mut source)
    {
        tracing::warn!(error = %e, "could not read the captured keymap");
        return None;
    }
    // The keymap arrives NUL-terminated; the trailing byte would otherwise be the
    // first thing the parser trips on.
    Some(source.trim_end_matches('\0').to_string())
}

/// Close the session and record why it ended.
async fn teardown(shared: &SharedSession, live: Live, reason: Ended) {
    match &reason {
        Ended::Stopped => shared.stopped(),
        Ended::Revoked(why) => {
            tracing::warn!(reason = %why, "the input-capture session was revoked; this machine can no longer drive a peer");
            shared.denied(why.clone());
        }
        Ended::Broken(why) => {
            tracing::warn!(reason = %why, "the input-capture session broke");
            shared.failed(why.clone());
        }
    }
    // Disarming before closing so a session the portal is slow to reap cannot go
    // on grabbing the pointer after the agent has stopped caring about it.
    let _ = tokio::time::timeout(
        TEARDOWN_TIMEOUT,
        live.portal.disable(&live.session, Default::default()),
    )
    .await;
    close(live.session).await;
}

async fn close(session: Session<InputCapture>) {
    match tokio::time::timeout(TEARDOWN_TIMEOUT, session.close()).await {
        Ok(Ok(())) => tracing::debug!("input-capture session closed"),
        Ok(Err(e)) => tracing::debug!(error = %e, "the input-capture session was already gone"),
        Err(_) => tracing::warn!("timed out closing the input-capture session"),
    }
}

/// How a failure during [`establish`] should be reported.
///
/// The same three-way split [`super::driver::Failure`] makes, and for the same
/// reason: "the user can fix this", "there is no portal here" and "something
/// broke" need different responses from the agent, and only one of them is
/// actionable. There is no restore token on this portal at version 1, so the
/// token-rejection branch injection needs has no counterpart here.
struct Failure {
    detail: String,
    kind: FailureKind,
}

enum FailureKind {
    Denied,
    Unsupported,
    Broken,
}

/// Where in the sequence the failing call sits, relative to the consent dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Getting hold of the interface: no dialog exists yet, and on this portal
    /// there is nothing before `CreateSession` that could be refused.
    BeforeConsent,
    /// `CreateSession`, which is the call the user answers on version 1.
    Consent,
    /// Everything after the grant: zones, barriers, the transport.
    AfterGrant,
}

impl Failure {
    fn denied(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: FailureKind::Denied,
        }
    }

    fn broken(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: FailureKind::Broken,
        }
    }

    fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: FailureKind::Unsupported,
        }
    }

    fn from_ashpd(e: ashpd::Error, stage: Stage) -> Self {
        use ashpd::desktop::ResponseError;
        use ashpd::PortalError;

        match e {
            // GNOME before 50 has no `InputCapture` implementation at all, and
            // neither does a headless session. No amount of consenting helps, and
            // reporting it as a refusal sends the user looking for a dialog that
            // was never shown.
            ashpd::Error::PortalNotFound(name) => {
                Self::unsupported(format!("this desktop has no {name} portal"))
            }
            ashpd::Error::RequiresVersion(needed, found) => Self::unsupported(format!(
                "the input-capture portal is version {found}; this needs {needed}"
            )),
            // Refused before any dialog existed, or after the grant. Neither is a
            // decision anybody made: the first is a desktop that would not create
            // the session — which is what a locked screen looks like — and the
            // second is a fault on a session the user had just approved.
            ashpd::Error::Response(_) if stage == Stage::BeforeConsent => Self::broken(
                "the desktop refused to create an input-capture session; the session is most \
                 likely locked or not ready yet",
            ),
            ashpd::Error::Response(_) if stage == Stage::AfterGrant => {
                Self::broken("the desktop portal granted input capture but would not set it up")
            }
            ashpd::Error::Response(ResponseError::Cancelled) => {
                Self::denied("the input-capture consent dialog was dismissed")
            }
            ashpd::Error::Response(ResponseError::Other) => {
                Self::denied("the desktop portal refused input capture")
            }
            ashpd::Error::Zbus(e) | ashpd::Error::Portal(PortalError::ZBus(e)) => {
                if super::driver::no_session_bus(&e) {
                    Self::unsupported(format!("no desktop session to ask for permission: {e}"))
                } else {
                    Self::broken(format!("talking to the desktop portal: {e}"))
                }
            }
            other => Self::broken(format!("the input-capture request failed: {other}")),
        }
    }

    fn report(self, shared: &SharedSession) {
        match self.kind {
            FailureKind::Denied => {
                tracing::warn!(reason = %self.detail, "the desktop portal refused input capture; this node can be driven but cannot drive");
                shared.denied(self.detail);
            }
            FailureKind::Unsupported => {
                tracing::info!(reason = %self.detail, "no input-capture portal; this node can still be driven by a peer");
                shared.unsupported(self.detail);
            }
            FailureKind::Broken => {
                tracing::warn!(reason = %self.detail, "the input-capture session could not be established");
                shared.failed(self.detail);
            }
        }
    }
}

/// Adapts "maybe a stream" into a stream that simply never yields when absent.
struct OptionStream<S>(Option<S>);

impl<S: futures_util::Stream + Unpin> futures_util::Stream for OptionStream<S> {
    type Item = S::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.get_mut().0.as_mut() {
            Some(stream) => std::pin::Pin::new(stream).poll_next(cx),
            None => std::task::Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_covering_both_devices_is_accepted() {
        assert!(accept_granted(PortalCapabilities::Keyboard | PortalCapabilities::Pointer).is_ok());
    }

    #[test]
    fn a_half_grant_is_refused_rather_than_advertised_as_capture() {
        // `CAPTURE_INPUT` promises the keyboard and the pointer together, so a
        // pointer-only grant has no honest way to be published: peers would route
        // keystrokes to a machine that cannot see any.
        let failure = accept_granted(PortalCapabilities::Pointer.into()).unwrap_err();
        assert!(matches!(failure.kind, FailureKind::Denied));
        assert!(
            failure.detail.contains("withheld keyboard"),
            "{}",
            failure.detail
        );
    }

    #[test]
    fn a_desktop_with_no_input_capture_portal_is_unsupported_rather_than_denied() {
        // GNOME before 50 ships no implementation. Telling the user their
        // permission was denied would send them to a settings panel that has
        // nothing in it to change.
        let failure = Failure::from_ashpd(
            ashpd::Error::PortalNotFound(
                ashpd::zbus::names::InterfaceName::from_static_str(
                    "org.freedesktop.portal.InputCapture",
                )
                .unwrap()
                .into(),
            ),
            Stage::BeforeConsent,
        );
        assert!(matches!(failure.kind, FailureKind::Unsupported));
    }

    #[test]
    fn a_dismissed_dialog_is_a_permission_problem_and_a_locked_screen_is_not() {
        // The same distinction injection makes, and for the same reason: only
        // `CreateSession` puts a dialog on screen on this portal, so a refusal
        // before it is a desktop that would not create the session — most likely a
        // lock screen — and recording that as a refusal would leave the agent
        // capture-dead for its whole run.
        let refused = Failure::from_ashpd(
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled),
            Stage::Consent,
        );
        assert!(matches!(refused.kind, FailureKind::Denied));

        let locked = Failure::from_ashpd(
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled),
            Stage::BeforeConsent,
        );
        assert!(matches!(locked.kind, FailureKind::Broken));
        assert!(locked.detail.contains("locked"));
    }

    #[test]
    fn a_headless_machine_has_no_portal_rather_than_a_broken_one() {
        let failure = Failure::from_ashpd(
            ashpd::Error::Zbus(ashpd::zbus::Error::Address("no bus here".into())),
            Stage::BeforeConsent,
        );
        assert!(matches!(failure.kind, FailureKind::Unsupported));
    }

    // -- which `Disabled` signal is ours ----------------------------------
    //
    // Nothing below arms a capture session or talks to a compositor: the two
    // decisions the re-arm sequence turns on are pure, which is the only way they
    // can be exercised at all — see the module docs on what is deliberately not
    // verified against a live desktop.

    #[test]
    fn each_disable_this_side_sent_absorbs_one_signal_and_no_more() {
        // The old flag latched, so two disables in flight and one `Disabled` back
        // left it standing to swallow a genuine revocation — and one disable
        // answered twice had the second read as the compositor ending the session,
        // costing a fresh consent dialog.
        let own = OwnDisable::default();
        own.expect();
        own.expect();
        let now = Instant::now();
        assert!(own.claim_at(now));
        assert!(own.claim_at(now));
        assert!(
            !own.claim_at(now),
            "a third `Disabled` with nothing outstanding is the compositor's own decision"
        );
    }

    #[test]
    fn a_disable_the_compositor_never_answered_stops_absorbing_signals() {
        // GNOME sends no `Deactivated` for a client-initiated `Release`, so it may
        // well send no `Disabled` for a client-initiated `Disable`. An expectation
        // that outlived its round trip must not swallow the screen lock that comes
        // an hour later.
        let own = OwnDisable::default();
        own.expect();
        let later = Instant::now() + OWN_DISABLE_WINDOW + Duration::from_secs(1);
        assert!(!own.claim_at(later));
    }

    #[test]
    fn a_disable_the_portal_refused_expects_nothing_back() {
        let own = OwnDisable::default();
        own.expect();
        own.forget();
        assert!(!own.claim_at(Instant::now()));
    }

    // -- when a re-arm happens --------------------------------------------

    fn some_exits() -> Vec<ScreenExits> {
        vec![ScreenExits {
            bounds: wx_proto::Rect::new(0, 0, 1920, 1080),
            edges: vec![wx_proto::Edge::Left],
        }]
    }

    #[test]
    fn a_layout_change_does_not_interrupt_a_drive_in_progress() {
        // Re-arming ends the activation, and the engine is not watching for that:
        // it would go on believing local input is suppressed while the pointer is
        // back on this machine and the remote cursor is frozen. So the change waits
        // — and is applied the moment the activation ends, whatever ended it.
        let mut pending = PendingRearm::default();
        pending.want(some_exits());
        let now = Instant::now();
        assert!(pending.due(now, true).is_none());
        assert!(pending.due(now, false).is_some());
    }

    #[test]
    fn nothing_is_owed_once_the_session_matches_the_layout() {
        let mut pending = PendingRearm::default();
        pending.want(some_exits());
        pending.applied();
        assert!(pending.due(Instant::now(), false).is_none());
        assert!(pending.wake_at(false).is_none());
    }

    #[test]
    fn a_refused_rearm_is_tried_again_rather_than_left_unarmed() {
        // Every layer above has already committed the new answer by the time the
        // portal refuses, so an identical push is never sent again. If the retry
        // did not live here, a transient refusal when a peer is added would leave
        // that edge with no barrier for good, with only a `warn!` to say so.
        let mut pending = PendingRearm::default();
        pending.want(some_exits());
        let now = Instant::now();
        pending.refused(now);
        assert!(pending.due(now, false).is_none(), "not immediately");
        assert_eq!(pending.wake_at(false), Some(now + REARM_RETRY_DELAYS[0]));
        assert!(pending.due(now + REARM_RETRY_DELAYS[0], false).is_some());
    }

    #[test]
    fn the_loop_does_not_wake_for_a_retry_it_would_refuse_anyway() {
        // The pairing that spins: a refused re-arm whose deadline passes during a
        // drive. `due` says no for as long as the activation lasts, so a deadline
        // offered to the loop is one it wakes for instantly and can do nothing
        // about — round and round, taking the capture lock each pass, through
        // exactly the drive the deferral is protecting. Whatever ends the
        // activation wakes the loop by itself, and the deadline is read again then.
        let mut pending = PendingRearm::default();
        pending.want(some_exits());
        let now = Instant::now();
        pending.refused(now);
        let overdue = now + REARM_RETRY_DELAYS[0] + Duration::from_secs(1);
        assert!(pending.due(overdue, true).is_none());
        assert!(pending.wake_at(true).is_none());
        assert!(pending.due(overdue, false).is_some());
        assert!(pending.wake_at(false).is_some());
    }

    #[test]
    fn a_rearm_the_portal_keeps_refusing_gives_up_rather_than_retrying_for_ever() {
        let mut pending = PendingRearm::default();
        pending.want(some_exits());
        let mut now = Instant::now();
        for delay in REARM_RETRY_DELAYS {
            assert!(pending.due(now, false).is_some());
            pending.refused(now);
            now += delay;
        }
        assert!(pending.due(now, false).is_some(), "the last attempt");
        pending.refused(now);
        assert!(
            pending
                .due(now + Duration::from_secs(3600), false)
                .is_none(),
            "a portal that has refused five times will not be talked round"
        );
        // A later layout change is a different answer and starts over, which is the
        // case the acceptance criterion is about.
        pending.want(some_exits());
        assert!(pending.due(now, false).is_some());
    }
}
