//! Platform abstraction for input, displays, and clipboard.
//!
//! Everything OS-specific lives behind the traits in [`traits`] so that
//! [`wx_core`](../wx_core/index.html) stays testable and portable. The backend for
//! the build target is assembled by [`current_platform`].
//!
//! # Backends
//!
//! | Platform | Capture | Inject | State |
//! |---|---|---|---|
//! | Windows | `WH_KEYBOARD_LL` / `WH_MOUSE_LL` | `SendInput` | implemented |
//! | macOS | `CGEventTap` | `CGEventPost` | skeleton |
//! | Linux/X11 | XInput2 raw events | XTEST | skeleton |
//! | Linux/Wayland | libei via the InputCapture portal | libei via the RemoteDesktop portal | capture, injection, displays and clipboard implemented; locking still a skeleton |
//! | Linux headless | evdev | uinput | skeleton |
//!
//! Wayland matters: it is the default on current Fedora, Ubuntu, and Steam Deck,
//! and the Synergy-lineage tools still do not support it. Supporting it is a
//! reason to pick WinXtend rather than a nice-to-have.
//!
//! # The one idea that has to survive every backend
//!
//! Keys are resolved to **text** on the machine they were typed on, using that
//! machine's own layout, before they ever reach the network. [`keyres`] owns that
//! translation and every backend routes through it, so the cross-layout guarantee
//! cannot be true on one platform and false on another. A backend that hands raw
//! keycodes upwards has moved the problem to the wrong side of the wire.
//!
//! # Layering
//!
//! ```text
//!   agent  ->  Box<dyn InputCapture>  ->  keyres::KeyResolver  ->  wx_proto::KeyEvent
//!   agent  <-  Box<dyn InputInjector> <-  wx_proto::InputEvent
//! ```
//!
//! Nothing here knows about the network, the layout engine, or async. Backends
//! deliver events to a callback and accept events one at a time.

pub mod coords;
pub mod error;
pub mod keyres;
pub mod traits;

// Backend modules are declared unconditionally even though only one is ever used.
// Compiling them everywhere stops the skeletons rotting: a change to `traits` breaks
// them here and now rather than months later on a machine nobody has. Where a
// backend has grown real platform dependencies — `linux_wayland` so far — the module
// itself still compiles everywhere and only the submodule holding the OS types is
// `cfg`-gated, so the interpretation around it stays unit-testable on any host.
pub mod linux_evdev;
pub mod linux_wayland;
pub mod linux_x11;
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

pub use error::{PlatformError, Result};
pub use keyres::{KeyResolver, RawKey};
pub use traits::{
    CaptureSink, CapturedEvent, ClipboardAccess, DisplayEnumerator, InputCapture, InputInjector,
    ScreenSaverControl,
};

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use wx_proto::{Capabilities, DisplayServer, Platform};

/// What a node advertises about itself in the handshake.
///
/// Assembled from what the backend can actually do rather than from what the OS
/// theoretically supports: on macOS the same binary reports different capabilities
/// depending on which permissions the user has granted, and a peer needs the real
/// answer to decide whether to offer clipboard sync.
///
/// This is the answer *at startup*. Where a backend's permissions can be granted
/// or withdrawn while the process runs, [`PlatformBackend::current_capabilities`]
/// is the authority — see [`LiveCapabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformInfo {
    pub platform: Platform,
    pub display_server: DisplayServer,
    pub capabilities: Capabilities,
}

/// A capability set that can change after startup.
///
/// Most backends know at construction what they can do and never revise it. The
/// Wayland backend cannot: its capture and injection permissions live in an
/// `xdg-desktop-portal` session that the user grants through a dialog and that the
/// compositor may revoke at any moment. A node that advertised `CAPTURE_INPUT`
/// once at startup and never corrected itself would keep telling peers it can
/// drive them long after the user said no.
///
/// Shared rather than copied, so the thread that owns the permission can publish
/// a change without a channel back to whoever is advertising.
#[derive(Clone, Debug)]
pub struct LiveCapabilities(Arc<AtomicU32>);

impl LiveCapabilities {
    /// For a backend whose capabilities are decided once and never change.
    pub fn fixed(capabilities: Capabilities) -> Self {
        Self(Arc::new(AtomicU32::new(capabilities.0)))
    }

    pub fn get(&self) -> Capabilities {
        Capabilities(self.0.load(Ordering::Relaxed))
    }

    /// Publish a new set. Returns whether it differs from what was published
    /// before, so a caller can re-advertise only on a real change.
    pub fn set(&self, capabilities: Capabilities) -> bool {
        self.0.swap(capabilities.0, Ordering::Relaxed) != capabilities.0
    }
}

