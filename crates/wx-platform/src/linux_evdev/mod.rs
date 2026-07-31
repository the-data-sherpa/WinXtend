//! Headless Linux backend over evdev/uinput — skeleton.
//!
//! Sketched but not implemented; see the note in [`crate::macos`] about why these
//! skeletons compile on every target.
//!
//! # What this backend is for
//!
//! A machine with no display server at all: a Raspberry Pi or mini-PC on the desk
//! with a keyboard plugged into it, used to drive the other machines. It reads
//! `/dev/input/event*` directly and writes through `/dev/uinput`, bypassing X11 and
//! Wayland entirely.
//!
//! It advertises no displays, so it never appears in the layout as a destination —
//! it is a pure input source, plus an injection target for anyone who wants to type
//! into its console. That asymmetry is why [`wx_proto::Capabilities`] is a bitset
//! rather than a role.
//!
//! # Why this needs root, and what that costs
//!
//! `/dev/input/event*` and `/dev/uinput` are root-only by default. The usual answer
//! is a udev rule granting the `input` group access, which is materially better
//! than running the whole agent as root. A permission failure here must surface as
//! [`PlatformError::PermissionDenied`] naming the device, because the difference
//! between "no keyboard attached" and "cannot open the keyboard" is the entire
//! diagnosis.

use wx_proto::{
    Capabilities, ClipboardFormat, DisplayServer, InputEvent, Monitor, NormPos, Platform,
};

use crate::error::{PlatformError, Result};
use crate::traits::{
    CaptureSink, ClipboardAccess, DisplayEnumerator, InputCapture, InputInjector, ScreenExits,
    ScreenSaverControl,
};
use crate::{PlatformBackend, PlatformInfo};

const BACKEND: &str = "linux-evdev";

fn todo_err(operation: &'static str) -> PlatformError {
    PlatformError::Unsupported {
        operation,
        backend: BACKEND,
    }
}

/// A headless node has no displays, and says so.
///
/// Returning an empty list rather than an error is the correct answer: enumeration
/// succeeded, there is simply nothing attached. An error would make the agent retry
/// forever against a machine that will never have a screen.
pub struct EvdevDisplays;

impl DisplayEnumerator for EvdevDisplays {
    fn monitors(&self) -> Result<Vec<Monitor>> {
        Ok(Vec::new())
    }
}

/// TODO: enumerate `/dev/input/event*`, keep the devices whose `EV_KEY` capability
/// bits include keyboard keys or mouse buttons, and read `input_event` structs from
/// each. `libevdev` or the `evdev` crate handles the ioctl detail.
///
/// Suppression is `EVIOCGRAB`, which takes the device exclusively so the local
/// console stops receiving it. That is a genuine grab, unlike Wayland — but it also
/// means a crash while grabbed leaves the keyboard unusable until the process is
/// reaped, so the grab must be released on every exit path.
///
/// Key resolution: evdev gives keycodes, so text has to come from libxkbcommon
/// (`xkb_state_key_get_utf8`) with a keymap chosen from configuration — there is no
/// display server to ask what layout the user picked. Defaulting to `us` and letting
/// the user override is the honest behaviour; silently guessing wrong turns every
/// non-US keystroke into the wrong character.
pub struct EvdevCapture;

impl InputCapture for EvdevCapture {
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

    /// Accepted and ignored: nothing here can take the pointer at a screen edge.
    ///
    /// The rule this carries is about backends that let the OS hand them the
    /// cursor when it reaches an edge. This one captures nothing at all, so there
    /// is no edge to arm and nothing to get wrong. An error would be a false
    /// alarm on every layout change.
    fn set_exits(&mut self, _exits: &[ScreenExits]) -> Result<()> {
        Ok(())
    }
}

/// TODO: create a virtual device with `/dev/uinput` (`UI_DEV_SETUP`,
/// `UI_SET_EVBIT`, `UI_DEV_CREATE`) and write `input_event` structs to it, each
/// batch followed by an `EV_SYN`/`SYN_REPORT`. Without the sync event the kernel
/// holds the batch and nothing is delivered.
///
/// Absolute pointer positioning needs `ABS_X`/`ABS_Y` declared with
/// `UI_ABS_SETUP`; a relative-only device cannot be placed at a wire position, so
/// [`InputInjector::warp_cursor`] would be unimplementable.
///
/// Text has the same keycode problem as the other Linux backends. With uinput there
/// is a clean answer: ship a generated xkb keymap for the virtual device, so any
/// character can be produced by a keycode we control.
pub struct EvdevInjector;

impl InputInjector for EvdevInjector {
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

/// A headless node has no clipboard, permanently.
///
/// [`PlatformError::Unsupported`] rather than an empty read: the caller must stop
/// advertising clipboard support entirely, and an empty read would look like "the
/// clipboard happens to be empty right now" and be polled forever.
pub struct EvdevClipboard;

impl ClipboardAccess for EvdevClipboard {
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

/// TODO: `loginctl lock-session` still works without a display server for a
/// logged-in console session; there is nothing to lock otherwise.
pub struct EvdevSession;

impl ScreenSaverControl for EvdevSession {
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
            display_server: DisplayServer::Headless,
            capabilities: Capabilities::NONE,
        },
        live_capabilities: crate::LiveCapabilities::fixed(Capabilities::NONE),
        displays: Box::new(EvdevDisplays),
        capture: Box::new(EvdevCapture),
        injector: Box::new(EvdevInjector),
        clipboard: Box::new(EvdevClipboard),
        screensaver: Box::new(EvdevSession),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_headless_node_reports_no_displays_rather_than_failing() {
        // Failing would make the agent retry enumeration forever on a machine that
        // will never have a screen.
        let backend = backend().unwrap();
        assert_eq!(backend.displays.monitors().unwrap().len(), 0);
        assert_eq!(backend.displays.primary().unwrap(), None);
        assert!(backend.displays.virtual_bounds().unwrap().is_empty());
    }

    #[test]
    fn a_headless_node_never_advertises_displays() {
        let backend = backend().unwrap();
        assert!(!backend
            .info
            .capabilities
            .contains(Capabilities::HAS_DISPLAYS));
        assert_eq!(backend.info.display_server, DisplayServer::Headless);
    }

    #[test]
    fn clipboard_absence_is_permanent_not_transient() {
        // A transient error would be retried; unsupported tells the caller to stop
        // advertising the capability.
        let backend = backend().unwrap();
        let err = backend.clipboard.change_serial().unwrap_err();
        assert!(matches!(err, PlatformError::Unsupported { .. }));
        assert!(!err.is_transient());
    }
}
