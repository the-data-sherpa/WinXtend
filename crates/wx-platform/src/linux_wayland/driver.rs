//! The thread that owns the portal session and the libei transport.
//!
//! Everything here is Linux-only and talks to a live desktop. The rules it enforces
//! live in [`super::session`], which is testable without one.
//!
//! # Why one thread with a runtime on it
//!
//! [`crate::traits`] is synchronous by design — a Windows low-level hook has no
//! runtime to await on — but the portal is request/response over D-Bus with `Request`
//! objects that answer on a signal, and the libei socket needs someone polling it for
//! as long as the session lives. Both are asynchronous and both are idle almost all
//! the time. So the backend spawns exactly one thread, gives it a current-thread
//! tokio runtime, and runs the D-Bus session and the `ei` event loop on it together.
//! Two threads would buy nothing and would need a channel between them.
//!
//! # Why `reis` rather than `libei`
//!
//! `reis` is a pure-Rust implementation of the same protocol, so the build needs no
//! `libei-dev`, no `bindgen`, and no C toolchain on any machine — including CI, which
//! does not have a desktop session to test against but does have to compile this.
//!
//! # What is tested here, and what is verified by hand
//!
//! The tests at the bottom of this file cover the decisions this module makes about
//! errors — which failure is the user's, which is ours, and which one throws the
//! stored restore token away — because those are pure functions of a portal error and
//! need no desktop.
//!
//! The granted branch of the sequence cannot be automated: it is gated on a human
//! pressing *Share* on the compositor's consent dialog, and that click **is** the
//! security property this module implements. Do not drive it with `ydotool` or any
//! other synthetic input — doing so on a desktop that is in use is unsafe, and a test
//! that clicks its own consent dialog tests nothing. It is verified by hand instead.
//!
//! Verified by hand on Ubuntu 26.04 / GNOME Shell 50 / `xdg-desktop-portal` 1.21.1
//! against this revision:
//!
//! * the full `CreateSession` → `SelectDevices` (`KEYBOARD|POINTER`, `persist_mode=2`)
//!   → `Start` → `ConnectToEIS` sequence;
//! * the libei seat ("mutter default seat") and the three devices mutter offers —
//!   virtual pointer, virtual keyboard, shared virtual absolute pointer;
//! * the restore token persisted `0o600`, 36 bytes; a relaunch reporting
//!   `restoring=true` and granted ~3.6 ms later with no dialog;
//! * capabilities moving `0` → `3` and being re-advertised, with the gaining edge
//!   starting capture;
//! * a dismissed dialog reported as `PermissionDenied` with exactly one `Start`
//!   attempt and no retry, leaving no token file behind;
//! * clean teardown of a live session in ~260 ms.
//!
//! Not proven, deliberately: portal-*initiated* revocation through the shell's
//! screen-sharing indicator. An external `Close` on the session object is refused with
//! "Access denied" because session handles are per D-Bus connection, so only
//! close-from-our-own-connection was exercised — it produces `DeviceRemoved`,
//! `SeatRemoved`, then `Disconnected` on the `ei` stream. The two-machine validation
//! issue covers the rest on real hardware.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use tokio::sync::oneshot;

use super::session::{SharedSession, SESSION_CAPABILITIES};
use super::token::RestoreTokenStore;

/// Name this client reports to the compositor.
///
/// It is user-visible: the probe on a GNOME 50 desktop showed mutter naming the
/// devices it creates after it — "WinXtend virtual keyboard" — and that string is
/// what a user sees when they go looking for what is controlling their machine.
const EI_CLIENT_NAME: &str = "WinXtend";

/// Device types the session asks for.
///
/// Asked for once, for both directions: the portal grants one session covering
/// capture and injection, so a second `CreateSession` would mean a second consent
/// dialog and a second thing to revoke.
fn wanted_devices() -> BitFlags<DeviceType> {
    DeviceType::Keyboard | DeviceType::Pointer
}

