//! WinXtend desktop UI: the Tauri host process.
//!
//! This process holds no state that matters. The agent owns the mesh, the trust
//! store, the layout and the cursor; everything here is a proxy over the local
//! control channel plus two things the webview cannot do for itself — start the
//! daemon, and run the engine's own layout validator.
//!
//! Keeping it that thin is deliberate. The UI is quit and reopened constantly while
//! the daemon runs for the whole session, so any state cached here would be a second
//! version of the truth that disagrees with the daemon after the window is closed.
//! The one thing worth remembering across a reconnect is nothing: the first thing
//! the frontend does on connecting is ask for a full [`wx_agent::ipc::StatusSnapshot`].
//!
//! # Commands
//!
//! * `agent_state` — is there a daemon, where is it, are we connected.
//! * `connect_agent` / `disconnect_agent` — attach to or detach from a running one.
//! * `start_agent` — launch one, detached, and wait for it to answer.
//! * `agent_request` — forward one [`Request`] and return its [`Response`].
//! * `validate_layout` — [`problems::validate`], run on the working layout.
//!
//! `agent_request` is one generic command rather than twenty typed ones because the
//! IPC enums are already tagged unions with camelCase `kind` fields, so the frontend
//! can build a request as a plain object and read the answer with a `switch`. A
//! typed wrapper per request would add a place for the two sides to disagree about
//! the contract without adding any checking that the enums do not already do.

pub mod link;
pub mod problems;
pub mod spawn;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;
use wx_agent::config::Config;
use wx_agent::ipc::{EndpointFile, Event, LayoutSpec, Request, Response};

use link::{ConnectError, Link, Notifier, StatusNote, EVENT_CHANNEL, STATUS_CHANNEL};
use problems::ProblemView;

/// Version of this UI, shown beside the agent's so a mismatch is visible.
pub const UI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Points the UI at a specific agent's configuration directory.
///
/// The counterpart of the daemon's `--config-dir`. Needed to drive a second agent on
/// one machine, which is how the mesh gets tested without a second machine.
pub const CONFIG_DIR_ENV: &str = "WINXTEND_CONFIG_DIR";

/// Why a command failed, in a shape the frontend can branch on.
///
/// A code as well as a message because the recovery differs: a missing daemon gets
/// a "Start the agent" button, a lost connection gets "Reconnect", and an internal
/// failure gets neither. Matching English text to decide that is how a reworded
/// message turns into a dead button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Fault {
    pub code: FaultCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FaultCode {
    /// No agent is running: offer to start one.
    NotRunning,
    /// An endpoint file exists but nothing usable is behind it.
    Stale,
    /// The UI is not attached to the agent right now: offer to reconnect.
    NotConnected,
    /// The connection died while this request was in flight.
    Disconnected,
    /// The UI itself refused to send this: a bug in the frontend, not a user error.
    Refused,
    Internal,
}

