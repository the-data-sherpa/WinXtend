//! Attaching to an agent this process did not start.
//!
//! The unit tests beside `link.rs` cover how a failure is *classified*; this one
//! covers the case that classification exists for. On an installed machine the
//! agent is brought up by the systemd user unit in `packaging/winxtend.service.in`
//! and the window is opened afterwards, so the only thing that ever finds the
//! daemon is [`Link::connect`] reading an endpoint file it did not write. Nothing
//! in the UI's own start-the-agent path is exercised by that, and the two had
//! never been run against each other.
//!
//! The agent here is a real [`IpcServer`] on a real loopback socket with a real
//! endpoint file, but not the real binary. Launching `wx-agent` would build a
//! platform backend, and on Wayland and macOS that means asking the OS for input
//! permission — `cargo test` is not allowed to put a consent dialog on anyone's
//! screen (see the `current_platform` / `current_platform_in` split in
//! `crates/wx-platform/src/lib.rs`). What is under test is the control channel,
//! which has nothing to do with input capture.

use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use winxtend_ui_lib::link::{ConnectError, Link, Notifier};
use wx_agent::ipc::{
    CursorSnapshot, EndpointFile, Event, IpcCommand, IpcServer, LayoutSpec, NoticeLevel, Request,
    Response, StatusSnapshot,
};

/// Where the fake agent puts its endpoint file: a directory of its own, never the
/// real one, because a stray `ipc.json` under a developer's config directory would
/// point their window at a socket that stopped existing when this test ended.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("winxtend-attach-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating a scratch config directory");
    dir
}

/// Collects what the connection reports, so a test can assert on it.
struct Collector {
    events: std_mpsc::Sender<Event>,
    lost: std_mpsc::Sender<String>,
}

impl Notifier for Collector {
    fn event(&self, event: Event) {
        let _ = self.events.send(event);
    }
    fn disconnected(&self, message: String) {
        let _ = self.lost.send(message);
    }
}

fn snapshot() -> StatusSnapshot {
    StatusSnapshot {
        node_id: "903ebcf9".repeat(8),
        node_name: "cowen-ubuntu".into(),
        agent_version: "0.1.0".into(),
        protocol: wx_proto::PROTOCOL_VERSION,
        port: 24800,
        uptime_secs: 314,
        platform: "linux".into(),
        display_server: "wayland".into(),
        capabilities: 2079,
        discovery: true,
        autostart: true,
        firewall: None,
        pairings: Vec::new(),
        monitors: Vec::new(),
        cursor: CursorSnapshot {
            owner: "903ebcf9".repeat(8),
            owner_name: "cowen-ubuntu".into(),
            monitor: Some(0),
            locked: false,
            local: true,
        },
        layout: LayoutSpec {
            revision: 1,
            placements: Vec::new(),
        },
        peers: Vec::new(),
    }
}

/// Stand up a control channel and publish it the way a running agent does.
///
/// Returns the directory the endpoint file was written to, and a handle for
/// pushing events at whoever attaches.
async fn agent_already_running(dir: &Path) -> tokio::sync::broadcast::Sender<Event> {
    let token = wx_agent::ipc::generate_token().expect("a connection token");
    let server = IpcServer::bind(token.clone())
        .await
        .expect("binding the control channel");
    let port = server.local_addr().expect("the bound port").port();
    let events = server.events();

    let (commands, mut inbox) = mpsc::unbounded_channel::<IpcCommand>();
    tauri::async_runtime::spawn(async move {
        while let Some(command) = inbox.recv().await {
            let response = match command.request {
                Request::Status => Response::Status {
                    status: Box::new(snapshot()),
                },
                Request::Hello { .. } => Response::Hello {
                    node_id: snapshot().node_id,
                    node_name: snapshot().node_name,
                    agent_version: snapshot().agent_version,
                    protocol: wx_proto::PROTOCOL_VERSION,
                },
                _ => Response::Ok,
            };
            let _ = command.reply.send(response);
        }
    });
    tauri::async_runtime::spawn(server.serve(commands));

    // Written last, exactly as the engine writes it: its appearance is the signal
    // that the channel is answerable.
    EndpointFile {
        port,
        token,
        pid: std::process::id(),
        agent_version: "0.1.0".into(),
    }
    .write(dir)
    .expect("publishing the endpoint file");

    events
}

#[test]
fn the_window_attaches_to_an_agent_it_did_not_start_and_gets_a_live_snapshot() {
    let dir = scratch_dir("live");
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    rt.block_on(async {
        let published = agent_already_running(&dir).await;

        let (events_tx, events) = std_mpsc::channel();
        let (lost_tx, _lost) = std_mpsc::channel();
        let link = Link::connect(
            &dir,
            Arc::new(Collector {
                events: events_tx,
                lost: lost_tx,
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("attaching failed: {}", describe(&e)));

        // The window shows which agent it is looking at, and it has to be the one
        // the file named rather than anything this process arranged.
        let published_file = EndpointFile::read(&dir).expect("the endpoint file");
        assert_eq!(link.endpoint.port, published_file.port);

        match link.request(Request::Status).await {
            Ok(Response::Status { status }) => {
                assert_eq!(status.node_name, "cowen-ubuntu");
                assert_eq!(status.uptime_secs, 314);
            }
            other => panic!("a status request came back as {other:?}"),
        }

        // Events reach the notifier, which is what the webview is fed from: an
        // attach that cannot carry news is a window that freezes without saying so.
        published
            .send(Event::Notice {
                level: NoticeLevel::Info,
                message: "a peer appeared".into(),
            })
            .expect("a subscriber is listening");
        let seen = tokio::task::spawn_blocking(move || events.recv_timeout(Duration::from_secs(5)))
            .await
            .expect("the collector thread")
            .expect("an event within five seconds");
        assert!(
            matches!(seen, Event::Notice { ref message, .. } if message == "a peer appeared"),
            "unexpected event: {seen:?}"
        );
    });
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_endpoint_file_left_behind_by_a_dead_agent_is_not_reported_as_nothing_running() {
    // The distinction the banner depends on: a user who can see the process in
    // `systemctl --user status` must never be told there is no agent, and a user
    // whose agent was killed must be told the file is stale rather than that
    // nothing was ever there.
    let dir = scratch_dir("stale");
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    rt.block_on(async {
        // A port nothing is listening on: bound, read, and dropped.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let port = dead.local_addr().expect("the bound port").port();
        drop(dead);

        EndpointFile {
            port,
            token: "0".repeat(64),
            pid: std::process::id(),
            agent_version: "0.1.0".into(),
        }
        .write(&dir)
        .expect("writing a stale endpoint file");

        let (events_tx, _events) = std_mpsc::channel();
        let (lost_tx, _lost) = std_mpsc::channel();
        match Link::connect(
            &dir,
            Arc::new(Collector {
                events: events_tx,
                lost: lost_tx,
            }),
        )
        .await
        {
            Err(ConnectError::Stale(_)) => {}
            Err(other) => panic!("wrong classification: {}", describe(&other)),
            Ok(_) => panic!("connected to a port nothing is listening on"),
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}

fn describe(error: &ConnectError) -> String {
    match error {
        ConnectError::NotRunning(path) => format!("NotRunning({})", path.display()),
        ConnectError::Stale(message) => format!("Stale({message})"),
        ConnectError::Failed(message) => format!("Failed({message})"),
    }
}
