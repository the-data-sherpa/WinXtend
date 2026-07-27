//! The local control channel between the agent and the UI.
//!
//! # The contract, in full
//!
//! The agent listens on **TCP, `127.0.0.1`, an OS-assigned port**, and writes the
//! port together with a 256-bit hex token to
//! `<config dir>/ipc.json`:
//!
//! ```json
//! { "port": 51234, "token": "9f3c…", "pid": 8421, "agentVersion": "0.1.0" }
//! ```
//!
//! A client reads that file, connects, and speaks **newline-delimited JSON**: one
//! complete JSON object per line, in both directions, UTF-8, no embedded newlines
//! (`serde_json` never emits any).
//!
//! ## Framing
//!
//! Client to agent:
//!
//! ```json
//! {"id":1,"request":{"kind":"hello","token":"9f3c…"}}
//! {"id":2,"request":{"kind":"listPeers"}}
//! ```
//!
//! Agent to client — a reply carries the `id` it answers, an event carries none:
//!
//! ```json
//! {"kind":"response","id":2,"response":{"kind":"peers","peers":[…]}}
//! {"kind":"event","event":{"kind":"peerChanged","peer":{…}}}
//! ```
//!
//! Every enum in this module is JSON-tagged by a `kind` field with camelCase
//! values, so a TypeScript client can discriminate on `kind` with no wrapper
//! objects to unwrap.
//!
//! ## Rules a client must follow
//!
//! 1. **`hello` first.** Any other request before a successful `hello` is answered
//!    with `{"kind":"error","code":"unauthorized"}` and the connection is closed.
//!    The token proves the client runs as the same user; it is not a password and
//!    is regenerated on every agent start.
//! 2. **Replies are matched by `id`, not by order.** Requests may be answered out
//!    of order, and events may arrive between a request and its reply.
//! 3. **Events must be asked for.** Nothing is pushed until `subscribe`, so a
//!    one-shot client (`wx-agent --status`) never has to filter them.
//! 4. **Ignore unknown `kind` values.** New requests, responses, and events are
//!    added over time; a client that treats an unknown event as an error breaks on
//!    the next agent release.
//! 5. **Lines are capped** at [`MAX_LINE_LEN`]. A longer line closes the
//!    connection rather than being buffered.
//!
//! ## Why TCP on loopback rather than a named pipe
//!
//! A named pipe would let Windows enforce the caller's identity with an ACL
//! instead of a token, which is genuinely better. It is not used because the same
//! code then cannot serve macOS and Linux, and a UI that speaks two different
//! transports depending on target is a permanent source of bugs in the part of the
//! system users touch most. The token file lands in the per-user config directory,
//! which is already ACL'd to the user on Windows and is `chmod 0600` here on unix.
//!
//! What that buys and does not buy: another process running as **the same user**
//! can read the token and drive the agent. That is not a boundary this design
//! defends, and it is not one a named pipe would defend either, because such a
//! process could inject input directly with far less effort. A process running as
//! a *different* user cannot read the file.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use wx_proto::{Capabilities, Layout, Monitor, MonitorId, NodeId, Rect};

use crate::config::{Config, SavedLayout};
use crate::state::{AgentState, ConnStatus, PeerState};

/// File in the config directory that tells a client where to connect.
pub const ENDPOINT_FILE: &str = "ipc.json";

/// Longest line accepted in either direction.
///
/// Generous enough for a layout with dozens of monitors, small enough that a
/// confused or hostile local client cannot make the agent allocate without bound.
pub const MAX_LINE_LEN: usize = 1024 * 1024;

/// Bytes of entropy in the connection token.
const TOKEN_BYTES: usize = 32;

/// How many events may queue for a subscriber before it is disconnected.
///
/// A UI that stops reading is a UI that has hung; dropping it beats growing the
/// agent's memory on its behalf.
const EVENT_QUEUE_DEPTH: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("the agent does not appear to be running (no {0})")]
    NotRunning(PathBuf),
    #[error("malformed JSON on the control channel: {0}")]
    Json(#[from] serde_json::Error),
    #[error("a control-channel line exceeded {MAX_LINE_LEN} bytes")]
    LineTooLong,
    #[error("the agent closed the control channel")]
    Closed,
    #[error("the agent refused the connection token")]
    Unauthorized,
    #[error("the agent reported: {message}")]
    Agent { code: ErrorCode, message: String },
    #[error("generating the control-channel token: {0}")]
    Random(getrandom::Error),
}

fn io_err(context: &'static str) -> impl FnOnce(io::Error) -> IpcError {
    move |source| IpcError::Io { context, source }
}

/// What a client reads to find the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointFile {
    pub port: u16,
    /// Hex-encoded shared secret for this run of the agent.
    pub token: String,
    /// Agent process id, so a client can tell a stale file from a live one.
    pub pid: u32,
    pub agent_version: String,
}

impl EndpointFile {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(ENDPOINT_FILE)
    }

    /// Write the file with user-only permissions.
    ///
    /// The permissions are the security boundary, so they are set before the
    /// token is written where the platform allows it — a token that is briefly
    /// world-readable is a token that leaked.
    pub fn write(&self, dir: &Path) -> Result<(), IpcError> {
        std::fs::create_dir_all(dir).map_err(io_err("creating the config directory"))?;
        let path = Self::path(dir);
        let json = serde_json::to_string_pretty(self)?;

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .map_err(io_err("creating the endpoint file"))?;
            file.write_all(json.as_bytes())
                .map_err(io_err("writing the endpoint file"))?;
            Ok(())
        }

        #[cfg(not(unix))]
        {
            // Windows: the config directory lives under the user's profile and is
            // already ACL'd to that user, which is the same boundary the unix mode
            // bits draw.
            std::fs::write(&path, json).map_err(io_err("writing the endpoint file"))
        }
    }

    pub fn read(dir: &Path) -> Result<Self, IpcError> {
        let path = Self::path(dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(IpcError::NotRunning(path)),
            Err(source) => Err(IpcError::Io {
                context: "reading the endpoint file",
                source,
            }),
        }
    }

    /// Remove the file on a clean shutdown, so `--status` says "not running"
    /// rather than failing to connect to a port nobody is listening on.
    pub fn remove(dir: &Path) {
        let _ = std::fs::remove_file(Self::path(dir));
    }
}

