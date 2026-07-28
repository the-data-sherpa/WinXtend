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
//! # Linux
//!
//! The same argument, one layer out: a systemd **user** unit rather than a
//! system one. A system unit has no Wayland display, no session D-Bus and no
//! `xdg-desktop-portal`, so it would start, look healthy, and capture nothing —
//! the identical failure the Windows service would have. The portal's consent
//! and its restore token are per-user, and `systemctl --user enable` needs no
//! root. The unit itself, and the reasoning inside it, is
//! `packaging/winxtend.service.in`.
//!
//! Registering is a filesystem fact here, not a call into a daemon:
//! [`install`] writes the unit and creates the same `.wants` symlink
//! `systemctl --user enable` would, then asks systemd to reload as a courtesy.
//! That split is what lets the whole path be tested — including on a CI runner
//! with no systemd user instance to talk to — and it is why the reload is
//! best-effort rather than fatal. See [`linux_impl`].
//!
//! macOS is still a stub that says what to write, rather than silently doing
//! nothing: a launch agent plist, per-user for exactly the same reason.

use std::path::PathBuf;

/// Name of the autostart entry, and the label a user will see in Task Manager's
/// startup list.
pub const ENTRY_NAME: &str = "WinXtend Agent";

/// File name of the systemd user unit `--install` writes on Linux.
pub const UNIT_NAME: &str = "winxtend.service";

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
    #[error("{operation} {}: {source}", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("this user has no configuration directory to write a unit into")]
    NoConfigDir,
    /// A path that no systemd unit can name, whatever the escaping around it.
    ///
    /// Reported when registering rather than written out and hoped for: systemd
    /// refuses to load a unit whose `ExecStart` path contains one of these, and
    /// that refusal happens at the next login where nobody is watching, while
    /// [`is_registered`] goes on answering yes. A loud failure here is the only
    /// version of this the user can act on.
    #[error("{path} cannot be named in a systemd unit: {reason}")]
    UnrepresentablePath { path: String, reason: String },
}

