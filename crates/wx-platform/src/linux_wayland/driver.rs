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
//! against this branch:
//!
//! * the full `CreateSession` → `SelectDevices` (`KEYBOARD|POINTER`, `persist_mode=2`)
//!   → `Start` → `ConnectToEIS` sequence;
//! * the libei seat ("mutter default seat") and the three devices mutter offers —
//!   virtual pointer, virtual keyboard, shared virtual absolute pointer;
//! * the restore token persisted `0o600`, 36 bytes; a relaunch reporting
//!   `restoring=true` and granted ~3.6 ms later with no dialog;
//! * capabilities moving `0` → `3` and being re-advertised, with the gaining edge
//!   starting capture, and back to `0` when the session ended;
//! * a dismissed dialog reported as `PermissionDenied` with exactly one `Start`
//!   attempt and no retry, leaving no token file behind;
//! * clean teardown of a live session in ~260 ms.
//!
//! The capability transition in that list was observed against a revision whose
//! session published `CAPTURE_INPUT | INJECT_INPUT` together. Capture comes from
//! the `InputCapture` portal and has a session of its own, so this session
//! publishes [`INJECT_CAPABILITIES`] and — separately —
//! [`CLIPBOARD_CAPABILITIES`]. The publish path is unchanged.
//!
//! Verified by hand on the same desktop while #8 added the clipboard, driving the
//! real `WaylandClipboard` with `wl-copy`/`wl-paste` as the application at the
//! other end of the selection:
//!
//! * `CreateSession` → `SelectDevices` → **`RequestClipboard`** → `Start` granted
//!   `devices=KEYBOARD|POINTER` and `clipboard_enabled=true` together, from one
//!   dialog, publishing `INJECT_INPUT, CLIPBOARD_TEXT, CLIPBOARD_IMAGE`;
//! * the restore token persisted for a clipboard-carrying request and restored on
//!   the next launch with `restoring=true` and no dialog;
//! * `SelectionOwnerChanged` arrives **immediately after `Start`** carrying the
//!   selection that was already on the clipboard, which is why the subscription is
//!   taken out before `Start` rather than after;
//! * all four formats round-tripping byte-exact in both directions, non-ASCII
//!   included; 24 MiB read in 262 ms and written in 414 ms with the event loop
//!   still answering;
//! * `SelectionRead` **failing** with `org.freedesktop.portal.Error.Failed:
//!   Internal error` for a selection the calling session owns — see
//!   [`super::clipboard::ClipboardState::read`], which is why a read of our own
//!   offer is answered locally.
//!
//! Not proven on hardware: a session granted with the clipboard toggle *off*. The
//! GNOME dialog does carry the switch — `xdg-desktop-portal-gnome` has
//! `allow_remote_clipboard_switch` — but the run made to exercise it came back
//! with `clipboard_enabled=true`, so only the tests cover that branch.
//!
//! Also verified by hand on the same desktop while #6 landed, through the real
//! `WaylandInjector` rather than a scratch client — see the note at the top of
//! [`super::inject`] for what the session's devices turn out to be good for:
//!
//! * `ei_text` is **not** offered: the target ships libei 1.5.0, whose EIS side does
//!   not implement the interface, so the capability is never negotiated even though
//!   this client asks for it;
//! * `RemoteDesktop.NotifyKeyboardKeysym` is refused once `ConnectToEIS` has been
//!   called — *"Session is not allowed to call NotifyKeyboard methods"* — so the
//!   portal's D-Bus input methods and libei are mutually exclusive and the choice
//!   made here is final for the session;
//! * the absolute pointer's region matches the monitor rectangle
//!   [`super::display`] reports exactly, offsets and all, so wire positions need no
//!   scaling to reach it.
//!
//! Not proven, deliberately: portal-*initiated* revocation through the shell's
//! screen-sharing indicator. An external `Close` on the session object is refused with
//! "Access denied" because session handles are per D-Bus connection, so only
//! close-from-our-own-connection was exercised — it produces `DeviceRemoved`,
//! `SeatRemoved`, then `Disconnected` on the `ei` stream. The two-machine validation
//! issue covers the rest on real hardware.
//!
//! Also not proven on hardware: the startup retry ([`STARTUP_RETRY_DELAYS`]). It
//! exists for an agent that reaches the portal before `xdg-desktop-portal` will
//! answer, which is a race at login on a machine that boots — not something a
//! developer session can be made to lose on demand, and not something worth
//! *winning* by hand-waving either. The failure is injected instead: the decision is
//! a pure function of a portal error and the loop is driven with a schedule of
//! zero-length waits, so the rules that matter — a refusal is never retried, nothing
//! past the dialog is retried, and no retry happens without a restore token to make
//! it silent — are proven rather than observed. What a real late portal does is for
//! the two-machine validation issue.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ashpd::desktop::clipboard::Clipboard;
use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use tokio::sync::{mpsc, oneshot};
use wx_proto::Capabilities;

use super::clipboard::ClipboardState;
use super::clipboard_portal;
use super::inject::Transport;
use super::session::{SharedSession, CLIPBOARD_CAPABILITIES, INJECT_CAPABILITIES};
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