/// A fresh connection token.
pub fn generate_token() -> Result<String, IpcError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(IpcError::Random)?;
    Ok(hex::encode(bytes))
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// One request from a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestEnvelope {
    /// Correlation id chosen by the client. Echoed in the reply.
    pub id: u64,
    pub request: Request,
}

/// Anything the UI can ask for.
///
/// Node ids are hex strings throughout — the same form the UI displays — so a
/// client never handles a 32-element byte array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Request {
    /// Authenticate. Must be the first request on a connection.
    Hello {
        token: String,
    },
    /// Everything needed to draw the main window in one round trip.
    Status,
    ListPeers,
    /// Start pairing with a discovered peer. This side generates the PIN and
    /// returns it; the user reads it out and types it on the other machine.
    BeginPairing {
        node: String,
    },
    /// Supply a PIN for a pairing the *peer* started. `node` may be omitted when
    /// exactly one pairing is pending, which is what `--pair <code>` relies on.
    ConfirmPairing {
        #[serde(default)]
        node: Option<String>,
        pin: String,
    },
    CancelPairing {
        node: String,
    },
    /// Forget a peer: remove it from the trust store and from the layout.
    ForgetPeer {
        node: String,
    },
    /// Refuse a peer permanently. Kept as an entry so it cannot re-offer pairing.
    BlockPeer {
        node: String,
    },
    SetPeerName {
        node: String,
        name: String,
    },
    SetPeerEnabled {
        node: String,
        enabled: bool,
    },
    GetLayout,
    /// Replace the layout. The revision is assigned by the agent, not the client:
    /// two UIs editing at once would otherwise both claim the same revision and
    /// the peers would disagree about which won.
    SetLayout {
        layout: LayoutSpec,
    },
    /// Re-derive placements automatically. With `node` set, only that node moves.
    AutoLayout {
        #[serde(default)]
        node: Option<String>,
    },
    /// Pin the cursor to the machine it is on. Omit `locked` to toggle.
    SetCursorLock {
        #[serde(default)]
        locked: Option<bool>,
    },
    /// Jump the cursor to a monitor, as when the user clicks a screen in the
    /// layout editor.
    WarpCursor {
        node: String,
        monitor: u32,
        /// Position within the monitor, `0.0..1.0`. Defaults to the centre.
        #[serde(default = "half")]
        x: f64,
        #[serde(default = "half")]
        y: f64,
    },
    /// Lock this session and every connected peer's, skipping any machine — this
    /// one included — that has not advertised `SCREENSAVER_SYNC`. The connected
    /// machines left unlocked are named in a warning [`Event::Notice`]; a paired
    /// machine that is currently offline or in dial backoff is neither sent the
    /// request nor named, so this is not a guarantee that every machine is locked.
    LockAll,
    GetConfig,
    SetNodeName {
        name: String,
    },
    /// Begin receiving [`Event`]s on this connection.
    Subscribe,
    /// Ask the agent to exit. Answered before the process goes away.
    Shutdown,
}

fn half() -> f64 {
    0.5
}

/// Anything the agent sends back in reply to a request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Response {
    /// Generic success for a request with nothing to return.
    Ok,
    Error {
        code: ErrorCode,
        message: String,
    },
    /// Answer to `hello`.
    Hello {
        node_id: String,
        node_name: String,
        agent_version: String,
        protocol: u16,
    },
    /// Boxed so that the common answers — `ok`, an error, a lock state — do not
    /// each carry the footprint of a full snapshot around by value.
    Status {
        status: Box<StatusSnapshot>,
    },
    Peers {
        peers: Vec<PeerSnapshot>,
    },
    Layout {
        layout: LayoutSpec,
    },
    /// Answer to `beginPairing`: show this PIN to the user.
    PairingStarted {
        node: String,
        pin: String,
    },
    CursorLock {
        locked: bool,
    },
    Config {
        config: Box<Config>,
    },
}

/// Machine-readable failure kinds.
///
/// A code rather than only a message so the UI can react — offer a pairing dialog,
/// grey out a button — without matching on English text that will be reworded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    /// No successful `hello` on this connection, or a bad token.
    Unauthorized,
    /// Malformed request, or a field that does not parse (a bad hex node id).
    BadRequest,
    /// No such peer is known.
    UnknownPeer,
    /// No pairing is in progress, or more than one is and none was named.
    NoPairing,
    /// The peer is not connected, so the request cannot be carried out now.
    NotConnected,
    /// The agent cannot do this on this platform or in this build.
    Unsupported,
    /// Something failed that the client cannot fix.
    Internal,
}

impl Response {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Response::Error {
            code,
            message: message.into(),
        }
    }
}

/// Anything the agent pushes to a subscribed client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Event {
    /// A peer appeared, or anything about it changed.
    PeerChanged {
        peer: PeerSnapshot,
    },
    PeerRemoved {
        node: String,
    },
    /// A peer wants to pair and is waiting for a PIN to be typed here.
    ///
    /// The counterpart of `beginPairing` on the other machine: that side shows a
    /// PIN, this side prompts for it.
    PairingRequested {
        node: String,
        name: String,
    },
    /// Pairing finished, one way or the other.
    PairingFinished {
        node: String,
        accepted: bool,
        #[serde(default)]
        message: Option<String>,
    },
    LayoutChanged {
        layout: LayoutSpec,
    },
    /// The cursor moved to a different machine. Emitted on ownership changes
    /// only, never per pixel — the input plane runs at hundreds of events a
    /// second and the UI has no use for that.
    CursorOwnerChanged {
        node: String,
        monitor: u32,
        local: bool,
    },
    CursorLockChanged {
        locked: bool,
    },
    /// This machine's displays changed.
    MonitorsChanged {
        monitors: Vec<MonitorSpec>,
    },
    /// Something the user should be told about, in words.
    Notice {
        level: NoticeLevel,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// Either kind of thing the agent writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ServerMessage {
    Response { id: u64, response: Response },
    Event { event: Event },
}

