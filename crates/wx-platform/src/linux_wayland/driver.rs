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
            // A stored token the portal would not accept must not be tried again on
            // every subsequent launch: clear it so the next run asks the user once,
            // rather than failing silently forever.
            if had_token && failure.discards_token {
                tracing::info!("the stored portal restore token was not accepted; forgetting it");
                store.clear();
            }
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
    shared.activate(SESSION_CAPABILITIES);

    let reason = pump(live.connection, live.events, &live.session, stop).await;
    teardown(shared, live.session, reason).await;
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
            failure: Failure::from_ashpd(e),
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
        .map_err(Failure::from_ashpd)?
        .response()
        .map_err(Failure::from_ashpd)?;

    tracing::info!(
        restoring = restore_token.is_some(),
        "starting the portal session; a consent dialog appears unless a restore token covers it"
    );
    let started = proxy
        .start(session, None, Default::default())
        .await
        .map_err(Failure::from_ashpd)?
        .response()
        .map_err(Failure::from_ashpd)?;

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
    if granted.is_empty() {
        return Err(Failure::refused(
            "the desktop portal granted no input devices",
        ));
    }
    if !granted.contains(DeviceType::Keyboard) || !granted.contains(DeviceType::Pointer) {
        tracing::warn!(devices = ?granted, "the portal granted only some of the devices asked for");
    }

    let fd = proxy
        .connect_to_eis(session, Default::default())
        .await
        .map_err(Failure::from_ashpd)?;

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
    let closed = match session.receive_closed().await {
        Ok(stream) => Some(stream),
        Err(e) => {
            // Losing this only costs the D-Bus half of revocation detection; the ei
            // stream still notices. Not worth refusing a working session over.
            tracing::warn!(error = %e, "cannot watch the portal session for revocation over D-Bus");
            None
        }
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

    /// Turn a portal error into the decision the agent has to make.
    ///
    /// The distinction that matters is "the user can fix this" versus "there is no
    /// portal here". Reporting a headless machine as a permission problem sends the
    /// user hunting for a dialog that was never shown; reporting a refusal as
    /// unsupported hides the one thing they could act on.
    fn from_ashpd(e: ashpd::Error) -> Self {
        use ashpd::desktop::ResponseError;

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
            ashpd::Error::Zbus(e) => {
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
