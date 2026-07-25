//! Linux/Wayland backend — skeleton.
//!
//! Sketched from the libei and xdg-desktop-portal APIs but not implemented; see the
//! note in [`crate::macos`] about why these skeletons compile on every target.
//!
//! # Why this backend matters more than its line count suggests
//!
//! Wayland is the default session on current Fedora, Ubuntu, and SteamOS, and the
//! Synergy-lineage tools still do not support it. Supporting it properly is a
//! reason to choose WinXtend rather than a checkbox, so this skeleton is written to
//! be filled in with the *portal* path rather than an X11 fallback hack.
//!
//! # The shape Wayland forces on this
//!
//! Wayland deliberately has no API for "read all input" or "synthesise input" —
//! that was the point of the redesign. Both go through
//! `xdg-desktop-portal`'s `RemoteDesktop` interface, which means:
//!
//! * A **user consent dialog** on first use, and a session handle that the portal
//!   can revoke at any moment. Losing it must be reported as
//!   [`PlatformError::PermissionDenied`] and must drop the capture capability, not
//!   retried in a loop against a user who said no.
//! * A **restore token** persisted between runs, so the consent prompt appears once
//!   rather than on every launch. Without it the product is unusable as a daemon.
//! * **libei** (`ei_*`) as the actual transport for events once the portal has
//!   handed over a file descriptor.
//!
//! There is no global grab, so [`InputCapture::set_suppress_local`] cannot be
//! implemented the way it is on X11 or Windows. The portal session itself is
//! exclusive while active, which is the closest equivalent; a compositor that does
//! not honour that leaves keystrokes reaching local windows, and the backend should
//! report that honestly rather than pretend.

use wx_proto::{
    Capabilities, ClipboardFormat, DisplayServer, InputEvent, Monitor, NormPos, Platform,
};

use crate::error::{PlatformError, Result};
use crate::traits::{
    CaptureSink, ClipboardAccess, DisplayEnumerator, InputCapture, InputInjector,
    ScreenSaverControl,
};
use crate::{PlatformBackend, PlatformInfo};

const BACKEND: &str = "linux-wayland";

fn todo_err(operation: &'static str) -> PlatformError {
    PlatformError::Unsupported {
        operation,
        backend: BACKEND,
    }
}

/// TODO: there is no privileged "list all outputs" call for an ordinary client.
/// Bind `wl_output` from the registry and read `xdg_output`'s `logical_position`
/// and `logical_size` events, which give the compositor's layout in the same
/// top-left-origin space [`Monitor`] uses. `wl_output.scale` is the integer scale;
/// `xdg_output` is needed for fractional scaling.
///
/// Identity comes from `wl_output`'s `name` event (`DP-1`) hashed through
/// [`crate::coords::stable_monitor_id`]; the registry object id is per-connection
/// and useless as a stable id.
pub struct WaylandDisplays;

impl DisplayEnumerator for WaylandDisplays {
    fn monitors(&self) -> Result<Vec<Monitor>> {
        Err(todo_err("display enumeration"))
    }
}

/// TODO: `org.freedesktop.portal.RemoteDesktop`: `CreateSession`,
/// `SelectDevices` with KEYBOARD | POINTER, `Start`, then `ConnectToEIS` to get the
/// libei file descriptor. Drive the resulting `ei` context on its own thread and
/// translate `ei_event`s into [`crate::traits::CapturedEvent`].
///
/// Key resolution uses the same libxkbcommon path as the X11 backend
/// (`xkb_state_key_get_utf8`), because libei reports evdev keycodes rather than
/// text. Store the portal's restore token so the consent prompt is a one-off.
pub struct WaylandCapture;

impl InputCapture for WaylandCapture {
    fn start(&mut self, _sink: CaptureSink) -> Result<()> {
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

/// TODO: inject through the same libei connection: `ei_device_keyboard_key`,
/// `ei_device_pointer_motion_absolute`, `ei_device_button_button`,
/// `ei_device_scroll_delta`, each followed by `ei_device_frame`. The frame call is
/// mandatory — events without a frame are buffered and never delivered, which
/// presents as injection silently doing nothing.
///
/// Text is the hard case again, for the same reason as X11: libei carries evdev
/// keycodes, not characters. The keymap must be searched for a keycode producing
/// the wanted keysym, and where none exists the compositor cannot be asked to
/// remap. Falling back to [`wx_proto::KeyPayload::RawKeyCode`] loses the character,
/// so the practical answer is to upload our own xkb keymap to the `ei` keyboard
/// device — which libei supports precisely for this case.
pub struct WaylandInjector;

impl InputInjector for WaylandInjector {
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

pub fn backend() -> Result<PlatformBackend> {
    Ok(PlatformBackend {
        info: PlatformInfo {
            platform: Platform::Linux,
            display_server: DisplayServer::Wayland,
            capabilities: Capabilities::NONE,
        },
        displays: Box::new(WaylandDisplays),
        capture: Box::new(WaylandCapture),
        injector: Box::new(WaylandInjector),
        clipboard: Box::new(WaylandClipboard),
        screensaver: Box::new(WaylandSession),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_skeleton_advertises_nothing_it_cannot_do() {
        let backend = backend().unwrap();
        assert!(backend.info.capabilities.is_empty());
        assert_eq!(backend.info.display_server, DisplayServer::Wayland);
    }

    #[test]
    fn suppression_is_refused_rather_than_silently_ignored() {
        // Silently succeeding here would mean keystrokes reach the local desktop
        // and the remote peer at the same time, with nothing to indicate why.
        let mut backend = backend().unwrap();
        assert!(backend.capture.set_suppress_local(true).is_err());
        assert!(!backend.capture.suppresses_local());
    }
}
