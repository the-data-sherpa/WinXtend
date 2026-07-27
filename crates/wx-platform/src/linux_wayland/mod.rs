//! Linux/Wayland backend.
//!
//! | Concern | Mechanism | State |
//! |---|---|---|
//! | Displays | `wl_output` + `xdg_output` | implemented, see [`display`] |
//! | Capture | libei via the RemoteDesktop portal | session and transport implemented; event translation a skeleton |
//! | Injection | libei | implemented, see [`inject`] and [`keymap`] |
//! | Clipboard | `wl_data_device` / `zwlr_data_control_manager_v1` | skeleton |
//! | Locking | systemd-logind | skeleton |
//!
//! The unimplemented parts are sketched from the libei and xdg-desktop-portal
//! APIs; see the note in [`crate::macos`] about why these skeletons compile on
//! every target.
//!
//! Display enumeration is deliberately first: it is the one piece that needs no
//! portal, no consent dialog and no libei, so a Linux machine can appear in the
//! layout editor and be arranged before it can capture or inject anything.
//! Injection came second, before capture, because a node that can only be driven
//! is already useful — it can be the receiving end of a mesh — and because it is
//! testable on one machine with no peer.
//!
//! # Why this backend matters more than its line count suggests
//!
//! Wayland is the default session on current Fedora, Ubuntu, and SteamOS, and the
//! Synergy-lineage tools still do not support it. Supporting it properly is a
//! reason to choose WinXtend rather than a checkbox, so the remaining pieces are
//! written to be filled in with the *portal* path rather than an X11 fallback hack.
//!
//! # The shape Wayland forces on this
//!
//! Wayland deliberately has no API for "read all input" or "synthesise input" —
//! that was the point of the redesign. Both go through
//! `xdg-desktop-portal`'s `RemoteDesktop` interface, which means:
//!
//! * A **user consent dialog** the first time the agent runs, and a session handle
//!   that the portal can revoke at any moment. Refusal and revocation are both final
//!   for that run: they are reported as [`PlatformError::PermissionDenied`], the
//!   input capabilities go away and the agent re-advertises the smaller set to its
//!   peers, and nothing puts the dialog back on the user's screen. [`session`] owns
//!   that rule.
//! * A **restore token** persisted between runs, so the consent prompt appears once
//!   rather than on every launch. Without it the product is unusable as a daemon.
//!   [`token`] owns that, including how a user asks to be prompted again.
//! * **libei** (`ei_*`) as the actual transport for events once the portal has
//!   handed over a file descriptor. The private `driver` module owns that.
//!
//! # One session, both directions
//!
//! `SelectDevices` asks for keyboard and pointer together and the portal answers with
//! a single session covering both, so [`WaylandCapture`] and [`WaylandInjector`] share
//! one [`PortalSession`]. Creating one each would mean two consent dialogs, two
//! sessions to revoke, and two chances to leak one.
//!
//! Session handles are per D-Bus connection, which is the other reason the connection
//! is owned in one place: reconnecting invalidates every handle taken out over the old
//! one.
//!
//! The session grants input permission and nothing else, which is why it publishes
//! capabilities *on top of* what the backend already advertises rather than in place
//! of them. Display enumeration needs no portal, so a refusal must not take this
//! machine's screens out of the layout.
//!
//! # What a live session advertises today
//!
//! [`Capabilities::INJECT_INPUT`], and only that. The portal grants keyboard and
//! pointer access that serves both directions, but a capability bit is a promise
//! about what this backend can *do*: [`WaylandInjector`] translates the whole of
//! [`InputEvent`] onto libei, while [`WaylandCapture::start`] still returns
//! [`PlatformError::Unsupported`] because the reverse translation is a skeleton.
//! So [`SESSION_CAPABILITIES`] — what a live session contributes to the advertised
//! set — is `INJECT_INPUT`, and capture is the half [`SESSION_OWNED_CAPABILITIES`]
//! is waiting on.
//!
//! Claiming capture here would have peers sit waiting for input from a machine
//! that will never send any. Claiming injection before it worked would have been
//! the worse mistake in the other direction — the cursor crossing onto a desktop
//! where the keyboard goes dead — which is why the bit was withheld until the
//! translation existed, and why it also silences `Engine::on_peer_ready`'s notice
//! about a peer that has screens and cannot take input.
//!
//! The lifecycle it feeds is not theoretical. The full transition — `0` →
//! `INJECT_INPUT` on the grant, back to `0` on revocation, with the new set
//! re-advertised to peers each time — was verified by hand on real hardware; see
//! the log at the top of the private `driver` module.
//!
//! One thing to check before building capture on top of this. Every device this
//! session offers is an *emulation* device — on GNOME 50 the seat produces "WinXtend
//! virtual pointer", "WinXtend virtual keyboard" and "WinXtend shared virtual absolute
//! pointer" — which is exactly what injection needed and is why this half landed on
//! the session as it stands. `RemoteDesktop` is the portal for *driving* a desktop;
//! the portal for *receiving* input is `org.freedesktop.portal.InputCapture`, which
//! is present on the alpha target (version 1, `SupportedCapabilities` 15) and has a
//! `ConnectToEIS` of its own. Whether capture can be served from this session or
//! needs that one is the first question to settle in the capture slice, not an
//! assumption to inherit.
//!
//! There is no global grab, so [`InputCapture::set_suppress_local`] cannot be
//! implemented the way it is on X11 or Windows. The portal session itself is
//! exclusive while active, which is the closest equivalent; a compositor that does
//! not honour that leaves keystrokes reaching local windows, and the backend should
//! report that honestly rather than pretend.