/// How long to wait before each further attempt at the startup sequence — and, by
/// having a fixed length, how many further attempts there are at all.
///
/// # Why there is a retry here at all
///
/// The agent is autostarted as a systemd **user** unit, which puts it in a race with
/// `xdg-desktop-portal` that the unit file cannot win on its own: `After=` orders it
/// against the portal's *unit*, and a unit that has been reached is not the same
/// thing as a portal that will answer a method call. Losing that race used to be
/// permanent — the sequence failed once, the session went terminally
/// [`super::SessionState::Unsupported`], and the machine had no input capability for
/// the rest of the run with nothing on screen to click. That is the whole failure
/// this schedule exists to close, and it is only visible on a machine that starts at
/// login, which is every machine the alpha is meant to be used on.
///
/// # Why it is this small, and bounded
///
/// Five attempts over thirty seconds. Finite because an unanswerable portal is
/// either late or absent, and thirty seconds is long enough to cover the first and
/// short enough that the second is reported honestly rather than hidden behind a
/// loop that never ends. What it must *not* become is a general reconnect: the rules
/// on [`retry_after`] are what keep it from ever putting a consent dialog on screen,
/// and they matter far more than the numbers here.
const STARTUP_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
];

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
pub fn start(
    shared: Arc<SharedSession>,
    transport: Arc<Transport>,
    clipboard: Arc<ClipboardState>,
    config_dir: PathBuf,
) -> Driver {
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
            runtime.block_on(run(
                &thread_shared,
                &transport,
                &clipboard,
                &config_dir,
                stop_rx,
            ));
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
    /// safe to wait for because no path inside [`run`] waits on anything without
    /// also watching for this: the portal calls are bounded by [`TEARDOWN_TIMEOUT`],
    /// and the startup retry's wait — the one deliberately long thing in there — is
    /// a `select!` branch against the same signal rather than a sleep.
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
    transport: &Transport,
    clipboard: &ClipboardState,
    config_dir: &std::path::Path,
    mut stop: oneshot::Receiver<()>,
) {
    let store = RestoreTokenStore::in_dir(config_dir);
    // Built here, before anything else, because the clipboard's signal streams
    // borrow it and have to outlive every step below. It asks the user for
    // nothing and costs one D-Bus introspection.
    let clipboard_proxy = clipboard_portal::proxy().await;

    let live = match attempt_establish(
        shared,
        &store,
        &mut stop,
        &STARTUP_RETRY_DELAYS,
        async || establish(&store, clipboard_proxy.as_deref()).await,
    )
    .await
    {
        Some(live) => live,
        // Every terminal state and every message a user reads was set inside; there
        // is nothing left for this thread to do.
        None => return,
    };

    tracing::info!(
        devices = ?live.devices,
        clipboard = live.clipboard.is_some(),
        capabilities = %live.granted.describe(),
        "the desktop portal granted a RemoteDesktop session"
    );
    // The device set is whole or absent, never a subset: a grant missing exactly
    // one device type never reaches here, because `accept_granted` refuses it.
    //
    // Injection and not capture: this portal's devices are emulation devices, so
    // what they buy is the ability to be driven. Capture is
    // `org.freedesktop.portal.InputCapture` and has a session of its own; see the
    // note at the top of `super::capture`.
    //
    // Both transports are published *before* the capabilities, so no peer can be
    // told this machine accepts input or clipboard content during the window where
    // the trait would still find nothing to send on.
    let (connection, events) = match live.input {
        Some(wired) => {
            transport.attach(wired.connection.clone());
            (Some(wired.connection), Some(wired.events))
        }
        None => (None, None),
    };
    // The sender is kept alive here for as long as the session is: the receiver
    // in `pump` only stops yielding once every sender is gone, and dropping this
    // one would turn that branch into a busy loop.
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    if live.clipboard.is_some() {
        clipboard.attach(clipboard_portal::transport(command_tx.clone()));
    }
    // What the grant is worth *now*, which is not yet all of it: the seat and its
    // devices arrive on the `ei` stream that `pump` has not started reading, so
    // injection is still a promise this machine could not keep. `pump` publishes
    // the rest the moment the devices show up. See `serviceable`.
    shared.activate(serviceable(live.granted, transport));

    let reason = pump(
        Running {
            session: Arc::clone(&live.session),
            granted: live.granted,
            connection,
            events,
            clipboard: live.clipboard,
            commands: command_rx,
        },
        shared,
        clipboard_proxy.as_ref(),
        transport,
        clipboard,
        stop,
    )
    .await;
    // Before the state change, so a caller that races teardown finds the transport
    // gone rather than a connection the compositor has already dropped.
    transport.detach();
    clipboard.detach();
    teardown(shared, &live.session, reason).await;
}

/// Run the startup sequence, retrying it on the schedule for the failures — and only
/// the failures — that a retry can honestly fix.
///
/// Returns `None` when there is nothing more to do: either the agent is shutting
/// down, or the sequence has failed for the last time and the failure has already
/// been reported through `shared`. Every terminal state this backend can reach at
/// startup is set here.
///
/// Generic over what the attempt produces so that the loop — which is the part with
/// the shutdown interrupt, the bound, and the token check in it — can be tested
/// without a desktop. The real caller passes [`establish`].
async fn attempt_establish<T>(
    shared: &SharedSession,
    store: &RestoreTokenStore,
    stop: &mut oneshot::Receiver<()>,
    delays: &[Duration],
    mut attempt: impl AsyncFnMut() -> Result<T, Aborted>,
) -> Option<T> {
    for n in 0.. {
        // Read per attempt rather than once: an attempt that threw a rejected token
        // away has changed the answer, and the next one must not be given credit for
        // a token that is no longer there.
        let had_token = store.load().is_some();

        // Shutdown has to be able to interrupt the sequence, not just the event loop
        // that follows it. `Start` blocks until the user answers the consent dialog,
        // and there is no upper bound on that — a dialog behind a full-screen window
        // can sit there for hours. Without this branch, `Driver::drop` would join a
        // thread waiting on it and the agent would never exit.
        let established = tokio::select! {
            biased;
            _ = &mut *stop => {
                tracing::debug!("shutting down before the portal session was granted");
                shared.stopped();
                return None;
            }
            established = attempt() => established,
        };

        let Aborted { failure, session } = match established {
            Ok(live) => return Some(live),
            Err(aborted) => aborted,
        };
        forget_rejected_token(store, had_token, &failure);
        // A session the portal already granted outlives this thread — nothing drops
        // it and ashpd keeps the bus connection process-wide — so the compositor
        // would go on showing this machine as remotely controlled by something that
        // will never use the session. Done before the decision below, so a retry
        // never leaves the one before it behind.
        if let Some(session) = session {
            close_session(&session).await;
        }

        let Some(delay) = retry_after(&failure, store.load().is_some(), n, delays) else {
            failure.report(shared);
            return None;
        };
        tracing::info!(
            attempt = n + 1,
            of = delays.len(),
            retry_in_ms = delay.as_millis(),
            reason = %failure.detail,
            "the desktop portal was not answerable yet; retrying against the stored restore \
             token, which is silent"
        );
        shared.retrying(failure.detail);

        tokio::select! {
            biased;
            _ = &mut *stop => {
                shared.stopped();
                return None;
            }
            () = tokio::time::sleep(delay) => {}
        }
        shared.starting();
    }
    // `0..` is only exhausted at `usize::MAX`, which `delays.len()` stops long first.
    unreachable!()
}

/// How long to wait before trying the startup sequence again, or `None` if it must
/// not be tried again at all.
///
/// Three conditions, and the first two are the design rather than a detail of it.
///
/// **A refusal is never retried.** A user who said no must not be asked again, and a
/// loop that raised consent dialogs against them would be worse than the bug this
/// retry exists to fix. [`FailureKind::Denied`] is where every refusal lands.
///
/// **Nothing past the dialog is retried either.** `before_consent` is true only for a
/// request that failed before `Start` was ever called — before any consent UI could
/// exist. That is what makes the retry provably silent rather than probably silent:
/// combined with the token check below, a retry cannot put a dialog on screen that
/// the first attempt would not have put there itself. A failure at or after the
/// dialog means the user has already been asked something, and asking again is the
/// one thing this must not do.
///
/// **A retry happens only with a restore token in hand.** Without one the next
/// `Start` *would* prompt, and a consent dialog appearing by itself some seconds
/// after login — with no user action behind it — is not a recovery, it is the
/// product asking for permission at a moment of its own choosing. A machine that has
/// never consented simply reports the failure and waits for a launch the user is
/// present for.
///
/// The bound is `delays`, which is finite; see [`STARTUP_RETRY_DELAYS`].
fn retry_after(
    failure: &Failure,
    has_token: bool,
    attempt: usize,
    delays: &[Duration],
) -> Option<Duration> {
    if matches!(failure.kind, FailureKind::Denied) {
        return None;
    }
    if !failure.before_consent {
        return None;
    }
    if !has_token {
        return None;
    }
    delays.get(attempt).copied()
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

/// A portal session that has been granted, with its transports connected.
struct Live<'a> {
    session: Arc<Session<RemoteDesktop>>,
    /// What the grant is worth to peers: input, clipboard, or both.
    granted: Capabilities,
    /// The device types the portal actually handed over, for the log.
    devices: BitFlags<DeviceType>,
    /// Absent when the user granted the clipboard and refused remote interaction.
    /// There is nothing to connect libei to in that case, and asking for it would
    /// fail a session that is perfectly good for what it was granted.
    input: Option<Wired>,
    /// Absent when the clipboard was refused, or when this desktop has no
    /// `Clipboard` portal at all.
    clipboard: Option<clipboard_portal::Portal<'a>>,
}