/// How long teardown may take before the thread gives up and exits anyway.
///
/// Bounded because [`Driver::drop`] joins this thread: an unresponsive D-Bus would
/// otherwise hang the agent's shutdown indefinitely, which is worse than leaving a
/// session for the portal to reap when the connection drops.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Handle to the running session. Dropping it tears the session down.
pub struct Driver {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// Start acquiring the portal session on a thread of its own.
///
/// Returns immediately: the consent dialog can sit on screen for as long as the user
/// takes, and nothing above this may block on that. Progress and failure are reported
/// through `shared`.
pub fn start(shared: Arc<SharedSession>, config_dir: PathBuf) -> Driver {
    shared.starting();
    let (stop_tx, stop_rx) = oneshot::channel();
    let thread_shared = Arc::clone(&shared);

    let spawned = std::thread::Builder::new()
        .name("wx-portal".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    thread_shared.failed(format!("starting the portal runtime: {e}"));
                    return;
                }
            };
            runtime.block_on(run(&thread_shared, &config_dir, stop_rx));
        });

    match spawned {
        Ok(thread) => Driver {
            stop: Some(stop_tx),
            thread: Some(thread),
        },
        Err(e) => {
            shared.failed(format!("starting the portal thread: {e}"));
            Driver {
                stop: None,
                thread: None,
            }
        }
    }
}

impl Drop for Driver {
    /// Tear the session down and wait for the thread to finish.
    ///
    /// Joining rather than detaching is the difference between "clean teardown" and
    /// an orphan thread holding a portal session open past the agent's exit. It is
    /// safe to wait for because every path inside [`run`] is bounded by
    /// [`TEARDOWN_TIMEOUT`].
    fn drop(&mut self) {
        // A closed channel is how the thread learns to stop; the receiver sees the
        // sender drop even if this send finds nobody listening.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            if let Err(e) = thread.join() {
                tracing::warn!(error = ?e, "the portal thread panicked");
            }
        }
    }
}

async fn run(
    shared: &SharedSession,
    config_dir: &std::path::Path,
    mut stop: oneshot::Receiver<()>,
) {
    let store = RestoreTokenStore::in_dir(config_dir);
    let had_token = store.load().is_some();

    // Shutdown has to be able to interrupt the sequence, not just the event loop that
    // follows it. `Start` blocks until the user answers the consent dialog, and there
    // is no upper bound on that — a dialog behind a full-screen window can sit there
    // for hours. Without this branch, `Driver::drop` would join a thread waiting on it
    // and the agent would never exit.
    let established = tokio::select! {
        biased;
        _ = &mut stop => {
            tracing::debug!("shutting down before the portal session was granted");
            shared.stopped();
            return;
        }
        established = establish(&store) => established,
    };

    let live = match established {
        Ok(live) => live,
        Err(Aborted { failure, session }) => {
            forget_rejected_token(&store, had_token, &failure);
            failure.report(shared);
            // A session the portal already granted outlives this thread — nothing
            // drops it and ashpd keeps the bus connection process-wide — so the
            // compositor would go on showing this machine as remotely controlled by
            // something that will never use the session.
            if let Some(session) = session {
                close_session(session).await;
            }
            return;
        }
    };

    tracing::info!(
        devices = ?live.granted,
        "the desktop portal granted a RemoteDesktop session"
    );
    // The whole set, not a subset of it: a grant missing either device type never
    // reaches here, because `accept_granted` refuses it.
    shared.activate(SESSION_CAPABILITIES);

    let reason = pump(live.connection, live.events, &live.session, stop).await;
    teardown(shared, live.session, reason).await;
}

/// Throw away a stored restore token the portal would not accept.
///
/// A token the portal refuses is not something a later launch can fix by itself: the
/// same bytes would be sent again and rejected the same way, leaving this backend
/// terminally [`super::SessionState::Failed`] with no way back. Forgetting it costs
/// the user one consent dialog and gets the session back, which is why the token is
/// only ever kept for failures that might not repeat.
fn forget_rejected_token(store: &RestoreTokenStore, had_token: bool, failure: &Failure) {
    if had_token && failure.discards_token {
        tracing::info!("the stored portal restore token was not accepted; forgetting it");
        store.clear();
    }
}

/// A portal session that has been granted, with its transport connected.
struct Live {
    session: Session<RemoteDesktop>,
    granted: BitFlags<DeviceType>,
    connection: reis::event::Connection,
    events: reis::tokio::EiConvertEventStream,
}

/// Why the event loop stopped.
enum Ended {
    /// The agent is shutting down.
    Stopped,
    /// The portal or the compositor took the session away.
    Revoked(String),
    /// The transport broke without anybody deciding to end it.
    Broken(String),
}

