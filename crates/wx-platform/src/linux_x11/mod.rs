//! Linux/X11 backend — a driven target.
//!
//! | Concern | Mechanism | State |
//! |---|---|---|
//! | Displays | RandR outputs and CRTCs | implemented, see [`display`] |
//! | Injection | XTEST | implemented, see [`inject`] and [`keymap`] |
//! | Capture | XInput2 raw events | skeleton, and deliberately so |
//! | Suppression | `XIGrabDevice` | skeleton, and deliberately so |
//! | Clipboard | ICCCM selections | skeleton |
//! | Locking | systemd-logind | skeleton |
//!
//! X11 was designed when any client could read and synthesise any input, so
//! everything a *driven* machine needs is a supported operation with no permission
//! prompt and no consent dialog — now or ever. That is a security disaster and the
//! reason Wayland exists; it is also exactly the property a remote server nobody
//! sits at needs, and it is why this backend exists rather than the machine being
//! moved to a Wayland session.
//!
//! # Why only half of it is implemented
//!
//! A node needs [`Capabilities::HAS_DISPLAYS`] and [`Capabilities::INJECT_INPUT`]
//! to be **driven**, and [`Capabilities::CAPTURE_INPUT`] plus working suppression to
//! **drive**. This backend implements the first pair and not the second, and that
//! is a decision rather than a gap: the machine this was written for is only ever
//! controlled *to*, and capture's suppression needs an exclusive `XIGrabDevice`
//! whose worst failure — a grab not released on some exit path — leaves the local
//! desktop apparently frozen with no way back. Building half of that is worse than
//! building none of it, so [`X11Capture`] still refuses and still says so.
//!
//! The skeletons that remain are sketched from the APIs and not implemented; see
//! the note in [`crate::macos`] about why they compile on every target rather than
//! being `cfg`-gated away.
//!
//! # A correction to the plan that was here
//!
//! The skeleton this replaced said to resolve keys by way of
//! `xkb_x11_get_keymap_as_string`, feeding the text to
//! [`crate::linux_wayland::keymap`], and to do that "rather than linking
//! `libxkbcommon`". That is not followed and the reasoning is in [`keymap`]: no
//! such symbol exists, the API it gestures at lives in **libxkbcommon-x11** —
//! which is the very library the sentence says not to link — and the whole
//! problem falls out of two *core* X11 requests with no C dependency at all.
//! Anyone costing this backend from that comment would have costed it wrong.
//!
//! # Structure
//!
//! Everything that can be decided without a server is separated from everything
//! that talks to one, so this crate still compiles and tests on Windows and macOS:
//! [`display`], [`keymap`] and the planning in [`inject`] are pure, and `server` —
//! which is where `x11rb` lives — is `cfg`-gated with a stub beside it. That is the
//! same division [`crate::linux_wayland`] keeps.
//!
//! `x11rb` is used with `default-features = false`, which leaves it a pure-Rust
//! implementation of the wire protocol. Enabling `allow-unsafe-code` or `dl-libxcb`
//! would put a C toolchain on every machine that builds this crate, CI included,
//! and no part of this backend needs one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wx_proto::{
    Capabilities, ClipboardFormat, DisplayServer, InputEvent, Monitor, NormPos, Platform,
};

use crate::error::{PlatformError, Result};
use crate::traits::{
    CaptureSink, ClipboardAccess, DisplayEnumerator, InputCapture, InputInjector,
    ScreenSaverControl,
};
use crate::{LiveCapabilities, PlatformBackend, PlatformInfo};

pub mod display;
pub mod inject;
pub mod keymap;

#[cfg(target_os = "linux")]
mod server;
#[cfg(not(target_os = "linux"))]
mod server_stub;
#[cfg(not(target_os = "linux"))]
use server_stub as server;

const BACKEND: &str = "linux-x11";

fn todo_err(operation: &'static str) -> PlatformError {
    PlatformError::Unsupported {
        operation,
        backend: BACKEND,
    }
}