impl Fault {
    fn new(code: FaultCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<ConnectError> for Fault {
    fn from(error: ConnectError) -> Self {
        match error {
            ConnectError::NotRunning(path) => Fault::new(
                FaultCode::NotRunning,
                format!("no agent is running (nothing at {})", path.display()),
            ),
            ConnectError::Stale(message) => Fault::new(FaultCode::Stale, message),
            ConnectError::Failed(message) => Fault::new(FaultCode::Internal, message),
        }
    }
}

/// A connection plus the generation it was made in.
///
/// The generation is what stops a dying connection from clearing a healthy one.
/// Both pump tasks report their own death asynchronously, so without it this
/// sequence loses the connection the user just made: connection 1 fails, the user
/// reconnects, connection 1's notification finally runs and wipes connection 2.
struct Connection {
    generation: u64,
    link: Arc<Link>,
}

pub struct AppState {
    /// `None` when detached. An `Arc` inside so a request can be issued without
    /// holding the lock across the round trip — otherwise one slow reply would
    /// freeze every other command, including the one that reports the freeze.
    link: Mutex<Option<Connection>>,
    config_dir: Option<PathBuf>,
    /// Whether [`CONFIG_DIR_ENV`] chose the directory, which decides whether a
    /// daemon we start is told about it explicitly.
    config_dir_overridden: bool,
    generations: AtomicU64,
}

impl AppState {
    fn new() -> Self {
        let overridden = std::env::var_os(CONFIG_DIR_ENV).is_some();
        Self {
            link: Mutex::new(None),
            config_dir: resolve_config_dir(),
            config_dir_overridden: overridden,
            generations: AtomicU64::new(1),
        }
    }

    fn dir(&self) -> Result<PathBuf, Fault> {
        self.config_dir.clone().ok_or_else(|| {
            Fault::new(
                FaultCode::Internal,
                "this system has no per-user configuration directory, so the agent's \
                 endpoint file cannot be found",
            )
        })
    }
}

/// Where the agent keeps its endpoint file.
///
/// Derived from [`Config::default_path`] rather than restated, so the UI cannot look
/// in a different place than the daemon writes to.
fn resolve_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    Config::default_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

/// What the frontend needs to render the connection banner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonView {
    pub connected: bool,
    /// Absent only on a system with no per-user config directory at all.
    pub config_dir: Option<String>,
    /// Read fresh from disk every time: an agent started outside the UI must show
    /// up without the user restarting anything.
    pub endpoint: Option<EndpointView>,
    /// Where a daemon would be started from, so a missing binary can be named.
    pub agent_path: Option<String>,
    pub ui_version: String,
    /// Set when the last attempt failed, for the banner's second line.
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointView {
    pub port: u16,
    pub pid: u32,
    pub agent_version: String,
}

impl From<&EndpointFile> for EndpointView {
    fn from(file: &EndpointFile) -> Self {
        Self {
            port: file.port,
            pid: file.pid,
            agent_version: file.agent_version.clone(),
        }
    }
}

fn daemon_view(state: &AppState, connected: bool, message: Option<String>) -> DaemonView {
    // Report why the endpoint could not be read rather than discarding the error.
    // A swallowed failure here is indistinguishable in the UI from "no agent is
    // running", which sends the user looking for a daemon that is already up.
    let endpoint = match state.config_dir.as_deref() {
        Some(dir) => match EndpointFile::read(dir) {
            Ok(file) => Some(file),
            Err(e) => {
                eprintln!("wx-ui: endpoint unreadable at {}: {e}", dir.display());
                None
            }
        },
        None => {
            eprintln!("wx-ui: no config directory resolved");
            None
        }
    };
    DaemonView {
        connected,
        config_dir: state
            .config_dir
            .as_ref()
            .map(|d| d.display().to_string()),
        endpoint: endpoint.as_ref().map(EndpointView::from),
        agent_path: spawn::find_agent().map(|p| p.display().to_string()),
        ui_version: UI_VERSION.to_string(),
        message,
    }
}

/// Republishes agent events into the webview.
struct AppNotifier {
    app: AppHandle,
    generation: u64,
}

impl Notifier for AppNotifier {
    fn event(&self, event: Event) {
        // A failed emit means the window is gone, which is not a problem the agent
        // or the user needs to hear about.
        let _ = self.app.emit(EVENT_CHANNEL, event);
    }

    fn disconnected(&self, message: String) {
        let app = self.app.clone();
        let generation = self.generation;
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            let mut guard = state.link.lock().await;
            let ours = guard.as_ref().is_some_and(|c| c.generation == generation);
            if ours {
                *guard = None;
            }
            drop(guard);
            // Both pump tasks fail together when a socket dies, so only the first
            // one to arrive tells the user; the second would produce a duplicate
            // "connection lost" line in the activity log for one event.
            if ours {
                let _ = app.emit(
                    STATUS_CHANNEL,
                    StatusNote {
                        connected: false,
                        message,
                    },
                );
            }
        });
    }
}

