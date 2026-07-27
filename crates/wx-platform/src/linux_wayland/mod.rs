//! Linux/Wayland backend.
//!
//! | Concern | Mechanism | State |
//! |---|---|---|
//! | Displays | `wl_output` + `xdg_output` | implemented, see [`display`] |
//! | Capture | libei via the InputCapture portal | implemented, see [`capture`] |
//! | Injection | libei via the RemoteDesktop portal | implemented, see [`inject`] and [`keymap`] |
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
//! that was the point of the redesign. Both go through `xdg-desktop-portal`, which
//! means:
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
//! # Two portals, two sessions — and why that is not the mistake it looks like
//!
//! Injection and capture are **different interfaces**, not two halves of one.
//!
//! `RemoteDesktop` is the portal for *driving* a desktop. Every device its session
//! offers is an emulation device — on GNOME 50 the seat produces "WinXtend virtual
//! pointer", "WinXtend virtual keyboard" and "WinXtend shared virtual absolute
//! pointer" — and its session object has no zones, no barriers, no
//! `Enable`/`Disable`/`Release` and no activation. Measured on the alpha target:
//! ten seconds of real typing and mousing through it produced zero captured
//! events, handshaking as both Receiver and Sender.
//!
//! `InputCapture` is the portal for *receiving* input, and it is what GNOME 50
//! ships for the first time. It has zones, barriers, an activation that pins the
//! pointer and suppresses local delivery, and a `ConnectToEIS` of its own.
//!
//! So [`PortalSession`] holds two sessions, and the user answers two consent
//! dialogs — once per launch each. Sharing one is not an option that was passed
//! over; it is not on offer. What *is* shared is the rule they both obey: a
//! session publishes only its own capability bit and leaves everything else on the
//! advertised set alone, so losing one does not unadvertise the other and neither
//! takes this machine's screens out of peers' layouts. [`session`] owns that.
//!
//! Session handles are per D-Bus connection, which is why each connection is owned
//! in one place: reconnecting invalidates every handle taken out over the old one.
//!
//! # What a live session advertises
//!
//! [`Capabilities::INJECT_INPUT`] from the `RemoteDesktop` session and
//! [`Capabilities::CAPTURE_INPUT`] from the `InputCapture` one, each only while its
//! own session is live and each dropped the moment it is revoked. A capability bit
//! is a promise about what this backend can *do*, so neither is claimed on the
//! strength of the other: a user who grants injection and refuses capture has a
//! machine that can be driven and cannot drive, and peers are told exactly that.
//!
//! Capture is probed at runtime rather than assumed. GNOME before 50 ships no
//! `InputCapture` implementation at all, and a desktop without one reports
//! [`SessionState::Unsupported`] and advertises nothing — it does not put a dialog
//! on screen and it does not retry.
//!
//! # Suppression
//!
//! There is no global grab on Wayland, so [`InputCapture::set_suppress_local`]
//! cannot be implemented the way it is on X11 or Windows. The `InputCapture`
//! session's *activation* is the equivalent, and on the alpha target it is a real
//! one: across five runs a focused full-screen editor received 0 keys, 0 buttons
//! and 0 scroll events while capture was active, and the pointer was pinned rather
//! than merely quiet. That measurement is inherited, not this change's own: it was
//! made by the investigation recorded on issue #7 that commissioned this slice,
//! and is not re-derived here.
//!
//! It stands, and so suppression is implemented and reported truthfully rather
//! than refused — returning `Unsupported` would be the inaccurate answer. The
//! one thing the portal will not do is let a client *start* an activation — only
//! the user crossing an armed barrier does that — so a request to suppress while
//! nothing is activated is an error rather than a silent success. The reasoning,
//! and the measurements, are at the top of [`capture`].

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

pub mod capture;
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
mod capture_driver;
#[cfg(not(target_os = "linux"))]
mod capture_driver_stub;
#[cfg(not(target_os = "linux"))]
use capture_driver_stub as capture_driver;