use std::path::Path;
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
pub mod keymap;
pub mod keys;
pub mod session;
pub mod token;

#[cfg(target_os = "linux")]
mod outputs;

#[cfg(target_os = "linux")]
mod driver;
#[cfg(not(target_os = "linux"))]
mod driver_stub;
#[cfg(not(target_os = "linux"))]
use driver_stub as driver;

#[cfg(target_os = "linux")]
mod inject;
#[cfg(not(target_os = "linux"))]
mod inject_stub;
#[cfg(not(target_os = "linux"))]
use inject_stub as inject;

pub use display::WaylandDisplays;
pub use session::{SessionState, SESSION_CAPABILITIES, SESSION_OWNED_CAPABILITIES};
pub use token::RESTORE_TOKEN_FILE;

use session::SharedSession;

const BACKEND: &str = "linux-wayland";

fn todo_err(operation: &'static str) -> PlatformError {
    PlatformError::Unsupported {
        operation,
        backend: BACKEND,
    }
}

/// The `RemoteDesktop` portal session, shared by capture and injection.
///
/// Held behind an [`Arc`] by both, so the session is torn down exactly once — when
/// the whole [`PlatformBackend`] is dropped — rather than when either side happens to
/// stop first. Injection outliving capture is the ordinary case: a machine can go on
/// being driven by a peer long after it has stopped driving anything itself.
pub struct PortalSession {
    /// Dropped first, which signals the thread and waits for it. Ordering is not
    /// load-bearing — the thread holds its own handle on the state — but reads in
    /// the order things actually happen.
    _driver: Option<driver::Driver>,
    shared: Arc<SharedSession>,
    /// The libei devices, published by the driver thread and used by
    /// [`WaylandInjector`]. Held here rather than on the injector so that the
    /// driver has somewhere to put them the moment the compositor offers them,
    /// which is before anything asks to inject.
    transport: Arc<inject::Transport>,
}

impl PortalSession {
    /// A session that was never asked for, and never will be.
    ///
    /// What [`crate::current_platform`] hands back: constructing a backend must not
    /// put a consent dialog on anyone's screen.
    fn inert(live: LiveCapabilities, base: Capabilities) -> Self {
        Self {
            _driver: None,
            shared: Arc::new(SharedSession::new(live, base)),
            transport: Arc::new(inject::Transport::new()),
        }
    }

    /// Start acquiring the session in the background.
    fn starting(live: LiveCapabilities, base: Capabilities, config_dir: &Path) -> Self {
        let shared = Arc::new(SharedSession::new(live, base));
        let transport = Arc::new(inject::Transport::new());
        let driver = driver::start(
            Arc::clone(&shared),
            Arc::clone(&transport),
            config_dir.to_path_buf(),
        );
        Self {
            _driver: Some(driver),
            shared,
            transport,
        }
    }

    /// Where the session is in its life. Public so the agent can say something
    /// useful about why input is unavailable.
    pub fn state(&self) -> SessionState {
        self.shared.state()
    }