/// The libei half of a granted session.
struct Wired {
    connection: reis::event::Connection,
    events: reis::tokio::EiConvertEventStream,
}

/// Everything [`pump`] drives, once the session is live.
///
/// A struct rather than seven parameters because the clipboard added three of
/// them, and because which ones are `None` is the interesting part: a session
/// with no `events` was granted the clipboard and not the devices, and a session
/// with no `clipboard` was granted the other way round.
struct Running<'a> {
    session: Arc<Session<RemoteDesktop>>,
    /// Everything the portal granted — the ceiling on what may ever be published
    /// for this session, which `serviceable` narrows to what works at any moment.
    granted: Capabilities,
    connection: Option<reis::event::Connection>,
    events: Option<reis::tokio::EiConvertEventStream>,
    clipboard: Option<clipboard_portal::Portal<'a>>,
    commands: mpsc::UnboundedReceiver<clipboard_portal::Command>,
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
    session: Option<Arc<Session<RemoteDesktop>>>,
}

impl Aborted {
    /// A failure from before `CreateSession` answered, so there is nothing to close.
    fn before_session(e: ashpd::Error) -> Self {
        Self {
            // Nothing before `SelectDevices` carries the restore token, so whatever
            // went wrong here cannot be the token's fault, and no dialog has been
            // shown yet so nobody has refused anything.
            failure: Failure::from_ashpd(e, TokenSent::No, Stage::BeforeConsent),
            session: None,
        }
    }
}