/// Connect and store the connection, assuming the caller already holds the lock.
async fn attach(
    app: &AppHandle,
    state: &AppState,
    guard: &mut Option<Connection>,
) -> Result<(), Fault> {
    if guard.is_some() {
        return Ok(());
    }
    let dir = state.dir()?;
    let generation = state.generations.fetch_add(1, Ordering::Relaxed);
    let notifier = Arc::new(AppNotifier {
        app: app.clone(),
        generation,
    });
    let link = Link::connect(&dir, notifier).await?;
    *guard = Some(Connection {
        generation,
        link: Arc::new(link),
    });
    let _ = app.emit(
        STATUS_CHANNEL,
        StatusNote {
            connected: true,
            message: format!("connected to the agent in {}", dir.display()),
        },
    );
    Ok(())
}

#[tauri::command]
async fn agent_state(state: State<'_, AppState>) -> Result<DaemonView, Fault> {
    let connected = state.link.lock().await.is_some();
    Ok(daemon_view(&state, connected, None))
}

#[tauri::command]
async fn connect_agent(app: AppHandle, state: State<'_, AppState>) -> Result<DaemonView, Fault> {
    let mut guard = state.link.lock().await;
    attach(&app, &state, &mut guard).await?;
    Ok(daemon_view(&state, true, None))
}

#[tauri::command]
async fn disconnect_agent(state: State<'_, AppState>) -> Result<DaemonView, Fault> {
    // Dropping the link closes the sockets, which ends both pump tasks. The agent
    // keeps running: closing the window is not an instruction to stop sharing the
    // keyboard.
    *state.link.lock().await = None;
    Ok(daemon_view(&state, false, None))
}

#[tauri::command]
async fn start_agent(app: AppHandle, state: State<'_, AppState>) -> Result<DaemonView, Fault> {
    let dir = state.dir()?;
    {
        // Scoped so the lock is not held across the start, which can take seconds:
        // every other command locks this too, including the one the interface polls
        // to draw the connection banner, and a frozen banner during a start looks
        // exactly like a hang.
        let mut guard = state.link.lock().await;
        if guard.is_some() {
            return Ok(daemon_view(&state, true, None));
        }
        // Already running but not attached — an agent from a previous session, or one
        // started from a terminal. Starting a second one would fight over the QUIC
        // port and the user would be told the port was in use by "something".
        if EndpointFile::read(&dir).is_ok() && attach(&app, &state, &mut guard).await.is_ok() {
            return Ok(daemon_view(&state, true, None));
        }
    }

    let agent = spawn::find_agent().ok_or_else(|| {
        Fault::new(
            FaultCode::Internal,
            format!(
                "cannot find the {} binary beside this application; set {} to its path",
                spawn::AGENT_EXE,
                spawn::AGENT_PATH_ENV
            ),
        )
    })?;
    // Only pass the directory on when it was overridden: otherwise the daemon
    // resolves it with its own code, and two independent resolutions that agree
    // today but drift later would be a bug that only shows on one platform.
    let explicit = state.config_dir_overridden.then_some(dir.as_path());
    spawn::start(&agent, explicit).map_err(|e| {
        Fault::new(
            FaultCode::Internal,
            format!("starting {}: {e}", agent.display()),
        )
    })?;
    if spawn::wait_for_endpoint(&dir, spawn::START_TIMEOUT)
        .await
        .is_none()
    {
        return Err(Fault::new(
            FaultCode::Internal,
            format!(
                "{} was started but did not open its control channel within {} seconds",
                agent.display(),
                spawn::START_TIMEOUT.as_secs()
            ),
        ));
    }
    let mut guard = state.link.lock().await;
    attach(&app, &state, &mut guard).await?;
    Ok(daemon_view(&state, true, None))
}

