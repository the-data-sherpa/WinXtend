//! Registering the agent to start with the user's session.
//!
//! # Why this is not a Windows service
//!
//! `--install` asks for "service registration", and on Windows the honest answer
//! is that a service is the wrong mechanism for *this* process. A service runs in
//! session 0, and session 0 cannot install a `WH_KEYBOARD_LL` hook for the
//! interactive desktop, cannot see the user's monitors, and cannot inject into
//! their windows. An agent registered as a service would start, report healthy,
//! and capture nothing — the worst possible failure, because everything looks
//! fine.
//!
//! So the registration this performs is the one that actually works: a per-user
//! autostart entry under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, which launches the agent
//! inside the interactive session where the input hooks can reach the desktop.
//!
//! A real service would still be worth having, for one reason only: injecting at
//! the login screen and over UAC prompts, which needs `SeTcbPrivilege` and a
//! session-0 process. The shape that works is a service plus a per-session helper
//! that the service launches with `CreateProcessAsUser` into whichever session is
//! active, with the hooks living in the helper. That is a different program, not a
//! flag on this one, and [`wx_platform`] deliberately never advertises
//! `PRIVILEGED_INJECT` for the same reason.
//!
//! macOS and Linux are stubs that say what to write, rather than silently doing
//! nothing: a launch agent plist and a systemd user unit respectively. Both are
//! per-user session services for exactly the same reason.

use std::path::PathBuf;

/// Name of the autostart entry, and the label a user will see in Task Manager's
/// startup list.
pub const ENTRY_NAME: &str = "WinXtend Agent";