/// The assembled backend for this machine.
///
/// Boxed trait objects rather than generics: the agent holds exactly one of these
/// for its whole life, so monomorphising five type parameters through every layer
/// above would buy nothing and make the engine's signatures depend on the build
/// target.
///
/// Every field is present even where the platform cannot honour it. A headless node
/// still has a `clipboard`, which returns [`PlatformError::Unsupported`] on every
/// call. That keeps the agent free of `Option` handling on paths it should never
/// take anyway, and [`PlatformInfo::capabilities`] is the authority on what to
/// attempt.
pub struct PlatformBackend {
    pub info: PlatformInfo,
    /// Capabilities as they stand now, which on some backends is not what
    /// `info.capabilities` said at startup. Read it through
    /// [`PlatformBackend::current_capabilities`].
    pub live_capabilities: LiveCapabilities,
    pub displays: Box<dyn DisplayEnumerator>,
    pub capture: Box<dyn InputCapture>,
    pub injector: Box<dyn InputInjector>,
    pub clipboard: Box<dyn ClipboardAccess>,
    pub screensaver: Box<dyn ScreenSaverControl>,
}

impl PlatformBackend {
    /// What this backend can honour right now.
    ///
    /// The value to advertise to peers. Callers should re-read it rather than
    /// caching `info.capabilities`, because on Wayland it drops to nothing the
    /// moment the portal session is revoked.
    pub fn current_capabilities(&self) -> Capabilities {
        self.live_capabilities.get()
    }
}

impl core::fmt::Debug for PlatformBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The trait objects have nothing useful to print, and the info is the part
        // that ends up in a bug report.
        f.debug_struct("PlatformBackend")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// Build the backend for the compile target, without acquiring any permission.
