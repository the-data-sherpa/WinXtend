//! Session locking on Windows.

use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_SWITCHDESKTOP,
};

use crate::error::{PlatformError, Result};

pub struct WindowsSession;

impl WindowsSession {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WindowsSession {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::traits::ScreenSaverControl for WindowsSession {
    fn lock_session(&self) -> Result<()> {
        // SAFETY: no arguments. Fails only when the calling process is not attached
        // to an interactive window station, e.g. a service in session 0.
        unsafe { LockWorkStation() }.map_err(|e| PlatformError::Os {
            context: "LockWorkStation",
            code: e.code().0,
        })
    }

    /// Whether the session is locked, inferred from access to the input desktop.
    ///
    /// Windows exposes no direct query. The lock screen runs on the separate
    /// `Winlogon` desktop, and a process in the user's session cannot open the
    /// input desktop while that one is in front — so a failure to open it means
    /// locked. This is a heuristic in one specific way: it also fails when a UAC
    /// secure prompt is up, which is not the same thing as locked. Callers use it
    /// to avoid pointless lock requests, never to decide whether to inject.
    fn is_locked(&self) -> Result<bool> {
        // SAFETY: no inherited handles requested; the returned desktop handle is
        // closed immediately.
        unsafe {
            match OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_SWITCHDESKTOP) {
                Ok(desktop) => {
                    let _ = CloseDesktop(desktop);
                    Ok(false)
                }
                Err(_) => Ok(true),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::ScreenSaverControl;

    #[test]
    fn an_interactive_test_run_is_not_locked() {
        // Tests run in the user's own session with the desktop in front. If this
        // ever reports locked, the heuristic has stopped working and screensaver
        // sync would start no-oping.
        assert!(!WindowsSession::new().is_locked().unwrap());
    }

    #[test]
    fn querying_the_lock_state_repeatedly_leaks_no_handles() {
        // The desktop handle must be closed on every successful open; leaking one
        // per poll would exhaust the handle table over a long session.
        let session = WindowsSession::new();
        for _ in 0..200 {
            let _ = session.is_locked();
        }
    }
}