/// Run the whole `CreateSession` → `SelectDevices` → `RequestClipboard` →
/// `Start` → `ConnectToEIS` sequence and bring the transports up.
async fn establish<'a>(
    store: &RestoreTokenStore,
    clipboard: Option<&'a Clipboard>,
) -> Result<Live<'a>, Aborted> {
    let proxy = RemoteDesktop::new()
        .await
        .map_err(Aborted::before_session)?;
    tracing::debug!(version = proxy.version(), "portal RemoteDesktop interface");

    let session = Arc::new(
        proxy
            .create_session(Default::default())
            .await
            .map_err(Aborted::before_session)?,
    );

    match negotiate(&proxy, &session, store, clipboard).await {
        Ok(negotiated) => Ok(Live {
            session,
            granted: negotiated.granted,
            devices: negotiated.devices,
            input: negotiated.input,
            clipboard: negotiated.clipboard,
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
async fn negotiate<'a>(
    proxy: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    store: &RestoreTokenStore,
    clipboard: Option<&'a Clipboard>,
) -> Result<Negotiated<'a>, Failure> {
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
        .map_err(|e| Failure::from_ashpd(e, sent, Stage::BeforeConsent))?
        .response()
        .map_err(|e| Failure::from_ashpd(e, sent, Stage::BeforeConsent))?;

    // After `SelectDevices` and before `Start`, which is the only window the
    // portal accepts it in: it is part of *what* the user is being asked to
    // allow, and `Start` answers it. A desktop with no `Clipboard` portal, or one
    // that refuses, leaves this `None` and costs nothing else — the injection
    // session the user is about to consent to is not given back over it.
    let clipboard = match clipboard {
        Some(proxy) => clipboard_portal::request(proxy, session).await,
        None => None,
    };

    tracing::info!(
        restoring = restore_token.is_some(),
        clipboard = clipboard.is_some(),
        "starting the portal session; a consent dialog appears unless a restore token covers it"
    );
    // `TokenSent::No` from here on: only `SelectDevices` carried the token, so an
    // argument the portal will not take on a later call is an argument of ours.
    // Reading one as a rejected token would delete a good one and cost the user a
    // consent dialog on the next launch.
    let started = proxy
        .start(session, None, Default::default())
        .await
        .map_err(|e| Failure::from_ashpd(e, TokenSent::No, Stage::Consent))?
        .response()
        .map_err(|e| Failure::from_ashpd(e, TokenSent::No, Stage::Consent))?;

    // Read from the results rather than assumed from having asked. The consent
    // dialog carries "Allow Clipboard Access" as a toggle independent of remote
    // interaction, so a user can grant one and refuse the other and this is the
    // only place that says which.
    let clipboard = match (clipboard, started.is_clipboard_enabled()) {
        (Some(portal), true) => Some(portal),
        (Some(_), false) => {
            tracing::info!(
                "the desktop portal granted the session without clipboard access; \
                 clipboard sync is not advertised"
            );
            None
        }
        (None, _) => None,
    };

    // Checked before the token is persisted: a token is the portal's promise to
    // grant the same thing again without asking, and persisting one for a grant
    // this session is about to refuse would make every later launch restore the
    // same half grant silently, with no dialog left to fix it at.
    let granted = accept_granted(started.devices(), clipboard.is_some())?;

    // Persisted before anything else can fail: a token thrown away because the libei
    // connection did not come up would cost the user another dialog for a problem
    // that had nothing to do with consent.
    record_token(store, granted, started.restore_token());

    let devices = started.devices();

    // Skipped when no devices were granted. `ConnectToEIS` on a session with
    // nothing to emulate on buys an empty seat at best, and failing over it would
    // throw away a clipboard grant the user did make.
    let input = if granted.contains(INJECT_CAPABILITIES) {
        let fd = proxy
            .connect_to_eis(session, Default::default())
            .await
            .map_err(|e| Failure::from_ashpd(e, TokenSent::No, Stage::AfterGrant))?;

        let context = ei::Context::new(UnixStream::from(fd))
            .map_err(|e| Failure::broken(format!("opening the libei transport: {e}")))?;
        let (connection, events) = context
            .handshake_tokio(EI_CLIENT_NAME, ei::handshake::ContextType::Sender)
            .await
            .map_err(|e| Failure::broken(format!("the libei handshake failed: {e}")))?;
        tracing::debug!("libei transport connected");
        Some(Wired { connection, events })
    } else {
        tracing::info!(
            "the desktop portal granted clipboard access without input devices; \
             this machine can sync the clipboard and cannot be driven"
        );
        None
    };

    Ok(Negotiated {
        granted,
        devices,
        input,
        clipboard,
    })
}

/// Keep, or deliberately forget, the token for the grant that just came back.
///
/// The decision itself is [`persists_token`]; this is the half that touches the
/// file, and the clearing is not incidental. A grant that arrives without devices
/// means the user answered the dialog differently from last time, so a token left
/// over from an earlier full grant no longer describes what they want and would be
/// sent again on the next launch — leaving the promise that the dialog comes back
/// unkept.
fn record_token(store: &RestoreTokenStore, granted: Capabilities, token: Option<&str>) {
    if !persists_token(granted) {
        store.clear();
        tracing::info!(
            "a clipboard-only grant is not recorded; the next launch shows the consent dialog again"
        );
        return;
    }
    match token {
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
}

/// Whether a grant of this shape may leave a restore token behind.
///
/// Only one that carries the devices. A restore token is the portal's promise to
/// grant the same thing again silently, so recording one for a clipboard-only grant
/// would restore that same grant on every later launch — and with no dialog left,
/// `SelectDevices` has no way to put the keyboard and pointer back. The machine
/// would be permanently undriveable, recoverable only by deleting a token file the
/// user does not know exists.
///
/// The costs are asymmetric, which is what decides it. Someone who wants
/// clipboard-only sees a dialog each launch, which is mildly annoying — and that
/// dialog is exactly how they keep making the choice. Someone who unticked "Allow
/// Remote Interaction" once, possibly by accident, would otherwise have no way back
/// at all. Never stranding a user outranks saving a dialog.
///
/// [`accept_granted`] warns about the same hazard for a half *device* grant, and
/// refuses that one outright because it cannot be published honestly. This grant can
/// be published honestly and is worth keeping for the run it was made in; it is only
/// worth nothing to the launch after.
fn persists_token(granted: Capabilities) -> bool {
    granted.contains(INJECT_CAPABILITIES)
}

/// The device types this session asks for, with the names a user reads.
const DEVICE_NAMES: [(DeviceType, &str); 2] = [
    (DeviceType::Keyboard, "keyboard"),
    (DeviceType::Pointer, "pointer"),
];

/// Decide what a grant is worth, and refuse the one shape that cannot be published
/// honestly.
///
/// One dialog answers two independent questions — the device list and the
/// clipboard toggle — so three of the four outcomes are real and each is
/// advertised as itself. A machine that can sync the clipboard and cannot be
/// driven is a perfectly good node, and giving that session back because the
/// devices were withheld would throw away a grant the user did make.
///
/// The refusal that remains is a *half* device grant. [`wx_proto::Capabilities`]
/// has no per-device granularity: `INJECT_INPUT` is keyboard and pointer together
/// or it is nothing. So a grant covering only the pointer has no honest way to be
/// published — claiming keyboard capability the session cannot deliver would have
/// peers route keystrokes here that silently go nowhere, against the rule this
/// backend is built on that it advertises nothing it cannot do.
///
/// What is most at stake is the restore token: this refusal runs before the token
/// is persisted, and a token recorded for a half grant would restore that same
/// half grant silently on every later launch, with no dialog left to correct it.
///
/// Reported as a refusal rather than a fault because that is what it is: the
/// portal answered the request, and what came back is less than was asked for. The
/// message names the missing device so a user who mis-clicked the dialog can see
/// which box to tick next time.
fn accept_granted(devices: BitFlags<DeviceType>, clipboard: bool) -> Result<Capabilities, Failure> {
    let missing: Vec<&str> = DEVICE_NAMES
        .into_iter()
        .filter(|(device, _)| !devices.contains(*device))
        .map(|(_, name)| name)
        .collect();

    // Some devices but not all: the one grant with no honest publication.
    if !missing.is_empty() && missing.len() < DEVICE_NAMES.len() {
        return Err(Failure::refused(format!(
            "the desktop portal withheld {} access; this machine needs keyboard and pointer together, so the session was given back",
            missing.join(" and ")
        )));
    }

    let granted = if missing.is_empty() {
        INJECT_CAPABILITIES
    } else {
        Capabilities::NONE
    }
    .union(if clipboard {
        CLIPBOARD_CAPABILITIES
    } else {
        Capabilities::NONE
    });

    if granted.is_empty() {
        return Err(Failure::refused(
            "the desktop portal withheld both input and clipboard access, so the session was given back",
        ));
    }
    Ok(granted)
}

/// Which of a grant's capabilities this machine can actually serve at this moment.
///
/// A grant is a permission and a capability is a promise that something *works*, and
/// on this backend the two are apart for a window at the start of every session. The
/// portal answers `Start`, `ConnectToEIS` hands over a socket, and the handshake
/// returns — and none of that is a device. The seat arrives as `SeatAdded` on the
/// `ei` stream, its capabilities have to be bound, the compositor then creates the
/// devices and sends `DeviceAdded`, and only `DeviceResumed` licenses anything to be
/// sent on them. All of that lands *after* the session is granted.
///
/// Publishing `INJECT_INPUT` on the strength of the grant alone means a peer can be
/// told this machine accepts input during that window, and a peer that acts on it
/// gets "the portal session offered no device for the keyboard" — the first keypress
/// after connecting silently doing nothing. The window is short and clears itself,
/// which is exactly why it will not turn up in casual testing and will turn up on
/// somebody else's two-machine setup.
///
/// So the injection bit is held back until the devices are there, and dropped again
/// if they go away — a compositor that pauses or removes them mid-session is the
/// same claim becoming false, and peers have to be told that too.
///
/// The clipboard bits are deliberately untouched. The clipboard rides this session
/// but not its libei transport: it is D-Bus calls on the session object, which work
/// the instant `Start` answers. Withholding them until an unrelated device turned up
/// would be the same dishonesty in the other direction.
fn serviceable(granted: Capabilities, transport: &Transport) -> Capabilities {
    if granted.contains(INJECT_CAPABILITIES) && !transport.can_inject() {
        return Capabilities(granted.0 & !INJECT_CAPABILITIES.0);
    }
    granted
}

/// What [`negotiate`] produces: everything a [`Live`] needs except the session.
struct Negotiated<'a> {
    granted: Capabilities,
    devices: BitFlags<DeviceType>,
    input: Option<Wired>,
    clipboard: Option<clipboard_portal::Portal<'a>>,
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
    running: Running<'_>,
    shared: &SharedSession,
    proxy: Option<&Arc<Clipboard>>,
    transport: &Transport,
    clipboard: &ClipboardState,
    mut stop: oneshot::Receiver<()>,
) -> Ended {
    let Running {
        session,
        granted,
        connection,
        events,
        clipboard: portal,
        mut commands,
    } = running;

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
    // Each of these is absent when its half of the session was not granted, and
    // `OptionStream` turns an absent one into a branch that simply never fires
    // rather than a second copy of this loop.
    let mut events = std::pin::pin!(OptionStream(events));
    let (owner_changes, transfers) = match portal {
        Some(portal) => (Some(portal.owner_changes), Some(portal.transfers)),
        None => (None, None),
    };
    let mut owner_changes = std::pin::pin!(OptionStream(owner_changes));
    let mut transfers = std::pin::pin!(OptionStream(transfers));

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
                    // Present whenever the stream is: both come from the same grant.
                    if let Some(connection) = &connection {
                        if let Some(ended) = on_ei_event(connection, transport, event) {
                            return ended;
                        }
                    }
                    // Every device event can move the answer in either direction —
                    // a resume is what makes injection real, a pause or a removal
                    // takes it away again — so it is asked after each one rather
                    // than at the arrivals only. `regrant` publishes what changed
                    // and nothing else.
                    shared.regrant(serviceable(granted, transport));
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

            // Somebody copied something, here or in another application. The one
            // event source `change_serial` needs: no polling, and no hashing a
            // megabyte of image to discover that nothing happened.
            change = owner_changes.next() => match change {
                Some((_, changed)) => {
                    tracing::debug!(
                        mime_types = ?changed.mime_types(),
                        ours = ?changed.session_is_owner(),
                        "the clipboard selection changed"
                    );
                    clipboard.selection_changed(
                        changed.mime_types().to_vec(),
                        changed.session_is_owner(),
                    );
                }
                // The signal stream only ends with the bus connection, which the
                // `closed` branch above reports; nothing more to say here.
                None => return Ended::Broken(
                    "the connection to the desktop portal went away".into(),
                ),
            },

            // Somebody is pasting from a selection this machine owns, and is
            // blocked in their own paste until the bytes arrive.
            transfer = transfers.next() => match transfer {
                Some((_, mime, serial)) => {
                    if let Some(proxy) = proxy {
                        clipboard_portal::serve_transfer(proxy, &session, clipboard, mime, serial);
                    }
                }
                None => return Ended::Broken(
                    "the connection to the desktop portal went away".into(),
                ),
            },

            // A trait call on another thread wants something of the portal.
            command = commands.recv() => match command {
                Some(command) => {
                    // Nothing holds a sender without a granted clipboard, so a
                    // command with no proxy behind it cannot arrive; the `if let`
                    // is what the type says rather than a case to handle.
                    if let Some(proxy) = proxy {
                        clipboard_portal::dispatch(command, proxy, &session);
                    }
                }
                // `run` keeps a sender alive for the life of the session, so this
                // only happens once the session is already ending.
                None => return Ended::Stopped,
            },
        }
    }
}