#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    #[error("this build cannot register autostart on {platform}: {hint}")]
    Unsupported {
        platform: &'static str,
        hint: &'static str,
    },
    #[error("finding this executable: {0}")]
    ExePath(#[source] std::io::Error),
    #[error("{operation} failed (os error {code})")]
    Os { operation: &'static str, code: u32 },
}

/// Whether the agent is registered to start with the session.
pub fn is_registered() -> Result<bool, AutostartError> {
    #[cfg(windows)]
    {
        windows_impl::is_registered()
    }
    #[cfg(not(windows))]
    {
        Err(unsupported())
    }
}

/// Register the agent to start with the session.
///
/// Idempotent, and it rewrites the recorded path every time: an entry pointing at
/// an executable that has been moved or upgraded in place is worse than no entry,
/// because it fails silently at every login.
pub fn install() -> Result<PathBuf, AutostartError> {
    #[cfg(windows)]
    {
        windows_impl::install()
    }
    #[cfg(not(windows))]
    {
        Err(unsupported())
    }
}

/// Remove the registration. Succeeds when there was nothing to remove.
pub fn uninstall() -> Result<(), AutostartError> {
    #[cfg(windows)]
    {
        windows_impl::uninstall()
    }
    #[cfg(not(windows))]
    {
        Err(unsupported())
    }
}

#[cfg(not(windows))]
fn unsupported() -> AutostartError {
    #[cfg(target_os = "macos")]
    return AutostartError::Unsupported {
        platform: "macOS",
        hint: "write a LaunchAgent plist to ~/Library/LaunchAgents/dev.winxtend.agent.plist \
               with RunAtLoad and KeepAlive, then `launchctl bootstrap gui/$UID <plist>`. \
               It must be a LaunchAgent, not a LaunchDaemon: a daemon has no window server \
               session and cannot create an event tap",
    };
    #[cfg(target_os = "linux")]
    return AutostartError::Unsupported {
        platform: "Linux",
        hint: "write a systemd *user* unit to ~/.config/systemd/user/winxtend-agent.service \
               with WantedBy=graphical-session.target, then `systemctl --user enable --now \
               winxtend-agent`. A system unit has no display server access",
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    AutostartError::Unsupported {
        platform: "this platform",
        hint: "no autostart mechanism is known",
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::{AutostartError, ENTRY_NAME};
    use std::path::PathBuf;
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    /// The command line to register.
    ///
    /// Quoted, because the path routinely contains spaces (`C:\Program Files\…`)
    /// and an unquoted entry makes Windows try to run `C:\Program` at every login.
    fn command_line() -> Result<(PathBuf, String), AutostartError> {
        let exe = std::env::current_exe().map_err(AutostartError::ExePath)?;
        Ok((exe.clone(), format!("\"{}\"", exe.display())))
    }

    /// A registry key handle that closes itself.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// Opens the Run key, or `Ok(None)` when the key itself does not exist.
    ///
    /// Windows creates `…\CurrentVersion\Run` lazily, so a user profile on which
    /// nothing has ever registered a startup entry simply has no such key, and
    /// `RegOpenKeyExW` answers `ERROR_FILE_NOT_FOUND`. For a reader that is not a
    /// failure but an answer: a key that does not exist holds no entry, so the
    /// honest result is "not registered" rather than an error. Reporting it as an
    /// error made [`is_registered`] fail on exactly the clean profiles it is most
    /// often asked about — including GitHub's Windows runners, whose freshly
    /// provisioned profiles are how this was found.
    fn open_existing(access: REG_SAM_FLAGS) -> Result<Option<Key>, AutostartError> {
        let mut key = HKEY::default();
        let path = HSTRING::from(RUN_KEY);
        let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &path, None, access, &mut key) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(AutostartError::Os {
                operation: "opening HKCU Run",
                code: status.0,
            });
        }
        Ok(Some(Key(key)))
    }

    /// Opens the Run key for writing, creating it when it is absent.
    ///
    /// A writer cannot treat the missing key as an answer the way a reader can:
    /// registering has to work on a profile that has never registered anything.
    /// `RegCreateKeyExW` opens the key when it exists and creates it when it does
    /// not, which is why installing uses it rather than [`open_existing`].
    fn open_or_create(access: REG_SAM_FLAGS) -> Result<Key, AutostartError> {
        let mut key = HKEY::default();
        let path = HSTRING::from(RUN_KEY);
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &path,
                None,
                PCWSTR::null(),
                // Non-volatile: an autostart entry that vanished at reboot would
                // be worse than none, because it would appear to work until then.
                REG_OPTION_NON_VOLATILE,
                access,
                None,
                &mut key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(AutostartError::Os {
                operation: "opening HKCU Run",
                code: status.0,
            });
        }
        Ok(Key(key))
    }

    pub fn install() -> Result<PathBuf, AutostartError> {
        let (exe, command) = command_line()?;
        let key = open_or_create(KEY_SET_VALUE)?;
        let name = HSTRING::from(ENTRY_NAME);
        // REG_SZ is a NUL-terminated wide string, and the terminator is part of
        // the value: without it Windows reads whatever follows in the buffer.
        let mut wide: Vec<u16> = command.encode_utf16().collect();
        wide.push(0);
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
        let status = unsafe { RegSetValueExW(key.0, &name, None, REG_SZ, Some(bytes)) };
        if status != ERROR_SUCCESS {
            return Err(AutostartError::Os {
                operation: "writing the autostart entry",
                code: status.0,
            });
        }
        Ok(exe)
    }

    pub fn uninstall() -> Result<(), AutostartError> {
        // No key at all means no entry, which is the end state being asked for —
        // the same reasoning as the ERROR_FILE_NOT_FOUND arm below.
        let Some(key) = open_existing(KEY_SET_VALUE)? else {
            return Ok(());
        };
        let name = HSTRING::from(ENTRY_NAME);
        let status = unsafe { RegDeleteValueW(key.0, &name) };
        // Not installed is the desired end state, so it is success.
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            return Err(AutostartError::Os {
                operation: "removing the autostart entry",
                code: status.0,
            });
        }
        Ok(())
    }

    pub fn is_registered() -> Result<bool, AutostartError> {
        let Some(key) = open_existing(KEY_QUERY_VALUE)? else {
            return Ok(false);
        };
        let name = HSTRING::from(ENTRY_NAME);
        let mut len: u32 = 0;
        let status = unsafe { RegQueryValueExW(key.0, &name, None, None, None, Some(&mut len)) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        if status != ERROR_SUCCESS {
            return Err(AutostartError::Os {
                operation: "reading the autostart entry",
                code: status.0,
            });
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises every test that touches the one real `HKCU` autostart value,
    /// and puts back what the developer had when the test ends.
    ///
    /// Both properties matter, and neither is a comment:
    ///
    /// * These tests share a single registry value. Cargo runs them on parallel
    ///   threads by default, so without the lock one test's `uninstall` lands
    ///   between another's `install` and its assertion. Holding the guard is the
    ///   only supported way to reach the real registry from a test — a future
    ///   test that mutates without it is the accident this exists to prevent.
    /// * Restoring happens in `Drop`, so it survives a panic. The earlier version
    ///   restored on the success path only, which meant a failing assertion left
    ///   the developer's own autostart setting changed by a test run.
    #[cfg(windows)]
    struct RealRegistry {
        _guard: std::sync::MutexGuard<'static, ()>,
        was: Option<bool>,
    }

    #[cfg(windows)]
    impl RealRegistry {
        fn acquire() -> Self {
            static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
            // Poison only means some other test panicked while holding this; the
            // registry is not left inconsistent by that, so carry on.
            let _guard = EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner());
            // `.ok()` rather than `.unwrap()`: whether querying can fail is the
            // very thing one of these tests asserts, and the guard must not be
            // the thing that panics on it.
            let was = is_registered().ok();
            Self { _guard, was }
        }
    }

    #[cfg(windows)]
    impl Drop for RealRegistry {
        fn drop(&mut self) {
            // Errors are deliberately swallowed: a restore that fails must not
            // replace the test's own failure with a less informative one.
            let _ = match self.was {
                Some(true) => install().map(|_| ()),
                Some(false) => uninstall(),
                None => Ok(()),
            };
        }
    }

    #[test]
    fn a_platform_with_no_mechanism_says_what_to_do_instead() {
        // The point of the error text: an unsupported platform must tell the user
        // what to write by hand, not merely that it will not help.
        #[cfg(not(windows))]
        {
            let err = install().unwrap_err();
            let text = err.to_string();
            assert!(
                text.contains("systemd") || text.contains("LaunchAgent"),
                "{text}"
            );
        }
        #[cfg(windows)]
        {
            // Windows has a real implementation; querying it must not error even
            // when nothing is registered — including on a profile so clean that
            // the Run key itself has never been created. `expect` rather than
            // `assert!(….is_ok())` so the failure names the OS error: the bare
            // assertion cost several CI runs to diagnose once.
            let _real = RealRegistry::acquire();
            is_registered().expect("querying autostart must not error when nothing is registered");
        }
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "no autostart mechanism on this platform")]
    fn registering_is_idempotent_and_removable() {
        // Touches the real registry, but only HKCU and only this one value.
        // `RealRegistry` serialises it against the other tests and restores
        // whatever it found, even if an assertion below panics.
        #[cfg(windows)]
        {
            let _real = RealRegistry::acquire();
            install().unwrap();
            assert!(is_registered().unwrap());
            install().unwrap();
            assert!(is_registered().unwrap());
            uninstall().unwrap();
            assert!(!is_registered().unwrap());
            // Removing twice is not an error: the desired end state is reached.
            uninstall().unwrap();
        }
    }

    /// Guards the property that actually broke CI, on every platform.
    ///
    /// Asking whether autostart is registered is a question about *this* profile,
    /// and "nothing has ever been registered here" is an answer to it, not a
    /// failure to answer. On Windows that means an absent Run key reads as
    /// `Ok(false)`; on a platform with no mechanism at all it means a refusal
    /// that still says what to do by hand. Neither may be an unexplained error.
    #[test]
    fn asking_about_a_profile_that_has_never_registered_anything_is_answerable() {
        #[cfg(windows)]
        {
            let _real = RealRegistry::acquire();
            // Remove the value, so the query below runs against a profile with
            // nothing registered — the state a fresh install is asked about.
            uninstall().unwrap();
            assert!(
                !is_registered().unwrap(),
                "with nothing registered the answer is a plain no"
            );
            // And installing must work from that state rather than failing for
            // want of a key: `RegCreateKeyExW` creates it when it is missing.
            install().unwrap();
            assert!(is_registered().unwrap());
        }
        #[cfg(not(windows))]
        {
            let err = is_registered().unwrap_err();
            assert!(matches!(err, AutostartError::Unsupported { .. }), "{err}");
        }
    }
}