/// A failed [`establish`], paired with the portal session that was created before it
/// failed, if the sequence got that far.
struct Aborted {
    failure: Failure,
    session: Option<Session<RemoteDesktop>>,
}

impl Aborted {
    /// A failure from before `CreateSession` answered, so there is nothing to close.
    fn before_session(e: ashpd::Error) -> Self {
        Self {
            // Nothing before `SelectDevices` carries the restore token, so whatever
            // went wrong here cannot be the token's fault.
            failure: Failure::from_ashpd(e, TokenSent::No),
            session: None,
        }
    }
}

/// Run the whole `CreateSession` → `SelectDevices` → `Start` → `ConnectToEIS`
/// sequence and bring the `ei` connection up.
async fn establish(store: &RestoreTokenStore) -> Result<Live, Aborted> {
    let proxy = RemoteDesktop::new()
        .await
        .map_err(Aborted::before_session)?;
    tracing::debug!(version = proxy.version(), "portal RemoteDesktop interface");

    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(Aborted::before_session)?;

    match negotiate(&proxy, &session, store).await {
        Ok(granted) => Ok(Live {
            session,
            granted: granted.devices,
            connection: granted.connection,
            events: granted.events,
        }),
        Err(failure) => Err(Aborted {
            failure,
            session: Some(session),
        }),
    }
}