/// Handle one `ei` event, returning `Some` if it ended the session.
fn on_ei_event(
    connection: &reis::event::Connection,
    transport: &Transport,
    event: EiEvent,
) -> Option<Ended> {
    match event {
        EiEvent::SeatAdded(added) => {
            // Nothing is offered until the capabilities are bound, so this is what
            // makes the compositor create the devices at all. Asking for everything
            // the session covers means #7 gets its devices without renegotiating —
            // and without a second dialog.
            added.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll
                    | DeviceCapability::Keyboard
                    // Asked for although the alpha target's EIS does not implement
                    // `ei_text`: binding a capability the server never offers costs
                    // nothing, and the day one does offer it the injector gets a
                    // direct "produce this string" route with no renegotiation. See
                    // the note at the top of `super::keymap`.
                    | DeviceCapability::Text,
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
                text = added.device.has_capability(DeviceCapability::Text),
                keymap = added.device.keymap().is_some(),
                "libei device offered"
            );
            transport.device_added(&added.device);
            None
        }
        // Nothing may be emulated before this, and everything queued before it is
        // discarded — which is one of the two ways injection ends up a silent
        // no-op, the other being a missing frame.
        EiEvent::DeviceResumed(resumed) => {
            transport.device_resumed(&resumed.device);
            None
        }
        // A pause is reversible and routine, so the device keeps its slot and its
        // keymap; only sending on it is forbidden until it resumes.
        EiEvent::DevicePaused(paused) => {
            transport.device_paused(&paused.device);
            None
        }
        EiEvent::DeviceRemoved(removed) => {
            transport.device_lost(&removed.device);
            None
        }
        // Carries the layout group and the desktop's locked modifiers, both of
        // which change what keycode produces a given character.
        EiEvent::KeyboardModifiers(mods) => {
            tracing::trace!(
                locked = mods.locked,
                group = mods.group,
                "libei keyboard modifier state"
            );
            transport.modifiers(&mods.device, mods.locked, mods.group);
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
async fn teardown(shared: &SharedSession, session: &Session<RemoteDesktop>, reason: Ended) {
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
async fn close_session(session: &Session<RemoteDesktop>) {
    match tokio::time::timeout(TEARDOWN_TIMEOUT, session.close()).await {
        Ok(Ok(())) => tracing::debug!("portal session closed"),
        Ok(Err(e)) => tracing::debug!(error = %e, "the portal session was already gone"),
        Err(_) => tracing::warn!("timed out closing the portal session"),
    }
}

/// How a failure during [`establish`] should be reported.
#[derive(Debug)]
struct Failure {
    detail: String,
    kind: FailureKind,
    /// Whether a stored restore token should be thrown away because of it.
    discards_token: bool,
    /// Whether the request that failed ran before `Start`, and so before any consent
    /// UI for this session could exist.
    ///
    /// The one thing [`retry_after`] needs that the error itself does not carry: it
    /// is what makes "trying again cannot prompt anybody" a structural claim rather
    /// than a hope. False unless [`Failure::from_ashpd`] was given
    /// [`Stage::BeforeConsent`], so a failure this module raises by hand — all of
    /// which happen at or after the grant — is never retried by accident.
    before_consent: bool,
}

#[derive(Debug)]
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

/// Where in the sequence the failing call sits, relative to the consent dialog.
///
/// Three positions, not two, because "the user was not asked" is true on either side
/// of the dialog and the two sides mean opposite things. Only `Start` puts a dialog on
/// screen, so only `Start` can fail because somebody said no; `CreateSession` and
/// `SelectDevices` run before any consent UI exists; and `ConnectToEIS` runs after a
/// grant has already come back. Collapsing the last into the first is how a transport
/// fault on a session the user had just approved came to be reported as a desktop that
/// would not create the session at all.
///
/// Kept structural rather than matched on the portal's message text, and it is what
/// stops a screen that happened to be locked at startup from being recorded as a
/// refusal — terminal, never retried, and reported to the user as a permission they
/// have to go and grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// `CreateSession`, `SelectDevices`: no dialog exists yet.
    BeforeConsent,
    /// `Start`: the call the user answers.
    Consent,
    /// `ConnectToEIS` and after: the session is granted and the user has consented.
    AfterGrant,
}

impl Failure {
    fn denied(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: FailureKind::Denied,
            discards_token: false,
            before_consent: false,
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
            before_consent: false,
        }
    }

    fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            kind: FailureKind::Unsupported,
            discards_token: false,
            before_consent: false,
        }
    }

    /// The same failure, marked as having happened before any consent UI existed.
    ///
    /// Applied in one place — [`Failure::from_ashpd`], from the stage it was told —
    /// so that "no dialog can have been shown" is read off the sequence rather than
    /// decided again at each construction site.
    fn before_consent(self) -> Self {
        Self {
            before_consent: true,
            ..self
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
    /// unsupported hides the one thing they could act on. Which of the two a refused
    /// request is depends on where in the sequence it failed — see [`Stage`].
    fn from_ashpd(e: ashpd::Error, sent: TokenSent, stage: Stage) -> Self {
        use ashpd::desktop::ResponseError;
        use ashpd::PortalError;

        let failure = match e {
            // A request turned down before the dialog existed. Nobody refused this:
            // the desktop would not set the session up at all, which is what a
            // compositor answers with when the session is locked or not yet ready —
            // both of which pass without anyone doing anything. Calling it a
            // permission problem would leave a daemon that started at a lock screen
            // input-dead for its whole run, with a message sending the user to a
            // settings panel where there is nothing to fix.
            ashpd::Error::Response(_) if stage == Stage::BeforeConsent => Self::broken(
                "the desktop refused to create a remote desktop session; the session is \
                 most likely locked or not ready yet",
            ),
            // Also nobody's decision, but the opposite situation: the user consented,
            // the portal granted, and the call that failed is the one that hands over
            // the libei socket. The pre-consent wording above would be wrong in both
            // of its clauses here and would send the user to check a lock screen for
            // what is a transport fault on a session they had just approved.
            ashpd::Error::Response(_) if stage == Stage::AfterGrant => Self::broken(
                "the desktop portal granted the remote desktop session but would not \
                 hand over the input transport",
            ),
            ashpd::Error::Response(ResponseError::Cancelled) => {
                Self::refused("the desktop portal consent dialog was dismissed")
            }
            // The portal's catch-all, from the call the user was answering. It covers
            // a compositor that failed as well as one that refused, so the token
            // stays: throwing away a good token over a transient failure would cost
            // the user a dialog they need not have seen.
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
        };
        // Read off the stage rather than off any one branch above: whether a dialog
        // could have been shown is a fact about where in the sequence the call sits,
        // and every reading of it — including the ones that get their own branch
        // here — has to agree with that or [`retry_after`] is deciding on something
        // other than what happened.
        if stage == Stage::BeforeConsent {
            failure.before_consent()
        } else {
            failure
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
pub(super) fn no_session_bus(e: &ashpd::zbus::Error) -> bool {
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

    use super::super::session::{SessionState, PORTAL_REMOTE_DESKTOP, REMOTE_DESKTOP_CAPABILITIES};

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
        (
            SharedSession::new(
                live.clone(),
                Capabilities::NONE,
                REMOTE_DESKTOP_CAPABILITIES,
                PORTAL_REMOTE_DESKTOP,
            ),
            live,
        )
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
            Stage::BeforeConsent,
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
            Stage::BeforeConsent,
        );
        forget_rejected_token(&store, had_token, &failure);

        assert_eq!(
            RestoreTokenStore::in_dir(&dir.0).load(),
            None,
            "the next launch must ask the user once rather than fail identically"
        );
    }

    #[test]
    fn a_grant_that_carries_the_devices_is_remembered() {
        // The half that hardware confirmed: a second launch came back restoring=true
        // with no dialog. Its opposite cannot be produced on the alpha target — the
        // dialog there would not hand back a clipboard-only grant — so the two tests
        // below are the only cover that side has.
        let dir = TempDir::new("with-devices");
        let store = RestoreTokenStore::in_dir(&dir.0);
        record_token(
            &store,
            INJECT_CAPABILITIES.union(CLIPBOARD_CAPABILITIES),
            Some("a-good-token"),
        );
        assert_eq!(store.load().as_deref(), Some("a-good-token"));
    }

    #[test]
    fn a_clipboard_only_grant_is_never_recorded() {
        // Restoring it silently would leave a machine that cannot be driven and has
        // no dialog left to add the devices back at. One dialog per launch is the
        // cheaper of the two failures by a long way.
        let dir = TempDir::new("clipboard-only");
        let store = RestoreTokenStore::in_dir(&dir.0);
        assert!(!persists_token(CLIPBOARD_CAPABILITIES));
        record_token(&store, CLIPBOARD_CAPABILITIES, Some("a-good-token"));
        assert_eq!(store.load(), None);
    }

    #[test]
    fn a_clipboard_only_grant_forgets_the_token_from_a_fuller_one() {
        // Without this the promise is not kept: the older token would still be sent
        // on the next launch, and the dialog the user is owed would never appear.
        let dir = TempDir::new("downgraded");
        let store = RestoreTokenStore::in_dir(&dir.0);
        store.save("from-a-full-grant").unwrap();
        record_token(&store, CLIPBOARD_CAPABILITIES, None);
        assert_eq!(
            RestoreTokenStore::in_dir(&dir.0).load(),
            None,
            "the user changed their answer, so the old token no longer describes it"
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
            Stage::BeforeConsent,
        );
        assert!(matches!(no_token.kind, FailureKind::Broken));
        assert!(!no_token.discards_token);

        let other_error = Failure::from_ashpd(
            method_error("org.freedesktop.DBus.Error.NoReply", "timed out"),
            TokenSent::Yes,
            Stage::BeforeConsent,
        );
        assert!(matches!(other_error.kind, FailureKind::Broken));
        assert!(!other_error.discards_token);
    }

    #[test]
    fn a_user_refusal_is_a_permission_problem_that_nothing_retries() {
        let failure = Failure::from_ashpd(
            ashpd::Error::Response(ResponseError::Cancelled),
            TokenSent::Yes,
            Stage::Consent,
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
        session.activate(REMOTE_DESKTOP_CAPABILITIES);
        assert_eq!(session.state(), SessionState::Denied);
        assert!(live.get().is_empty());
    }

    #[test]
    fn a_desktop_that_will_not_create_the_session_is_not_a_user_saying_no() {
        // Measured on GNOME 50: with the screen locked, mutter answers
        // `CreateSession` with a refusal — "Session creation inhibited" — long before
        // any dialog exists. Recording that as a refusal made the agent permanently
        // input-dead for a lock the user walked away from, and told them to go and
        // grant a permission that was never in question.
        for (label, error) in [
            ("other", ashpd::Error::Response(ResponseError::Other)),
            (
                "cancelled",
                ashpd::Error::Response(ResponseError::Cancelled),
            ),
        ] {
            let failure = Failure::from_ashpd(error, TokenSent::Yes, Stage::BeforeConsent);
            assert!(matches!(failure.kind, FailureKind::Broken), "{label}");
            assert!(
                !failure.discards_token,
                "a lock screen is not the token's fault: {label}"
            );
            assert!(
                failure.detail.contains("locked")
                    && failure.detail.contains("create a remote desktop session"),
                "the message must name what the desktop did and the likely reason: {}",
                failure.detail
            );

            let (session, live) = session();
            session.starting();
            failure.report(&session);

            assert_eq!(session.state(), SessionState::Failed, "{label}");
            assert!(live.get().is_empty(), "{label}");
            assert!(
                !matches!(session.error(), Some(PlatformError::PermissionDenied(_))),
                "nobody refused anything here: {label}"
            );
        }
    }

    #[test]
    fn a_transport_failure_after_the_grant_does_not_read_as_a_locked_screen() {
        // `ConnectToEIS` runs after `Start` came back granted, so neither clause of
        // the pre-consent message is true here: the user consented and the session
        // exists. Sharing that wording sent somebody looking at a lock screen for a
        // fault in handing over the libei socket.
        let failure = Failure::from_ashpd(
            ashpd::Error::Response(ResponseError::Other),
            TokenSent::No,
            Stage::AfterGrant,
        );
        assert!(matches!(failure.kind, FailureKind::Broken));
        assert!(!failure.discards_token, "the grant itself was fine");
        assert!(
            !failure.detail.contains("locked"),
            "the session is not locked; it was granted: {}",
            failure.detail
        );
        assert!(
            failure.detail.contains("granted") && failure.detail.contains("transport"),
            "the message must say the grant succeeded and the transport did not: {}",
            failure.detail
        );

        let (session, _) = session();
        session.starting();
        failure.report(&session);
        assert_eq!(session.state(), SessionState::Failed);
        assert!(!matches!(
            session.error(),
            Some(PlatformError::PermissionDenied(_))
        ));
    }

    #[test]
    fn a_grant_covering_both_devices_is_accepted() {
        assert_eq!(
            accept_granted(wanted_devices(), false).unwrap(),
            INJECT_CAPABILITIES
        );
    }

    #[test]
    fn one_dialog_answers_for_input_and_the_clipboard_separately() {
        // "Allow Clipboard Access" is its own toggle beside "Allow Remote
        // Interaction", so all four answers are reachable and three of them are
        // grants. A machine that can sync copy and paste and cannot be driven is
        // a perfectly good node, and handing that session back over the devices
        // would throw away a grant the user did make.
        assert_eq!(
            accept_granted(wanted_devices(), true).unwrap(),
            INJECT_CAPABILITIES.union(CLIPBOARD_CAPABILITIES)
        );
        assert_eq!(
            accept_granted(BitFlags::empty(), true).unwrap(),
            CLIPBOARD_CAPABILITIES,
            "clipboard granted and input refused is a session worth keeping"
        );
        assert_eq!(
            accept_granted(wanted_devices(), false).unwrap(),
            INJECT_CAPABILITIES,
            "input granted and the clipboard refused must not claim the clipboard"
        );
    }

    #[test]
    fn a_grant_of_nothing_at_all_is_still_given_back() {
        let failure = accept_granted(BitFlags::empty(), false).unwrap_err();
        assert!(matches!(failure.kind, FailureKind::Denied));
        assert!(
            failure.detail.contains("input and clipboard"),
            "{}",
            failure.detail
        );
    }

    #[test]
    fn a_half_grant_is_refused_rather_than_advertised_as_both() {
        // `Capabilities` cannot say "pointer but not keyboard", so a session that
        // published anything here would be claiming a capability it cannot honour
        // and peers would send keystrokes into a void. Refusing says so instead.
        let failure = accept_granted(DeviceType::Pointer.into(), false).unwrap_err();
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
        let no_keyboard = accept_granted(DeviceType::Pointer.into(), false).unwrap_err();
        assert!(no_keyboard.discards_token);
        // And it stays refused when the clipboard *was* granted: the half grant is
        // the shape with no honest publication, whatever else came with it.
        let with_clipboard = accept_granted(DeviceType::Pointer.into(), true).unwrap_err();
        assert!(with_clipboard.discards_token);

        let none_at_all = accept_granted(BitFlags::empty(), false).unwrap_err();
        assert!(none_at_all.discards_token);
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
            Stage::BeforeConsent,
        );
        assert!(matches!(failure.kind, FailureKind::Unsupported));
        assert!(!failure.discards_token);

        // And the case with no bus at all, which fails before any method call and so
        // is the one shape that really does arrive as a bare `zbus` error.
        let failure = Failure::from_ashpd(
            ashpd::Error::Zbus(ashpd::zbus::Error::Address("no bus here".to_string())),
            TokenSent::Yes,
            Stage::BeforeConsent,
        );
        assert!(matches!(failure.kind, FailureKind::Unsupported));
        assert!(!failure.discards_token);
    }

    /// The startup retry.
    ///
    /// The defect is a race with `xdg-desktop-portal` at login, so none of it is
    /// provable by racing it. Everything below injects the failure instead: the
    /// decision is a pure function of a portal error, and the loop is driven with a
    /// schedule of zero-length waits and an attempt that fails exactly as often as
    /// the test says. What is *not* covered here is a real portal that is late,
    /// which needs a machine that boots — see the note at the top of this file.
    mod startup_retry {
        use super::*;

        /// A schedule with the same shape as [`STARTUP_RETRY_DELAYS`] and none of
        /// its waiting, so these tests take microseconds and cannot flake on a slow
        /// box.
        const NO_WAIT: [Duration; 3] = [Duration::ZERO, Duration::ZERO, Duration::ZERO];

        /// Long enough that a test which reaches it has already failed.
        const FOREVER: [Duration; 1] = [Duration::from_secs(3600)];

        const A_TOKEN: &str = "6f1a7c58-0c37-4a6b-9c8e-2a1f5d3e7b90";

        fn block_on<F: std::future::Future>(f: F) -> F::Output {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("a test runtime")
                .block_on(f)
        }

        /// What an agent that beat `xdg-desktop-portal` to the bus at login gets:
        /// a failure from before any dialog could exist, that nobody decided.
        fn not_answerable_yet() -> Aborted {
            Aborted::before_session(ashpd::Error::Zbus(ashpd::zbus::Error::Address(
                "no bus here yet".to_string(),
            )))
        }

        /// The user dismissing the consent dialog, which is the one answer that must
        /// never be asked again.
        fn dismissed() -> Aborted {
            Aborted {
                failure: Failure::from_ashpd(
                    ashpd::Error::Response(ResponseError::Cancelled),
                    TokenSent::No,
                    Stage::Consent,
                ),
                session: None,
            }
        }

        fn store_in(dir: &TempDir, token: Option<&str>) -> RestoreTokenStore {
            let store = RestoreTokenStore::in_dir(&dir.0);
            if let Some(token) = token {
                store.save(token).expect("writing the token");
            }
            store
        }

        #[test]
        fn a_refusal_is_never_retried_however_early_it_arrived() {
            // The rule the whole feature is subordinate to. Even given every other
            // reason to try again — a token in hand, attempts left, and a failure
            // marked as pre-consent — a refusal ends it.
            let mut refusal = Failure::denied("the user said no").before_consent();
            assert_eq!(retry_after(&refusal, true, 0, &NO_WAIT), None);
            refusal = Failure::refused("the consent dialog was dismissed").before_consent();
            assert_eq!(retry_after(&refusal, true, 0, &NO_WAIT), None);
        }

        #[test]
        fn nothing_that_happened_at_or_after_the_dialog_is_retried() {
            // What makes "a retry cannot prompt anybody" structural. Past `Start`
            // the user has been asked something, and running the sequence again from
            // the top could ask them again.
            let after = Failure::broken("the transport went away");
            assert!(!after.before_consent);
            assert_eq!(retry_after(&after, true, 0, &NO_WAIT), None);
        }

        #[test]
        fn a_machine_that_has_never_consented_is_not_retried_into_a_dialog() {
            // Without a token the next `Start` shows a consent dialog, and one that
            // appears by itself seconds after login — with no user action behind it
            // — is not a recovery. That machine reports the failure and waits for a
            // launch its user is present for.
            let late = Failure::unsupported("no bus here yet").before_consent();
            assert_eq!(retry_after(&late, false, 0, &NO_WAIT), None);
            assert_eq!(
                retry_after(&late, true, 0, &NO_WAIT),
                Some(Duration::ZERO),
                "the same failure with a token is exactly the case this exists for"
            );
        }

        #[test]
        fn the_retry_schedule_is_finite_and_short() {
            // "Bounded" is the property, and it is bounded in both senses: a fixed
            // number of attempts, over a window a user would not sit through twice.
            let late = Failure::unsupported("no bus here yet").before_consent();
            for attempt in 0..STARTUP_RETRY_DELAYS.len() {
                assert!(
                    retry_after(&late, true, attempt, &STARTUP_RETRY_DELAYS).is_some(),
                    "attempt {attempt}"
                );
            }
            assert_eq!(
                retry_after(
                    &late,
                    true,
                    STARTUP_RETRY_DELAYS.len(),
                    &STARTUP_RETRY_DELAYS
                ),
                None,
                "the schedule has to run out"
            );
            let total: Duration = STARTUP_RETRY_DELAYS.iter().sum();
            assert!(total <= Duration::from_secs(60), "{total:?}");
        }

        #[test]
        fn a_portal_that_is_merely_late_is_retried_until_it_answers() {
            // The defect, closed. The first two attempts land where an autostarted
            // agent lands at login; the third is the portal having come up, and the
            // session must reach it rather than having been written off at the
            // first.
            let dir = TempDir::new("late-portal");
            let store = store_in(&dir, Some(A_TOKEN));
            let (shared, _live) = session();
            shared.starting();
            let (_stop_tx, mut stop) = oneshot::channel();
            let attempts = std::cell::Cell::new(0usize);

            let got = block_on(attempt_establish(
                &shared,
                &store,
                &mut stop,
                &NO_WAIT,
                async || {
                    attempts.set(attempts.get() + 1);
                    if attempts.get() < 3 {
                        Err(not_answerable_yet())
                    } else {
                        Ok("the session")
                    }
                },
            ));

            assert_eq!(got, Some("the session"));
            assert_eq!(attempts.get(), 3);
            assert_eq!(
                shared.state(),
                SessionState::Starting,
                "the session that is about to be published must not be left mid-retry"
            );
            assert!(
                store.load().is_some(),
                "a portal that never answered rejected nothing, so the token stands"
            );
        }

        #[test]
        fn a_portal_that_never_answers_is_reported_honestly_rather_than_retried_forever() {
            // The other end of "bounded". A desktop with no portal at all reaches
            // this too, and it has to end up saying so.
            let dir = TempDir::new("absent-portal");
            let store = store_in(&dir, Some(A_TOKEN));
            let (shared, live) = session();
            shared.starting();
            let (_stop_tx, mut stop) = oneshot::channel();
            let attempts = std::cell::Cell::new(0usize);

            let got: Option<&str> = block_on(attempt_establish(
                &shared,
                &store,
                &mut stop,
                &NO_WAIT,
                async || {
                    attempts.set(attempts.get() + 1);
                    Err(not_answerable_yet())
                },
            ));

            assert!(got.is_none());
            assert_eq!(
                attempts.get(),
                NO_WAIT.len() + 1,
                "one attempt, then one per delay in the schedule, and no more"
            );
            assert_eq!(shared.state(), SessionState::Unsupported);
            assert!(live.get().is_empty());
        }

        #[test]
        fn the_user_dismissing_the_dialog_ends_it_at_the_first_attempt() {
            // The end-to-end form of the rule above, through the loop that would be
            // the thing putting dialogs back on screen if it got this wrong.
            let dir = TempDir::new("dismissed");
            let store = store_in(&dir, Some(A_TOKEN));
            let (shared, _live) = session();
            shared.starting();
            let (_stop_tx, mut stop) = oneshot::channel();
            let attempts = std::cell::Cell::new(0usize);

            let got: Option<&str> = block_on(attempt_establish(
                &shared,
                &store,
                &mut stop,
                &NO_WAIT,
                async || {
                    attempts.set(attempts.get() + 1);
                    Err(dismissed())
                },
            ));

            assert!(got.is_none());
            assert_eq!(attempts.get(), 1, "the user was asked once and answered");
            assert_eq!(shared.state(), SessionState::Denied);
        }

        #[test]
        fn a_rejected_token_stops_the_retry_because_the_next_one_would_prompt() {
            // The two rules meeting. A token the portal threw out is forgotten, and
            // the moment it is gone the next attempt would raise a dialog — so there
            // must not be a next attempt, however pre-consent the failure was.
            let dir = TempDir::new("rejected-token-retry");
            let store = store_in(&dir, Some("not-a-uuid"));
            let (shared, _live) = session();
            shared.starting();
            let (_stop_tx, mut stop) = oneshot::channel();
            let attempts = std::cell::Cell::new(0usize);

            let got: Option<&str> = block_on(attempt_establish(
                &shared,
                &store,
                &mut stop,
                &NO_WAIT,
                async || {
                    attempts.set(attempts.get() + 1);
                    Err(Aborted {
                        failure: Failure::from_ashpd(
                            method_error(INVALID_ARGUMENT, "Restore token is not a valid UUID"),
                            TokenSent::Yes,
                            Stage::BeforeConsent,
                        ),
                        session: None,
                    })
                },
            ));

            assert!(got.is_none());
            assert_eq!(attempts.get(), 1);
            assert!(store.load().is_none(), "the bad token is still forgotten");
            assert_eq!(shared.state(), SessionState::Failed);
        }

        #[test]
        fn shutting_down_while_waiting_to_retry_stops_rather_than_waiting_out_the_delay() {
            // `Driver::drop` joins this thread, so every wait inside it has to be
            // interruptible. A schedule of an hour proves it: a test that reached
            // the sleep would not finish.
            let dir = TempDir::new("stop-mid-retry");
            let store = store_in(&dir, Some(A_TOKEN));
            let (shared, _live) = session();
            shared.starting();
            let (stop_tx, mut stop) = oneshot::channel();
            let stop_tx = std::cell::RefCell::new(Some(stop_tx));
            let attempts = std::cell::Cell::new(0usize);

            let got: Option<&str> = block_on(attempt_establish(
                &shared,
                &store,
                &mut stop,
                &FOREVER,
                async || {
                    attempts.set(attempts.get() + 1);
                    // The agent is asked to shut down while this attempt is in
                    // flight, which is exactly when a login-time retry is pending.
                    if let Some(tx) = stop_tx.borrow_mut().take() {
                        let _ = tx.send(());
                    }
                    Err(not_answerable_yet())
                },
            ));

            assert!(got.is_none());
            assert_eq!(attempts.get(), 1);
            assert_eq!(
                shared.state(),
                SessionState::Stopped,
                "a shutdown during the retry wait is not a portal failure"
            );
        }
    }
}

/// The loopback EIS server: see the module docs for what it covers that the
/// tests above cannot.
#[cfg(test)]
#[path = "eis_loopback.rs"]
mod eis_loopback;