#[tauri::command]
async fn agent_request(state: State<'_, AppState>, request: Request) -> Result<Response, Fault> {
    if link::is_reserved(&request) {
        return Err(Fault::new(
            FaultCode::Refused,
            "the connection handshake and the event subscription are managed by the \
             host process and cannot be sent from the interface",
        ));
    }
    let link = {
        let guard = state.link.lock().await;
        let Some(connection) = guard.as_ref() else {
            return Err(Fault::new(
                FaultCode::NotConnected,
                "not connected to an agent",
            ));
        };
        Arc::clone(&connection.link)
    };
    link.request(request)
        .await
        .map_err(|message| Fault::new(FaultCode::Disconnected, message))
}

/// Validate a layout the user is still editing.
///
/// Synchronous and pure, so it can be called on every drag without a round trip to
/// the agent — and so an unsaved arrangement can be warned about before it is
/// pushed anywhere.
#[tauri::command]
fn validate_layout(layout: LayoutSpec) -> Vec<ProblemView> {
    problems::validate(&layout)
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            agent_state,
            connect_agent,
            disconnect_agent,
            start_agent,
            agent_request,
            validate_layout
        ])
        .run(tauri::generate_context!())
        .expect("starting the WinXtend window");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_daemon_and_a_dropped_connection_are_different_faults() {
        // The frontend offers different recoveries for these two, so they must not
        // collapse into one code.
        let missing: Fault = ConnectError::NotRunning(PathBuf::from("C:/x/ipc.json")).into();
        assert_eq!(missing.code, FaultCode::NotRunning);
        assert!(missing.message.contains("ipc.json"), "{}", missing.message);

        let stale: Fault = ConnectError::Stale("nothing listening".into()).into();
        assert_eq!(stale.code, FaultCode::Stale);
    }

    #[test]
    fn a_fault_serialises_with_a_camel_case_code_the_frontend_can_switch_on() {
        let text = serde_json::to_string(&Fault::new(FaultCode::NotRunning, "x")).unwrap();
        assert_eq!(text, r#"{"code":"notRunning","message":"x"}"#);
        let text = serde_json::to_string(&Fault::new(FaultCode::NotConnected, "x")).unwrap();
        assert_eq!(text, r#"{"code":"notConnected","message":"x"}"#);
    }

    #[test]
    fn the_config_directory_is_the_one_the_agent_writes_its_endpoint_file_into() {
        // Not asserting an absolute path — it differs per platform and per user —
        // but that the UI derives it from the daemon's own config path rather than
        // guessing a directory name of its own.
        if let (Ok(config), Some(dir)) = (Config::default_path(), resolve_config_dir()) {
            assert_eq!(config.parent().unwrap(), dir.as_path());
            assert_eq!(
                EndpointFile::path(&dir),
                dir.join(wx_agent::ipc::ENDPOINT_FILE)
            );
        }
    }

    #[test]
    fn the_config_directory_can_be_pointed_at_a_second_agent_for_testing() {
        // Two agents on one machine is how the mesh is tested without a second
        // machine, and the UI has to be able to talk to the second one.
        assert_eq!(CONFIG_DIR_ENV, "WINXTEND_CONFIG_DIR");
    }

    #[test]
    fn an_endpoint_file_is_reported_to_the_ui_without_a_connection() {
        // The banner has to be able to say "an agent is listening on port N but we
        // are not attached", which is the state after the UI is reopened.
        let file = EndpointFile {
            port: 5000,
            token: "0".repeat(64),
            pid: 42,
            agent_version: "0.1.0".into(),
        };
        let view = EndpointView::from(&file);
        assert_eq!(view.port, 5000);
        assert_eq!(view.pid, 42);
        // The token is deliberately absent: it is a credential and the webview has
        // no use for it.
        let text = serde_json::to_string(&view).unwrap();
        assert!(!text.contains(&file.token), "{text}");
    }
}
