//! The UI's connection to the running agent.
//!
//! The agent is the authority for everything on screen, so this module contains no
//! model of its own: it forwards [`Request`]s and republishes [`Event`]s. The only
//! decisions made here are about connection lifetime, and they exist because of
//! two properties of the control channel described in [`wx_agent::ipc`].
//!
//! **Two sockets, not one.** [`IpcClient::call`] drops any event that arrives
//! while it is waiting for a reply, because a caller in the middle of a request has
//! nowhere to put one. A single connection would therefore lose exactly the events
//! that fire while the UI is busy asking about them — a peer appearing during a
//! `status` round trip would vanish until the next manual refresh. So requests get
//! one connection and the event subscription gets another. The agent accepts any
//! number of clients, and each has its own subscription state.
//!
//! **One task owns each socket.** Tauri commands run concurrently on the async
//! runtime, and two of them writing interleaved lines to the same socket would
//! corrupt the framing. Requests are therefore queued to a single task over a
//! channel rather than guarded by a lock, which also means a slow reply cannot be
//! observed as a deadlock by the second caller.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use wx_agent::ipc::{EndpointFile, Event, IpcClient, IpcError, Request, Response};

/// Webview event carrying one agent [`Event`] through verbatim.
pub const EVENT_CHANNEL: &str = "agent-event";
/// Webview event carrying a change in the connection itself.
pub const STATUS_CHANNEL: &str = "agent-status";

/// How many requests may be waiting on the request task.
///
/// Small on purpose: the UI issues requests in response to clicks, so a queue this
/// deep already means the agent has stopped answering, and blocking the caller is
/// more honest than buffering work that will time out anyway.
const REQUEST_QUEUE_DEPTH: usize = 32;

/// What the UI tells the user when the connection changes state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusNote {
    pub connected: bool,
    pub message: String,
}

/// Where connection-level news goes.
///
/// A trait rather than an `AppHandle` so the connection logic does not depend on a
/// webview existing, which is what lets it be exercised without one.
pub trait Notifier: Send + Sync + 'static {
    fn event(&self, event: Event);
    /// The connection ended. Called at most once per connection.
    fn disconnected(&self, message: String);
}

/// Why a connection attempt failed, in the terms the UI has to act on.
#[derive(Debug)]
pub enum ConnectError {
    /// No endpoint file: nothing is running, so offer to start it.
    NotRunning(PathBuf),
    /// The endpoint file exists but the agent behind it does not answer.
    ///
    /// Almost always a file left behind by an agent that was killed rather than
    /// asked to stop. Distinguished from [`ConnectError::NotRunning`] so the UI can
    /// say so instead of claiming nothing was ever there — a user who can see the
    /// process in Task Manager stops trusting a UI that says it is not running.
    Stale(String),
    Failed(String),
}

impl ConnectError {
    fn from_ipc(error: IpcError) -> Self {
        match error {
            IpcError::NotRunning(path) => ConnectError::NotRunning(path),
            // A token mismatch means the file on disk belongs to an older run of
            // the agent: the fix is the same as for a stale port, so it is reported
            // the same way rather than as an authentication problem the user could
            // do nothing about.
            IpcError::Unauthorized => ConnectError::Stale(
                "the endpoint file belongs to an agent that is no longer running".to_string(),
            ),
            IpcError::Io { context, source } => ConnectError::Stale(format!("{context}: {source}")),
            other => ConnectError::Failed(other.to_string()),
        }
    }
}

/// A request queued for the agent, with the channel its answer comes back on.
struct Job {
    request: Request,
    reply: oneshot::Sender<Result<Response, String>>,
}

/// A live connection to the agent.
pub struct Link {
    jobs: mpsc::Sender<Job>,
    /// Endpoint the connection was made through, shown in the UI so a user with
    /// two agents on one machine can tell which one they are looking at.
    pub endpoint: EndpointFile,
}

impl Link {
    /// Connect both sockets and start pumping events.
    pub async fn connect(
        dir: &Path,
        notifier: std::sync::Arc<dyn Notifier>,
    ) -> Result<Self, ConnectError> {
        let endpoint = EndpointFile::read(dir).map_err(ConnectError::from_ipc)?;
        let requests = IpcClient::connect(dir)
            .await
            .map_err(ConnectError::from_ipc)?;
        let events = IpcClient::connect(dir)
            .await
            .map_err(ConnectError::from_ipc)?;

        let (tx, rx) = mpsc::channel(REQUEST_QUEUE_DEPTH);
        tauri::async_runtime::spawn(pump_requests(requests, rx, notifier.clone()));
        tauri::async_runtime::spawn(pump_events(events, notifier));
        Ok(Self { jobs: tx, endpoint })
    }