/// Everything [`establish`] does once the session object exists.
///
/// Split out so that every failure from here on hands the session back to the caller
/// to close, rather than abandoning it to the portal.
async fn negotiate(
    proxy: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    store: &RestoreTokenStore,
) -> Result<Negotiated, Failure> {
    // The restore token rides on SelectDevices, not on CreateSession: it is part of
    // *what* is being asked for, and the portal matches it against the device types
    // requested. Getting this wrong is silent — the session works and prompts every
    // time.
    let restore_token = store.load();
    // Carried through every classification below, so that "the portal would not take
    // this token" can only be concluded about a request that actually sent one.
    let sent = if restore_token.is_some() {
        TokenSent::Yes
    } else {
        TokenSent::No
    };
    proxy
        .select_devices(
            session,
            SelectDevicesOptions::default()
                .set_devices(wanted_devices())
                .set_restore_token(restore_token.as_deref())
                // Persist until the user revokes it, rather than for the lifetime of
                // this process: the agent restarts with the machine, and a token that
                // died with the process would prompt at every boot.
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await
        .map_err(|e| Failure::from_ashpd(e, sent))?
        .response()
        .map_err(|e| Failure::from_ashpd(e, sent))?;

    tracing::info!(
        restoring = restore_token.is_some(),
        "starting the portal session; a consent dialog appears unless a restore token covers it"
    );
    // `TokenSent::No` from here on: only `SelectDevices` carried the token, so an
    // argument the portal will not take on a later call is an argument of ours.
    // Reading one as a rejected token would delete a good one and cost the user a
    // consent dialog on the next launch.
    let started = proxy
        .start(session, None, Default::default())
        .await
        .map_err(|e| Failure::from_ashpd(e, TokenSent::No))?
        .response()
        .map_err(|e| Failure::from_ashpd(e, TokenSent::No))?;

    // Checked before the token is persisted: a token is the portal's promise to
    // grant the same thing again without asking, and persisting one for a grant
    // this session is about to refuse would make every later launch restore the
    // same half grant silently, with no dialog left to fix it at.
    accept_granted(started.devices())?;

    // Persisted before anything else can fail: a token thrown away because the libei
    // connection did not come up would cost the user another dialog for a problem
    // that had nothing to do with consent.
    match started.restore_token() {
        Some(token) => {
            if let Err(e) = store.save(token) {
                // Not fatal. The session in hand is fine; only the next launch pays.
                tracing::warn!(error = %e, "could not persist the portal restore token; the next launch will prompt");
            } else {
                tracing::debug!("portal restore token persisted");
            }
        }
        None => tracing::warn!(
            "the portal returned no restore token; every launch will show a consent dialog"
        ),
    }

    let granted = started.devices();

    let fd = proxy
        .connect_to_eis(session, Default::default())
        .await
        .map_err(|e| Failure::from_ashpd(e, TokenSent::No))?;

    let context = ei::Context::new(UnixStream::from(fd))
        .map_err(|e| Failure::broken(format!("opening the libei transport: {e}")))?;
    let (connection, events) = context
        .handshake_tokio(EI_CLIENT_NAME, ei::handshake::ContextType::Sender)
        .await
        .map_err(|e| Failure::broken(format!("the libei handshake failed: {e}")))?;
    tracing::debug!("libei transport connected");

    Ok(Negotiated {
        devices: granted,
        connection,
        events,
    })
}

/// Refuse a grant that is missing either device type.
///
/// [`wx_proto::Capabilities`] has no per-device granularity: the session either
/// advertises `CAPTURE_INPUT | INJECT_INPUT` or it advertises nothing. So a grant
/// covering only the pointer has no honest way to be published — claiming keyboard
/// capability the session cannot deliver would have peers route keystrokes here
/// that silently go nowhere, against the rule this backend is built on that it
/// advertises nothing it cannot do.
///
/// Reported as a refusal rather than a fault because that is what it is: the portal
/// answered the request, and what came back is less than was asked for. The message
/// names the missing device so a user who mis-clicked the dialog can see which box
/// to tick next time.
fn accept_granted(granted: BitFlags<DeviceType>) -> Result<(), Failure> {
    let missing: Vec<&str> = [
        (DeviceType::Keyboard, "keyboard"),
        (DeviceType::Pointer, "pointer"),
    ]
    .into_iter()
    .filter(|(device, _)| !granted.contains(*device))
    .map(|(_, name)| name)
    .collect();

    if missing.is_empty() {
        return Ok(());
    }
    Err(Failure::refused(format!(
        "the desktop portal withheld {} access; this machine needs keyboard and pointer together, so the session was given back",
        missing.join(" and ")
    )))
}

/// What [`negotiate`] produces: everything a [`Live`] needs except the session.
struct Negotiated {
    devices: BitFlags<DeviceType>,
    connection: reis::event::Connection,
    events: reis::tokio::EiConvertEventStream,
}

/// Drive the `ei` connection until the session ends.
///
/// Also the revocation detector. Two things can report that the session is gone and
/// they do not always both fire: the portal's `Closed` signal is not emitted when the
/// session ends by our own hand, and a compositor that drops the EIS socket may not
/// send `Closed` at all. Watching both is what makes revocation reliable — measured
/// on GNOME 50, losing the session arrives as `DeviceRemoved`, `SeatRemoved`, then
/// `Disconnected` on the `ei` stream.
async fn pump(
    connection: reis::event::Connection,
    mut events: reis::tokio::EiConvertEventStream,
    session: &Session<RemoteDesktop>,
    mut stop: oneshot::Receiver<()>,
) -> Ended {
    // Subscribing is a D-Bus round trip, and shutdown has to be able to interrupt it
    // like everything else in here: `Driver::drop` joins this thread on the promise
    // that no path is bounded only by however long the bus takes to answer.
    let closed = tokio::select! {
        biased;
        _ = &mut stop => return Ended::Stopped,
        subscribed = session.receive_closed() => match subscribed {
            Ok(stream) => Some(stream),
            Err(e) => {
                // Losing this only costs the D-Bus half of revocation detection; the
                // ei stream still notices. Not worth refusing a working session over.
                tracing::warn!(error = %e, "cannot watch the portal session for revocation over D-Bus");
                None
            }
        },
    };
    let mut closed = std::pin::pin!(OptionStream(closed));

    loop {
        tokio::select! {
            // Shutdown wins over a revocation arriving in the same instant: the
            // session is going away either way and the user has nothing to fix.
            biased;

            _ = &mut stop => return Ended::Stopped,

            signal = closed.next() => match signal {
                // The portal really closed the session: somebody decided this.
                Some(_) => {
                    return Ended::Revoked(
                        "the desktop portal closed the remote desktop session".into(),
                    );
                }
                // The signal stream only ends when the bus connection does, and a
                // D-Bus that went away is not a decision anybody made.
                None => {
                    return Ended::Broken(
                        "the connection to the desktop portal went away".into(),
                    );
                }
            },

            event = events.next() => match event {
                Some(Ok(event)) => {
                    if let Some(ended) = on_ei_event(&connection, event) {
                        return ended;
                    }
                }
                Some(Err(e)) => {
                    return Ended::Broken(format!("the libei transport failed: {e}"));
                }
                None => {
                    // EOF. The compositor hung up, which is what revocation looks
                    // like from this side when D-Bus says nothing.
                    return Ended::Revoked(
                        "the compositor closed the libei transport".into(),
                    );
                }
            },
        }
    }
}

/// Handle one `ei` event, returning `Some` if it ended the session.
fn on_ei_event(connection: &reis::event::Connection, event: EiEvent) -> Option<Ended> {
    match event {
        EiEvent::SeatAdded(added) => {
            // Nothing is offered until the capabilities are bound, so this is what
            // makes the compositor create the devices at all. Asking for everything
            // the session covers now means #6 and #7 have their devices without
            // renegotiating — and without a second dialog.
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
            tracing::debug!(seat = ?added.seat.name(), "libei seat bound");
            None
        }
        EiEvent::DeviceAdded(added) => {
            tracing::debug!(
                device = ?added.device.name(),
                keyboard = added.device.has_capability(DeviceCapability::Keyboard),
                pointer = added.device.has_capability(DeviceCapability::Pointer),
                absolute = added.device.has_capability(DeviceCapability::PointerAbsolute),
                "libei device offered"
            );
            None
        }
        EiEvent::Disconnected(gone) => {
            let detail = format!(
                "the compositor disconnected the libei transport ({:?})",
                gone.reason
            );
            // Only reason zero means the client was purposely disconnected. The rest
            // — a protocol violation, a rejected handshake mode, a transport error —
            // are faults on one side or the other, and telling the user their
            // permission was withdrawn would send them to a settings panel to fix a
            // bug that is ours.
            match gone.reason {
                ei::connection::DisconnectReason::Disconnected => Some(Ended::Revoked(detail)),
                _ => Some(Ended::Broken(detail)),
            }
        }
        _ => None,
    }
}

/// Close the session and record why it ended.
async fn teardown(shared: &SharedSession, session: Session<RemoteDesktop>, reason: Ended) {
    // The state is set before the close is attempted, so a portal that never answers
    // cannot leave the agent advertising capabilities it no longer has.
    match &reason {
        Ended::Stopped => shared.stopped(),
        Ended::Revoked(why) => {
            tracing::warn!(reason = %why, "the portal session was revoked; input capture and injection are no longer available");
            shared.denied(why.clone());
        }
        Ended::Broken(why) => {
            tracing::warn!(reason = %why, "the portal session broke");
            shared.failed(why.clone());
        }
    }

    close_session(session).await;
}

/// Hand a session back to the portal, bounded by [`TEARDOWN_TIMEOUT`].
///
/// Closing a session the portal already closed fails, and that is fine: the point is
/// to leave nothing behind when *we* are the ones ending it.
async fn close_session(session: Session<RemoteDesktop>) {
    match tokio::time::timeout(TEARDOWN_TIMEOUT, session.close()).await {
        Ok(Ok(())) => tracing::debug!("portal session closed"),
        Ok(Err(e)) => tracing::debug!(error = %e, "the portal session was already gone"),
        Err(_) => tracing::warn!("timed out closing the portal session"),
    }
}

/// How a failure during [`establish`] should be reported.
struct Failure {
    detail: String,
    kind: FailureKind,
    /// Whether a stored restore token should be thrown away because of it.
    discards_token: bool,
}

enum FailureKind {
    Denied,
    Unsupported,
    Broken,
}

/// Whether the request that failed carried the stored restore token.
///
/// Threaded in explicitly rather than read back off disk, so that classification stays
/// a pure function of the portal error and "the token was rejected" can never be
/// concluded about a request that sent no token — which would silently delete a good
/// one over an unrelated fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenSent {
    Yes,
    No,
}