    /// The error to report to a caller that needs the session, or `None` if it is
    /// live.
    fn require(&self) -> Result<()> {
        match self.shared.error() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Input capture over the portal session.
///
/// The session and its libei transport are in place; what is not yet here is the
/// translation of `ei_event`s into [`crate::traits::CapturedEvent`], which needs the
/// same libxkbcommon path as the X11 backend (`xkb_state_key_get_utf8`) because libei
/// reports evdev keycodes rather than text.
///
/// [`InputCapture::start`] still reports the *session's* answer first, so a caller
/// that has lost permission is told that rather than being told capture is merely
/// unimplemented — those need different responses from the agent, and only one of
/// them is something the user can act on.
pub struct WaylandCapture {
    portal: Arc<PortalSession>,
}

impl InputCapture for WaylandCapture {
    fn start(&mut self, _sink: CaptureSink) -> Result<()> {
        self.portal.require()?;
        Err(todo_err("input capture"))
    }

    fn stop(&mut self) -> Result<()> {
        Err(PlatformError::NotCapturing)
    }

    fn is_capturing(&self) -> bool {
        false
    }

    /// TODO: Wayland has no grab. The portal session is the only exclusivity
    /// available, so this should report [`PlatformError::Unsupported`] on
    /// compositors that do not provide it rather than silently letting input reach
    /// both machines.
    fn set_suppress_local(&mut self, _suppress: bool) -> Result<()> {
        Err(todo_err("input suppression"))
    }

    fn suppresses_local(&self) -> bool {
        false
    }
}

/// Input injection over the portal session's libei transport.
///
/// The translation itself is [`inject`]; what lives here is the order the two
/// answers are given in. The *session's* answer comes first, so a caller that has
/// lost permission is told that rather than being told the transport is merely
/// absent — those need different responses from the agent, and only one of them is
/// something the user can act on.
///
/// Text is resolved against the receiving desktop's own keyboard layout; see
/// [`keymap`] for why that is the only route Wayland leaves open and what it
/// cannot do.
pub struct WaylandInjector {
    portal: Arc<PortalSession>,
    inner: inject::Injector,
}

impl InputInjector for WaylandInjector {
    fn inject(&mut self, monitor: &Monitor, event: &InputEvent) -> Result<()> {
        self.portal.require()?;
        self.inner.inject(monitor, event)
    }

    fn warp_cursor(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()> {
        self.portal.require()?;
        self.inner.warp_cursor(monitor, pos)
    }

    /// Deliberately not gated on [`PortalSession::require`].
    ///
    /// This runs on disconnect and at shutdown, which are exactly the moments the
    /// session may already have been revoked — and refusing to clear then is how a
    /// modifier ends up stranded on a desktop with nothing left to release it.
    fn release_all(&mut self) -> Result<()> {
        self.inner.release_all()
    }
}

/// TODO: `wl_data_device` for the clipboard, or
/// `zwlr_data_control_manager_v1` where the compositor offers it — the latter is
/// what makes clipboard access work without a focused surface, which a headless
/// agent never has.
///
/// MIME types map directly: `text/plain;charset=utf-8`, `text/html`, `image/png`,
/// `text/uri-list`. No change counter exists, so
/// [`ClipboardAccess::change_serial`] must be synthesised from selection events.
pub struct WaylandClipboard;

impl ClipboardAccess for WaylandClipboard {
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

/// TODO: `loginctl lock-session` via the systemd-logind D-Bus interface, which is
/// compositor-independent and the only route that works on every desktop.
pub struct WaylandSession;

impl ScreenSaverControl for WaylandSession {
    fn lock_session(&self) -> Result<()> {
        Err(todo_err("session locking"))
    }

    fn is_locked(&self) -> Result<bool> {
        Err(todo_err("session locking"))
    }
}

/// Everything this build can do on Wayland *before* any portal session exists.
///
/// Display enumeration is the only such piece, and it is advertised only when a
/// display was actually found. A node that claims `HAS_DISPLAYS` with no screens
/// invites peers to route the cursor into a desktop that does not exist, and there
/// is no error to report when they do.
///
/// Capture and injection are absent here because at startup nobody has consented to
/// them. Injection appears in [`PlatformBackend::current_capabilities`] once the
/// portal grants a session — [`SESSION_CAPABILITIES`] is where that happens — and
/// goes away again the moment the session does. Capture, clipboard and screensaver
/// sync stay unadvertised in both places because they still return
/// [`PlatformError::Unsupported`]. Claiming any of them would make a peer hand this
/// node the cursor and then watch it disappear.
fn capabilities(has_displays: bool) -> Capabilities {
    if has_displays {
        Capabilities::HAS_DISPLAYS
    } else {
        Capabilities::NONE
    }
}

/// The backend with no portal session, and so no input permission.
///
/// See [`crate::current_platform`] for why constructing a backend must not prompt.
pub fn backend() -> Result<PlatformBackend> {
    let (displays, base) = enumerate();
    let live = LiveCapabilities::fixed(base);
    Ok(assemble(
        displays,
        base,
        live.clone(),
        PortalSession::inert(live, base),
    ))
}

/// The backend with the portal session being acquired in the background.
///
/// `config_dir` is where the restore token lives, beside the identity key and trust
/// store. The consent dialog — if one is needed at all — appears while this function
/// has already returned.
pub fn backend_in(config_dir: &Path) -> Result<PlatformBackend> {
    let (displays, base) = enumerate();
    let live = LiveCapabilities::fixed(base);
    Ok(assemble(
        displays,
        base,
        live.clone(),
        PortalSession::starting(live, base, config_dir),
    ))
}

/// Enumerate the outputs once, and decide what that alone lets the node advertise.
fn enumerate() -> (WaylandDisplays, Capabilities) {
    let displays = WaylandDisplays::new();
    // A failed enumeration is not fatal: the node can still forward, and it can
    // still inject once the portal grants a session. Treating it as fatal would
    // take a whole machine out of the mesh over a compositor that happens not to
    // offer `xdg_output`. It is logged, because "my screens are missing from the
    // layout" is otherwise unattributable.
    let has_displays = match displays.monitors() {
        Ok(monitors) => !monitors.is_empty(),
        Err(e) => {
            tracing::warn!(error = %e, "display enumeration failed; advertising no displays");
            false
        }
    };
    (displays, capabilities(has_displays))
}

fn assemble(
    displays: WaylandDisplays,
    base: Capabilities,
    live: LiveCapabilities,
    portal: PortalSession,
) -> PlatformBackend {
    let portal = Arc::new(portal);
    PlatformBackend {
        info: PlatformInfo {
            platform: Platform::Linux,
            display_server: DisplayServer::Wayland,
            // Displays and nothing else, always: on Wayland every input capability
            // depends on a portal session that has not been granted yet — and may
            // never be. `live_capabilities` is where the real answer appears.
            capabilities: base,
        },
        live_capabilities: live,
        displays: Box::new(displays),
        capture: Box::new(WaylandCapture {
            portal: Arc::clone(&portal),
        }),
        injector: Box::new(WaylandInjector {
            inner: inject::Injector::new(Arc::clone(&portal.transport)),
            portal,
        }),
        clipboard: Box::new(WaylandClipboard),
        screensaver: Box::new(WaylandSession),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_advertises_nothing_it_cannot_do() {
        // Display enumeration is implemented; nothing else is without a portal
        // session, and constructing the backend never asks for one. Anything beyond
        // HAS_DISPLAYS here would be a promise the backend cannot keep.
        let backend = backend().unwrap();
        for unimplemented in [
            Capabilities::CAPTURE_INPUT,
            Capabilities::INJECT_INPUT,
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
        assert_eq!(backend.info.display_server, DisplayServer::Wayland);
    }

    #[test]
    fn screens_are_advertised_only_when_there_are_some() {
        assert!(capabilities(true).contains(Capabilities::HAS_DISPLAYS));
        assert!(!capabilities(false).contains(Capabilities::HAS_DISPLAYS));
    }

    #[test]
    fn a_compositor_that_cannot_be_reached_is_not_fatal() {
        // The headless CI runner takes this path. Losing the whole backend here
        // would remove the node from the mesh over a missing compositor rather
        // than just from the layout.
        assert!(backend().is_ok());
    }

    #[test]
    fn suppression_is_refused_rather_than_silently_ignored() {
        // Silently succeeding here would mean keystrokes reach the local desktop
        // and the remote peer at the same time, with nothing to indicate why.
        let mut backend = backend().unwrap();
        assert!(backend.capture.set_suppress_local(true).is_err());
        assert!(!backend.capture.suppresses_local());
    }

    #[test]
    fn constructing_the_backend_never_asks_the_portal_for_anything() {
        // The guard on the whole design: `cargo test` and `wx-agent --status` must
        // not put a consent dialog on the developer's screen. A session that has not
        // been started is `Idle`, and no thread exists to start one — so no input
        // capability can appear, whatever the displays did.
        let mut backend = backend().unwrap();
        let live = backend.current_capabilities();
        assert!(!live.contains(Capabilities::CAPTURE_INPUT));
        assert!(!live.contains(Capabilities::INJECT_INPUT));
        // Capture and injection both refuse, and neither refusal is a permission
        // problem: nobody has been asked yet.
        assert!(!matches!(
            backend.capture.start(Box::new(|_| {})),
            Err(PlatformError::PermissionDenied(_))
        ));
        assert!(backend.injector.release_all().is_ok());
    }

    #[test]
    fn capture_and_injection_share_one_session() {
        // Two sessions would mean two consent dialogs and two things to revoke. The
        // observable consequence of sharing: revoking once silences both.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let shared = Arc::new(SharedSession::new(live.clone(), Capabilities::NONE));
        let portal = detached(Arc::clone(&shared));
        let mut capture = WaylandCapture {
            portal: Arc::clone(&portal),
        };
        let mut injector = injector_on(Arc::clone(&portal));

        shared.starting();
        shared.activate(SESSION_OWNED_CAPABILITIES);
        assert_eq!(portal.state(), SessionState::Active);

        shared.denied("the user closed the sharing indicator");

        assert!(
            live.get().is_empty(),
            "a revoked session advertises nothing"
        );
        assert!(matches!(
            capture.start(Box::new(|_| {})),
            Err(PlatformError::PermissionDenied(_))
        ));
        assert!(matches!(
            injector.inject(&monitor(), &nudge()),
            Err(PlatformError::PermissionDenied(_))
        ));
    }

    #[test]
    fn a_live_session_advertises_what_it_was_granted_and_only_that() {
        // The dynamic half of `PlatformInfo`: a Wayland node holds these only while
        // the portal session does, so peers get the real answer rather than the one
        // that was true at startup. Driven with an explicit grant rather than with
        // the shipped `SESSION_CAPABILITIES`, which is only half of it today.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let shared = SharedSession::new(live.clone(), Capabilities::NONE);
        shared.starting();
        assert!(
            live.get().is_empty(),
            "nothing is advertised while the dialog is still up"
        );
        shared.activate(SESSION_OWNED_CAPABILITIES);
        assert!(live
            .get()
            .contains(Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT));
    }

    #[test]
    fn a_granted_session_with_no_transport_reports_that_rather_than_a_refusal() {
        // The session state and the transport are two different answers, and the
        // agent responds to them differently: a refusal is something the user can
        // act on, a missing transport is not. So a session the portal granted but
        // whose libei devices never arrived must not come back as
        // `PermissionDenied` and send somebody to a settings panel.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let shared = Arc::new(SharedSession::new(live.clone(), Capabilities::NONE));
        let mut injector = injector_on(detached(Arc::clone(&shared)));

        shared.starting();
        shared.activate(SESSION_CAPABILITIES);
        assert_eq!(shared.state(), SessionState::Active);
        assert!(live.get().contains(Capabilities::INJECT_INPUT));

        let err = injector.inject(&monitor(), &nudge()).unwrap_err();
        assert!(
            !matches!(err, PlatformError::PermissionDenied(_)),
            "nobody refused anything; the transport is simply not there: {err}"
        );
    }

    /// A portal session with no driver behind it, for the rules that are about
    /// state rather than about a live compositor.
    fn detached(shared: Arc<SharedSession>) -> Arc<PortalSession> {
        Arc::new(PortalSession {
            _driver: None,
            shared,
            transport: Arc::new(inject::Transport::new()),
        })
    }

    fn injector_on(portal: Arc<PortalSession>) -> WaylandInjector {
        WaylandInjector {
            inner: inject::Injector::new(Arc::clone(&portal.transport)),
            portal,
        }
    }

    #[test]
    fn the_portal_session_never_unadvertises_the_screens() {
        // Where display enumeration and the portal session meet: the session
        // publishes by replacing the advertised set, so it has to be told what the
        // backend already claimed without it. A node whose screens vanished from
        // peers' layouts the moment the user granted — or refused — input would be
        // a regression nobody would think to look for in the session code.
        let base = capabilities(true);
        let live = LiveCapabilities::fixed(base);
        let shared = SharedSession::new(live.clone(), base);

        shared.starting();
        shared.activate(SESSION_OWNED_CAPABILITIES);
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));

        shared.denied("the desktop portal revoked the session");
        assert!(live.get().contains(Capabilities::HAS_DISPLAYS));
        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(!live.get().contains(Capabilities::INJECT_INPUT));
    }

    fn monitor() -> Monitor {
        Monitor {
            id: wx_proto::MonitorId(0),
            name: "fake".into(),
            local_bounds: wx_proto::Rect::new(0, 0, 1920, 1080),
            scale: 1.0,
            primary: true,
        }
    }

    fn nudge() -> InputEvent {
        InputEvent::Pointer(wx_proto::PointerEvent::MoveBy { dx: 1.0, dy: 0.0 })
    }
}