    /// Send one request and wait for its answer.
    ///
    /// An `Err` here always means the connection is gone, never that the agent
    /// refused the request: a refusal comes back as [`Response::Error`], which the
    /// UI renders in place rather than treating as a disconnection.
    pub async fn request(&self, request: Request) -> Result<Response, String> {
        let (reply, answer) = oneshot::channel();
        self.jobs
            .send(Job { request, reply })
            .await
            .map_err(|_| "the connection to the agent has closed".to_string())?;
        answer
            .await
            .map_err(|_| "the connection to the agent closed before it answered".to_string())?
    }
}

async fn pump_requests(
    mut client: IpcClient,
    mut jobs: mpsc::Receiver<Job>,
    notifier: std::sync::Arc<dyn Notifier>,
) {
    while let Some(job) = jobs.recv().await {
        match client.call(job.request).await {
            Ok(response) => {
                // A caller that has gone away — the window closed mid-request — is
                // not an error worth reporting.
                let _ = job.reply.send(Ok(response));
            }
            Err(e) => {
                let message = e.to_string();
                let _ = job.reply.send(Err(message.clone()));
                notifier.disconnected(message);
                return;
            }
        }
    }
}

async fn pump_events(mut client: IpcClient, notifier: std::sync::Arc<dyn Notifier>) {
    // Nothing is pushed until this succeeds, so a failure here has to end the
    // connection: a UI silently receiving no events looks identical to a mesh where
    // nothing ever happens.
    match client.call(Request::Subscribe).await {
        Ok(Response::Ok) => {}
        Ok(other) => {
            notifier.disconnected(format!(
                "the agent refused the event subscription: {other:?}"
            ));
            return;
        }
        Err(e) => {
            notifier.disconnected(e.to_string());
            return;
        }
    }
    loop {
        match client.next_event().await {
            Ok(event) => notifier.event(event),
            Err(e) => {
                notifier.disconnected(e.to_string());
                return;
            }
        }
    }
}

/// Requests the UI is not allowed to send through the request socket.
///
/// Both would break the connection rather than do something useful:
///
/// * `hello` is answered by comparing the token, and the agent closes the socket on
///   a mismatch. The token was already presented by [`IpcClient::connect`], so a
///   second `hello` from the webview can only be a wrong one.
/// * `subscribe` on the request socket would start delivering events to a reader
///   that discards them, and the event socket would keep working, so the bug would
///   show up as intermittently missing events rather than as an error.
pub fn is_reserved(request: &Request) -> bool {
    matches!(request, Request::Hello { .. } | Request::Subscribe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_requests_the_ui_must_never_forward_are_refused() {
        assert!(is_reserved(&Request::Hello {
            token: "anything".into()
        }));
        assert!(is_reserved(&Request::Subscribe));
        // Everything else is the agent's business, including shutdown: the UI has
        // a stop button and the agent is the one that decides whether to obey.
        assert!(!is_reserved(&Request::Status));
        assert!(!is_reserved(&Request::Shutdown));
        assert!(!is_reserved(&Request::AutoLayout { node: None }));
    }

    #[test]
    fn a_missing_endpoint_file_is_reported_as_not_running_not_as_a_failure() {
        // The distinction drives which recovery the UI offers, so it is asserted
        // rather than left to the message text.
        let missing = PathBuf::from("nowhere").join("ipc.json");
        match ConnectError::from_ipc(IpcError::NotRunning(missing.clone())) {
            ConnectError::NotRunning(path) => assert_eq!(path, missing),
            other => panic!("wrong classification: {other:?}"),
        }
    }

    #[test]
    fn a_refused_port_and_a_rejected_token_both_mean_a_stale_endpoint_file() {
        let refused = ConnectError::from_ipc(IpcError::Io {
            context: "connecting to the agent",
            source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        });
        assert!(matches!(refused, ConnectError::Stale(_)));
        assert!(matches!(
            ConnectError::from_ipc(IpcError::Unauthorized),
            ConnectError::Stale(_)
        ));
        // A protocol-level fault is not a stale file and must not suggest a restart
        // as the fix.
        assert!(matches!(
            ConnectError::from_ipc(IpcError::Closed),
            ConnectError::Failed(_)
        ));
    }
}