/// The whole of what a UI needs to render itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    pub node_id: String,
    pub node_name: String,
    pub agent_version: String,
    pub protocol: u16,
    pub port: u16,
    pub uptime_secs: u64,
    pub platform: String,
    pub display_server: String,
    /// Raw [`wx_proto::Capabilities`] bits, so a UI built against a newer
    /// protocol can show a capability this build has never heard of.
    pub capabilities: u32,
    pub discovery: bool,
    pub autostart: bool,
    pub monitors: Vec<MonitorSpec>,
    pub cursor: CursorSnapshot,
    pub layout: LayoutSpec,
    pub peers: Vec<PeerSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorSnapshot {
    /// Node currently holding the cursor.
    pub owner: String,
    pub owner_name: String,
    /// Monitor within that node, absent before any layout exists.
    #[serde(default)]
    pub monitor: Option<u32>,
    pub locked: bool,
    /// Whether the owner is this machine. Derivable from `owner`, included
    /// because it is what the UI actually branches on.
    pub local: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSnapshot {
    pub node: String,
    pub name: String,
    /// Name the peer advertises, shown when it differs from the local label.
    pub advertised_name: String,
    pub status: PeerStatusSpec,
    pub paired: bool,
    pub blocked: bool,
    pub enabled: bool,
    pub addresses: Vec<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub agent_version: Option<String>,
    #[serde(default)]
    pub rtt_ms: Option<u64>,
    pub monitors: Vec<MonitorSpec>,
    /// Raw [`wx_proto::Capabilities`] bits this peer advertised, zero before it has
    /// introduced itself. Raw for the same reason the local set is (see
    /// [`StatusSnapshot::capabilities`]), and present at all so the UI can show what
    /// each machine will actually do rather than what the software wishes it would.
    #[serde(default)]
    pub capabilities: u32,
}

impl PeerSnapshot {
    pub fn of(peer: &PeerState) -> Self {
        Self {
            node: peer.node.to_hex(),
            name: peer.display_name().to_string(),
            advertised_name: peer.advertised_name.clone(),
            status: PeerStatusSpec::of(&peer.status),
            paired: peer.paired,
            blocked: peer.blocked,
            enabled: peer.enabled,
            addresses: peer.addresses.iter().map(|a| a.to_string()).collect(),
            platform: peer
                .info
                .as_ref()
                .map(|i| platform_name(i.platform).to_string()),
            agent_version: peer.info.as_ref().map(|i| i.agent_version.clone()),
            rtt_ms: peer.rtt.map(|d| d.as_millis() as u64),
            monitors: peer.monitors().iter().map(MonitorSpec::of).collect(),
            capabilities: peer.capabilities().0,
        }
    }
}

/// Connection state, as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum PeerStatusSpec {
    Offline,
    Discovered,
    Connecting,
    /// A session exists but the peer is untrusted and may send only pairing
    /// messages.
    Pairing,
    Connected,
    Failed {
        error: String,
    },
}

impl PeerStatusSpec {
    pub fn of(status: &ConnStatus) -> Self {
        match status {
            ConnStatus::Offline => PeerStatusSpec::Offline,
            ConnStatus::Discovered => PeerStatusSpec::Discovered,
            ConnStatus::Connecting => PeerStatusSpec::Connecting,
            ConnStatus::Pairing => PeerStatusSpec::Pairing,
            ConnStatus::Connected => PeerStatusSpec::Connected,
            ConnStatus::Failed(error) => PeerStatusSpec::Failed {
                error: error.clone(),
            },
        }
    }
}

/// A display, as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorSpec {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub scale: f32,
    pub primary: bool,
}

impl MonitorSpec {
    pub fn of(m: &Monitor) -> Self {
        Self {
            id: m.id.0,
            name: m.name.clone(),
            x: m.local_bounds.x,
            y: m.local_bounds.y,
            w: m.local_bounds.w,
            h: m.local_bounds.h,
            scale: m.scale,
            primary: m.primary,
        }
    }

    pub fn to_monitor(&self) -> Monitor {
        Monitor {
            id: MonitorId(self.id),
            name: self.name.clone(),
            local_bounds: Rect::new(self.x, self.y, self.w, self.h),
            scale: self.scale,
            primary: self.primary,
        }
    }
}

/// The layout, in the same hex-keyed shape the config file uses.
///
/// Shared with [`crate::config`] on purpose: one representation of a layout means
/// the UI can round-trip what it read from the agent without a second conversion,
/// and a layout that survives the config file survives the IPC channel.
pub type LayoutSpec = SavedLayout;

pub use crate::config::SavedPlacement as PlacementSpec;