impl Failure {
    fn denied(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: FailureKind::Denied,
            discards_token: false,
        }
    }

    /// The user was asked and said no.
    ///
    /// Distinct from [`Failure::denied`] only in what it does to the stored token.
    /// A dialog appearing at all means the token we hold did not cover this request,
    /// so keeping it would mean sending something the portal has already ignored on
    /// every launch from here on.
    fn refused(detail: impl Into<String>) -> Self {
        Self {
            discards_token: true,
            ..Self::denied(detail)
        }
    }

    fn broken(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: FailureKind::Broken,
            discards_token: false,
        }
    }

    fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: FailureKind::Unsupported,
            discards_token: false,
        }
    }

    /// The portal threw the request out over what was in it, and the restore token is
    /// the only thing in it that can go bad.
    ///
    /// Still [`FailureKind::Broken`], not a refusal: a rejected token is not a
    /// decision anybody made, and surfacing it as
    /// [`crate::PlatformError::PermissionDenied`] would send the user to a permission
    /// panel over a damaged file. Only the token goes — and it must, because the same
    /// bytes would be rejected identically on every launch from here on.
    fn rejected_token(detail: impl Into<String>) -> Self {
        Self {
            discards_token: true,
            ..Self::broken(detail)
        }
    }

    /// Turn a portal error into the decision the agent has to make.
    ///
    /// The distinction that matters is "the user can fix this" versus "there is no
    /// portal here". Reporting a headless machine as a permission problem sends the
    /// user hunting for a dialog that was never shown; reporting a refusal as
    /// unsupported hides the one thing they could act on.
    fn from_ashpd(e: ashpd::Error, sent: TokenSent) -> Self {
        use ashpd::desktop::ResponseError;
        use ashpd::PortalError;

        match e {
            ashpd::Error::Response(ResponseError::Cancelled) => {
                Self::refused("the desktop portal consent dialog was dismissed")
            }
            // The portal's catch-all. It covers a compositor that failed as well as
            // one that refused, so the token stays: throwing away a good token over a
            // transient failure would cost the user a dialog they need not have seen.
            ashpd::Error::Response(ResponseError::Other) => {
                Self::denied("the desktop portal refused the remote desktop session")
            }
            // The desktop has no RemoteDesktop backend, or one too old for
            // `ConnectToEIS`. Either way no amount of consenting helps.
            ashpd::Error::PortalNotFound(name) => {
                Self::unsupported(format!("this desktop has no {name} portal"))
            }
            ashpd::Error::RequiresVersion(needed, found) => Self::unsupported(format!(
                "the desktop portal is version {found}; the libei transport needs {needed}"
            )),
            // The portal rejected the request's arguments while we were sending a
            // stored token — a corrupted or hand-edited token file, which nothing but
            // forgetting it will ever clear. Deliberately narrow: this is the one
            // error that plainly means "these arguments are not acceptable", and on
            // the live portal a token that is not a valid UUID comes back exactly so.
            // Widening it to failures that might not repeat would trade a wedged
            // backend for a token deleted over a transient fault.
            ashpd::Error::Portal(PortalError::InvalidArgument(detail))
                if sent == TokenSent::Yes =>
            {
                Self::rejected_token(format!(
                    "the desktop portal rejected the stored restore token: {detail}"
                ))
            }
            // Every *method* error on a portal proxy is routed through `PortalError`,
            // and the ones whose D-Bus name is not one of the portal's own are handed
            // back through its `ZBus` variant — so "there is no portal on this bus"
            // arrives here rather than as a bare `Error::Zbus`. A bare one still turns
            // up from connecting to the bus in the first place, which is the headless
            // case, so both shapes get the same reading.
            ashpd::Error::Zbus(e) | ashpd::Error::Portal(PortalError::ZBus(e)) => {
                if no_session_bus(&e) {
                    Self::unsupported(format!("no desktop session to ask for permission: {e}"))
                } else {
                    Self::broken(format!("talking to the desktop portal: {e}"))
                }
            }
            other => Self::broken(format!("the desktop portal request failed: {other}")),
        }
    }

    fn report(self, shared: &SharedSession) {
        match self.kind {
            FailureKind::Denied => {
                tracing::warn!(reason = %self.detail, "the desktop portal refused input permission; this node cannot capture or inject");
                shared.denied(self.detail);
            }
            FailureKind::Unsupported => {
                tracing::info!(reason = %self.detail, "no usable desktop portal; this node can still be driven by a peer");
                shared.unsupported(self.detail);
            }
            FailureKind::Broken => {
                tracing::warn!(reason = %self.detail, "the desktop portal session could not be established");
                shared.failed(self.detail);
            }
        }
    }
}