/// The X server this backend talks to, or the reason it has none.
///
/// Shared rather than opened per user, because the displays and the injector are
/// two views of one connection — and because the keyboard layout is cached behind
/// it, invalidated by the server's own `MappingNotify`.
///
/// The reason is kept verbatim so that every refusal can name the real cause. "no
/// display server available" and "this X server has no XTEST extension" send
/// whoever reads the log to entirely different places, and collapsing them into
/// one `Unsupported` is what made this backend's previous state so hard to read.
#[derive(Clone)]
pub struct Session {
    server: Option<Arc<server::Server>>,
    reason: Arc<str>,
    /// Whether the connection still has a server on the other end.
    ///
    /// Shared with every clone, because the displays and the injector are two views
    /// of one socket and the one that notices it has gone is not the one that has to
    /// stop making promises about it.
    ///
    /// Latched: it starts true and can only fall. There is no reconnect path here on
    /// purpose, so a connection that has died stays dead until the agent is
    /// restarted, and this must not flicker back on the strength of a request that
    /// happened not to fail.
    alive: Arc<AtomicBool>,
}

impl Session {
    /// Open the connection. Never prompts, because on X11 there is nothing to
    /// prompt for — see [`crate::current_platform`] for the rule this keeps.
    pub fn connect() -> Self {
        match server::connect() {
            Ok(server) => Self {
                server: Some(server),
                reason: Arc::from(""),
                alive: Arc::new(AtomicBool::new(true)),
            },
            Err(e) => Self {
                server: None,
                reason: Arc::from(e.to_string().as_str()),
                alive: Arc::new(AtomicBool::new(false)),
            },
        }
    }

