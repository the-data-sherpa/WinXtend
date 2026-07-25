//! macOS backend — skeleton.
//!
//! Sketched from the documented Quartz API but **not implemented**, because it
//! cannot be compiled or tested from the Windows development machine and a
//! plausible-looking untested `unsafe` block around `CGEventTap` is worse than an
//! honest `Unsupported`. Every method below names the exact call sequence it needs,
//! so filling this in is transcription rather than research.
//!
//! # Why this module is not `#[cfg(target_os = "macos")]`
//!
//! It compiles on every target on purpose. It has no Apple dependencies yet, so
//! including it in the Windows build is free and keeps it from rotting: a
//! refactor of [`crate::traits`] breaks it immediately instead of six months later
//! on someone's Mac. When real Core Graphics calls land, the *call sites* get
//! gated, not the module.
//!
//! # The permission wall, which dominates this backend
//!
//! macOS gates both halves of what WinXtend does, separately, and silently:
//!
//! * **Input Monitoring** (`kTCCServiceListenEvent`) is required for
//!   `CGEventTapCreate` to see keystrokes. Without it the tap is created
//!   successfully and simply never fires — there is no error to report.
//! * **Accessibility** (`kTCCServicePostEvent`) is required for `CGEventPost` to
//!   inject. Without it the calls return success and nothing happens.
//!
//! Both must be probed with `CGPreflightListenEventAccess` and
//! `AXIsProcessTrusted` and surfaced as [`PlatformError::PermissionDenied`], or
//! the product appears to work while doing nothing at all. That is the single
//! largest source of "it doesn't work on my Mac" reports for tools in this space.

use wx_proto::{
    Capabilities, ClipboardFormat, DisplayServer, InputEvent, Monitor, NormPos, Platform,
};

use crate::error::{PlatformError, Result};
use crate::traits::{
    CaptureSink, ClipboardAccess, DisplayEnumerator, InputCapture, InputInjector,
    ScreenSaverControl,
};
use crate::{PlatformBackend, PlatformInfo};

const BACKEND: &str = "macos";

fn todo_err(operation: &'static str) -> PlatformError {
    PlatformError::Unsupported {
        operation,
        backend: BACKEND,
    }
}

/// Quartz display enumeration.
///
/// TODO: `CGGetActiveDisplayList` for the display ids, then per display:
/// `CGDisplayBounds` for the global-space rectangle, `CGDisplayModeGetPixelWidth`
/// divided by `CGDisplayModeGetWidth` for the backing scale factor (this is how
/// Retina reports 2.0), and `CGDisplayIsMain` for the primary flag.
///
/// Identity must come from `CGDisplayCreateUUIDFromDisplayID`, not from the
/// `CGDirectDisplayID`: the id is reassigned across sleep/wake and display
/// reconfiguration, so a saved layout would start addressing a different screen.
/// Hash the UUID through [`crate::coords::stable_monitor_id`].
///
/// Note that macOS reports display bounds with a top-left origin in the global
/// space, matching [`wx_proto::Rect`], but *event* coordinates are flipped on some
/// APIs. Mixing the two is the classic macOS off-by-a-screen-height bug.
pub struct MacDisplays;

impl DisplayEnumerator for MacDisplays {
    fn monitors(&self) -> Result<Vec<Monitor>> {
        Err(todo_err("display enumeration"))
    }
}

/// Capture via a Quartz event tap.
///
/// TODO: `CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap, options,
/// mask, callback, user_info)` with a mask covering `kCGEventKeyDown`,
/// `kCGEventKeyUp`, `kCGEventFlagsChanged`, the mouse moved/dragged events, the
/// button events, and `kCGEventScrollWheel`. Add the returned source to a
/// `CFRunLoop` on a dedicated thread and run it, mirroring the Windows hook thread.
///
/// Suppression is `kCGEventTapOptionDefault` plus returning `NULL` from the
/// callback to swallow the event; a listen-only tap
/// (`kCGEventTapOptionListenOnly`) cannot suppress, so it is the wrong option here
/// even though it needs no Accessibility grant.
///
/// Key resolution: `CGEventKeyboardGetUnicodeString` gives the text the *current*
/// layout produces, which is exactly the [`crate::keyres::RawKey::text`] contract.
/// `UCKeyTranslate` with `kUCKeyTranslateNoDeadKeysMask` is the equivalent of the
/// Windows no-state-change flag, and is needed for the same reason: composing
/// locally must not be disturbed.
///
/// A tap is disabled by the system if the callback is too slow
/// (`kCGEventTapDisabledByTimeout`). That event must be handled by re-enabling the
/// tap with `CGEventTapEnable`, or capture dies silently after one hiccup.
pub struct MacCapture;

