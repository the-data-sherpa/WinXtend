//! Persistence for the portal's restore token.
//!
//! The token is the difference between a daemon and a nuisance. `xdg-desktop-portal`
//! shows a consent dialog every time a `RemoteDesktop` session is started, unless the
//! session was asked to persist and the token it handed back is presented again. A
//! WinXtend agent starts with the machine, so without this the user approves a dialog
//! at every boot.
//!
//! It lives under [`RESTORE_TOKEN_FILE`], beside `identity.key` and
//! `trusted-peers.toml` in the per-user config directory, written `0o600` for the same
//! reason the identity key is: anyone holding it can silently re-acquire
//! remote-control permission for this desktop.
//!
//! Deleting the file is the supported way for a user to withdraw that standing
//! consent: the next run finds nothing to present, and the desktop asks again.
//!
//! Plain text rather than TOML or JSON, because the content is one opaque string the
//! portal gave us and never anything we parse.

use std::path::{Path, PathBuf};

use crate::error::{PlatformError, Result};

/// Name of the token file inside the config directory.
pub const RESTORE_TOKEN_FILE: &str = "wayland-portal.token";

/// Longest token accepted from disk.
///
/// The portal returns a UUID today (36 bytes), but the format is explicitly opaque
/// and a future portal could return more. The cap only exists so a corrupted or
/// hostile file cannot make the agent send a megabyte of junk over D-Bus.
const MAX_TOKEN_LEN: usize = 4096;

/// Reads and writes the restore token for one config directory.
#[derive(Debug, Clone)]
pub struct RestoreTokenStore {
    path: PathBuf,
}

impl RestoreTokenStore {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            path: dir.join(RESTORE_TOKEN_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored token, or `None` if there is nothing usable.
    ///
    /// Every failure is `None` rather than an error. A missing, unreadable, empty
    /// or oversized file means exactly one thing to the caller — start the session
    /// without a token and let the user approve it once more — and making that an
    /// error would tempt a caller into treating a first run as a failure.
    pub fn load(&self) -> Option<String> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        // The portal's tokens are opaque, so the only cleanup that is safe is
        // trimming whitespace a text editor may have added.
        let token = raw.trim();
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return None;
        }
        Some(token.to_string())
    }

    /// Replace the stored token.
    pub fn save(&self, token: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io_error("creating the config directory", &e))?;
        }
        write_private(&self.path, token.as_bytes())
    }

    /// Forget the token, so the next session asks the user again.
    ///
    /// Used when the portal rejects what we stored: keeping a token the portal has
    /// already refused would mean sending it again on every subsequent launch.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn io_error(context: &'static str, e: &std::io::Error) -> PlatformError {
    PlatformError::Other(format!("{context}: {e}"))
}

/// Write `data` to `path` so that only the owner can read it.
///
/// The permissions are the security boundary, so they are in place before any
/// content is: the file is created empty, tightened, and only then written. A token
/// that is briefly world-readable is a token that leaked, and an existing file left
/// over from an earlier build would keep its old mode if we relied on `open`'s mode
/// argument alone — that only applies at creation.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| io_error("creating the restore token file", &e))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io_error("restricting the restore token file", &e))?;
        file.write_all(data)
            .map_err(|e| io_error("writing the restore token file", &e))?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        // Windows: the config directory lives under the user's profile and is
        // already ACL'd to that user, which is the same boundary the unix mode bits
        // draw. Kept compiling off-Linux so this module's tests run everywhere.
        std::fs::write(path, data).map_err(|e| io_error("writing the restore token file", &e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans up after itself, so these tests need no
    /// dev-dependency and leave nothing behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "wx-token-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_first_run_has_no_token() {
        let dir = TempDir::new("first-run");
        assert_eq!(RestoreTokenStore::in_dir(&dir.0).load(), None);
    }

    #[test]
    fn a_saved_token_survives_to_the_next_run() {
        // The whole point: launch two never sees a consent dialog.
        let dir = TempDir::new("roundtrip");
        let store = RestoreTokenStore::in_dir(&dir.0);
        store.save("6e7c1a2b-0000-4d1e-9f00-abcdefabcdef").unwrap();

        let next_launch = RestoreTokenStore::in_dir(&dir.0);
        assert_eq!(
            next_launch.load().as_deref(),
            Some("6e7c1a2b-0000-4d1e-9f00-abcdefabcdef")
        );
    }

    #[test]
    fn the_token_file_is_named_beside_the_identity_key() {
        let store = RestoreTokenStore::in_dir(Path::new("/tmp/cfg"));
        assert_eq!(store.path(), Path::new("/tmp/cfg/wayland-portal.token"));
    }

    #[test]
    fn saving_creates_the_config_directory() {
        // First run of a fresh install: nothing exists yet.
        let dir = TempDir::new("mkdir");
        let nested = dir.0.join("deeper");
        let store = RestoreTokenStore::in_dir(&nested);
        store.save("tok").unwrap();
        assert_eq!(store.load().as_deref(), Some("tok"));
    }

    #[test]
    fn a_replacement_token_does_not_leave_the_old_one_behind() {
        // Written with `truncate`, so a shorter token must not leave a tail of the
        // longer one it replaced — which would be sent to the portal verbatim.
        let dir = TempDir::new("replace");
        let store = RestoreTokenStore::in_dir(&dir.0);
        store
            .save("a-very-long-token-from-the-previous-run")
            .unwrap();
        store.save("short").unwrap();
        assert_eq!(store.load().as_deref(), Some("short"));
    }

    #[test]
    fn clearing_takes_the_prompt_back_to_a_first_run() {
        let dir = TempDir::new("clear");
        let store = RestoreTokenStore::in_dir(&dir.0);
        store.save("tok").unwrap();
        store.clear();
        assert_eq!(store.load(), None);
        // Clearing what is not there is not an error: it happens on every launch
        // where the portal never issued a token.
        store.clear();
    }

    #[test]
    fn whitespace_around_a_hand_edited_token_is_ignored() {
        let dir = TempDir::new("trim");
        let store = RestoreTokenStore::in_dir(&dir.0);
        std::fs::write(store.path(), "  tok\n").unwrap();
        assert_eq!(store.load().as_deref(), Some("tok"));
    }

    #[test]
    fn an_empty_or_oversized_file_reads_as_no_token() {
        // Both are what a truncated write or a corrupted disk leaves behind, and
        // both must degrade to "ask the user once", not to sending junk to D-Bus.
        let dir = TempDir::new("junk");
        let store = RestoreTokenStore::in_dir(&dir.0);

        std::fs::write(store.path(), "   \n").unwrap();
        assert_eq!(store.load(), None);

        std::fs::write(store.path(), "x".repeat(MAX_TOKEN_LEN + 1)).unwrap();
        assert_eq!(store.load(), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_token_is_not_readable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("mode");
        let store = RestoreTokenStore::in_dir(&dir.0);
        store.save("tok").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "restore token file is {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_file_from_an_older_build_is_tightened() {
        // `OpenOptions::mode` only applies when the file is created, so rewriting a
        // token left behind by a version that did not set the mode would otherwise
        // keep it readable.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("tighten");
        let store = RestoreTokenStore::in_dir(&dir.0);
        std::fs::write(store.path(), "old").unwrap();
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();

        store.save("new").unwrap();

        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "restore token file is {mode:o}");
    }
}