/// Build the snapshot a `status` request answers with.
///
/// A free function taking the pieces rather than a method on the engine, so that
/// the shape of the UI's view of the world is defined here next to the types it
/// uses, and can be tested without an engine.
///
/// `advertised` is what this machine has actually told the mesh it can do, and has
/// to be passed in rather than read from `platform`: `PlatformInfo::capabilities`
/// is fixed when the backend is built, while the advertisement is recomputed on
/// every display hotplug. The snapshot's `capabilities` is the row a user reads
/// when a feature is not happening between two machines, so it has to be the set
/// peers were given, not the one this process booted with.
#[allow(clippy::too_many_arguments)]
pub fn status_snapshot(
    state: &AgentState,
    config: &Config,
    layout: &Layout,
    platform: &wx_platform::PlatformInfo,
    advertised: Capabilities,
    agent_version: &str,
    uptime: Duration,
) -> StatusSnapshot {
    let owner = state.cursor_owner();
    let owner_name = if owner == state.local() {
        state.local_name().to_string()
    } else {
        state
            .peer(&owner)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| owner.short())
    };

    let mut peers: Vec<PeerSnapshot> = state.peers().map(PeerSnapshot::of).collect();
    // Sorted by name so the UI's list does not reshuffle on every refresh, which
    // is what a HashMap iteration order would do.
    peers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.node.cmp(&b.node)));

    StatusSnapshot {
        node_id: state.local().to_hex(),
        node_name: state.local_name().to_string(),
        agent_version: agent_version.to_string(),
        protocol: wx_proto::PROTOCOL_VERSION,
        port: config.network.port,
        uptime_secs: uptime.as_secs(),
        platform: platform_name(platform.platform).to_string(),
        display_server: display_server_name(platform.display_server).to_string(),
        capabilities: advertised.0,
        discovery: config.network.discovery,
        autostart: config.node.autostart,
        monitors: state.local_monitors().iter().map(MonitorSpec::of).collect(),
        cursor: CursorSnapshot {
            owner: owner.to_hex(),
            owner_name,
            monitor: state.cursor_on().map(|m| m.monitor.0),
            locked: state.cursor_locked(),
            local: owner == state.local(),
        },
        layout: SavedLayout::from_layout(layout),
        peers,
    }
}

pub fn platform_name(p: wx_proto::Platform) -> &'static str {
    match p {
        wx_proto::Platform::Windows => "windows",
        wx_proto::Platform::MacOs => "macos",
        wx_proto::Platform::Linux => "linux",
        wx_proto::Platform::Other => "other",
    }
}

pub fn display_server_name(d: wx_proto::DisplayServer) -> &'static str {
    match d {
        wx_proto::DisplayServer::Windows => "windows",
        wx_proto::DisplayServer::Quartz => "quartz",
        wx_proto::DisplayServer::X11 => "x11",
        wx_proto::DisplayServer::Wayland => "wayland",
        wx_proto::DisplayServer::Headless => "headless",
    }
}