/// Whether a D-Bus error means "there is no session bus here" rather than "the call
/// went wrong".
///
/// This is the headless and CI case, and it has to come out as
/// [`crate::PlatformError::Unsupported`] with a clear message rather than as a
/// permission problem nobody can fix.
fn no_session_bus(e: &ashpd::zbus::Error) -> bool {
    match e {
        // No `DBUS_SESSION_BUS_ADDRESS`, or one pointing at a socket that is not
        // there: exactly what a CI container and a bare tty look like.
        ashpd::zbus::Error::Address(_) | ashpd::zbus::Error::InputOutput(_) => true,
        // The bus exists but nothing implements the portal — a desktop-less user
        // session with dbus running.
        ashpd::zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
                || name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner"
        }
        _ => false,
    }
}

/// Adapts "maybe a stream" into a stream that simply never yields when absent.
///
/// Lets the `select!` in [`pump`] treat a missing `Closed` watcher as a branch that
/// never fires, instead of needing a second copy of the loop.
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

/// The two branches a portal failure has to be kept apart into: one the user decided
/// and one nobody did.
///
/// Neither needs a desktop — see the note at the top of this file for what does, and
/// why it is verified by hand rather than automated.
#[cfg(test)]
mod tests {
    use super::*;

    use ashpd::desktop::ResponseError;
    use wx_proto::Capabilities;

