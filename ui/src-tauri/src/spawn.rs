//! Starting the agent when it is not already running.
//!
//! The UI can start the daemon but never owns it: the child is spawned detached
//! with no inherited console and no pipes, and closing the window leaves it
//! running. That is the whole point of the split — input capture has to survive the
//! UI being quit — so anything that would tie the daemon's lifetime to the window
//! is a bug, including a captured stdout pipe. A pipe nobody drains fills after a
//! few kilobytes of log output and blocks the writing thread, which on this daemon
//! is the thread routing keystrokes.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use wx_agent::ipc::EndpointFile;

/// Environment variable that overrides the search entirely.
///
/// Exists so a developer can point the UI at a specific build, and so a packager
/// whose layout does not match any of the guesses below has a supported answer
/// rather than a patch.
pub const AGENT_PATH_ENV: &str = "WINXTEND_AGENT";

/// File name of the daemon on this platform.
pub const AGENT_EXE: &str = if cfg!(windows) {
    "wx-agent.exe"
} else {
    "wx-agent"
};

/// How long to wait for a freshly started agent to publish its endpoint file.
///
/// It has to bind a QUIC socket and load or generate an identity key first, and key
/// generation on a cold start is the slow part. Long enough not to give up on a
/// working start, short enough that a failed one does not look like a hang.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the agent might be, in the order the locations are tried.
///
/// Pure and parameterised rather than reading the environment itself, so the order
/// can be asserted. The order matters: the binary shipped beside this one is the
/// only one guaranteed to match this UI's protocol version, and a stale build in a
/// development target directory must never win over it.
///
/// What puts a binary there is `bundle.externalBin` in `tauri.conf.json`, built by
/// `ui/scripts/bundle-agent.mjs`. Both halves land in `/usr/bin` in the `.deb`, so
/// the first candidate is the one that hits on an installed machine; the two
/// development paths below only ever answer in a checkout.
pub fn candidates(exe_dir: Option<&Path>, manifest_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = exe_dir {
        out.push(dir.join(AGENT_EXE));
    }
    // Development layout: the workspace target directory two levels up from
    // ui/src-tauri. Debug before release because a developer running the UI from
    // source is iterating on the debug build.
    for profile in ["debug", "release"] {
        out.push(
            manifest_dir
                .join("..")
                .join("..")
                .join("target")
                .join(profile)
                .join(AGENT_EXE),
        );
    }
    out
}

/// First candidate that exists, or `None` to fall back to `PATH`.
pub fn find_agent() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(AGENT_PATH_ENV) {
        let path = PathBuf::from(path);
        // Honoured even when it does not exist: a typo in an override must surface
        // as "that file is not there", not as the UI quietly starting a different
        // binary than the one asked for.
        return Some(path);
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    candidates(exe_dir.as_deref(), Path::new(env!("CARGO_MANIFEST_DIR")))
        .into_iter()
        .find(|c| c.is_file())
}

/// Launch the agent, detached from this process.
pub fn start(agent: &Path, config_dir: Option<&Path>) -> io::Result<u32> {
    let mut command = Command::new(agent);
    if let Some(dir) = config_dir {
        command.arg("--config-dir").arg(dir);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_window(&mut command);
    let child = command.spawn()?;
    Ok(child.id())
}

#[cfg(windows)]
fn no_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW. Without it a console window flashes up and stays, because
    // the daemon runs in the foreground for the life of the session.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_command: &mut Command) {}

/// Wait for a just-started agent to become connectable.
///
/// Polls for the endpoint file rather than sleeping a fixed time: startup is fast
/// on the second run and slow on the first, and a fixed wait would be wrong in both
/// directions. The file is only written once the control channel is listening, so
/// its appearance is the ready signal.
pub async fn wait_for_endpoint(dir: &Path, timeout: Duration) -> Option<EndpointFile> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(endpoint) = EndpointFile::read(dir) {
            return Some(endpoint);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_beside_the_ui_is_preferred_over_any_development_build() {
        // An installed WinXtend has both binaries in one directory, and that pair
        // is the only combination that has been tested together.
        let list = candidates(
            Some(Path::new("C:/Program Files/WinXtend")),
            Path::new("C:/src/WinXtend/ui/src-tauri"),
        );
        assert_eq!(
            list[0],
            Path::new("C:/Program Files/WinXtend").join(AGENT_EXE)
        );
        assert!(list.len() > 1, "development fallbacks are still offered");
    }

    #[test]
    fn a_development_build_is_found_two_levels_up_from_the_crate() {
        let list = candidates(None, Path::new("/src/WinXtend/ui/src-tauri"));
        let debug = list
            .iter()
            .find(|p| p.components().any(|c| c.as_os_str() == "debug"))
            .expect("a debug candidate");
        // Normalised because the candidate keeps the `..` segments it was built
        // from; what matters is that it resolves into the workspace target dir.
        let text = debug.to_string_lossy().replace('\\', "/");
        assert!(text.contains("ui/src-tauri/../../target/debug/"), "{text}");
        assert!(text.ends_with(AGENT_EXE), "{text}");
    }

    #[test]
    fn debug_is_searched_before_release_so_an_old_release_build_cannot_win() {
        let list = candidates(None, Path::new("/src/WinXtend/ui/src-tauri"));
        let idx = |needle: &str| {
            list.iter()
                .position(|p| p.to_string_lossy().replace('\\', "/").contains(needle))
                .unwrap_or_else(|| panic!("no {needle} candidate"))
        };
        assert!(idx("/target/debug/") < idx("/target/release/"));
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_for_an_agent_that_never_starts_gives_up_instead_of_hanging() {
        let dir = std::env::temp_dir().join(format!("winxtend-ui-none-{}", std::process::id()));
        assert!(
            wait_for_endpoint(&dir, Duration::from_secs(2))
                .await
                .is_none(),
            "a directory with no endpoint file must time out"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_endpoint_file_that_is_already_there_is_returned_without_waiting() {
        let dir = std::env::temp_dir().join(format!("winxtend-ui-ready-{}", std::process::id()));
        let file = EndpointFile {
            port: 40404,
            token: "aa".repeat(32),
            pid: std::process::id(),
            agent_version: "0.1.0".into(),
        };
        file.write(&dir).unwrap();
        let found = wait_for_endpoint(&dir, Duration::from_secs(1)).await;
        EndpointFile::remove(&dir);
        assert_eq!(found, Some(file));
    }
}
