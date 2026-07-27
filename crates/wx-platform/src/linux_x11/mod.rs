//! Linux/X11 backend — skeleton.
//!
//! Sketched from the XInput2 and XTEST APIs but not implemented; see the note in
//! [`crate::macos`] about why these skeletons are compiled on every target rather
//! than `cfg`-gated away.
//!
//! X11 is the easy Linux case: it was designed when any client could read and
//! synthesise any input, so everything WinXtend needs is a supported operation with
//! no permission prompt. That is also why it is a security disaster and why
//! Wayland exists.

use wx_proto::{
    Capabilities, ClipboardFormat, DisplayServer, InputEvent, Monitor, NormPos, Platform,
};

use crate::error::{PlatformError, Result};
use crate::traits::{
    CaptureSink, ClipboardAccess, DisplayEnumerator, InputCapture, InputInjector,
    ScreenSaverControl,
};
use crate::{PlatformBackend, PlatformInfo};

const BACKEND: &str = "linux-x11";

fn todo_err(operation: &'static str) -> PlatformError {
    PlatformError::Unsupported {
        operation,
        backend: BACKEND,
    }
}

/// TODO: `XRRGetScreenResourcesCurrent` plus `XRRGetOutputInfo` and
/// `XRRGetCrtcInfo` for each connected output. The CRTC gives the rectangle in the
/// X screen's coordinate space, which is already the shape [`Monitor`] wants.
///
/// Use `XRRGetOutputPrimary` for the primary flag, and the output *name* (`DP-1`,
/// `eDP-1`) as the identity source through
/// [`crate::coords::stable_monitor_id`] — RandR output ids are per-session and
/// change on replug.
///
/// Scale: X11 has no per-monitor DPI. The honest answer is 1.0, optionally divided
/// out of the `Xft.dpi` resource. Reporting a guessed per-monitor scale would be
/// worse than reporting none, since wire positions are normalized and never use it.
pub struct X11Displays;

impl DisplayEnumerator for X11Displays {
    fn monitors(&self) -> Result<Vec<Monitor>> {
        Err(todo_err("display enumeration"))
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
/// back or the local desktop appears frozen.
///
/// Key resolution: build an `xkb_state` from `xkb_x11_keymap_new_from_device`, feed
/// it with `xkb_state_update_key`, and read text with `xkb_state_key_get_utf8`.
/// That already applies the user's layout, which is the
/// [`crate::keyres::RawKey::text`] contract; mask out Control before asking, the
/// same as every other backend.
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

/// TODO: inject with XTEST — `XTestFakeMotionEvent`, `XTestFakeButtonEvent`,
/// `XTestFakeKeyEvent`, then `XFlush`.
///
/// Text injection is the awkward part, and the reason this backend needs more care
/// than the others. XTEST takes a *keycode*, so producing an arbitrary character
/// means: find a keycode already mapped to the character's keysym and use it, or —
/// when the character is not on the layout at all, which is the entire point of the
/// cross-layout design — temporarily remap a scratch keycode with
/// `XChangeKeyboardMapping`, inject, and restore it.
///
/// Two failure modes to respect while doing that:
///
/// * The remap must be restored even on panic, or the user's keyboard is left with
///   a corrupted key until they log out.
/// * Applications cache the keymap on `MappingNotify`, so remapping on every
///   keystroke causes visible stalls in some toolkits. Batch a whole burst of text
///   into one remap where possible.
pub struct X11Injector;

impl InputInjector for X11Injector {
    fn inject(&mut self, _monitor: &Monitor, _event: &InputEvent) -> Result<()> {
        Err(todo_err("input injection"))
    }

    fn warp_cursor(&mut self, _monitor: &Monitor, _pos: NormPos) -> Result<()> {
        Err(todo_err("cursor warping"))
    }

    fn release_all(&mut self) -> Result<()> {
        Ok(())
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

pub fn backend() -> Result<PlatformBackend> {
    Ok(PlatformBackend {
        info: PlatformInfo {
            platform: Platform::Linux,
            display_server: DisplayServer::X11,
            capabilities: Capabilities::NONE,
        },
        live_capabilities: crate::LiveCapabilities::fixed(Capabilities::NONE),
        displays: Box::new(X11Displays),
        capture: Box::new(X11Capture),
        injector: Box::new(X11Injector),
        clipboard: Box::new(X11Clipboard),
        screensaver: Box::new(X11Session),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skeleton_advertises_nothing_it_cannot_do() {
        let backend = backend().unwrap();
        assert!(backend.info.capabilities.is_empty());
        assert_eq!(backend.info.platform, Platform::Linux);
        assert_eq!(backend.info.display_server, DisplayServer::X11);
    }

    #[test]
    fn unimplemented_operations_report_unsupported_not_success() {
        let backend = backend().unwrap();
        assert!(matches!(
            backend.displays.monitors(),
            Err(PlatformError::Unsupported { .. })
        ));
    }
}