    use crate::error::PlatformError;
    use crate::LiveCapabilities;

    use super::super::session::SessionState;

    /// What the portal answers with when a request's arguments are not acceptable.
    const INVALID_ARGUMENT: &str = "org.freedesktop.portal.Error.InvalidArgument";

    /// A D-Bus error raised by a portal method call, in the shape `ashpd` actually
    /// hands back.
    ///
    /// Built by running the raw `zbus` error through the same `PortalError`
    /// conversion `ashpd` applies to every method call, so these tests cannot pass on
    /// a shape the portal never produces. On the live portal a damaged restore token
    /// arrives exactly so: `org.freedesktop.portal.Error.InvalidArgument: Restore
    /// token is not a valid UUID string`.
    fn method_error(name: &str, detail: &str) -> ashpd::Error {
        let reply = ashpd::zbus::message::Message::method_call(
            "/org/freedesktop/portal/desktop",
            "SelectDevices",
        )
        .unwrap()
        .build(&())
        .unwrap();
        ashpd::Error::Portal(ashpd::PortalError::from(ashpd::zbus::Error::MethodError(
            ashpd::zbus::names::OwnedErrorName::try_from(name.to_string()).unwrap(),
            Some(detail.to_string()),
            reply,
        )))
    }

    /// A session on a backend that advertises nothing without the portal, so
    /// these tests see the session's own contribution and nothing else.
    fn session() -> (SharedSession, LiveCapabilities) {
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        (SharedSession::new(live.clone(), Capabilities::NONE), live)
    }

    /// A scratch config directory, cleaned up on drop, so this needs no
    /// dev-dependency and leaves nothing behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("wx-driver-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_restore_token_the_portal_rejects_is_not_reported_as_a_permission_problem() {
        // A token the portal will not take is nobody's decision, so it must not send
        // the user off to revoke and re-grant a permission that is fine.
        let failure = Failure::from_ashpd(
            method_error(INVALID_ARGUMENT, "Restore token is not a valid UUID string"),
            TokenSent::Yes,
        );
        assert!(matches!(failure.kind, FailureKind::Broken));
        assert!(
            failure.discards_token,
            "a token the portal rejected must not be sent again"
        );

        let (session, live) = session();
        session.starting();
        failure.report(&session);