///
/// On Linux the choice is made at runtime rather than compile time, because one
/// binary has to serve X11, Wayland, and headless machines — see
/// [`detect_display_server`].
///
/// # Why this does not prompt
///
/// Constructing a backend must be free of side effects the user can see. On
/// Wayland, acquiring input permission means an `xdg-desktop-portal` consent
/// dialog; a `cargo test` run that popped one on the developer's desktop would be
/// indefensible. The daemon therefore calls [`current_platform_in`], which is the
/// same thing plus permission acquisition.
pub fn current_platform() -> Result<PlatformBackend> {
    #[cfg(target_os = "windows")]
    {
        windows::backend()
    }
    #[cfg(target_os = "macos")]
    {
        macos::backend()
    }
    #[cfg(target_os = "linux")]
    {
        match detect_display_server(&EnvVars::from_process()) {
            DisplayServer::Wayland => linux_wayland::backend(),
            DisplayServer::X11 => linux_x11::backend(),
            _ => linux_evdev::backend(),
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Err(PlatformError::Unsupported {
            operation: "any platform backend",
            backend: "unknown",
        })
    }
}

/// Build the backend and start acquiring whatever OS permissions it needs.
///
/// The daemon's entry point. `config_dir` is the per-user directory holding the
/// identity key and trust store, and is where a backend persists anything it needs
/// between runs — on Wayland, the portal restore token that keeps the consent
/// dialog to a one-off.
///
/// Acquisition is asynchronous and never blocks: the Wayland consent dialog can sit
/// on screen for as long as the user takes to read it, and an agent that could not
/// answer `--status` until then would look hung. Capabilities appear in
/// [`PlatformBackend::current_capabilities`] once permission is granted, and vanish
/// again if it is withdrawn.
pub fn current_platform_in(config_dir: &Path) -> Result<PlatformBackend> {
    #[cfg(target_os = "linux")]
    {
        if detect_display_server(&EnvVars::from_process()) == DisplayServer::Wayland {
            return linux_wayland::backend_in(config_dir);
        }
    }
    let _ = config_dir;
    current_platform()
}

/// The environment variables that decide which Linux backend to use.
///
/// Passed in as data rather than read inside the decision function so the decision
/// is testable: setting process environment variables from a test races every other
/// test in the binary.
#[derive(Debug, Default, Clone)]
pub struct EnvVars {
    pub wayland_display: Option<String>,
    pub display: Option<String>,
    pub xdg_session_type: Option<String>,
}

impl EnvVars {
    pub fn from_process() -> Self {
        Self {
            wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
            display: std::env::var("DISPLAY").ok(),
            xdg_session_type: std::env::var("XDG_SESSION_TYPE").ok(),
        }
    }
}

/// Decide which display server a Linux session is running.
///
/// `WAYLAND_DISPLAY` wins over `DISPLAY` when both are set, and both are set
/// constantly: every Wayland session runs Xwayland, which exports `DISPLAY`. Picking
/// X11 there would work — through Xwayland — but only for X11 clients, so captured
/// input would miss every native Wayland window. That failure looks like "keyboard
/// sharing works in my terminal but not in Firefox", which is exactly the kind of
/// bug that never gets diagnosed correctly.
///
/// `XDG_SESSION_TYPE` is consulted only as a tiebreak, because it is set by the
/// display manager and is wrong or absent often enough not to be trusted first.
pub fn detect_display_server(env: &EnvVars) -> DisplayServer {
    let non_empty = |v: &Option<String>| v.as_deref().map(|s| !s.is_empty()).unwrap_or(false);

    if non_empty(&env.wayland_display) {
        return DisplayServer::Wayland;
    }
    if non_empty(&env.display) {
        return DisplayServer::X11;
    }
    match env.xdg_session_type.as_deref() {
        Some("wayland") => DisplayServer::Wayland,
        Some("x11") => DisplayServer::X11,
        // No display server: a headless forwarder driving peers via evdev.
        _ => DisplayServer::Headless,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(wayland: Option<&str>, x11: Option<&str>, session: Option<&str>) -> EnvVars {
        EnvVars {
            wayland_display: wayland.map(str::to_string),
            display: x11.map(str::to_string),
            xdg_session_type: session.map(str::to_string),
        }
    }

    #[test]
    fn wayland_wins_when_xwayland_also_advertises_a_display() {
        // Every Wayland session exports DISPLAY for Xwayland. Choosing X11 there
        // would silently miss input in native Wayland windows.
        assert_eq!(
            detect_display_server(&env(Some("wayland-0"), Some(":0"), Some("wayland"))),
            DisplayServer::Wayland
        );
    }

    #[test]
    fn a_plain_x11_session_is_detected() {
        assert_eq!(
            detect_display_server(&env(None, Some(":0"), Some("x11"))),
            DisplayServer::X11
        );
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        // Shell profiles routinely export empty values; treating one as a live
        // display server would pick a backend that cannot connect.
        assert_eq!(
            detect_display_server(&env(Some(""), Some(""), None)),
            DisplayServer::Headless
        );
    }

    #[test]
    fn a_bare_session_type_is_enough_when_no_socket_is_exported() {
        assert_eq!(
            detect_display_server(&env(None, None, Some("wayland"))),
            DisplayServer::Wayland
        );
        assert_eq!(
            detect_display_server(&env(None, None, Some("x11"))),
            DisplayServer::X11
        );
    }

    #[test]
    fn nothing_set_means_headless() {
        assert_eq!(
            detect_display_server(&EnvVars::default()),
            DisplayServer::Headless
        );
    }

    #[test]
    fn an_unrecognised_session_type_falls_back_to_headless() {
        // `tty` is the common real value. Guessing X11 would make the agent hang
        // trying to open a display that does not exist.
        assert_eq!(
            detect_display_server(&env(None, None, Some("tty"))),
            DisplayServer::Headless
        );
    }

    #[test]
    fn the_current_platform_reports_the_host_it_was_built_for() {
        let backend = current_platform().unwrap();
        #[cfg(target_os = "windows")]
        {
            assert_eq!(backend.info.platform, Platform::Windows);
            assert_eq!(backend.info.display_server, DisplayServer::Windows);
        }
        // The debug impl is what ends up in bug reports; make sure it says
        // something useful rather than panicking on the trait objects.
        assert!(format!("{backend:?}").contains("PlatformInfo"));
    }

    #[test]
    fn a_usable_platform_advertises_capture_and_injection() {
        // The two capabilities without which a node cannot participate at all.
        let backend = current_platform().unwrap();
        #[cfg(target_os = "windows")]
        assert!(backend
            .info
            .capabilities
            .contains(Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT));
        let _ = backend;
    }

    #[test]
    fn no_backend_claims_a_capability_nothing_implements() {
        // Runs on every target, so the claim cannot come back on one platform while
        // a test on another goes on passing. `FILE_TRANSFER` has no implementation
        // anywhere in the agent: a peer that believed the claim would offer a file
        // and wait for an answer no code path produces. The bit itself stays defined
        // — protocol enums and their capabilities are append-only — it is only never
        // advertised.
        let backend = current_platform().unwrap();
        assert!(!backend
            .info
            .capabilities
            .contains(Capabilities::FILE_TRANSFER));
        // Same rule, already relied on by the Windows backend's own tests: reaching
        // the secure desktop needs a service this process is not.
        assert!(!backend
            .info
            .capabilities
            .contains(Capabilities::PRIVILEGED_INJECT));
    }
}
