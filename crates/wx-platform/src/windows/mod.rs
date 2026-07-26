//! The Windows backend.
//!
//! Complete and exercised: this is the one platform the project can compile and
//! test on directly, so it is also the reference for what the other backends have
//! to do.
//!
//! | Concern | Mechanism |
//! |---|---|
//! | Displays | `EnumDisplayMonitors` + `GetMonitorInfoW` + `GetDpiForMonitor` |
//! | Injection | `SendInput`, `KEYEVENTF_UNICODE` for text |
//! | Capture | `WH_KEYBOARD_LL` and `WH_MOUSE_LL` on a dedicated thread |
//! | Clipboard | Win32 clipboard, `CF_UNICODETEXT` / `CF_HTML` / `PNG` / `CF_HDROP` |
//! | Locking | `LockWorkStation` |

pub mod capture;
pub mod cfhtml;
pub mod clipboard;
pub mod display;
pub mod inject;
pub mod session;
pub mod vk;

pub use capture::WindowsCapture;
pub use clipboard::WindowsClipboard;
pub use display::WindowsDisplays;
pub use inject::WindowsInjector;
pub use session::WindowsSession;

use wx_proto::{Capabilities, DisplayServer, Platform};

use crate::error::Result;
use crate::traits::DisplayEnumerator;
use crate::{PlatformBackend, PlatformInfo};

/// Everything this build can do on Windows.
///
/// `HAS_DISPLAYS` is added only when a display is actually attached, so a machine
/// running headless over RDP-less remote access does not advertise screens that no
/// peer can route the cursor to.
///
/// `PRIVILEGED_INJECT` is deliberately not claimed. It needs a service in session
/// 0 with `SeTcbPrivilege` to reach the secure desktop, and advertising it from an
/// ordinary user process would make peers believe input will keep working across a
/// UAC prompt when it silently will not.
///
/// `FILE_TRANSFER` is not claimed either, and for the same reason: nothing in this
/// build implements the `FileTransfer*` half of the protocol, so a peer that took
/// the claim at face value would offer a file and wait forever for an answer that
/// no code path produces. The capability bit and the wire variants stay defined —
/// protocol enums are append-only — but this machine does not pretend to speak
/// them.
fn capabilities(has_displays: bool) -> Capabilities {
    let mut caps = Capabilities::CAPTURE_INPUT
        | Capabilities::INJECT_INPUT
        | Capabilities::CLIPBOARD_TEXT
        | Capabilities::CLIPBOARD_IMAGE
        | Capabilities::SCREENSAVER_SYNC;
    if has_displays {
        caps = caps.union(Capabilities::HAS_DISPLAYS);
    }
    caps
}

pub fn backend() -> Result<PlatformBackend> {
    let displays = WindowsDisplays::new();
    // A failed enumeration is not fatal: the node can still inject and forward.
    // Treating it as fatal would take a whole machine out of the mesh over a
    // transient GDI error. It is logged, because "my screens are missing from the
    // layout" is otherwise unattributable.
    let has_displays = match displays.monitors() {
        Ok(monitors) => !monitors.is_empty(),
        Err(e) => {
            tracing::warn!(error = %e, "display enumeration failed; advertising no displays");
            false
        }
    };

    Ok(PlatformBackend {
        info: PlatformInfo {
            platform: Platform::Windows,
            display_server: DisplayServer::Windows,
            capabilities: capabilities(has_displays),
        },
        displays: Box::new(displays),
        capture: Box::new(WindowsCapture::new()),
        injector: Box::new(WindowsInjector::new()),
        clipboard: Box::new(WindowsClipboard::new()),
        screensaver: Box::new(WindowsSession::new()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_with_screens_advertises_them() {
        assert!(capabilities(true).contains(Capabilities::HAS_DISPLAYS));
        assert!(!capabilities(false).contains(Capabilities::HAS_DISPLAYS));
    }

    #[test]
    fn privileged_injection_is_never_claimed_from_a_user_process() {
        // Claiming it would promise input across UAC prompts that this process
        // cannot deliver.
        assert!(!capabilities(true).contains(Capabilities::PRIVILEGED_INJECT));
    }

    #[test]
    fn file_transfer_is_never_claimed_because_nothing_implements_it() {
        // No `FileTransfer*` message is handled anywhere in the agent, so a peer
        // that believed this claim would offer a file and wait for an answer that
        // no code path produces.
        assert!(!capabilities(true).contains(Capabilities::FILE_TRANSFER));
        assert!(!capabilities(false).contains(Capabilities::FILE_TRANSFER));
    }

    #[test]
    fn video_capabilities_are_left_to_the_video_crate() {
        // wx-platform has no encoder, so advertising a video source here would make
        // peers request a stream that never arrives.
        assert!(!capabilities(true).contains(Capabilities::VIDEO_SOURCE));
        assert!(!capabilities(true).contains(Capabilities::VIDEO_SINK));
    }

    #[test]
    fn the_windows_backend_reports_itself_as_windows() {
        let backend = backend().unwrap();
        assert_eq!(backend.info.platform, Platform::Windows);
        assert_eq!(backend.info.display_server, DisplayServer::Windows);
        assert!(backend
            .info
            .capabilities
            .contains(Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT));
    }

    #[test]
    fn the_backend_finds_this_machines_displays() {
        let backend = backend().unwrap();
        assert!(!backend.displays.monitors().unwrap().is_empty());
        assert!(backend
            .info
            .capabilities
            .contains(Capabilities::HAS_DISPLAYS));
    }
}