        assert_eq!(session.state(), SessionState::Failed);
        assert!(live.get().is_empty());
        assert!(!matches!(
            session.error(),
            Some(PlatformError::PermissionDenied(_))
        ));
    }

    #[test]
    fn the_launch_after_a_rejected_token_starts_with_no_token_at_all() {
        // The self-heal: without this the same bad file is resent forever and the
        // backend stays Failed with nothing the user can do about it.
        let dir = TempDir::new("rejected");
        let store = RestoreTokenStore::in_dir(&dir.0);
        store.save("not-a-uuid").unwrap();
        let had_token = store.load().is_some();

        let failure = Failure::from_ashpd(
            method_error(INVALID_ARGUMENT, "Restore token is not a valid UUID string"),
            TokenSent::Yes,
        );
        forget_rejected_token(&store, had_token, &failure);

        assert_eq!(
            RestoreTokenStore::in_dir(&dir.0).load(),
            None,
            "the next launch must ask the user once rather than fail identically"
        );
    }

    #[test]
    fn a_rejection_of_something_other_than_the_token_leaves_a_good_token_alone() {
        // Both halves of the guard: the same error against a request that carried no
        // token, and a different error against one that did. Deleting a working token
        // over either would cost a dialog for a problem it had nothing to do with.
        let no_token = Failure::from_ashpd(
            method_error(INVALID_ARGUMENT, "some other argument"),
            TokenSent::No,
        );
        assert!(matches!(no_token.kind, FailureKind::Broken));
        assert!(!no_token.discards_token);

        let other_error = Failure::from_ashpd(
            method_error("org.freedesktop.DBus.Error.NoReply", "timed out"),
            TokenSent::Yes,
        );
        assert!(matches!(other_error.kind, FailureKind::Broken));
        assert!(!other_error.discards_token);
    }

    #[test]
    fn a_user_refusal_is_a_permission_problem_that_nothing_retries() {
        let failure = Failure::from_ashpd(
            ashpd::Error::Response(ResponseError::Cancelled),
            TokenSent::Yes,
        );
        assert!(matches!(failure.kind, FailureKind::Denied));
        assert!(
            failure.discards_token,
            "a dialog appearing at all proves the stored token did not cover the request"
        );

        let (session, live) = session();
        session.starting();
        failure.report(&session);

        assert_eq!(session.state(), SessionState::Denied);
        assert!(live.get().is_empty());
        assert!(matches!(
            session.error(),
            Some(PlatformError::PermissionDenied(_))
        ));

        // Terminal: nothing may put the consent dialog back in front of a user who
        // already said no, so a later attempt to start cannot take the refusal back.
        assert!(session.state().is_terminal());
        session.starting();
        session.activate(SESSION_CAPABILITIES);
        assert_eq!(session.state(), SessionState::Denied);
        assert!(live.get().is_empty());
    }

    #[test]
    fn a_grant_covering_both_devices_is_accepted() {
        assert!(accept_granted(wanted_devices()).is_ok());
    }

    #[test]
    fn a_half_grant_is_refused_rather_than_advertised_as_both() {
        // `Capabilities` cannot say "pointer but not keyboard", so a session that
        // published anything here would be claiming a capability it cannot honour
        // and peers would send keystrokes into a void. Refusing says so instead.
        let failure = accept_granted(DeviceType::Pointer.into()).unwrap_err();
        assert!(matches!(failure.kind, FailureKind::Denied));
        assert!(
            failure.detail.contains("withheld keyboard"),
            "the message must name what was withheld: {}",
            failure.detail
        );
        assert!(!failure.detail.contains("withheld pointer"));

        let (session, live) = session();
        session.starting();
        failure.report(&session);

        assert_eq!(session.state(), SessionState::Denied);
        assert!(
            live.get().is_empty(),
            "a half grant must advertise neither capability"
        );
        assert!(matches!(
            session.error(),
            Some(PlatformError::PermissionDenied(_))
        ));
    }

    #[test]
    fn a_half_grant_does_not_leave_a_token_that_would_restore_it_silently() {
        // The self-heal for the other direction: a token for a grant that is refused
        // would restore the same half grant on every later launch with no dialog, so
        // the session would be wedged with nothing the user could act on.
        let no_keyboard = accept_granted(DeviceType::Pointer.into()).unwrap_err();
        assert!(no_keyboard.discards_token);

        let none_at_all = accept_granted(BitFlags::empty()).unwrap_err();
        assert!(none_at_all.discards_token);
        assert!(none_at_all.detail.contains("withheld keyboard and pointer"));
    }

    #[test]
    fn a_headless_machine_is_still_unsupported_rather_than_a_rejected_token() {
        // The no-session-bus check has to keep winning over the new branch, or CI
        // would report a missing desktop as a token problem. A bus with nothing
        // implementing the portal answers a method call this way, and the name is not
        // the portal's own, so `ashpd` passes it back through `PortalError::ZBus`.
        let failure = Failure::from_ashpd(
            method_error(
                "org.freedesktop.DBus.Error.ServiceUnknown",
                "no such service",
            ),
            TokenSent::Yes,
        );
        assert!(matches!(failure.kind, FailureKind::Unsupported));
        assert!(!failure.discards_token);

        // And the case with no bus at all, which fails before any method call and so
        // is the one shape that really does arrive as a bare `zbus` error.
        let failure = Failure::from_ashpd(
            ashpd::Error::Zbus(ashpd::zbus::Error::Address("no bus here".to_string())),
            TokenSent::Yes,
        );
        assert!(matches!(failure.kind, FailureKind::Unsupported));
        assert!(!failure.discards_token);
    }
}