impl InputCapture for MacCapture {
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

/// Injection via `CGEventPost`.
///
/// TODO: text payloads go through `CGEventCreateKeyboardEvent(source, 0, true)`
/// followed by `CGEventKeyboardSetUnicodeString`. That pairing is the macOS
/// equivalent of `KEYEVENTF_UNICODE` and is what makes cross-layout typing work
/// here; posting a virtual keycode instead would remap through the receiver's
/// layout and defeat the whole design.
///
/// Chords are the same exception as on Windows: when
/// [`wx_proto::KeyEvent::injection_modifiers`] still holds Command, Control, or
/// Option, the character has to be posted as a keycode with `CGEventSetFlags`,
/// because applications match Command-C on the keycode.
///
/// Pointer: `CGWarpMouseCursorPosition` for absolute placement plus
/// `CGAssociateMouseAndMouseCursorPosition` care — warping without posting a
/// `kCGEventMouseMoved` leaves applications unaware the cursor moved.
pub struct MacInjector;

impl InputInjector for MacInjector {
    fn inject(&mut self, _monitor: &Monitor, _event: &InputEvent) -> Result<()> {
        Err(todo_err("input injection"))
    }

    fn warp_cursor(&mut self, _monitor: &Monitor, _pos: NormPos) -> Result<()> {
        Err(todo_err("cursor warping"))
    }

    fn release_all(&mut self) -> Result<()> {
        // Safe to succeed: nothing was ever pressed, so there is nothing stuck.
        Ok(())
    }
}

/// Clipboard via `NSPasteboard`.
///
/// TODO: `NSPasteboard.generalPasteboard`, with `changeCount` as the change serial
/// — it is exactly the counter [`ClipboardAccess::change_serial`] describes.
/// Formats map to `NSPasteboardTypeString`, `NSPasteboardTypeHTML`,
/// `NSPasteboardTypePNG`, and `NSPasteboardTypeFileURL`.
///
/// Note that macOS file references are `file://` URLs, not paths, so the
/// `FileList` conversion is not a straight copy in either direction.
pub struct MacClipboard;

impl ClipboardAccess for MacClipboard {
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

/// TODO: locking is not a public API. The practical route is the private
/// `SACLockScreenImmediate` in `login.framework`, loaded with `dlopen`; the
/// supported-but-slower alternative is sending the "Lock Screen" menu command.
/// Because it is private, a failure to resolve the symbol must degrade to dropping
/// the [`Capabilities::SCREENSAVER_SYNC`] bit rather than erroring on every lock.
pub struct MacSession;

impl ScreenSaverControl for MacSession {
    fn lock_session(&self) -> Result<()> {
        Err(todo_err("session locking"))
    }

    fn is_locked(&self) -> Result<bool> {
        Err(todo_err("session locking"))
    }
}

/// What this skeleton honestly supports: nothing yet.
///
/// Reporting the intended capability set instead would make peers offer clipboard
/// and video to a node that cannot answer, and every request would time out rather
/// than being cleanly refused.
pub fn backend() -> Result<PlatformBackend> {
    Ok(PlatformBackend {
        info: PlatformInfo {
            platform: Platform::MacOs,
            display_server: DisplayServer::Quartz,
            capabilities: Capabilities::NONE,
        },
        displays: Box::new(MacDisplays),
        capture: Box::new(MacCapture),
        injector: Box::new(MacInjector),
        clipboard: Box::new(MacClipboard),
        screensaver: Box::new(MacSession),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skeleton_advertises_nothing_it_cannot_do() {
        // An over-claiming skeleton turns every peer request into a timeout instead
        // of a clean refusal.
        let backend = backend().unwrap();
        assert!(backend.info.capabilities.is_empty());
        assert_eq!(backend.info.platform, Platform::MacOs);
        assert_eq!(backend.info.display_server, DisplayServer::Quartz);
    }

    #[test]
    fn unimplemented_operations_report_unsupported_not_success() {
        let backend = backend().unwrap();
        assert!(matches!(
            backend.displays.monitors(),
            Err(PlatformError::Unsupported { .. })
        ));
        assert!(matches!(
            backend.clipboard.change_serial(),
            Err(PlatformError::Unsupported { .. })
        ));
    }

    #[test]
    fn releasing_nothing_succeeds_so_disconnect_paths_stay_clean() {
        // `release_all` runs on every disconnect; failing it would make an ordinary
        // teardown look like an error.
        let mut backend = backend().unwrap();
        assert!(backend.injector.release_all().is_ok());
    }
}
