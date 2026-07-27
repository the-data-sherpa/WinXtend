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
//! * barriers accepted on all four edges once placed the way [`barriers_for`]
//!   places them, and rejected otherwise — see that function for the geometry the
//!   compositor actually enforces;
//! * thirteen `Activated` signals across one session with no further consent, each
//!   carrying a real cursor position and the barrier that fired;
//! * `Release` handing the pointer back and the session re-arming immediately;
//! * keyboard, button and scroll events arriving as evdev codes and 120-unit
//!   detents, with no `ei_keyboard.modifiers` and no absolute motion at all.
//!
//! The consent dialog itself cannot be automated, and must not be: that click *is*
//! the security property. See the same note at the top of [`super::driver`].

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

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
        established = establish() => established,
    };

    let (live, mut events) = match established {
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
        barriers = live.barriers,
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

    let reason = pump(&live, &mut events, capture, rx, stop).await;
    capture.detach();
    teardown(shared, live, reason).await;
}

/// A capture session that has been granted, with its transport connected.
struct Live {
    portal: InputCapture,
    session: Session<InputCapture>,
    granted: BitFlags<PortalCapabilities>,
    connection: reis::event::Connection,
    /// Which edge each barrier id sits on and which zone it bounds, so an
    /// activation knows which way the pointer came in — and therefore which way is
    /// back out, and which screen it is really on.
    edges: Vec<(BarrierID, BarrierEdge, Zone)>,
    /// Every region the portal reported, kept rather than counted so an activation
    /// the compositor did not attribute to a barrier can still be clamped onto a
    /// screen that exists.
    zones: Vec<Zone>,
    barriers: usize,
}

impl Live {
    fn barrier(&self, id: BarrierID) -> Option<(BarrierEdge, Zone)> {
        self.edges
            .iter()
            .find(|(b, _, _)| *b == id)
            .map(|(_, e, z)| (*e, *z))
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
async fn establish() -> Result<(Live, reis::tokio::EiConvertEventStream), Aborted> {
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

    match negotiate(&portal, &session, granted).await {
        Ok(negotiated) => Ok((
            Live {
                portal,
                session,
                granted,
                connection: negotiated.connection,
                edges: negotiated.edges,
                zones: negotiated.zones,
                barriers: negotiated.barriers,
            },
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
    edges: Vec<(BarrierID, BarrierEdge, Zone)>,
    zones: Vec<Zone>,
    barriers: usize,
}

async fn negotiate(
    portal: &InputCapture,
    session: &Session<InputCapture>,
    granted: BitFlags<PortalCapabilities>,
) -> Result<Negotiated, Failure> {
    accept_granted(granted)?;

    let zones = portal
        .zones(session, Default::default())
        .await
        .map_err(|e| Failure::from_ashpd(e, Stage::AfterGrant))?
        .response()
        .map_err(|e| Failure::from_ashpd(e, Stage::AfterGrant))?;

    let regions: Vec<Zone> = zones.regions().iter().copied().map(zone_of).collect();
    let placed = barriers_for(&regions);
    // A plan whose id the portal cannot express is one this build asked for and
    // cannot name; dropping it loses an edge, which is far better than shifting
    // every later id and mis-attributing a refusal.
    let barriers: Vec<Barrier> = placed
        .iter()
        .filter_map(|p| Some(Barrier::new(BarrierID::new(p.id)?, p.position)))
        .collect();
    let response = portal
        .set_pointer_barriers(session, &barriers, zones.zone_set(), Default::default())
        .await
        .map_err(|e| Failure::from_ashpd(e, Stage::AfterGrant))?
        .response()
        .map_err(|e| Failure::from_ashpd(e, Stage::AfterGrant))?;

    // A barrier the compositor would not place is one edge the cursor cannot
    // leave by. Not fatal — the other three still work, and a node with one usable
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
    if edges.is_empty() {
        return Err(Failure::broken(
            "the compositor placed none of the pointer barriers, so nothing can start a capture",
        ));
    }

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
        barriers: edges.len(),
        edges,
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

    loop {
        tokio::select! {
            biased;

            _ = &mut stop => return Ended::Stopped,

            // `Some(..)` rather than a match on the option: a closed channel yields
            // `None` immediately and forever, and a branch that accepted it would
            // spin this thread at 100% for the rest of the session. Not matching
            // disables the branch instead, which is what "there is nobody left to
            // send commands" should mean.
            Some(command) = commands.recv() => {
                if let Err(e) = obey(live, command).await {
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
                    Some(ActivatedBarrier::Barrier(id)) => live.barrier(id),
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
        }
    }
}

/// Carry out one [`Command`] from the trait side.
async fn obey(live: &Live, command: Command) -> ashpd::Result<()> {
    match command {
        Command::Enable => {
            tracing::debug!("arming the pointer barriers");
            live.portal.enable(&live.session, Default::default()).await
        }
        Command::Disable => {
            tracing::debug!("disarming the pointer barriers");
            live.portal.disable(&live.session, Default::default()).await
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
}