    /// A session that was never opened.
    ///
    /// Only tests need to construct one deliberately — everywhere else it is what
    /// [`Session::connect`] returns on a machine with no X server — but the rules
    /// that hang off it are the ones that matter most: what a node with no
    /// connection advertises, and that `release_all` still succeeds against it.
    #[cfg(test)]
    fn absent(reason: &str) -> Self {
        Self {
            server: None,
            reason: Arc::from(reason),
            alive: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A session whose connection was made and has since gone.
    ///
    /// The state a real one reaches when the X server restarts under a running agent
    /// — a logout, an `Xorg` crash, `systemctl restart display-manager` — which is
    /// otherwise only reproducible by killing a server out from under a test. A
    /// probe with no socket to ask down cannot come back, which is precisely the
    /// answer this stands for.
    #[cfg(test)]
    fn lost(reason: &str) -> Self {
        Self {
            server: None,
            reason: Arc::from(reason),
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    fn require(&self) -> Result<&Arc<server::Server>> {
        self.server
            .as_ref()
            .ok_or_else(|| PlatformError::Other(self.reason.to_string()))
    }

    /// Whether this backend can inject right now.
    ///
    /// The question [`Capabilities::INJECT_INPUT`] is a promise about. X11 has no
    /// grant to be given or withdrawn, so the permission half of the answer really
    /// is settled at startup — but the connection is not. An X server restarts on a
    /// logout, a crash or a `systemctl restart display-manager` while an agent
    /// running as a user service carries on, and from that moment this socket
    /// delivers nothing. See [`Session::went_away`].
    fn can_inject(&self) -> bool {
        self.server.is_some() && self.alive.load(Ordering::Relaxed)
    }

    /// Notice, from a request that just failed, that the connection itself has gone.
    ///
    /// Returns whether this is the call that noticed, so the caller withdraws and
    /// logs once rather than on every tick that follows.
    fn went_away(&self, error: &PlatformError) -> bool {
        // Also the guard against there having been no connection in the first place:
        // `alive` starts true only for a session that opened one. A headless runner
        // fails this way on every tick forever and must withdraw nothing and log
        // nothing.
        if !self.alive.load(Ordering::Relaxed) {
            return false;
        }
        if !is_connection_loss(error, || {
            self.server
                .as_ref()
                .is_some_and(|server| server.is_reachable())
        }) {
            return false;
        }
        self.alive.store(false, Ordering::Relaxed);
        true
    }
}

/// Whether a failed request means there is no longer a server to talk to.
///
/// Two things this deliberately does not do. It does not read the answer out of the
/// error, because x11rb reports "the server raised a protocol error against that one
/// request" and "the socket has closed" through the same `Result`, and only the
/// first of those leaves a connection that can still be injected into; a round trip
/// is the only thing that tells them apart. And it does not probe at all for a
/// refusal this backend makes for its own reasons — a server with no RandR answers
/// `Unsupported` on every tick forever while injecting perfectly well, and probing
/// it would be a round trip a second in exchange for an answer that cannot change.
///
/// Split out from [`Session::went_away`] so the rule is tested on the machines that
/// have no X server to disagree with it, which is the same division the rest of this
/// backend keeps.
fn is_connection_loss(error: &PlatformError, reachable: impl FnOnce() -> bool) -> bool {
    !matches!(error, PlatformError::Unsupported { .. }) && !reachable()
}

/// Display enumeration over RandR.
///
/// Re-read on every call rather than cached, which is what
/// [`DisplayEnumerator::monitors`] asks for and what keeps a monitor that was
/// unplugged from lingering in the layout. The rules that decide which outputs
/// count are [`display`]; the requests are `server`.
pub struct X11Displays {
    session: Session,
    /// What this node advertises, so a connection found dead here stops the node
    /// claiming things that connection was the whole basis of. See
    /// [`X11Displays::withdraw_if_gone`].
    live: LiveCapabilities,
}

impl X11Displays {
    fn new(session: Session, live: LiveCapabilities) -> Self {
        Self { session, live }
    }

    /// Stop advertising a connection that has gone.
    ///
    /// This is the only place a dead X connection is noticed, and it is enough: the
    /// engine polls `monitors` on its housekeeping tick and reads
    /// [`crate::PlatformBackend::current_capabilities`] on the same one, so a
    /// withdrawal made here reaches peers within a tick with no thread to shut down.
    ///
    /// It matters because of what the alternative looks like from the other end. A
    /// node still claiming `INJECT_INPUT` and a monitor list — the engine keeps the
    /// last list rather than dropping the machine out of every layout over one bad
    /// poll — is a node peers go on routing the cursor into, where every event fails
    /// and is swallowed at debug level, and which the reachability reclaim does not
    /// rescue because the QUIC session is perfectly healthy. The cursor is simply
    /// gone. Everything is withdrawn rather than injection alone: displays came off
    /// the same connection.
    ///
    /// Nothing is reconnected. That is deliberately out of scope, and it is why
    /// [`Session::alive`] latches: this backend's answer to a dead X server is to
    /// say so honestly and let the agent be restarted with the session.
    fn withdraw_if_gone(&self, error: &PlatformError) {
        if !self.session.went_away(error) {
            return;
        }
        if self.live.set(capabilities(false, false)) {
            tracing::warn!(
                error = %error,
                "the X connection is gone; this machine no longer accepts injected input"
            );
        }
    }
}

impl DisplayEnumerator for X11Displays {
    fn monitors(&self) -> Result<Vec<Monitor>> {
        // Split in two on purpose: the server is asked for raw outputs, and the
        // rules that decide which of them is a screen live in `display`, where they
        // are tested on machines with no X server to disagree with them. It also
        // keeps those rules referenced from a path that compiles on every target,
        // so they cannot rot behind a `cfg`.
        match self.session.require().and_then(|server| server.outputs()) {
            Ok(outputs) => Ok(display::to_monitors(outputs)),
            Err(e) => {
                self.withdraw_if_gone(&e);
                Err(e)
            }
        }
    }
}

/// TODO: capture with XInput2 raw events — `XISelectEvents` on the root window for
/// `XI_RawKeyPress`, `XI_RawKeyRelease`, `XI_RawMotion`, `XI_RawButtonPress`,
/// `XI_RawButtonRelease`. Raw events are the right choice because they arrive
/// before the pointer is confined by grabs and report unaccelerated deltas.
///
/// Suppression needs a real grab: `XIGrabDevice` on the keyboard and pointer, or
/// the events reach the focused client as well and keystrokes land on two machines
/// at once. A grab is exclusive, so it must be released the instant control comes
/// back or the local desktop appears frozen — which is why this is a separate piece
/// of work rather than something to add to the injection slice. Every exit path
/// needs to release it, panics included, and a guard type rather than a `Drop`-free
/// code path is the way to get that right.
///
/// Key resolution is the half that is already solved, in pure Rust and with no
/// C library: [`keymap`] reads what a keycode produces from `GetKeyboardMapping`,
/// which is the same table injection resolves against, so the two directions cannot
/// disagree. Mask out Control before asking, the same as every other backend; that
/// is the [`crate::keyres::RawKey::text`] contract.
pub struct X11Capture;

impl InputCapture for X11Capture {
    fn start(&mut self, _sink: CaptureSink) -> Result<()> {
        Err(todo_err("input capture"))
    }

    fn stop(&mut self) -> Result<()> {
        Err(PlatformError::NotCapturing)
    }

    fn is_capturing(&self) -> bool {
        false
    }

    fn set_suppress_local(&mut self, _suppress: bool) -> Result<()> {
        Err(todo_err("input suppression"))
    }

    fn suppresses_local(&self) -> bool {
        false
    }
}

/// Input injection over XTEST.
///
/// The translation is [`inject`] and the requests are `server`; what lives here is
/// the ordering rule the Wayland backend also keeps. `release_all` is deliberately
/// *not* gated on the session being live: it runs on disconnect and at shutdown,
/// which are exactly the moments the connection may already have gone, and refusing
/// to clear then is how a modifier ends up stranded on a desktop with nothing left
/// to release it.
pub struct X11Injector {
    inner: inject::Injector,
}

impl InputInjector for X11Injector {
    fn inject(&mut self, monitor: &Monitor, event: &InputEvent) -> Result<()> {
        self.inner.inject(monitor, event)
    }

    fn warp_cursor(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()> {
        self.inner.warp_cursor(monitor, pos)
    }

    fn release_all(&mut self) -> Result<()> {
        self.inner.release_all()
    }
}

/// TODO: X11 clipboard is an ICCCM selection protocol, not storage: the owner has
/// to stay alive and answer `SelectionRequest` events. That means a long-lived
/// window and event loop, and it means the clipboard content vanishes when the
/// process exits unless a clipboard manager has taken a copy.
///
/// Handle both `CLIPBOARD` and `PRIMARY`; targets are `UTF8_STRING`,
/// `text/html`, `image/png`, and `text/uri-list`. `INCR` transfers must be
/// implemented for anything large, which a screenshot always is.
///
/// There is no change counter, so [`ClipboardAccess::change_serial`] has to be
/// synthesised from `XFixesSelectSelectionInput` notifications.
///
/// None of the Wayland clipboard code is reusable, because it is portal-shaped.
/// This is a separate decision rather than a line item on this backend.
pub struct X11Clipboard;

impl ClipboardAccess for X11Clipboard {
    fn available_formats(&self) -> Result<Vec<ClipboardFormat>> {
        Err(todo_err("clipboard access"))
    }

    fn read(&self, _format: ClipboardFormat) -> Result<Vec<u8>> {
        Err(todo_err("clipboard access"))
    }

    fn write(&self, _format: ClipboardFormat, _data: &[u8]) -> Result<()> {
        Err(todo_err("clipboard access"))
    }

    fn change_serial(&self) -> Result<u64> {
        Err(todo_err("clipboard access"))
    }
}

/// TODO: prefer the desktop's own lock over `XScreenSaverSuspend`, since the
/// screensaver and the lock screen are different things on Linux. In practice:
/// `loginctl lock-session` over the systemd-logind D-Bus interface, falling back to
/// the `org.freedesktop.ScreenSaver` portal.
pub struct X11Session;

impl ScreenSaverControl for X11Session {
    fn lock_session(&self) -> Result<()> {
        Err(todo_err("session locking"))
    }

    fn is_locked(&self) -> Result<bool> {
        Err(todo_err("session locking"))
    }
}

/// Everything this backend can honour, given what the connection turned out to be.
///
/// [`Capabilities::HAS_DISPLAYS`] only when enumeration *actually found a screen* —
/// not when it merely did not fail. A node that claims it with no screens invites
/// peers to route the cursor into a desktop that does not exist, and there is no
/// error to report when they do.
///
/// [`Capabilities::INJECT_INPUT`] whenever there is a connection, because on X11
/// that is the whole of the question: XTEST was checked when the connection was
/// made, and there is no grant that can be withdrawn afterwards.
///
/// Capture, suppression, the clipboard and screensaver sync are all absent because
/// each still returns [`PlatformError::Unsupported`]. Claiming any of them would
/// make a peer hand this node work it cannot do — and `SCREENSAVER_SYNC` in
/// particular is one a backend must implement and advertise together or it silently
/// stops locking its own screen.
fn capabilities(has_displays: bool, can_inject: bool) -> Capabilities {
    let mut caps = Capabilities::NONE;
    if has_displays {
        caps = caps.union(Capabilities::HAS_DISPLAYS);
    }
    if can_inject {
        caps = caps.union(Capabilities::INJECT_INPUT);
    }
    caps
}

pub fn backend() -> Result<PlatformBackend> {
    Ok(assemble(Session::connect()))
}

fn assemble(session: Session) -> PlatformBackend {
    // Made before the first enumeration rather than after it, because that
    // enumeration is itself the liveness check and it needs somewhere to publish a
    // connection that died between `connect` and here. It carries nothing until the
    // real set is put in below.
    let live = LiveCapabilities::fixed(Capabilities::NONE);
    let displays = X11Displays::new(session.clone(), live.clone());
    // A failed enumeration is not fatal: the node can still be told about peers,
    // and it can still inject into whatever it does have. Taking a whole machine
    // out of the mesh over a server that happens to lack RandR would be worse. It
    // is logged, because "my screens are missing from the layout" is otherwise
    // unattributable — and it is logged as a *failure*, distinct from finding no
    // screens, which is the confusion that had a machine with a 34" monitor
    // reporting itself headless.
    let has_displays = match displays.monitors() {
        Ok(monitors) => !monitors.is_empty(),
        Err(e) => {
            tracing::warn!(error = %e, "display enumeration failed; advertising no displays");
            false
        }
    };

    // Read after the enumeration, not before: if that enumeration is what found the
    // connection gone, `can_inject` already knows and this must not put the promise
    // back.
    let caps = capabilities(has_displays, session.can_inject());
    live.set(caps);
    let sink: Arc<dyn inject::Sink> = match session.require() {
        Ok(server) => Arc::clone(server) as Arc<dyn inject::Sink>,
        Err(e) => Arc::new(inject::NoServer::new(e.to_string())),
    };

    PlatformBackend {
        info: PlatformInfo {
            platform: Platform::Linux,
            display_server: DisplayServer::X11,
            capabilities: caps,
        },
        // X11 has no consent dialog and no revocable grant, so unlike Wayland
        // nothing here is taken away by a decision the user makes. What can still go
        // is the connection everything hangs off — see `withdraw_if_gone`, which is
        // the only writer after this point. `HAS_DISPLAYS` also changes with hotplug,
        // and the engine recomputes that from the monitor list on every tick; see
        // `with_displays` there, which is the one place that rule lives.
        live_capabilities: live,
        displays: Box::new(displays),
        capture: Box::new(X11Capture),
        injector: Box::new(X11Injector {
            inner: inject::Injector::new(sink),
        }),
        clipboard: Box::new(X11Clipboard),
        screensaver: Box::new(X11Session),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{MonitorId, PointerEvent, Rect};

    /// The backend as it is on a machine with no X server, which is what every
    /// non-Linux target and every headless runner has.
    fn offline() -> PlatformBackend {
        assemble(Session::absent("no display server available"))
    }

    #[test]
    fn the_backend_advertises_nothing_it_cannot_do() {
        for backend in [offline(), backend().unwrap()] {
            for unimplemented in [
                Capabilities::CAPTURE_INPUT,
                Capabilities::CLIPBOARD_TEXT,
                Capabilities::CLIPBOARD_IMAGE,
                Capabilities::FILE_TRANSFER,
                Capabilities::SCREENSAVER_SYNC,
                Capabilities::PRIVILEGED_INJECT,
                Capabilities::VIDEO_SOURCE,
                Capabilities::VIDEO_SINK,
                Capabilities::RELAY,
            ] {
                assert!(
                    !backend.info.capabilities.contains(unimplemented),
                    "advertised {unimplemented:?} while it still returns Unsupported"
                );
            }
            assert_eq!(backend.info.platform, Platform::Linux);
            assert_eq!(backend.info.display_server, DisplayServer::X11);
        }
    }

    #[test]
    fn injection_is_advertised_exactly_when_there_is_a_server_to_inject_into() {
        // The bit the captain's machine was missing. Without it nothing routed to
        // this node could ever be delivered, whatever else it advertised.
        assert!(!offline()
            .info
            .capabilities
            .contains(Capabilities::INJECT_INPUT));

        let live = backend().unwrap();
        assert_eq!(
            live.info.capabilities.contains(Capabilities::INJECT_INPUT),
            Session::connect().can_inject(),
            "injection was advertised without a connection, or withheld with one"
        );
    }

    #[test]
    fn screens_are_advertised_only_when_enumeration_actually_found_one() {
        // Not when it merely did not fail. A node claiming `HAS_DISPLAYS` with no
        // screens invites peers to route the cursor into a desktop that is not
        // there, and nothing reports an error when they do.
        assert!(capabilities(true, false).contains(Capabilities::HAS_DISPLAYS));
        assert!(!capabilities(false, true).contains(Capabilities::HAS_DISPLAYS));
        assert!(!offline()
            .info
            .capabilities
            .contains(Capabilities::HAS_DISPLAYS));
    }

    #[test]
    fn a_refusal_this_backend_makes_itself_never_costs_a_round_trip() {
        // A server with no RandR answers `Unsupported` on every housekeeping tick
        // for as long as the agent runs, and it injects perfectly well throughout.
        // Probing it would buy a round trip a second for an answer that cannot
        // change — and, worse, a probe that ever failed spuriously would withdraw
        // injection from a machine whose connection is fine.
        assert!(!is_connection_loss(
            &todo_err("display enumeration"),
            || panic!("a refusal this backend made itself must not be probed"),
        ));
    }

    #[test]
    fn injection_is_withdrawn_only_when_the_server_stops_answering() {
        // The distinction the probe exists for: x11rb reports a protocol error the
        // server raised against one request and a socket that has closed through the
        // same `Result`, and only the second means nothing can be injected again.
        let failed = PlatformError::Other("asking randr for the current screen resources".into());
        assert!(!is_connection_loss(&failed, || true));
        assert!(is_connection_loss(&failed, || false));
    }

    #[test]
    fn a_connection_that_died_stops_advertising_what_it_can_no_longer_do() {
        // What a peer must see. A node that keeps claiming `INJECT_INPUT` into a
        // dead X connection has peers route the cursor into it, every event fails
        // and is swallowed at debug level, and the reachability reclaim never fires
        // because the QUIC session is healthy — the cursor is simply gone.
        let session = Session::lost("the X server went away");
        let live = LiveCapabilities::fixed(capabilities(true, true));
        let displays = X11Displays::new(session.clone(), live.clone());

        assert!(displays.monitors().is_err());
        assert_eq!(live.get(), Capabilities::NONE);
        assert!(!session.can_inject());

        // And it stays gone. There is no reconnect path, so nothing may put the
        // promise back — and the same failing poll every tick must not re-announce
        // it either.
        assert!(
            !session.went_away(&PlatformError::Other("the next tick".into())),
            "a connection already known to be gone was reported gone again"
        );
    }

    #[test]
    fn a_failed_enumeration_with_no_connection_behind_it_withdraws_nothing() {
        // Every non-Linux target and every headless runner enumerates and fails on
        // every tick. There was never a connection to lose, the advertised set is
        // already the empty one, and this must not become a warning per tick.
        let live = LiveCapabilities::fixed(capabilities(false, false));
        let session = Session::absent("no display server available");
        let displays = X11Displays::new(session.clone(), live.clone());
        assert!(displays.monitors().is_err());
        assert!(!session.went_away(&PlatformError::Other("anything".into())));
        assert_eq!(live.get(), capabilities(false, false));
    }

    #[test]
    fn what_the_backend_advertises_at_startup_is_what_it_publishes_live() {
        // The two are read by different callers — the handshake takes `info`, the
        // engine's tick takes `current_capabilities` — and a node whose first
        // advertisement disagreed with its second would re-advertise on the first
        // tick for no reason.
        for backend in [offline(), backend().unwrap()] {
            assert_eq!(backend.info.capabilities, backend.current_capabilities());
        }
    }

    #[test]
    fn a_server_that_cannot_be_reached_is_not_fatal() {
        // The headless CI runner and every non-Linux target take this path. Losing
        // the whole backend here would remove the node from the mesh rather than
        // just from the layout.
        assert!(backend().is_ok());
    }

    #[test]
    fn every_refusal_names_the_reason_rather_than_collapsing_to_unsupported() {
        // The defect this backend was written against: "there are no displays" and
        // "I cannot tell you about displays" read identically to the agent, which
        // is how a machine with a 3440x1440 monitor came to report itself headless.
        let backend = offline();
        let err = backend.displays.monitors().unwrap_err();
        assert!(
            err.to_string().contains("no display server available"),
            "{err}"
        );
    }

    #[test]
    fn suppression_is_refused_rather_than_silently_ignored() {
        // A target-only backend never suppresses, and saying otherwise would be the
        // "my keystrokes went to both computers" failure. It must stay an error
        // rather than a quiet `Ok`.
        let mut backend = offline();
        assert!(backend.capture.set_suppress_local(true).is_err());
        assert!(!backend.capture.suppresses_local());
        assert!(backend.capture.start(Box::new(|_| {})).is_err());
    }

    #[test]
    fn release_all_succeeds_on_a_backend_that_never_had_a_server() {
        // Called on every disconnect and at shutdown, including on a node that
        // never connected to anything. An error here would be logged forever.
        let mut backend = offline();
        assert!(backend.injector.release_all().is_ok());
    }

    #[test]
    fn injection_into_a_backend_with_no_server_is_refused_rather_than_swallowed() {
        let mut backend = offline();
        let monitor = Monitor {
            id: MonitorId(0),
            name: "fake".into(),
            local_bounds: Rect::new(0, 0, 1920, 1080),
            scale: 1.0,
            primary: true,
        };
        assert!(backend
            .injector
            .inject(
                &monitor,
                &InputEvent::Pointer(PointerEvent::MoveBy { dx: 1.0, dy: 0.0 }),
            )
            .is_err());
    }

    #[test]
    fn constructing_the_backend_asks_the_user_for_nothing() {
        // The rule `current_platform` states, and the one X11 keeps for free: there
        // is no dialog to raise, now or ever, which is the property that made this
        // the right backend for a machine nobody sits at.
        let backend = backend().unwrap();
        assert!(!backend
            .info
            .capabilities
            .contains(Capabilities::CAPTURE_INPUT));
    }
}