/// Parse a hex node id from a request.
pub fn parse_node(hex: &str) -> Result<NodeId, Response> {
    NodeId::from_hex(hex.trim()).map_err(|e| {
        Response::error(
            ErrorCode::BadRequest,
            format!("{hex:?} is not a node id: {e}"),
        )
    })
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// A request handed to the engine, with the channel to answer on.
///
/// The engine owns all mutable state and runs single-threaded, so IPC handling is
/// a message rather than a lock: a UI request is queued alongside captured input
/// and network events and processed in the same loop, which is what makes the
/// ordering of "set the layout" against "route this keystroke" well defined.
pub struct IpcCommand {
    pub request: Request,
    pub reply: oneshot::Sender<Response>,
}

/// The listening side of the control channel.
pub struct IpcServer {
    listener: TcpListener,
    token: Arc<String>,
    events: broadcast::Sender<Event>,
}

impl IpcServer {
    /// Bind to an OS-assigned loopback port.
    ///
    /// Ephemeral rather than fixed so that two agents — a stale one shutting down,
    /// a new one starting — never fight over a port, and so nothing has to be
    /// chosen that might already be in use on the user's machine.
    pub async fn bind(token: String) -> Result<Self, IpcError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(io_err("binding the control channel"))?;
        let (events, _) = broadcast::channel(EVENT_QUEUE_DEPTH);
        Ok(Self {
            listener,
            token: Arc::new(token),
            events,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Handle for publishing events to every subscribed client.
    pub fn events(&self) -> broadcast::Sender<Event> {
        self.events.clone()
    }

    /// Accept connections until the process ends.
    ///
    /// One task per connection: a UI that stops reading must not stall the agent,
    /// and each connection has its own subscription state.
    pub async fn serve(self, commands: mpsc::UnboundedSender<IpcCommand>) {
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "control channel accept failed");
                    // A transient accept error must not spin the loop; a
                    // permanent one is reported once per occurrence anyway.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let token = Arc::clone(&self.token);
            let commands = commands.clone();
            let events = self.events.subscribe();
            tokio::spawn(async move {
                if let Err(e) = serve_client(stream, token, commands, events).await {
                    tracing::debug!(client = %peer, error = %e, "control channel client ended");
                }
            });
        }
    }
}

async fn serve_client(
    stream: TcpStream,
    token: Arc<String>,
    commands: mpsc::UnboundedSender<IpcCommand>,
    mut events: broadcast::Receiver<Event>,
) -> Result<(), IpcError> {
    // Nagle would add up to 40ms to every reply on a channel where the round trip
    // is otherwise microseconds.
    let _ = stream.set_nodelay(true);
    let (read_half, mut write) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut lines = LineReader::default();
    let mut authenticated = false;
    let mut subscribed = false;

    loop {
        // `lines` owns the partial-line buffer rather than the future, which is
        // what makes the read branch safe to cancel: `select!` drops the losing
        // future, and a future holding half a line would lose those bytes and
        // desynchronise the channel.
        let line = tokio::select! {
            biased;
            line = lines.next_line(&mut reader) => line?,
            event = events.recv(), if subscribed => {
                match event {
                    Ok(event) => {
                        write_message(&mut write, &ServerMessage::Event { event }).await?;
                        continue;
                    }
                    // The client fell far enough behind that events were dropped.
                    // Silence would leave stale state on screen forever, so the
                    // connection is ended and the UI reconnects and re-reads.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "control channel subscriber fell behind");
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        };
        let Some(line) = line else {
            return Ok(());
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let envelope: RequestEnvelope = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(e) => {
                // Answered with id 0 because the id could not be recovered: the
                // client at least learns its request was rejected rather than
                // waiting forever for a reply.
                let response = Response::error(ErrorCode::BadRequest, e.to_string());
                write_message(&mut write, &ServerMessage::Response { id: 0, response }).await?;
                continue;
            }
        };

        let response = match (&envelope.request, authenticated) {
            (Request::Hello { token: given }, _) => {
                if !wx_net::pairing::constant_time_eq(given.as_bytes(), token.as_bytes()) {
                    write_message(
                        &mut write,
                        &ServerMessage::Response {
                            id: envelope.id,
                            response: Response::error(
                                ErrorCode::Unauthorized,
                                "connection token does not match",
                            ),
                        },
                    )
                    .await?;
                    return Ok(());
                }
                authenticated = true;
                dispatch(
                    &commands,
                    Request::Hello {
                        token: String::new(),
                    },
                )
                .await
            }
            (_, false) => Response::error(
                ErrorCode::Unauthorized,
                "send a hello request with the connection token first",
            ),
            (Request::Subscribe, true) => {
                subscribed = true;
                Response::Ok
            }
            (_, true) => dispatch(&commands, envelope.request.clone()).await,
        };

        let shutdown = matches!(envelope.request, Request::Shutdown)
            && !matches!(response, Response::Error { .. });
        write_message(
            &mut write,
            &ServerMessage::Response {
                id: envelope.id,
                response,
            },
        )
        .await?;
        if !authenticated || shutdown {
            return Ok(());
        }
    }
}

/// Hand a request to the engine and wait for its answer.
async fn dispatch(commands: &mpsc::UnboundedSender<IpcCommand>, request: Request) -> Response {
    let (reply, answer) = oneshot::channel();
    if commands.send(IpcCommand { request, reply }).is_err() {
        return Response::error(ErrorCode::Internal, "the agent is shutting down");
    }
    answer
        .await
        .unwrap_or_else(|_| Response::error(ErrorCode::Internal, "the agent dropped the request"))
}

async fn write_message<W>(write: &mut W, msg: &ServerMessage) -> Result<(), IpcError>
where
    W: AsyncWriteExt + Unpin,
{
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    write
        .write_all(&line)
        .await
        .map_err(io_err("writing to the control channel"))?;
    write
        .flush()
        .await
        .map_err(io_err("flushing the control channel"))
}

/// Newline framing with a bounded buffer and a cancellation-safe partial line.
///
/// Two failure modes it exists to prevent:
///
/// * `AsyncBufReadExt::read_line` grows its target string until the process runs
///   out of memory, which turns a confused local client into a denial of service
///   against the input path. Hence [`MAX_LINE_LEN`].
/// * A future that has consumed half a line and is then dropped — which is
///   exactly what `select!` does to the losing branch — takes those bytes with
///   it, and every subsequent line is misparsed. Hence the buffer living in this
///   struct rather than on the stack of the future.
#[derive(Debug, Default)]
struct LineReader {
    pending: Vec<u8>,
}

impl LineReader {
    /// Next complete line without its terminator, or `None` at end of input.
    async fn next_line<R>(&mut self, reader: &mut R) -> Result<Option<String>, IpcError>
    where
        R: AsyncBufReadExt + Unpin,
    {
        loop {
            if let Some(idx) = self.pending.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=idx).collect();
                line.pop();
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            if self.pending.len() > MAX_LINE_LEN {
                return Err(IpcError::LineTooLong);
            }
            // One await per iteration, and nothing is consumed until it resolves,
            // so cancelling here loses nothing.
            let available = reader
                .fill_buf()
                .await
                .map_err(io_err("reading the control channel"))?;
            if available.is_empty() {
                // End of input. A trailing fragment with no newline is not a
                // message and is discarded rather than half-parsed.
                return Ok(None);
            }
            let taken = available.len();
            self.pending.extend_from_slice(available);
            reader.consume(taken);
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// The CLI's and the UI's side of the channel.
pub struct IpcClient {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    write: tokio::net::tcp::OwnedWriteHalf,
    next_id: u64,
    lines: LineReader,
}

impl IpcClient {
    /// Connect using the endpoint file in `dir` and complete the `hello`.
    pub async fn connect(dir: &Path) -> Result<Self, IpcError> {
        let endpoint = EndpointFile::read(dir)?;
        let stream = TcpStream::connect(("127.0.0.1", endpoint.port))
            .await
            .map_err(io_err("connecting to the agent"))?;
        let _ = stream.set_nodelay(true);
        let (read_half, write) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            write,
            next_id: 1,
            lines: LineReader::default(),
        };
        match client
            .call(Request::Hello {
                token: endpoint.token,
            })
            .await?
        {
            Response::Hello { .. } | Response::Ok => Ok(client),
            Response::Error {
                code: ErrorCode::Unauthorized,
                ..
            } => Err(IpcError::Unauthorized),
            Response::Error { code, message } => Err(IpcError::Agent { code, message }),
            other => Err(IpcError::Agent {
                code: ErrorCode::Internal,
                message: format!("unexpected answer to hello: {other:?}"),
            }),
        }
    }

    /// Send a request and wait for its reply.
    ///
    /// Events arriving in the meantime are dropped rather than queued: a caller
    /// that wants events uses [`IpcClient::next_event`] instead, and a caller in
    /// the middle of a request has nowhere to put them.
    pub async fn call(&mut self, request: Request) -> Result<Response, IpcError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&RequestEnvelope { id, request })?;
        line.push(b'\n');
        self.write
            .write_all(&line)
            .await
            .map_err(io_err("writing to the agent"))?;
        self.write
            .flush()
            .await
            .map_err(io_err("flushing to the agent"))?;

        loop {
            match self.read_message().await? {
                ServerMessage::Response { id: got, response } if got == id => return Ok(response),
                // A reply to a request we are no longer waiting for, or an event.
                // Ignored rather than treated as a protocol error, because the
                // rules explicitly allow both.
                _ => continue,
            }
        }
    }

    /// Wait for the next pushed event. Requires a prior `subscribe`.
    pub async fn next_event(&mut self) -> Result<Event, IpcError> {
        loop {
            if let ServerMessage::Event { event } = self.read_message().await? {
                return Ok(event);
            }
        }
    }

    /// Convenience: unwrap an `Ok`/`Error` answer into a `Result`.
    pub fn expect_ok(response: Response) -> Result<Response, IpcError> {
        match response {
            Response::Error { code, message } => Err(IpcError::Agent { code, message }),
            other => Ok(other),
        }
    }

    async fn read_message(&mut self) -> Result<ServerMessage, IpcError> {
        loop {
            let line = self
                .lines
                .next_line(&mut self.reader)
                .await?
                .ok_or(IpcError::Closed)?;
            if line.trim().is_empty() {
                continue;
            }
            return Ok(serde_json::from_str(line.trim())?);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::NodeId;

    fn round_trip_request(request: Request) -> Request {
        let envelope = RequestEnvelope { id: 7, request };
        let text = serde_json::to_string(&envelope).unwrap();
        let back: RequestEnvelope = serde_json::from_str(&text).unwrap();
        assert_eq!(back.id, 7);
        back.request
    }

    fn round_trip_server(msg: ServerMessage) -> ServerMessage {
        let text = serde_json::to_string(&msg).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn every_request_round_trips_through_json() {
        for request in [
            Request::Hello {
                token: "abc".into(),
            },
            Request::Status,
            Request::ListPeers,
            Request::BeginPairing {
                node: NodeId([1u8; 32]).to_hex(),
            },
            Request::ConfirmPairing {
                node: None,
                pin: "123456".into(),
            },
            Request::CancelPairing {
                node: NodeId([1u8; 32]).to_hex(),
            },
            Request::ForgetPeer {
                node: NodeId([2u8; 32]).to_hex(),
            },
            Request::BlockPeer {
                node: NodeId([2u8; 32]).to_hex(),
            },
            Request::SetPeerName {
                node: NodeId([2u8; 32]).to_hex(),
                name: "mac".into(),
            },
            Request::SetPeerEnabled {
                node: NodeId([2u8; 32]).to_hex(),
                enabled: false,
            },
            Request::GetLayout,
            Request::SetLayout {
                layout: LayoutSpec::default(),
            },
            Request::AutoLayout { node: None },
            Request::SetCursorLock { locked: Some(true) },
            Request::WarpCursor {
                node: NodeId([3u8; 32]).to_hex(),
                monitor: 1,
                x: 0.25,
                y: 0.75,
            },
            Request::LockAll,
            Request::GetConfig,
            Request::SetNodeName {
                name: "desk".into(),
            },
            Request::Subscribe,
            Request::Shutdown,
        ] {
            assert_eq!(round_trip_request(request.clone()), request);
        }
    }

    #[test]
    fn every_response_round_trips_through_json() {
        for response in [
            Response::Ok,
            Response::error(ErrorCode::UnknownPeer, "no such peer"),
            Response::Hello {
                node_id: NodeId([4u8; 32]).to_hex(),
                node_name: "desk".into(),
                agent_version: "0.1.0".into(),
                protocol: 1,
            },
            Response::Peers { peers: vec![] },
            Response::Layout {
                layout: LayoutSpec::default(),
            },
            Response::PairingStarted {
                node: NodeId([5u8; 32]).to_hex(),
                pin: "004312".into(),
            },
            Response::CursorLock { locked: true },
            Response::Config {
                config: Box::new(Config::default()),
            },
        ] {
            let msg = ServerMessage::Response {
                id: 3,
                response: response.clone(),
            };
            assert_eq!(
                round_trip_server(msg),
                ServerMessage::Response { id: 3, response }
            );
        }
    }

    #[test]
    fn every_event_round_trips_through_json() {
        for event in [
            Event::PeerRemoved {
                node: NodeId([6u8; 32]).to_hex(),
            },
            Event::PairingRequested {
                node: NodeId([6u8; 32]).to_hex(),
                name: "laptop".into(),
            },
            Event::PairingFinished {
                node: NodeId([6u8; 32]).to_hex(),
                accepted: false,
                message: Some("wrong code".into()),
            },
            Event::LayoutChanged {
                layout: LayoutSpec::default(),
            },
            Event::CursorOwnerChanged {
                node: NodeId([6u8; 32]).to_hex(),
                monitor: 2,
                local: false,
            },
            Event::CursorLockChanged { locked: true },
            Event::MonitorsChanged { monitors: vec![] },
            Event::Notice {
                level: NoticeLevel::Warning,
                message: "clipboard is busy".into(),
            },
        ] {
            assert_eq!(
                round_trip_server(ServerMessage::Event {
                    event: event.clone()
                }),
                ServerMessage::Event { event }
            );
        }
    }

    #[test]
    fn requests_are_tagged_by_a_camel_case_kind_a_ui_can_switch_on() {
        // The UI codes against these strings; a rename is a breaking change.
        let text = serde_json::to_string(&RequestEnvelope {
            id: 1,
            request: Request::ListPeers,
        })
        .unwrap();
        assert_eq!(text, r#"{"id":1,"request":{"kind":"listPeers"}}"#);
    }

    #[test]
    fn responses_and_events_are_distinguishable_without_looking_at_the_id() {
        let response = serde_json::to_string(&ServerMessage::Response {
            id: 1,
            response: Response::Ok,
        })
        .unwrap();
        let event = serde_json::to_string(&ServerMessage::Event {
            event: Event::CursorLockChanged { locked: false },
        })
        .unwrap();
        assert!(response.starts_with(r#"{"kind":"response""#), "{response}");
        assert!(event.starts_with(r#"{"kind":"event""#), "{event}");
    }

    #[test]
    fn optional_fields_may_be_omitted_by_a_client() {
        // A hand-written or older client will not send every field; requiring
        // them would break it for no reason.
        let request: Request =
            serde_json::from_str(r#"{"kind":"confirmPairing","pin":"123456"}"#).unwrap();
        assert_eq!(
            request,
            Request::ConfirmPairing {
                node: None,
                pin: "123456".into()
            }
        );

        let warp: Request =
            serde_json::from_str(r#"{"kind":"warpCursor","node":"ab","monitor":0}"#).unwrap();
        assert_eq!(
            warp,
            Request::WarpCursor {
                node: "ab".into(),
                monitor: 0,
                x: 0.5,
                y: 0.5
            }
        );
    }

    #[test]
    fn an_unknown_request_kind_is_rejected_rather_than_guessed() {
        // Hostile or version-skewed input off the channel must not deserialise
        // into some neighbouring variant.
        assert!(serde_json::from_str::<Request>(r#"{"kind":"formatDisk"}"#).is_err());
        assert!(
            serde_json::from_str::<RequestEnvelope>(r#"{"request":{"kind":"status"}}"#).is_err()
        );
        assert!(serde_json::from_str::<Request>("null").is_err());
    }

    #[test]
    fn a_status_snapshot_survives_the_channel() {
        let snapshot = StatusSnapshot {
            node_id: NodeId([1u8; 32]).to_hex(),
            node_name: "desk".into(),
            agent_version: "0.1.0".into(),
            protocol: 1,
            port: 24800,
            uptime_secs: 91,
            platform: "windows".into(),
            display_server: "windows".into(),
            capabilities: 7,
            discovery: true,
            autostart: false,
            monitors: vec![MonitorSpec {
                id: 0,
                name: "DP-1".into(),
                x: 0,
                y: 0,
                w: 3840,
                h: 2160,
                scale: 1.5,
                primary: true,
            }],
            cursor: CursorSnapshot {
                owner: NodeId([1u8; 32]).to_hex(),
                owner_name: "desk".into(),
                monitor: Some(0),
                locked: false,
                local: true,
            },
            layout: LayoutSpec {
                revision: 3,
                placements: vec![PlacementSpec {
                    node: NodeId([1u8; 32]).to_hex(),
                    monitor: 0,
                    x: 0,
                    y: 0,
                    w: 3840,
                    h: 2160,
                    cursor_scale: 1.0,
                }],
            },
            peers: vec![PeerSnapshot {
                node: NodeId([2u8; 32]).to_hex(),
                name: "laptop".into(),
                advertised_name: "DESKTOP-8K3QJ1".into(),
                status: PeerStatusSpec::Failed {
                    error: "timed out".into(),
                },
                paired: true,
                blocked: false,
                enabled: true,
                addresses: vec!["10.0.0.5:24800".into()],
                platform: Some("linux".into()),
                agent_version: Some("0.1.0".into()),
                rtt_ms: Some(3),
                monitors: vec![],
                capabilities: (wx_proto::Capabilities::CAPTURE_INPUT
                    | wx_proto::Capabilities::INJECT_INPUT)
                    .0,
            }],
        };
        let msg = ServerMessage::Response {
            id: 1,
            response: Response::Status {
                status: Box::new(snapshot.clone()),
            },
        };
        match round_trip_server(msg) {
            ServerMessage::Response {
                response: Response::Status { status },
                ..
            } => assert_eq!(*status, snapshot),
            other => panic!("wrong message back: {other:?}"),
        }
    }

    #[test]
    fn the_status_snapshot_reports_what_the_mesh_was_told() {
        // A laptop that boots with the lid shut advertises no displays and gains
        // the bit when a monitor is plugged in. The platform's own info still says
        // what it said at construction, so a snapshot built from that would show a
        // set no peer was ever given — and the local row is the one a user reads
        // first when a feature is not happening between two machines.
        let at_boot = wx_platform::PlatformInfo {
            platform: wx_proto::Platform::Linux,
            display_server: wx_proto::DisplayServer::Wayland,
            capabilities: Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT,
        };
        let advertised = at_boot.capabilities | Capabilities::HAS_DISPLAYS;
        let state = AgentState::new(
            NodeId([1u8; 32]),
            "laptop".into(),
            std::time::Instant::now(),
        );
        let snapshot = status_snapshot(
            &state,
            &Config::default(),
            &Layout::default(),
            &at_boot,
            advertised,
            "0.1.0",
            Duration::from_secs(7),
        );
        assert_eq!(snapshot.capabilities, advertised.0);
        assert_ne!(
            snapshot.capabilities, at_boot.capabilities.0,
            "the snapshot reported the construction-time set, not the advertised one"
        );
        // The rest of the platform row still comes from the platform.
        assert_eq!(snapshot.platform, "linux");
        assert_eq!(snapshot.display_server, "wayland");
    }

    #[test]
    fn a_monitor_round_trips_through_its_json_shape() {
        let m = Monitor {
            id: MonitorId(2),
            name: "Built-in Retina Display".into(),
            local_bounds: Rect::new(-1440, 200, 1440, 900),
            scale: 2.0,
            primary: false,
        };
        assert_eq!(MonitorSpec::of(&m).to_monitor(), m);
    }

    #[test]
    fn peer_status_maps_both_ways() {
        for status in [
            ConnStatus::Offline,
            ConnStatus::Discovered,
            ConnStatus::Connecting,
            ConnStatus::Pairing,
            ConnStatus::Connected,
            ConnStatus::Failed("boom".into()),
        ] {
            let spec = PeerStatusSpec::of(&status);
            let text = serde_json::to_string(&spec).unwrap();
            assert_eq!(serde_json::from_str::<PeerStatusSpec>(&text).unwrap(), spec);
        }
    }

    #[test]
    fn a_bad_hex_node_id_is_a_bad_request_not_a_panic() {
        let err = parse_node("zzzz").unwrap_err();
        assert!(matches!(
            err,
            Response::Error {
                code: ErrorCode::BadRequest,
                ..
            }
        ));
        assert!(parse_node(&NodeId([9u8; 32]).to_hex()).is_ok());
    }

    #[test]
    fn the_endpoint_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("wx-agent-ipc-{}", std::process::id()));
        let file = EndpointFile {
            port: 51234,
            token: generate_token().unwrap(),
            pid: std::process::id(),
            agent_version: "0.1.0".into(),
        };
        file.write(&dir).unwrap();
        assert_eq!(EndpointFile::read(&dir).unwrap(), file);
        EndpointFile::remove(&dir);
        assert!(matches!(
            EndpointFile::read(&dir),
            Err(IpcError::NotRunning(_))
        ));
    }

    #[test]
    fn tokens_are_long_and_never_repeat() {
        // A short or predictable token would let any process on the machine drive
        // the agent, which includes injecting keystrokes.
        let a = generate_token().unwrap();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert_ne!(a, generate_token().unwrap());
    }

    #[tokio::test]
    async fn a_client_must_authenticate_before_anything_else_works() {
        let server = IpcServer::bind("s3cret".into()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<IpcCommand>();
        tokio::spawn(async move {
            // Answer whatever the engine would be asked.
            while let Some(cmd) = rx.recv().await {
                let _ = cmd.reply.send(Response::Ok);
            }
        });
        tokio::spawn(server.serve(tx));

        let stream = TcpStream::connect(addr).await.unwrap();
        let (read_half, mut write) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // Status before hello: refused.
        write
            .write_all(b"{\"id\":1,\"request\":{\"kind\":\"status\"}}\n")
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let msg: ServerMessage = serde_json::from_str(line.trim()).unwrap();
        match msg {
            ServerMessage::Response {
                response: Response::Error { code, .. },
                ..
            } => assert_eq!(code, ErrorCode::Unauthorized),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // And the connection is closed, so a client cannot keep guessing.
        line.clear();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused_and_the_connection_dropped() {
        let server = IpcServer::bind("correct-token".into()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<IpcCommand>();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let _ = cmd.reply.send(Response::Ok);
            }
        });
        tokio::spawn(server.serve(tx));

        let stream = TcpStream::connect(addr).await.unwrap();
        let (read_half, mut write) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        write
            .write_all(b"{\"id\":1,\"request\":{\"kind\":\"hello\",\"token\":\"wrong\"}}\n")
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.contains("unauthorized"), "{line}");
        line.clear();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_request_reaches_the_engine_and_its_answer_comes_back() {
        let token = generate_token().unwrap();
        let server = IpcServer::bind(token.clone()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let events = server.events();
        let (tx, mut rx) = mpsc::unbounded_channel::<IpcCommand>();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let response = match cmd.request {
                    Request::Hello { .. } => Response::Hello {
                        node_id: NodeId([1u8; 32]).to_hex(),
                        node_name: "desk".into(),
                        agent_version: "0.1.0".into(),
                        protocol: 1,
                    },
                    Request::SetCursorLock { locked } => Response::CursorLock {
                        locked: locked.unwrap_or(true),
                    },
                    _ => Response::Ok,
                };
                let _ = cmd.reply.send(response);
            }
        });
        tokio::spawn(server.serve(tx));

        // Write the endpoint file so the client's own discovery path is exercised.
        let dir = std::env::temp_dir().join(format!(
            "wx-agent-ipc-live-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        EndpointFile {
            port: addr.port(),
            token,
            pid: std::process::id(),
            agent_version: "0.1.0".into(),
        }
        .write(&dir)
        .unwrap();

        let mut client = IpcClient::connect(&dir).await.unwrap();
        assert_eq!(
            client
                .call(Request::SetCursorLock {
                    locked: Some(false)
                })
                .await
                .unwrap(),
            Response::CursorLock { locked: false }
        );

        // Events only arrive after subscribing.
        assert_eq!(client.call(Request::Subscribe).await.unwrap(), Response::Ok);
        events
            .send(Event::CursorLockChanged { locked: true })
            .unwrap();
        assert_eq!(
            client.next_event().await.unwrap(),
            Event::CursorLockChanged { locked: true }
        );
        EndpointFile::remove(&dir);
    }

    #[tokio::test]
    async fn an_oversized_line_closes_the_connection_instead_of_being_buffered() {
        let server = IpcServer::bind("t".into()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<IpcCommand>();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let _ = cmd.reply.send(Response::Ok);
            }
        });
        tokio::spawn(server.serve(tx));

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let flood = vec![b'x'; MAX_LINE_LEN + 4096];
        // The write may fail partway once the agent hangs up, which is the point.
        let _ = stream.write_all(&flood).await;
        let _ = stream.flush().await;
        let mut sink = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut sink).await;
        // Nothing useful was ever answered, and the agent is still alive to serve
        // the next client.
        let second = TcpStream::connect(addr).await;
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn malformed_json_is_answered_and_the_connection_stays_open() {
        let token = "tok".to_string();
        let server = IpcServer::bind(token.clone()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<IpcCommand>();
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let _ = cmd.reply.send(Response::Ok);
            }
        });
        tokio::spawn(server.serve(tx));

        let stream = TcpStream::connect(addr).await.unwrap();
        let (read_half, mut write) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        write.write_all(b"{not json}\n").await.unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.contains("badRequest"), "{line}");

        // Still usable: a UI typo must not require a reconnect.
        write
            .write_all(
                format!("{{\"id\":9,\"request\":{{\"kind\":\"hello\",\"token\":\"{token}\"}}}}\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.contains("\"id\":9"), "{line}");
    }
}