/// Whether the agent is registered to start with the session.
pub fn is_registered() -> Result<bool, AutostartError> {
    #[cfg(windows)]
    {
        windows_impl::is_registered()
    }
    #[cfg(target_os = "linux")]
    {
        linux_impl::is_registered_in(&linux_impl::config_root()?)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
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
    #[cfg(target_os = "linux")]
    {
        let exe = linux_impl::install_in(&linux_impl::config_root()?)?;
        // Best-effort, and after the files are on disk: systemd has to be told
        // to re-read the unit directory or the new unit is invisible until the
        // next login. It is not part of the registration, which is why its
        // failure is not one — a CI runner and a container both have the files
        // and no user systemd instance to reload.
        linux_impl::daemon_reload();
        Ok(exe)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
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
    #[cfg(target_os = "linux")]
    {
        linux_impl::uninstall_in(&linux_impl::config_root()?)?;
        linux_impl::daemon_reload();
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(unsupported())
    }
}

/// Where the registration lives, for a message that tells the user what was
/// touched. `None` on a platform with no mechanism, and on any platform where
/// the location cannot be determined.
pub fn location() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::config_root()
            .ok()
            .map(|r| linux_impl::unit_path(&r))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn unsupported() -> AutostartError {
    #[cfg(target_os = "macos")]
    return AutostartError::Unsupported {
        platform: "macOS",
        hint: "write a LaunchAgent plist to ~/Library/LaunchAgents/dev.winxtend.agent.plist \
               with RunAtLoad and KeepAlive, then `launchctl bootstrap gui/$UID <plist>`. \
               It must be a LaunchAgent, not a LaunchDaemon: a daemon has no window server \
               session and cannot create an event tap",
    };
    #[cfg(not(target_os = "macos"))]
    AutostartError::Unsupported {
        platform: "this platform",
        hint: "no autostart mechanism is known",
    }
}

#[cfg(target_os = "linux")]
pub mod linux_impl {
    use super::{AutostartError, UNIT_NAME};
    use std::path::{Path, PathBuf};

    /// The unit template, and the reasoning behind every line of it.
    ///
    /// `include_str!` rather than a string literal here so that there is exactly
    /// one unit text in the tree: the packaging step substitutes the same file
    /// to produce the copy the `.deb` ships. A build failure is the intended
    /// outcome of moving or deleting it.
    const UNIT_TEMPLATE: &str = include_str!("../../../packaging/winxtend.service.in");

    /// Placeholder the template leaves for the agent's absolute path.
    const EXEC_START: &str = "@EXEC_START@";

    /// The target the unit is enabled into.
    ///
    /// `graphical-session.target` rather than `default.target`: the agent needs
    /// a session, not a boot. See the unit template.
    pub const WANTED_BY: &str = "graphical-session.target";

    /// The unit, with `exec_start` filled in.
    ///
    /// Takes the path already rendered rather than a `Path` so that the one
    /// caller that has no real binary — the packaging step's equivalent — and
    /// the one that does agree on the rendering.
    pub fn unit_text(exec_start: &Path) -> Result<String, AutostartError> {
        representable(exec_start)?;
        Ok(UNIT_TEMPLATE.replace(EXEC_START, &quoted(exec_start)))
    }

    /// Characters systemd will not accept in an `ExecStart` executable path,
    /// whatever is written around them.
    ///
    /// systemd unquotes and unescapes the value first and then requires the
    /// resulting path to be "safe": no ASCII control character, no quote of
    /// either kind, no backslash. Escaping does not buy its way past that check,
    /// it only reaches it: `systemd-analyze verify --user` reads the escaped
    /// newline in `ExecStart="/opt/win\ntend/wx-agent"` back as a real newline,
    /// exactly as intended, and then calls the unit a fatal error that will not
    /// be started. The same for `\t`, for `\x0b`, and for a backslash or a
    /// double quote written as `\\` or `\"`, which is why [`quoted`] does not
    /// bother writing those.
    ///
    /// A space and a percent are deliberately not in this set: both are
    /// expressible, and rendering them correctly is what [`quoted`] is for. This
    /// is the set with no correct rendering at all.
    fn unrepresentable(c: char) -> bool {
        c.is_ascii_control() || matches!(c, '"' | '\'' | '\\')
    }

    /// Whether this path can be written into a unit and read back as itself.
    ///
    /// Two ways it cannot. It may hold a character systemd rejects outright, and
    /// then no rendering works. Or it may not be UTF-8, and then a unit file —
    /// which is text — can only carry a lossy transcription of it: the unit
    /// would load happily and name a binary that does not exist, which is the
    /// silent-at-every-login failure with the diagnosis removed.
    fn representable(path: &Path) -> Result<(), AutostartError> {
        let Some(rendered) = path.to_str() else {
            return Err(AutostartError::UnrepresentablePath {
                path: path.display().to_string(),
                reason: "it is not valid UTF-8, and a unit file can only carry text — \
                         systemd would read back a different path than this one"
                    .to_owned(),
            });
        };
        match rendered.chars().find(|c| unrepresentable(*c)) {
            None => Ok(()),
            Some(bad) => Err(AutostartError::UnrepresentablePath {
                path: rendered.escape_debug().to_string(),
                reason: format!(
                    "systemd rejects '{}' in an ExecStart path however it is escaped; \
                     move or reinstall the agent somewhere without it",
                    bad.escape_debug()
                ),
            }),
        }
    }

    /// A path as a single `ExecStart` word.
    ///
    /// systemd splits `ExecStart` on whitespace, so a path containing a space —
    /// `~/Downloads/WinXtend 0.1.0/wx-agent` is an ordinary place to find one —
    /// becomes a different command with an argument after it, and fails at every
    /// login while the registration still reads as present. This is the Linux
    /// twin of the quoting the Windows `command_line` does, for the same reason.
    ///
    /// Quoted unconditionally, even when nothing needs it: `ui/scripts/bundle-agent.mjs`
    /// substitutes the same template for the packaged unit, and two renderings
    /// that differ only in an edge case are two renderings that will drift.
    ///
    /// Two things happen here and only two. The quotes are for the space. The
    /// doubling is for `%`, which quoting does not cover at all — systemd
    /// resolves its specifiers across the whole directive — so a literal percent
    /// has to be written `%%` or the command silently becomes a different one,
    /// or the unit refuses to load, at the next login only.
    ///
    /// Nothing else is escaped, because nothing else can be. The characters
    /// systemd will not take in an executable path it rejects *after*
    /// unescaping, so writing `\\` or `\"` for them would produce exactly the
    /// path it refuses; they are turned away by [`representable`] instead, which
    /// [`unit_text`] calls before this. This renders; it does not decide.
    fn quoted(path: &Path) -> String {
        let escaped = path.display().to_string().replace('%', "%%");
        format!("\"{escaped}\"")
    }

    /// `$XDG_CONFIG_HOME`, or `~/.config`.
    ///
    /// [`directories::BaseDirs`] rather than reading `HOME` directly because it
    /// applies the XDG rule that a relative `XDG_CONFIG_HOME` is ignored rather
    /// than joined onto the current directory, which is how a unit ends up
    /// somewhere nothing will ever look for it.
    pub fn config_root() -> Result<PathBuf, AutostartError> {
        directories::BaseDirs::new()
            .map(|dirs| dirs.config_dir().to_path_buf())
            .ok_or(AutostartError::NoConfigDir)
    }

    /// `<root>/systemd/user`.
    pub fn unit_dir(root: &Path) -> PathBuf {
        root.join("systemd").join("user")
    }

    /// Where `--install` writes the unit.
    pub fn unit_path(root: &Path) -> PathBuf {
        unit_dir(root).join(UNIT_NAME)
    }

    /// The symlink that makes the unit start with the session.
    ///
    /// This is precisely what `systemctl --user enable` creates from the unit's
    /// `[Install] WantedBy=`; creating it directly is not a shortcut around
    /// systemd but the on-disk form of the same statement, and it is why
    /// registering works on a machine whose user systemd instance cannot be
    /// reached.
    pub fn link_path(root: &Path) -> PathBuf {
        unit_dir(root)
            .join(format!("{WANTED_BY}.wants"))
            .join(UNIT_NAME)
    }

    fn io(operation: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> AutostartError {
        let path = path.to_path_buf();
        move |source| AutostartError::Io {
            operation,
            path,
            source,
        }
    }

    /// Write the unit and enable it, under an arbitrary config root.
    ///
    /// Parameterised on the root for the same reason `open_existing_at` is
    /// parameterised on the subkey in the Windows implementation: so the tests
    /// can exercise the real code against a directory of their own instead of
    /// the developer's actual autostart configuration. Unlike `HKCU`, this one
    /// can be redirected completely, so the Linux tests touch nothing real.
    pub fn install_in(root: &Path) -> Result<PathBuf, AutostartError> {
        let exe = std::env::current_exe().map_err(AutostartError::ExePath)?;
        // Rendered before anything is created, so that a path systemd cannot
        // name is a refusal and not a half-built registration: no directory, no
        // unit, no link, and an error that says which character did it.
        let text = unit_text(&exe)?;
        let dir = unit_dir(root);
        std::fs::create_dir_all(&dir).map_err(io("creating", &dir))?;

        // Rewritten every time, not written only when absent: a unit whose
        // ExecStart names a binary that has moved fails at every login and says
        // nothing, which is worse than not being registered at all.
        let unit = unit_path(root);
        std::fs::write(&unit, text).map_err(io("writing", &unit))?;

        let link = link_path(root);
        let wants = link.parent().expect("the link path has a parent");
        std::fs::create_dir_all(wants).map_err(io("creating", wants))?;
        // Removed first rather than checked: an existing link may be dangling or
        // may point at an older location, and `symlink` will not replace either.
        // Not-there is the state being aimed for, so its absence is not an error.
        match std::fs::remove_file(&link) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io("replacing", &link)(e)),
        }
        // Absolute, which is what `systemctl --user enable` writes, so that a
        // unit enabled by hand and one enabled here are indistinguishable.
        std::os::unix::fs::symlink(&unit, &link).map_err(io("linking", &link))?;
        Ok(exe)
    }

    /// Remove the unit and its enablement symlink. Absent is success.
    pub fn uninstall_in(root: &Path) -> Result<(), AutostartError> {
        for path in [link_path(root), unit_path(root)] {
            // `remove_file` on a symlink removes the link, never its target,
            // which is what disabling means here.
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io("removing", &path)(e)),
            }
        }
        Ok(())
    }

    /// Whether the agent will start with the next session.
    ///
    /// The enablement symlink has to be there *and* has to resolve. A `.wants`
    /// symlink with no unit behind it is a registration that cannot start
    /// anything, so reporting it as registered would be the
    /// silent-at-every-login failure in a different costume — but the unit it
    /// resolves to need not be the one [`install_in`] wrote. `systemctl --user
    /// enable winxtend.service` against the unit the `.deb` ships creates only
    /// this link, pointing at `/usr/lib/systemd/user/winxtend.service`, and
    /// writes nothing under the user's own config root; demanding a local unit
    /// file would report that perfectly good registration as absent.
    pub fn is_registered_in(root: &Path) -> Result<bool, AutostartError> {
        let link = link_path(root);
        // `metadata` follows the link, which is the whole question — is there a
        // unit at the other end. It answers no for a link with nothing behind it
        // and no for no link at all, which are the two ways of not being
        // registered; an `lstat` alongside it would add nothing, because a path
        // `stat` succeeds on is a path `lstat` cannot fail on.
        Ok(std::fs::metadata(&link).is_ok())
    }

    /// How long to wait for the reload before giving up on it.
    ///
    /// A reload of a healthy user instance takes tens of milliseconds. A wedged
    /// one can sit on the D-Bus method call for its full ~25s timeout, and the
    /// caller of this is a synchronous request the engine is waiting on, so an
    /// unbounded wait is a stall of the whole agent. Seconds rather than
    /// milliseconds because a loaded machine is slow, not broken.
    const RELOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

    /// Ask systemd to re-read the unit directory, if there is one listening.
    ///
    /// Deliberately silent about failing, and bounded. There is no user systemd
    /// instance in a container, on a CI runner, or over a bare SSH session, and
    /// on all three the registration itself is still perfectly valid — it takes
    /// effect at the next login either way. Treating "no bus to talk to" as a
    /// failed registration would make `--install` fail in exactly the
    /// environments where it has nothing to do, and a reload that never returns
    /// is the same non-event: the files are already on disk.
    pub fn daemon_reload() {
        let child = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return };

        // Polled rather than waited on: a bounded wait for a child otherwise
        // needs either a signal handler or another crate, and this is a courtesy
        // call with nothing to report.
        let deadline = std::time::Instant::now() + RELOAD_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Err(_) => return,
                Ok(None) => {}
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // Killed and reaped, so a stuck `systemctl` neither holds this thread
        // nor is left behind as a zombie for the rest of the agent's life.
        let _ = child.kill();
        let _ = child.wait();
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
        REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
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
    pub(super) struct Key(HKEY);

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
        open_existing_at(RUN_KEY, access)
    }

    /// [`open_existing`] against an arbitrary `HKCU` subkey.
    ///
    /// Split out purely so a test can name a subkey that certainly does not
    /// exist and reach the absent-key branch deterministically, without the
    /// alternative — deleting the real Run key — which would wipe every other
    /// application's startup entries on a developer's machine.
    pub(super) fn open_existing_at(
        subkey: &str,
        access: REG_SAM_FLAGS,
    ) -> Result<Option<Key>, AutostartError> {
        let mut key = HKEY::default();
        let path = HSTRING::from(subkey);
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
                operation: "creating HKCU Run",
                code: status.0,
            });
        }
        Ok(Key(key))
    }

    /// A registry value exactly as it is stored: its type as well as its bytes.
    ///
    /// The type travels with the bytes because it is not this code's to choose
    /// when putting a value back. An entry written as `REG_EXPAND_SZ` by whatever
    /// installer created it must come back as `REG_EXPAND_SZ`, or the `%VAR%` in
    /// it stops expanding and the command line silently stops resolving.
    #[cfg(test)]
    pub(super) struct RawEntry {
        pub(super) kind: REG_VALUE_TYPE,
        pub(super) bytes: Vec<u8>,
    }

    /// Reads the autostart value verbatim, or `Ok(None)` when there is nothing
    /// there to read — absent key and absent value both mean the same thing.
    ///
    /// This exists so a test can put back exactly what it found. Reconstructing
    /// the value by calling [`install`] cannot do that: `install` writes *this*
    /// executable's path, which under `cargo test` is the hashed test binary.
    #[cfg(test)]
    pub(super) fn read_raw_entry() -> Result<Option<RawEntry>, AutostartError> {
        let Some(key) = open_existing(KEY_QUERY_VALUE)? else {
            return Ok(None);
        };
        let name = HSTRING::from(ENTRY_NAME);
        let mut kind = REG_VALUE_TYPE::default();
        let mut len: u32 = 0;
        // Sizing call first: only the value itself knows how long it is.
        let status =
            unsafe { RegQueryValueExW(key.0, &name, None, Some(&mut kind), None, Some(&mut len)) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(AutostartError::Os {
                operation: "reading the autostart entry",
                code: status.0,
            });
        }
        let mut bytes = vec![0u8; len as usize];
        let mut got = len;
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                &name,
                None,
                Some(&mut kind),
                Some(bytes.as_mut_ptr()),
                Some(&mut got),
            )
        };
        // The value can be deleted between the two calls, which is the same
        // answer as never having been there.
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(AutostartError::Os {
                operation: "reading the autostart entry",
                code: status.0,
            });
        }
        bytes.truncate(got as usize);
        Ok(Some(RawEntry { kind, bytes }))
    }

    /// Writes the autostart value verbatim, creating the Run key when absent.
    pub(super) fn write_raw_entry(
        kind: REG_VALUE_TYPE,
        bytes: &[u8],
    ) -> Result<(), AutostartError> {
        let key = open_or_create(KEY_SET_VALUE)?;
        let name = HSTRING::from(ENTRY_NAME);
        let status = unsafe { RegSetValueExW(key.0, &name, None, kind, Some(bytes)) };
        if status != ERROR_SUCCESS {
            return Err(AutostartError::Os {
                operation: "writing the autostart entry",
                code: status.0,
            });
        }
        Ok(())
    }

    pub fn install() -> Result<PathBuf, AutostartError> {
        let (exe, command) = command_line()?;
        // REG_SZ is a NUL-terminated wide string, and the terminator is part of
        // the value: without it Windows reads whatever follows in the buffer.
        let mut wide: Vec<u16> = command.encode_utf16().collect();
        wide.push(0);
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
        write_raw_entry(REG_SZ, bytes)?;
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

    /// What the autostart value was before a test touched it, and how to put it
    /// back.
    ///
    /// A boolean is not enough. Restoring a "there was one" by calling
    /// [`install`] writes *this* executable's path, and under `cargo test` that
    /// is `target\debug\deps\wx_agent-<hash>.exe` — so a developer who really had
    /// WinXtend registered would end a test run with their Run entry pointing at
    /// a hashed test binary that the next `cargo clean` deletes. That is exactly
    /// the silent-at-every-login failure [`install`]'s own doc comment calls
    /// worse than no entry, caused by the test suite. So the bytes and the value
    /// type are captured and written back verbatim instead.
    ///
    /// `Absent` restores to absent by *deleting*, never by writing an empty
    /// value: an empty `Run` entry is not the same state as no entry.
    ///
    /// `Unknown` is the one case that cannot be restored faithfully, and it is
    /// named rather than rounded up: when the capturing read itself fails there
    /// is nothing trustworthy to put back, so the guard leaves the registry
    /// alone rather than guessing a state the developer never had.
    #[cfg(windows)]
    enum Restore {
        Value(windows_impl::RawEntry),
        Absent,
        Unknown,
    }

    #[cfg(windows)]
    impl Restore {
        fn capture() -> Self {
            // Matching on the error rather than `.unwrap()`: whether querying can
            // fail is the very thing one of these tests asserts, and the guard
            // must not be the thing that panics on it.
            match windows_impl::read_raw_entry() {
                Ok(Some(entry)) => Self::Value(entry),
                Ok(None) => Self::Absent,
                Err(_) => Self::Unknown,
            }
        }

        fn apply(&self) {
            // Errors are deliberately swallowed: a restore that fails must not
            // replace the test's own failure with a less informative one.
            let _ = match self {
                Self::Value(entry) => windows_impl::write_raw_entry(entry.kind, &entry.bytes),
                Self::Absent => uninstall(),
                Self::Unknown => Ok(()),
            };
        }
    }

    /// Serialises every test that touches the one real `HKCU` autostart value,
    /// and puts back the exact value the developer had when the test ends.
    ///
    /// All three properties matter, and none is a comment:
    ///
    /// * These tests share a single registry value. Cargo runs them on parallel
    ///   threads by default, so without the lock one test's `uninstall` lands
    ///   between another's `install` and its assertion. Holding the guard is the
    ///   only supported way to reach the real registry from a test — a future
    ///   test that mutates without it is the accident this exists to prevent.
    ///   `a_platform_with_no_mechanism_says_what_to_do_instead` takes the guard
    ///   even though it only reads, both so it cannot observe another test
    ///   mid-write and so it is not the exception that teaches the next test to
    ///   skip the guard.
    /// * The restore is byte-faithful rather than a re-`install`; see
    ///   [`Restore`] for why the difference is the developer's autostart still
    ///   working tomorrow.
    /// * Restoring happens in `Drop`, so it survives a panic. The earlier version
    ///   restored on the success path only, which meant a failing assertion left
    ///   the developer's own autostart setting changed by a test run.
    #[cfg(windows)]
    struct RealRegistry {
        _guard: std::sync::MutexGuard<'static, ()>,
        was: Restore,
    }

    #[cfg(windows)]
    impl RealRegistry {
        fn acquire() -> Self {
            static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
            // Poison only means some other test panicked while holding this; the
            // registry is not left inconsistent by that, so carry on.
            let _guard = EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner());
            let was = Restore::capture();
            Self { _guard, was }
        }
    }

    #[cfg(windows)]
    impl Drop for RealRegistry {
        fn drop(&mut self) {
            // Still inside the lock: the restore is as much a mutation of the
            // shared value as the test body was.
            self.was.apply();
        }
    }

    /// A private config root that cleans itself up, for the Linux tests.
    ///
    /// The Linux registration is a pair of files under a directory this code
    /// chooses, so unlike `HKCU` it can be redirected wholesale — which is why
    /// the Linux tests need no equivalent of [`RealRegistry`]: they never touch
    /// the developer's own `~/.config/systemd/user`, so there is nothing to
    /// serialise against and nothing to restore. Named after the test so a
    /// leftover directory says which one died.
    #[cfg(target_os = "linux")]
    struct TempRoot(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl TempRoot {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("winxtend-autostart-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a temporary config root");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_platform_with_no_mechanism_says_what_to_do_instead() {
        // The point of the error text: an unsupported platform must tell the user
        // what to write by hand, not merely that it will not help.
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let err = install().unwrap_err();
            let text = err.to_string();
            assert!(text.contains("LaunchAgent"), "{text}");
        }
        #[cfg(target_os = "linux")]
        {
            // Linux has a real implementation now, so the question this test
            // asks becomes the Windows one: querying must answer rather than
            // error when nothing is registered.
            let root = TempRoot::new("query");
            assert!(!linux_impl::is_registered_in(root.path()).unwrap());
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
    #[cfg_attr(
        not(any(windows, target_os = "linux")),
        ignore = "no autostart mechanism on this platform"
    )]
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
        // The same properties, against a config root of this test's own. It runs
        // the real `install_in`/`uninstall_in`, not a rehearsal of them; only the
        // directory they are pointed at is the test's. The `systemctl` reload
        // `install()` adds on top is skipped deliberately — a CI runner has no
        // user systemd instance, and reloading is not part of the registration.
        #[cfg(target_os = "linux")]
        {
            let root = TempRoot::new("idempotent");
            let root = root.path();

            linux_impl::install_in(root).unwrap();
            assert!(linux_impl::is_registered_in(root).unwrap());
            linux_impl::install_in(root).unwrap();
            assert!(linux_impl::is_registered_in(root).unwrap());

            linux_impl::uninstall_in(root).unwrap();
            assert!(!linux_impl::is_registered_in(root).unwrap());
            // Removing twice is not an error: the desired end state is reached.
            linux_impl::uninstall_in(root).unwrap();
        }
    }

    /// The unit `--install` writes has to say the things it exists to say.
    ///
    /// Asserted against the rendered text rather than trusted from the template,
    /// because the template is substituted by two different callers and a
    /// placeholder left in place produces a unit systemd refuses to load — with
    /// an error nobody sees until the next login.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_unit_is_a_session_service_ordered_after_the_portal() {
        let unit = linux_impl::unit_text(std::path::Path::new("/usr/bin/wx-agent")).unwrap();

        assert!(
            unit.contains("ExecStart=\"/usr/bin/wx-agent\""),
            "the binary path is substituted:\n{unit}"
        );
        assert!(
            !unit.contains("@EXEC_START@"),
            "no placeholder survives substitution:\n{unit}"
        );
        // A user unit that starts with the session, not at boot. Anything else
        // and the agent has no display, no portal and no session bus.
        assert!(
            unit.contains(&format!("WantedBy={}", linux_impl::WANTED_BY)),
            "{unit}"
        );
        // The startup race: without this ordering an autostarted agent can reach
        // the portal before it is answerable. The agent's own bounded retry is
        // what closes that race; this ordering is what keeps it from being needed.
        assert!(unit.contains("After=xdg-desktop-portal.service"), "{unit}");
        // `Requires=` would let a portal failure stop an agent that is still
        // useful as a machine that receives input, and would add no ordering.
        assert!(
            !unit.contains("Requires=xdg-desktop-portal"),
            "the portal is wanted, not required:\n{unit}"
        );
    }

    /// A path with a space in it must reach systemd as one argument.
    ///
    /// systemd splits `ExecStart` on whitespace, so an unquoted
    /// `/opt/win xtend/wx-agent` becomes the command `/opt/win` with
    /// `xtend/wx-agent` as its argument: it fails at every login, silently, while
    /// the registration still reads as present. The packaged agent at
    /// `/usr/bin/wx-agent` never sees it, but `--install` is reachable from the
    /// UI on any machine, and `~/Downloads/WinXtend 0.1.0/wx-agent` is an
    /// ordinary place to have unpacked one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_path_with_a_space_stays_one_argument() {
        let unit = linux_impl::unit_text(std::path::Path::new("/opt/win xtend/wx-agent")).unwrap();
        assert!(
            unit.contains("ExecStart=\"/opt/win xtend/wx-agent\""),
            "{unit}"
        );
    }

    /// A path with a percent in it must not be read as a systemd specifier.
    ///
    /// systemd resolves `%`-specifiers in `ExecStart` regardless of quoting, so
    /// `/opt/wx%20build/wx-agent` either names a different binary — `%2` is not
    /// a specifier, but `%h`, `%u` and friends are, and one of those is a
    /// plausible thing to find in an unpacked directory name — or fails the unit
    /// with "Failed to resolve specifiers". Both are the same
    /// silent-at-every-login failure the quoting above exists to prevent, so the
    /// literal form `%%` is the only correct rendering.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_path_with_a_percent_is_not_read_as_a_specifier() {
        let unit = linux_impl::unit_text(std::path::Path::new("/opt/wx%20build/wx-agent")).unwrap();
        assert!(
            unit.contains("ExecStart=\"/opt/wx%%20build/wx-agent\""),
            "{unit}"
        );
    }

    /// A path systemd cannot name is refused, not written out and hoped for.
    ///
    /// The escaping route was tried against real systemd first, because it would
    /// have been the better answer. It does not exist. `systemd-analyze verify
    /// --user` on a unit rendering `ExecStart="/opt/win\ntend/wx-agent"` reads
    /// the escape back correctly — it prints the path with a real newline in it
    /// — and then says `Executable path contains special characters` and
    /// `Unit configuration has fatal error, unit will not be started`. The
    /// unescaped form fails differently and just as fatally, with
    /// `Unbalanced quoting` and then `Service has no ExecStart=`. systemd
    /// unescapes and *then* demands a "safe" path, so there is no rendering that
    /// makes one of these work.
    ///
    /// Which leaves two options, and only one of them is honest: write a unit
    /// that cannot load while `is_registered_in` reports the agent registered —
    /// the exact silent-at-every-login failure this module is written against —
    /// or refuse at `--install` time with an error naming the character. Hence
    /// this.
    ///
    /// The same verification found three more characters in the same position,
    /// which is why the check is a set rather than a newline: systemd rejects
    /// `"`, `'` and `\` in an executable path too, and `/home/o'brien/wx-agent`
    /// is a great deal more likely to exist than a path with a newline in it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_path_systemd_cannot_express_is_refused_at_install_time() {
        for bad in [
            "/opt/win\ntend/wx-agent",
            "/opt/win\ttend/wx-agent",
            "/opt/win\x0btend/wx-agent",
            "/home/o'brien/wx-agent",
            "/opt/win\"tend/wx-agent",
            "/opt/win\\tend/wx-agent",
        ] {
            let err = linux_impl::unit_text(std::path::Path::new(bad))
                .expect_err("systemd will not load a unit naming this path");
            assert!(
                matches!(err, AutostartError::UnrepresentablePath { .. }),
                "{bad:?}: {err}"
            );
            // The message has to be actionable: a user reading it must be able
            // to tell which path was refused without re-running anything.
            let text = err.to_string();
            assert!(text.contains("wx-agent"), "{bad:?}: {text}");
            assert!(text.contains("systemd unit"), "{bad:?}: {text}");
        }
    }

    /// Enabling means the symlink `systemctl --user enable` would have made, in
    /// the directory it would have made it in.
    #[cfg(target_os = "linux")]
    #[test]
    fn enabling_writes_the_wants_symlink_systemd_looks_for() {
        let root = TempRoot::new("wants");
        let root = root.path();
        linux_impl::install_in(root).unwrap();

        let link = linux_impl::link_path(root);
        assert!(
            link.ends_with(format!("{}.wants/{UNIT_NAME}", linux_impl::WANTED_BY)),
            "{}",
            link.display()
        );
        let target = std::fs::read_link(&link).expect("the enablement entry is a symlink");
        assert_eq!(
            target,
            linux_impl::unit_path(root),
            "the link points at the unit, by absolute path"
        );
    }

    /// A `.wants` link with no unit behind it is not a registration.
    ///
    /// This is the state an interrupted upgrade leaves, and reporting it as
    /// registered would mean the UI says the agent starts with the session while
    /// systemd has nothing to start — the silent failure the whole module is
    /// written against.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dangling_enablement_link_does_not_count_as_registered() {
        let root = TempRoot::new("dangling");
        let root = root.path();
        linux_impl::install_in(root).unwrap();
        std::fs::remove_file(linux_impl::unit_path(root)).unwrap();

        assert!(
            !linux_impl::is_registered_in(root).unwrap(),
            "a link with no unit cannot start anything"
        );
        // And installing again must repair it rather than trip over the link
        // that is already there.
        linux_impl::install_in(root).unwrap();
        assert!(linux_impl::is_registered_in(root).unwrap());
    }

    /// Enabling the unit the `.deb` ships counts as registered.
    ///
    /// `systemctl --user enable winxtend.service` against
    /// `/usr/lib/systemd/user/winxtend.service` creates the `.wants` link and
    /// nothing else — no unit is written under the user's own config root. A
    /// check that demanded one there reported that as "not registered" while
    /// systemd would in fact start the agent at the next login, which is the
    /// stale-config failure this module exists to prevent, inverted.
    #[cfg(target_os = "linux")]
    #[test]
    fn enabling_the_packaged_unit_counts_as_registered() {
        let root = TempRoot::new("packaged");
        let root = root.path();

        // Standing in for `/usr/lib/systemd/user/winxtend.service`: outside the
        // config root, which is the whole point, but inside the temporary
        // directory so the test writes nothing the machine keeps.
        let packaged = root.join("usr-lib-winxtend.service");
        std::fs::write(
            &packaged,
            linux_impl::unit_text(std::path::Path::new("/usr/bin/wx-agent")).unwrap(),
        )
        .unwrap();

        let link = linux_impl::link_path(root);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&packaged, &link).unwrap();

        assert!(
            !linux_impl::unit_path(root).exists(),
            "enabling the packaged unit writes nothing under the config root"
        );
        assert!(
            linux_impl::is_registered_in(root).unwrap(),
            "a link resolving to the packaged unit will start the agent"
        );
    }

    /// Registering must work on a user who has never had a systemd user
    /// directory — the Linux twin of the absent-`Run`-key case that broke CI.
    #[cfg(target_os = "linux")]
    #[test]
    fn registering_creates_the_directories_it_needs() {
        let root = TempRoot::new("mkdir");
        let root = root.path();
        assert!(
            !linux_impl::unit_dir(root).exists(),
            "the test starts from a root with no systemd directory"
        );
        linux_impl::install_in(root).unwrap();
        assert!(linux_impl::is_registered_in(root).unwrap());
    }

    /// With no autostart *value* registered, the answer is a plain no — and
    /// installing works from there.
    ///
    /// This covers the absent-value state, not the absent-*key* state that broke
    /// CI: `uninstall` deletes the value and leaves the Run key standing, and
    /// whether the key exists at all is decided by the profile the suite happens
    /// to run on. [`an_absent_run_key_reads_as_absent_rather_than_erroring`] is
    /// the one that reproduces the key condition deterministically.
    ///
    /// On a platform with no mechanism at all the same question earns a refusal
    /// that still says what to write by hand. Neither may be an unexplained
    /// error.
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
        #[cfg(target_os = "linux")]
        {
            // A root with nothing in it is the state a fresh install is asked
            // about, and it must be an answer rather than an error.
            let root = TempRoot::new("never-registered");
            let root = root.path();
            assert!(
                !linux_impl::is_registered_in(root).unwrap(),
                "with nothing registered the answer is a plain no"
            );
            linux_impl::install_in(root).unwrap();
            assert!(linux_impl::is_registered_in(root).unwrap());
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let err = is_registered().unwrap_err();
            assert!(matches!(err, AutostartError::Unsupported { .. }), "{err}");
        }
    }

    /// Guards the property that actually broke CI.
    ///
    /// `Os { operation: "opening HKCU Run", code: 2 }` — `ERROR_FILE_NOT_FOUND`
    /// on the *key*, not the value — is what failed on the Windows runners,
    /// because Windows creates `…\CurrentVersion\Run` lazily and a profile that
    /// has never registered a startup entry has no such key. A key that is not
    /// there holds no entry, so the reader's honest answer is `Ok(None)`.
    ///
    /// It is asked of a subkey that does not exist rather than of the real Run
    /// key, because reproducing the condition on the real one would mean
    /// deleting every other application's startup entries. That makes it
    /// deterministic and independent of thread ordering and of whatever the
    /// runner's profile happens to contain.
    ///
    /// It reads a key that is absent, so it creates nothing and deletes nothing
    /// and needs no `RealRegistry` guard — deliberately, not by omission.
    #[cfg(windows)]
    #[test]
    fn an_absent_run_key_reads_as_absent_rather_than_erroring() {
        use windows::Win32::System::Registry::KEY_QUERY_VALUE;

        // `expect` rather than a bare assertion so a regression names the OS
        // error, which is what took three CI runs to read the first time.
        let opened = windows_impl::open_existing_at(
            r"Software\WinXtend\absent-key-regression-guard",
            KEY_QUERY_VALUE,
        )
        .expect("a Run key that does not exist is an answer, not an error");
        assert!(opened.is_none(), "there is no key there to hand back");
    }

    /// A developer's own autostart entry must survive a test run unchanged.
    ///
    /// The guard is exercised through [`Restore`] rather than by nesting a second
    /// `RealRegistry`, which would deadlock on the very mutex that makes these
    /// tests safe. The outer guard still holds the lock and still puts the real
    /// machine back the way it was.
    #[cfg(windows)]
    #[test]
    fn a_captured_entry_is_restored_byte_for_byte() {
        use windows::Win32::System::Registry::REG_SZ;

        let _real = RealRegistry::acquire();
        // A value `install()` could never produce: it names a path that is not
        // this test binary, so a restore that goes through `install()` instead of
        // the captured bytes fails here rather than on a developer's machine.
        let planted: Vec<u8> = r#""C:\Program Files\WinXtend\wx-agent.exe" --from-a-real-install"#
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        windows_impl::write_raw_entry(REG_SZ, &planted).unwrap();

        let restore = Restore::capture();
        // Whatever a test body does to the value, up to and including the
        // rewrite that made restoring by `install()` wrong.
        install().unwrap();
        restore.apply();

        let after = windows_impl::read_raw_entry()
            .unwrap()
            .expect("a value that was there before the test must be there after it");
        assert_eq!(after.bytes, planted, "the restored value must be verbatim");
        assert_eq!(after.kind, REG_SZ, "the value type is restored too");
    }

    /// The other direction, which a boolean got right only by accident: a
    /// profile with nothing registered must still have nothing registered, and
    /// specifically must not gain an empty entry.
    #[cfg(windows)]
    #[test]
    fn nothing_to_restore_leaves_nothing_behind() {
        let _real = RealRegistry::acquire();
        uninstall().unwrap();

        let restore = Restore::capture();
        assert!(matches!(restore, Restore::Absent), "nothing was registered");
        install().unwrap();
        restore.apply();

        assert!(
            windows_impl::read_raw_entry().unwrap().is_none(),
            "an absent entry must come back absent, not as an empty value"
        );
        assert!(!is_registered().unwrap());
    }
}