#[cfg(target_os = "linux")]
mod inject;
#[cfg(not(target_os = "linux"))]
mod inject_stub;
#[cfg(not(target_os = "linux"))]
use inject_stub as inject;

pub use capture::CaptureState;
pub use display::WaylandDisplays;
pub use session::{
    SessionState, INPUT_CAPTURE_CAPABILITIES, PORTAL_INPUT_CAPTURE, PORTAL_REMOTE_DESKTOP,
    REMOTE_DESKTOP_CAPABILITIES,
};
pub use token::RESTORE_TOKEN_FILE;

use session::SharedSession;

const BACKEND: &str = "linux-wayland";

fn todo_err(operation: &'static str) -> PlatformError {
    PlatformError::Unsupported {
        operation,
        backend: BACKEND,
    }
}

/// The `RemoteDesktop` portal session, which injection sends on.
///
/// Held behind an [`Arc`], so the session is torn down exactly once — when the
/// whole [`PlatformBackend`] is dropped — rather than when any one user of it
/// happens to stop first.
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
            shared: Arc::new(remote_desktop_session(live, base)),
            transport: Arc::new(inject::Transport::new()),
        }
    }

    /// Start acquiring the session in the background.
    fn starting(live: LiveCapabilities, base: Capabilities, config_dir: &Path) -> Self {
        let shared = Arc::new(remote_desktop_session(live, base));
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

fn remote_desktop_session(live: LiveCapabilities, base: Capabilities) -> SharedSession {
    SharedSession::new(
        live,
        base,
        REMOTE_DESKTOP_CAPABILITIES,
        PORTAL_REMOTE_DESKTOP,
    )
}

fn input_capture_session(live: LiveCapabilities, base: Capabilities) -> SharedSession {
    SharedSession::new(live, base, INPUT_CAPTURE_CAPABILITIES, PORTAL_INPUT_CAPTURE)
}

/// The `InputCapture` portal session, which capture receives on.
///
/// Its own type rather than a second field on [`PortalSession`] because it is a
/// different portal with a different lifecycle: it can be granted while injection
/// is refused, or the other way round, and each has to be able to say so.
pub struct CaptureSession {
    _driver: Option<capture_driver::Driver>,
    shared: Arc<SharedSession>,
    state: Arc<CaptureState>,
}

impl CaptureSession {
    /// A session that was never asked for, and never will be.
    fn inert(live: LiveCapabilities, base: Capabilities) -> Self {
        Self {
            _driver: None,
            shared: Arc::new(input_capture_session(live, base)),
            state: Arc::new(CaptureState::new()),
        }
    }

    /// Start acquiring the session in the background.
    fn starting(live: LiveCapabilities, base: Capabilities) -> Self {
        let shared = Arc::new(input_capture_session(live, base));
        let state = Arc::new(CaptureState::new());
        let driver = capture_driver::start(Arc::clone(&shared), Arc::clone(&state));
        Self {
            _driver: Some(driver),
            shared,
            state,
        }
    }

    pub fn state(&self) -> SessionState {
        self.shared.state()
    }

    /// The error to give a caller that wants to *begin* capturing.
    ///
    /// Deliberately more permissive than asking whether the session is live. The
    /// agent starts capture as it boots, which is while the consent dialog is
    /// still on screen — there is no other moment for it to do so, because it
    /// starts capture once and never retries. Refusing then would leave a machine
    /// that can never drive anything however the user answers the dialog.
    ///
    /// So a session that is still being acquired accepts the sink and arms itself
    /// the moment it is granted; [`CaptureState::attach`] is where that happens. A
    /// session that has already failed, been refused, or was never started at all
    /// refuses, because nothing is coming.
    fn require_startable(&self) -> Result<()> {
        match self.shared.state() {
            SessionState::Starting | SessionState::Active => Ok(()),
            // `Idle` means no driver was ever spawned — `current_platform` rather
            // than `current_platform_in`. Nothing will ever grant this.
            _ => Err(self.shared.error().unwrap_or_else(capture::no_session)),
        }
    }
}

/// Input capture over the `InputCapture` portal session.
///
/// The translation itself is [`capture`]; what lives here is the order the two
/// answers are given in. The *session's* answer comes first, so a caller that has
/// lost permission — or is running on a desktop with no `InputCapture` portal at
/// all — is told that rather than being told capture merely is not running. Those
/// need different responses from the agent, and only one of them is something the
/// user can act on.
///
/// [`InputCapture::stop`] and [`InputCapture::set_suppress_local`] are
/// deliberately *not* gated on it, for the same reason
/// [`InputInjector::release_all`] is not: they run when input is being torn down,
/// which is exactly the moment the session may already have gone, and refusing
/// then is how a modifier ends up stranded on a peer.
pub struct WaylandCapture {
    session: Arc<CaptureSession>,
}

impl InputCapture for WaylandCapture {
    fn start(&mut self, sink: CaptureSink) -> Result<()> {
        self.session.require_startable()?;
        self.session.state.start(sink)
    }

    fn stop(&mut self) -> Result<()> {
        self.session.state.stop()
    }

    fn is_capturing(&self) -> bool {
        self.session.state.is_capturing()
    }

    /// Suppression is real on this backend, and is reported truthfully.
    ///
    /// See the module docs for what was measured, and [`capture`] for why asking
    /// to suppress while the compositor has not activated the session is an error
    /// rather than a quiet success.
    fn set_suppress_local(&mut self, suppress: bool) -> Result<()> {
        self.session.state.set_suppress_local(suppress)
    }

    fn suppresses_local(&self) -> bool {
        self.session.state.suppresses_local()
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
/// them. Each appears in [`PlatformBackend::current_capabilities`] once *its* portal
/// grants a session — [`REMOTE_DESKTOP_CAPABILITIES`] and
/// [`INPUT_CAPTURE_CAPABILITIES`] are where that happens — and goes away again the
/// moment that session does. Clipboard and screensaver sync stay unadvertised in
/// both places because they still return [`PlatformError::Unsupported`]. Claiming
/// either would make a peer hand this node work it cannot do.
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
        PortalSession::inert(live.clone(), base),
        CaptureSession::inert(live, base),
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
        PortalSession::starting(live.clone(), base, config_dir),
        CaptureSession::starting(live, base),
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
    capture: CaptureSession,
) -> PlatformBackend {
    let portal = Arc::new(portal);
    let capture = Arc::new(capture);
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
        capture: Box::new(WaylandCapture { session: capture }),
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
        // The contract this test has always encoded, unchanged: suppression must
        // never be silently false. What changed underneath it is *why* it refuses.
        // Suppression on this backend is real — the `InputCapture` portal's
        // activation is exclusive, and that was measured — but the portal gives a
        // client no way to start an activation, so on a backend with no session at
        // all there is nothing suppressing anything and saying otherwise would be
        // the "my keystrokes went to both computers" failure.
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
    fn each_portal_can_be_refused_without_taking_the_other_with_it() {
        // Capture and injection are separate interfaces with separate consent
        // dialogs, so either can be granted while the other is refused. A user who
        // says yes to being driven and no to driving has a machine that must
        // advertise exactly that — and revoking one must not silence the other.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let inject_shared = Arc::new(remote_desktop_session(live.clone(), Capabilities::NONE));
        let capture_shared = Arc::new(input_capture_session(live.clone(), Capabilities::NONE));
        let mut capture = capture_on(Arc::clone(&capture_shared));
        let mut injector = injector_on(detached(Arc::clone(&inject_shared)));

        inject_shared.starting();
        inject_shared.activate(REMOTE_DESKTOP_CAPABILITIES);
        capture_shared.starting();
        capture_shared.activate(INPUT_CAPTURE_CAPABILITIES);
        assert!(live
            .get()
            .contains(Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT));

        capture_shared.denied("the user closed the sharing indicator");

        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(
            live.get().contains(Capabilities::INJECT_INPUT),
            "losing capture stopped this machine being driven"
        );
        assert!(matches!(
            capture.start(Box::new(|_| {})),
            Err(PlatformError::PermissionDenied(_))
        ));
        assert!(injector.inject(&monitor(), &nudge()).is_err());
    }

    #[test]
    fn a_live_session_advertises_what_it_was_granted_and_only_that() {
        // The dynamic half of `PlatformInfo`: a Wayland node holds these only while
        // its portal session does, so peers get the real answer rather than the one
        // that was true at startup.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let shared = remote_desktop_session(live.clone(), Capabilities::NONE);
        shared.starting();
        assert!(
            live.get().is_empty(),
            "nothing is advertised while the dialog is still up"
        );
        shared.activate(REMOTE_DESKTOP_CAPABILITIES);
        assert!(live.get().contains(Capabilities::INJECT_INPUT));
        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));
    }

    #[test]
    fn capture_can_be_started_while_the_consent_dialog_is_still_up() {
        // The agent starts capture as it boots, once, and never retries — see
        // `Engine::start_capture`. The portal session takes as long as the user
        // takes to answer, so refusing during that window leaves a machine that
        // can never drive anything however they answer.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let shared = Arc::new(input_capture_session(live, Capabilities::NONE));
        let mut capture = capture_on(Arc::clone(&shared));

        shared.starting();
        assert!(capture.start(Box::new(|_| {})).is_ok());
        assert!(capture.is_capturing());
        // ...and nothing is claimed to be suppressed on the strength of it.
        assert!(!capture.suppresses_local());
    }

    #[test]
    fn capture_cannot_be_started_on_a_session_that_will_never_arrive() {
        // The other side of the same rule. A refusal is final, and a backend built
        // by `current_platform` has no driver at all, so accepting a sink either
        // way would be a promise of events that are never coming.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let never_asked = Arc::new(input_capture_session(live.clone(), Capabilities::NONE));
        assert!(capture_on(never_asked).start(Box::new(|_| {})).is_err());

        let refused = Arc::new(input_capture_session(live, Capabilities::NONE));
        refused.starting();
        refused.denied("the consent dialog was dismissed");
        assert!(matches!(
            capture_on(refused).start(Box::new(|_| {})),
            Err(PlatformError::PermissionDenied(_))
        ));
    }

    #[test]
    fn capture_is_advertised_only_while_the_portal_is_really_capturing_for_us() {
        // `CAPTURE_INPUT` is a promise that this machine can drive a peer. A
        // desktop with no `InputCapture` portal at all — everything before GNOME
        // 50 — must never make it, and must not be reported as a permission the
        // user could go and grant.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let shared = input_capture_session(live.clone(), Capabilities::NONE);
        shared.starting();
        shared.unsupported("this desktop has no org.freedesktop.portal.InputCapture portal");
        assert!(!live.get().contains(Capabilities::CAPTURE_INPUT));
        assert!(matches!(
            shared.error(),
            Some(PlatformError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_granted_session_with_no_transport_reports_that_rather_than_a_refusal() {
        // The session state and the transport are two different answers, and the
        // agent responds to them differently: a refusal is something the user can
        // act on, a missing transport is not. So a session the portal granted but
        // whose libei devices never arrived must not come back as
        // `PermissionDenied` and send somebody to a settings panel.
        let live = LiveCapabilities::fixed(Capabilities::NONE);
        let shared = Arc::new(remote_desktop_session(live.clone(), Capabilities::NONE));
        let mut injector = injector_on(detached(Arc::clone(&shared)));

        shared.starting();
        shared.activate(REMOTE_DESKTOP_CAPABILITIES);
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

    fn capture_on(shared: Arc<SharedSession>) -> WaylandCapture {
        WaylandCapture {
            session: Arc::new(CaptureSession {
                _driver: None,
                shared,
                state: Arc::new(CaptureState::new()),
            }),
        }
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
        let shared = remote_desktop_session(live.clone(), base);

        shared.starting();
        shared.activate(REMOTE_DESKTOP_CAPABILITIES);
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
