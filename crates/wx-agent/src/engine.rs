//! The run loop: platform capture in, network and injection out.
//!
//! # Shape
//!
//! ```text
//!   InputCapture ─┐                              ┌─> per-peer send task ─> QUIC
//!   QUIC session ─┤                              │
//!   mDNS browser  ├─> one Wake queue ─> Engine ──┼─> InputInjector
//!   IPC clients  ─┤                    (one task)│
//!   2s ticker    ─┘                              └─> IPC event broadcast
//! ```
//!
//! Everything funnels into a single unbounded queue consumed by a single task, so
//! there is no `select!` in the hot path and no lock around the router. That is
//! worth more than it looks: `select!` drops the losing branch's future, and a
//! half-completed stream read or a half-applied handoff is a class of bug that
//! only appears under load and is close to impossible to reproduce. With one
//! queue, the ordering between "the user pressed a key" and "the UI changed the
//! layout" is defined by arrival order and nothing else.
//!
//! Sends are the exception: they go to a per-peer task over a channel rather than
//! being awaited inline. A peer whose QUIC stream has filled its congestion window
//! would otherwise stall the loop, and with it every other machine's cursor. The
//! per-peer channel also preserves the ordering
//! [`wx_core::RouteAction`](wx_core::RouteAction) requires — releases, then yield,
//! then handoff — which sending each message on its own task would not.
//!
//! # The two things most likely to be got wrong
//!
//! * **Not injecting what the OS already delivered.** While the cursor is on this
//!   machine, local input is not suppressed, so the OS has already applied it.
//!   Injecting the router's `Local` actions as well would double every keystroke.
//!   See [`should_inject_locally`].
//! * **Losing the cursor when a peer dies.** A cursor on an unreachable machine
//!   cannot be brought back by moving the mouse, because the mouse only moves the
//!   cursor that is already stranded. See [`reclaim_cursor`].

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, OwnedSemaphorePermit, Semaphore};
use wx_core::{GlobalLayout, InputRouter, RouteAction, VirtualCursor};
use wx_net::{
    Advertiser, Browser, DiscoveryEvent, Endpoint, Established, Events, Identity, PairingSession,
    Pin, Session, SessionEvent, SessionSetup, TrustStore,
};
use wx_platform::{CapturedEvent, PlatformBackend, PlatformError};
use wx_proto::{
    Capabilities, ControlMsg, GlobalMonitorId, InputEvent, InputFrame, KeyAction, KeyPayload,
    Layout, Monitor, MonitorId, NodeId, NodeInfo, NormPos, Point, PointerEvent, RejectReason,
    Reliability, SequenceGate,
};

use crate::config::{Config, HotkeyAction};
use crate::ipc::{self, ErrorCode, Event, IpcCommand, IpcServer, Request, Response};
use crate::state::{AgentState, ConnStatus};
use crate::{autolayout, autostart, state};

/// Version reported to peers and to the UI.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often the housekeeping tick runs.
///
/// Drives reconnection, display hotplug detection, and round-trip-time refresh.
/// Fast enough that a machine coming back is picked up while the user is still
/// looking at the screen, slow enough to be free.
const TICK: Duration = Duration::from_secs(2);

/// How often the machine holding the cursor is probed, while it is not this one.
///
/// Separate from [`TICK`] and much faster, because of what it guards: while a peer
/// owns the cursor, local input is suppressed, so the user's keyboard and mouse do
/// nothing at all on this machine. Every millisecond spent not noticing that the
/// peer has died is a millisecond the user's input is silently discarded.
const CURSOR_PROBE: Duration = Duration::from_millis(500);

/// How long the machine holding the cursor may go silent before it is written off.
///
/// This is not belt and braces for QUIC's own liveness detection — it is the
/// primary mechanism, because QUIC's is far too slow for this particular failure.
/// A peer that loses power sends no close frame, so the connection is only declared
/// dead at the idle timeout, twenty seconds later. Twenty seconds of a dead
/// keyboard and mouse is indistinguishable from the machine having crashed, and it
/// is the single worst thing this software can do to someone.
///
/// Two probe intervals plus a margin: long enough that a busy peer or a brief
/// wireless stall does not cost the user their cursor, short enough that a dead
/// machine is noticed before they reach for the power button.
const CURSOR_LIVENESS: Duration = Duration::from_millis(2_000);

/// How far the real pointer must be from where the virtual cursor believes it is
/// before [`Engine::resync_local_cursor`] believes the pointer instead.
///
/// A fraction of a monitor, so it is resolution-independent — normalised
/// coordinates are what the rest of the layout speaks. A tenth is well beyond any
/// single motion event on an ordinary mouse and well below the width of a screen,
/// which is the gap this exists to close: on Wayland the pointer can be anywhere
/// by the time capture activates.
///
/// The cost of being wrong in each direction is not symmetric, and that is why
/// this is a threshold rather than a correction on every event. Too high and the
/// cursor takes a moment to catch up. Too low and the virtual cursor is welded to
/// the physical one, which cannot leave its own screen — so the cursor could never
/// cross onto a peer at all.
const RESYNC_THRESHOLD: f64 = 0.1;

/// Distance between two normalised positions, as a fraction of a monitor.
fn norm_distance(a: NormPos, b: NormPos) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Believe the capture backend about where the real pointer is, when it says
/// something no delta could explain.
///
/// This is what [`CapturedEvent::PointerMotion`]'s `position` is for. The virtual
/// cursor is integrated from deltas, so anything that moves the real pointer
/// without producing one — a focus-follows-mouse warp, a game recentring, or a
/// backend that only sees input some of the time — leaves the two out of step, and
/// the drift never corrects itself.
///
/// It matters most on Wayland, where the portal delivers input only while it has
/// capture *activated*: the user moves their pointer freely in between and the
/// router sees none of it, so without this the first crossing after any ordinary
/// mouse use would need a whole screen's width of travel.
///
/// Deliberately only on a discontinuity, and only while the cursor is on this
/// machine. Correcting on every event would weld the virtual cursor to the
/// physical one, and a physical pointer cannot leave the edge of its own screen —
/// so the motion that is *supposed* to carry the cursor onto a peer would be
/// undone before it ever crossed.
///
/// Returns whether anything was corrected. Any actions the warp produces are
/// discarded on purpose: this fixes what the router *believes*, and moving
/// anything is the job of the events that follow.
fn resync_cursor(
    router: &mut InputRouter,
    local: NodeId,
    monitors: &[Monitor],
    position: Point,
) -> bool {
    if !router.owns_cursor() {
        return false;
    }
    let Some(monitor) = monitors.iter().find(|m| m.local_bounds.contains(position)) else {
        // A position on no monitor this machine has. Nothing to resynchronise
        // against, and guessing would be worse than the drift.
        return false;
    };
    let target = GlobalMonitorId::new(local, monitor.id);
    let believed = router.cursor().norm_position(router.layout());
    let landing = monitor.local_bounds.normalize(position);
    if target == router.cursor().monitor() && norm_distance(believed, landing) < RESYNC_THRESHOLD {
        return false;
    }
    match router.warp(target, landing) {
        Ok(_) => {
            tracing::trace!(
                x = position.x,
                y = position.y,
                "the real pointer moved without us seeing it; resynchronising"
            );
            true
        }
        Err(e) => {
            tracing::debug!(error = %e, "could not resynchronise the cursor");
            false
        }
    }
}

/// How long a pairing exchange may sit waiting for someone to type a PIN.
///
/// Bounded because a pending pairing holds a session from an *untrusted* peer
/// open; without a timeout, anything on the LAN could keep one alive forever.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);

/// How many messages may be waiting for one peer before something has to give.
///
/// Bounded, and the reason is the same one [`wx_net`] gives for its own inbound
/// queue: an unbounded queue turns a stalled consumer into unbounded memory
/// growth. A peer can keep a QUIC connection perfectly alive while its flow
/// control window stays shut — an agent wedged on a slow injector, a machine
/// suspending, a saturated link — and the writer task then blocks on `write_all`
/// forever while everything the router produces piles up behind it. Scroll events
/// and relative motion are reliable, so a free-spinning wheel enqueues at wheel
/// rate, and a clipboard payload can be tens of megabytes on its own.
///
/// Deep enough to absorb a burst of scrolls plus a large payload without shedding
/// anything, shallow enough that the backlog is a fraction of a second of input
/// rather than a minute of stale keystrokes replayed at the user.
const OUTBOUND_QUEUE_DEPTH: usize = 256;

/// How many events arrived from one peer may wait for the engine at once.
///
/// The transport caps its own queue for exactly this reason, but the pump used to
/// drain that bounded queue into the engine's unbounded one, which cancels the
/// cap: a peer writing reliable frames faster than the single engine loop can
/// inject them made memory grow without limit, and an injector stalled by the
/// secure desktop turned a hard cap into an unbounded backlog that was later
/// replayed as minutes of stale input. One permit per queued event makes the pump
/// stop reading instead, which pushes back through QUIC's flow control to the
/// sender.
const INBOUND_QUEUE_DEPTH: usize = 256;

/// Hands out an identity for each connection, distinct from the peer's node id.
///
/// A `NodeId` does not identify a *session*, and treating it as though it does is
/// how a healthy connection gets torn down by the death of a different one: two
/// connections to the same machine exist whenever both ends dial at once, and one
/// replaces another every time pairing restarts (which closes the session and
/// immediately redials). The generation lets a teardown notification be matched to
/// the connection it came from.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn next_session_generation() -> u64 {
    NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
}

/// Whether a session pump's teardown notification still describes the connection
/// this machine is using for that peer.
///
/// The failure it prevents, in the deterministic case: `begin_pairing` closes the
/// existing session and redials at once, so the old pump's notification arrives
/// *after* the new session has been installed. Keyed only by node id, it closed
/// the new session, and pairing appeared to drop the peer the instant it started.
///
/// A notification for a peer with no session at all is honoured, because that is
/// the ordinary disconnect: the teardown still has to mark the peer down and
/// rescue the cursor.
fn teardown_is_current(current: Option<u64>, reported: u64) -> bool {
    match current {
        Some(generation) => generation == reported,
        None => true,
    }
}

/// Everything the loop consumes, from every source.
enum Wake {
    /// Local keyboard or mouse activity.
    Captured(CapturedEvent),
    /// A session finished its handshake.
    Session(Box<NewSession>),
    /// Something arrived from a peer.
    Peer {
        node: NodeId,
        event: SessionEvent,
        /// Held until the engine has finished with this event, so the number of
        /// queued events from one peer cannot exceed [`INBOUND_QUEUE_DEPTH`].
        permit: OwnedSemaphorePermit,
    },
    /// A session ended, cleanly or otherwise.
    PeerGone {
        node: NodeId,
        /// Which connection died. See [`teardown_is_current`].
        generation: u64,
        reason: Option<String>,
    },
    /// A dial never got as far as a session.
    DialFailed {
        node: NodeId,
        error: String,
    },
    Discovery(DiscoveryEvent),
    Ipc(IpcCommand),
    Tick,
    /// Check that the machine holding the cursor is still answering.
    Probe,
    /// Ctrl-C, a service stop, or an IPC shutdown request.
    Shutdown,
}

struct NewSession {
    session: Session,
    established: Box<Established>,
    /// Whether this side dialled. Decides which end of the pairing exchange this
    /// machine plays, and so whether it shows a PIN or asks for one.
    initiated_locally: bool,
    /// Identity of this connection, matched against later teardown reports.
    generation: u64,
}

/// Something to write to a peer, in order.
enum Outbound {
    Input(InputFrame),
    Control(ControlMsg),
}

impl Outbound {
    /// Whether losing this message is recoverable.
    ///
    /// Only absolute pointer positions are: a later one supersedes it, so the
    /// cursor self-corrects. Everything else latches state — key and button
    /// transitions, relative motion, handoff, clipboard — and dropping one is
    /// a stuck modifier or a lost keystroke.
    fn is_sheddable(&self) -> bool {
        match self {
            Outbound::Input(frame) => frame.event.reliability() == Reliability::BestEffort,
            Outbound::Control(_) => false,
        }
    }
}

/// What happened to a message handed to a peer's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Queued {
    Sent,
    /// A superseded pointer position was dropped because the queue was full.
    ShedMotion,
    /// The queue is full of messages that cannot be dropped. The session has to
    /// go: closing beats growing, and once anything that latches state is lost the
    /// router's ordering guarantee is void anyway, so there is nothing left to
    /// preserve by waiting.
    Unresponsive,
    /// The writer task has already exited, so the session is dead and the pump is
    /// about to say so. Nothing to do here.
    AlreadyClosed,
}

/// Offer a message to a peer's bounded queue without ever blocking the loop.
///
/// Blocking here would be the original sin this whole design avoids: one peer
/// whose window is shut would stall every other machine's cursor.
fn enqueue(out: &mpsc::Sender<Outbound>, msg: Outbound) -> Queued {
    match out.try_send(msg) {
        Ok(()) => Queued::Sent,
        Err(mpsc::error::TrySendError::Full(msg)) => {
            if msg.is_sheddable() {
                Queued::ShedMotion
            } else {
                Queued::Unresponsive
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Queued::AlreadyClosed,
    }
}

/// A live peer connection and the queue feeding it.
struct PeerLink {
    session: Session,
    out: mpsc::Sender<Outbound>,
    /// Which connection this is. See [`teardown_is_current`].
    generation: u64,
}

/// Which peer, if any, is currently injecting into this machine.
///
/// The router cannot answer this: when a peer takes control of this machine the
/// cursor really is here, so `router.owner()` is the local node and nothing
/// records who is driving. Without that record, a driving peer that loses power
/// leaves every key, modifier and mouse button it pushed down held indefinitely —
/// there is no `Goodbye`, no `YieldControl` and no `ReleaseControl`, so nothing
/// ever tells the injector to let go.
#[derive(Debug, Default)]
struct DrivenBy(Option<NodeId>);

impl DrivenBy {
    /// Record that `node` is now driving, reporting any *different* peer it
    /// displaced without that peer ever having let go.
    ///
    /// Two peers really can both believe they own the cursor: each agent's router
    /// owns its own, so nothing stops B walking onto this machine while A still
    /// thinks it is driving it. Overwriting the record silently orphaned everything
    /// A had pushed down, because every path that releases a driver's held input
    /// — the disconnect in `on_peer_gone`, the liveness probe, the tick fallback —
    /// tests against the *recorded* peer and so tested against B. A's Ctrl stayed
    /// held and its left button stayed down (which means dragging) until an
    /// unrelated `YieldControl` arrived or the agent exited.
    #[must_use = "a displaced peer's keys and buttons are still held down"]
    fn took_control(&mut self, node: NodeId) -> Option<NodeId> {
        let displaced = self.0.filter(|previous| *previous != node);
        self.0 = Some(node);
        displaced
    }

    /// Record that `node` is no longer driving. Returns whether it was, and so
    /// whether anything it left held has to be released now.
    fn let_go(&mut self, node: NodeId) -> bool {
        if self.0 == Some(node) {
            self.0 = None;
            return true;
        }
        false
    }

    fn peer(&self) -> Option<NodeId> {
        self.0
    }
}

/// PINs this machine generated, showed the user, and has yet to bind to a session.
///
/// A plain map was not enough, and the reason is an ordering nobody would guess.
/// `begin_pairing` shows a code, then closes any session that already exists for
/// that peer and redials at once — the cross-initiation case is the *normal* one,
/// because the other machine dials first whenever it is in pairing mode. That close
/// is local, so its `Wake::PeerGone` reaches the single FIFO wake queue microseconds
/// later, while the redial needs a QUIC handshake plus an application handshake. The
/// teardown therefore always runs first, and when it cleared the code the redialled
/// session found none, logged "no pairing code was generated" and abandoned the
/// pairing — after the UI had already put the code on screen for the user to read
/// out. The session generation cannot rescue this: it only suppresses a teardown once
/// a *newer* link is installed, and at that moment there is none.
///
/// So a code is held against the dial it was generated for, and only a teardown with
/// no such dial outstanding may discard it.
#[derive(Default)]
struct OfferedPins {
    pins: HashMap<NodeId, Pin>,
    /// Peers whose code belongs to a connection that has not been made yet.
    awaiting_dial: HashSet<NodeId>,
}

impl OfferedPins {
    /// Record a code for a connection that is about to be dialled.
    fn offer(&mut self, node: NodeId, pin: Pin) {
        self.pins.insert(node, pin);
        self.awaiting_dial.insert(node);
    }

    /// Take the code for a session that has just come up.
    fn claim(&mut self, node: NodeId) -> Option<Pin> {
        self.awaiting_dial.remove(&node);
        self.pins.remove(&node)
    }

    /// Forget the code: the pairing is over, cancelled, or refused.
    fn discard(&mut self, node: NodeId) {
        self.awaiting_dial.remove(&node);
        self.pins.remove(&node);
    }

    /// A session for `node` ended. Keeps a code whose own connection has not
    /// happened yet; see the type's documentation for why that matters.
    fn on_session_ended(&mut self, node: NodeId) {
        if !self.awaiting_dial.contains(&node) {
            self.pins.remove(&node);
        }
    }
}

/// A pairing exchange in progress.
struct PendingPairing {
    node: NodeId,
    name: String,
    initiated_locally: bool,
    established: Box<Established>,
    /// Absent on the side that is waiting for the user to type the PIN.
    pairing: Option<PairingSession>,
    started: Instant,
}

/// What the agent needs in order to start.
pub struct EngineOptions {
    /// Directory holding the identity key, trust store, config, and IPC endpoint
    /// file. Overridable so that tests and a second instance can be isolated.
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
    pub config: Config,
}

/// Where a batch of route actions came from.
///
/// The distinction decides whether locally targeted events are injected. Input
/// this machine captured has already been delivered by the OS; input a peer sent,
/// or a warp the UI asked for, has not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    LocalCapture,
    Remote,
}

/// Whether an event addressed to this machine has to be injected.
///
/// The failure this prevents: while the cursor is here, local input is not
/// suppressed, so the OS already gave the keystroke to the focused window.
/// Injecting the router's matching `Local` action as well types everything twice.
///
/// Releases are the exception and are always injected. A release that is skipped
/// leaves a modifier or a mouse button latched, and the user cannot clear it
/// without physically pressing that key on the affected machine — so the cost of
/// a redundant release (nothing) is not comparable to the cost of a missing one.
pub fn should_inject_locally(event: &InputEvent, suppressed: bool, from_remote: bool) -> bool {
    if from_remote || suppressed {
        return true;
    }
    match event {
        InputEvent::ReleaseControl => true,
        InputEvent::Key(ev) => ev.action == KeyAction::Release,
        InputEvent::Pointer(PointerEvent::Button { pressed, .. }) => !pressed,
        _ => false,
    }
}

/// Whether an untrusted session may act on this message.
///
/// A session admitted for pairing has proved possession of its key but nothing
/// else: no human has approved it. It must therefore be unable to inject input,
/// rewrite the layout, or read the clipboard. Allowing exactly the pairing
/// exchange plus liveness is the whole permitted surface.
pub fn is_permitted_while_unpaired(msg: &ControlMsg) -> bool {
    matches!(
        msg,
        ControlMsg::PairRequest { .. }
            | ControlMsg::PairConfirm { .. }
            | ControlMsg::PairResult { .. }
            | ControlMsg::Reject { .. }
            | ControlMsg::Goodbye { .. }
            | ControlMsg::Ping { .. }
            | ControlMsg::Pong { .. }
    )
}

/// Bring the cursor back from a machine that can no longer be reached.
///
/// This is the recovery the whole product depends on being right: the cursor is a
/// shared resource, and moving a mouse can only move the cursor that already
/// exists. Once it is on an unreachable machine, no amount of mouse movement can
/// retrieve it — every delta is routed to a peer that is not listening — so the
/// only way back is for this machine to notice and warp it.
///
/// Returns the actions the caller must carry out, which include telling the dead
/// peer to let go: if it comes back it must not still believe it owns the cursor.
pub fn reclaim_cursor(
    router: &mut InputRouter,
    local: NodeId,
    is_reachable: impl Fn(NodeId) -> bool,
) -> Vec<RouteAction> {
    let on = router.cursor().monitor();
    let Some(target) = state::reclaim_target(router.layout(), on, local, is_reachable) else {
        return Vec::new();
    };
    // Centre rather than the seam: the cursor is not crossing an edge, it is
    // being rescued, and the middle of the screen is where the user will look.
    router
        .warp(target, NormPos::new(0.5, 0.5))
        .unwrap_or_default()
}

/// Start the liveness clock for a machine this one has just begun to depend on.
///
/// The bug this exists for, found by warping the cursor onto an idle peer: a
/// healthy peer that is not driving anything sends no application traffic for
/// minutes at a time, because there is nothing to send. So by the time the cursor
/// crosses onto it, its last-heard timestamp is already far older than
/// [`CURSOR_LIVENESS`], and the very first probe declares a perfectly alive machine
/// dead and yanks the cursor straight back. The cursor would appear to bounce off
/// the seam.
///
/// Resetting the baseline at the moment control changes hands is the fix: the
/// deadline measures "silent since we started needing an answer", not "silent
/// ever".
fn begin_liveness_window(
    last_heard: &mut HashMap<NodeId, Instant>,
    owner: NodeId,
    local: NodeId,
    now: Instant,
) {
    if owner != local {
        last_heard.insert(owner, now);
    }
}

/// Whether a peer's monitor list contains a screen the layout has no place for.
///
/// Only then is it right to re-run the automatic placement: a peer re-announcing
/// the same screens must not have the user's arrangement thrown away, but a
/// newly plugged-in display that appears nowhere is unreachable until something
/// places it.
pub fn needs_placement(layout: &GlobalLayout, node: NodeId, monitors: &[Monitor]) -> bool {
    monitors
        .iter()
        .filter(|m| !m.local_bounds.is_empty())
        .any(|m| layout.rect(GlobalMonitorId::new(node, m.id)).is_none())
}

/// Whether an incoming layout should replace the one in use.
///
/// Highest revision wins, as the protocol says. Ties are the interesting case,
/// and getting them wrong is not cosmetic — it was found by running two agents
/// against each other. Both machines bootstrap their own layout, both reach
/// revision 2 with two placements after pairing, and both place *themselves* on
/// the left. With no tie-break each keeps its own, and the desk behaves like a
/// ring: moving right from either machine arrives at the other, and moving left
/// arrives nowhere, so the cursor can never be walked back the way it came.
///
/// So the ordering has to be total, and it has to be one both sides compute the
/// same answer from:
///
/// 1. Higher revision wins — a deliberate edit beats a guess.
/// 2. Then more placements win — a layout that knows about more monitors has seen
///    more of the mesh.
/// 3. Then the canonically smaller layout wins. Arbitrary, but *symmetric*: both
///    machines compare the same two values and pick the same winner, which is the
///    only property that matters. Convergence then terminates, because once both
///    hold the same layout neither accepts it again.
pub fn accept_layout(current: &Layout, incoming: &Layout) -> bool {
    match incoming.revision.cmp(&current.revision) {
        std::cmp::Ordering::Greater => return true,
        std::cmp::Ordering::Less => return false,
        std::cmp::Ordering::Equal => {}
    }
    match incoming.placements.len().cmp(&current.placements.len()) {
        std::cmp::Ordering::Greater => return true,
        std::cmp::Ordering::Less => return false,
        std::cmp::Ordering::Equal => {}
    }
    layout_key(incoming) < layout_key(current)
}

/// A layout reduced to a value two machines can order identically.
///
/// Sorted, because neither the wire order nor the layout's insertion order is
/// stable, and an ordering that depended on either would have the two ends
/// disagree about which layout was smaller — reintroducing exactly the
/// oscillation the tie-break exists to stop.
fn layout_key(layout: &Layout) -> Vec<(String, u32, i32, i32, u32, u32)> {
    let mut key: Vec<(String, u32, i32, i32, u32, u32)> = layout
        .placements
        .iter()
        .map(|p| {
            (
                p.monitor.node.to_hex(),
                p.monitor.monitor.0,
                p.global_bounds.x,
                p.global_bounds.y,
                p.global_bounds.w,
                p.global_bounds.h,
            )
        })
        .collect();
    key.sort();
    key
}

/// The daemon.
pub struct Engine {
    local: NodeId,
    identity: Arc<Identity>,
    trust: Arc<Mutex<TrustStore>>,
    config: Config,
    config_path: PathBuf,
    config_dir: PathBuf,
    platform: PlatformBackend,
    state: AgentState,
    router: InputRouter,
    endpoint: Arc<Endpoint>,
    local_info: Arc<Mutex<NodeInfo>>,
    pairing_open: Arc<AtomicBool>,
    sessions: HashMap<NodeId, PeerLink>,
    gates: HashMap<NodeId, SequenceGate>,
    /// When each peer was last heard from, at all.
    ///
    /// The liveness signal behind [`CURSOR_LIVENESS`]. Updated on any inbound
    /// traffic rather than only on a `Pong`, because a peer that is busy sending
    /// input is self-evidently alive and should not have to answer a probe to
    /// prove it.
    last_heard: HashMap<NodeId, Instant>,
    pending: HashMap<NodeId, PendingPairing>,
    /// PINs generated for pairings this side started, held until the session
    /// exists to bind them to. See [`OfferedPins`].
    offered_pins: OfferedPins,
    dialing: HashSet<NodeId>,
    events: broadcast::Sender<Event>,
    wake: mpsc::UnboundedSender<Wake>,
    /// Last value pushed to the capture backend, so it is only changed on a real
    /// transition — flipping the suppression flag loses events.
    suppressed: bool,
    /// Key payload whose release must be swallowed because its press was consumed
    /// as a hotkey. Without it the peer sees a release for a key it never saw
    /// pressed.
    swallow_release: Option<KeyPayload>,
    /// The peer currently driving this machine, if any. See [`DrivenBy`].
    driven_by: DrivenBy,
    last_owner: NodeId,
    /// Host firewall warning, decided once at startup and then reported for the
    /// life of the run. Held rather than recomputed per status request because
    /// changing it means the user ran `ufw` since, at which point they know.
    firewall: Option<String>,
    shutting_down: bool,
}

/// Start the agent and run until it is told to stop.
pub async fn run(opts: EngineOptions) -> anyhow::Result<()> {
    let (wake_tx, mut wake_rx) = mpsc::unbounded_channel();
    let mut engine = Engine::start(opts, wake_tx.clone()).await?;

    // Signals get their own task rather than a branch in the loop, so that a
    // wedged handler cannot make Ctrl-C unresponsive.
    {
        let wake = wake_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = wake.send(Wake::Shutdown);
            }
        });
    }
    // SIGTERM as well as SIGINT, because a supervisor sends the former and a
    // terminal the latter. Without this, `systemctl --user stop winxtend` — and
    // every logout, which stops the unit via `PartOf=graphical-session.target` —
    // kills the process outright: peers are never told goodbye, held modifiers
    // are never released on the machines they are held down on, and the endpoint
    // file is left behind so the next `--status` tries to reach a port nothing is
    // listening on. See `shutdown`, which is the whole reason a signal is turned
    // into an ordinary wake rather than an exit.
    #[cfg(unix)]
    {
        let wake = wake_tx.clone();
        tokio::spawn(async move {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(term) => term,
                    // Nothing to be done about it, and it must not stop the
                    // agent starting: Ctrl-C still works.
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot listen for SIGTERM");
                        return;
                    }
                };
            if term.recv().await.is_some() {
                let _ = wake.send(Wake::Shutdown);
            }
        });
    }
    {
        let wake = wake_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CURSOR_PROBE);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if wake.send(Wake::Probe).is_err() {
                    return;
                }
            }
        });
    }
    {
        let wake = wake_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(TICK);
            // Skip missed ticks rather than firing a burst of them after the
            // machine wakes from sleep, which would dial every peer at once.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if wake.send(Wake::Tick).is_err() {
                    return;
                }
            }
        });
    }

    while let Some(wake) = wake_rx.recv().await {
        engine.handle(wake).await;
        if engine.shutting_down {
            break;
        }
    }
    engine.shutdown().await;
    Ok(())
}

impl Engine {
    async fn start(opts: EngineOptions, wake: mpsc::UnboundedSender<Wake>) -> anyhow::Result<Self> {
        let EngineOptions {
            config_dir,
            config_path,
            config,
        } = opts;

        let identity = Arc::new(Identity::load_or_create(&config_dir)?);
        let local = identity.node_id();
        let trust = TrustStore::load(&config_dir)?;
        tracing::info!(
            node = %local,
            name = %config.node.name,
            paired = trust.len(),
            "starting WinXtend agent {AGENT_VERSION}"
        );

        // `current_platform_in` rather than `current_platform`: this is the daemon,
        // so it is the one caller allowed to acquire OS permissions — on Wayland,
        // the portal consent dialog — and the config directory is where the backend
        // keeps what it needs between runs.
        let platform = wx_platform::current_platform_in(&config_dir)?;
        let monitors = platform.displays.monitors().unwrap_or_else(|e| {
            // Not fatal. A node with no readable displays still forwards input to
            // its peers, and taking the whole machine out of the mesh over a
            // transient enumeration failure would be worse.
            tracing::warn!(error = %e, "could not enumerate displays");
            Vec::new()
        });

        let now = Instant::now();
        let mut state = AgentState::new(local, config.node.name.clone(), now);
        state.set_local_monitors(monitors.clone());

        // A saved layout is authoritative, but it may predate a monitor being
        // plugged in, so the automatic pass fills any gaps rather than replacing
        // it.
        let mut layout = match &config.layout {
            Some(saved) => GlobalLayout::from_layout(&saved.to_layout()),
            None => autolayout::bootstrap(local, &monitors),
        };
        if needs_placement(&layout, local, &monitors) {
            autolayout::apply(&mut layout, local, &monitors);
        }

        let cursor = VirtualCursor::anywhere(&layout).unwrap_or_else(|| {
            // No usable monitor anywhere: a headless node with no peers yet. The
            // cursor has to exist for the router to be constructible, so it is
            // parked on a placeholder that the first layout update replaces.
            let mut placeholder = GlobalLayout::new();
            placeholder.insert(wx_proto::Placement {
                monitor: GlobalMonitorId::new(local, MonitorId(u32::MAX)),
                global_bounds: wx_proto::Rect::new(0, 0, 1, 1),
                cursor_scale: 1.0,
            });
            VirtualCursor::anywhere(&placeholder).expect("placeholder monitor is usable")
        });
        state.set_cursor(cursor.monitor(), false);
        let router = InputRouter::new(local, layout, cursor);

        let bind: SocketAddr = format!("{}:{}", config.network.bind, config.network.port)
            .parse()
            .map_err(|e| {
                anyhow::anyhow!(
                    "{}:{} is not an address to bind: {e}",
                    config.network.bind,
                    config.network.port
                )
            })?;
        let endpoint = Arc::new(Endpoint::bind(bind)?);
        let port = endpoint
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(config.network.port);
        tracing::info!(%bind, "listening for peers");

        let token = ipc::generate_token()?;
        let ipc_server = IpcServer::bind(token.clone()).await?;
        let ipc_port = ipc_server.local_addr()?.port();
        ipc::EndpointFile {
            port: ipc_port,
            token,
            pid: std::process::id(),
            agent_version: AGENT_VERSION.to_string(),
        }
        .write(&config_dir)?;
        let events = ipc_server.events();

        let local_info = Arc::new(Mutex::new(NodeInfo {
            id: local,
            name: config.node.name.clone(),
            platform: platform.info.platform,
            display_server: platform.info.display_server,
            capabilities: capabilities_for(&platform, &monitors),
            monitors,
            agent_version: AGENT_VERSION.to_string(),
        }));

        let mut engine = Self {
            local,
            identity,
            trust: Arc::new(Mutex::new(trust)),
            config,
            config_path,
            config_dir,
            platform,
            state,
            router,
            endpoint: Arc::clone(&endpoint),
            local_info: Arc::clone(&local_info),
            pairing_open: Arc::new(AtomicBool::new(false)),
            sessions: HashMap::new(),
            gates: HashMap::new(),
            last_heard: HashMap::new(),
            pending: HashMap::new(),
            offered_pins: OfferedPins::default(),
            dialing: HashSet::new(),
            events,
            wake: wake.clone(),
            suppressed: false,
            swallow_release: None,
            driven_by: DrivenBy::default(),
            last_owner: local,
            // Against the port actually bound, not the configured one: the two
            // differ when the config asks for port 0, and advice naming a port
            // nothing is listening on is worse than none.
            firewall: crate::firewall::warning(port),
            shutting_down: false,
        };
        engine.pairing_open.store(
            engine.config.network.accept_pairing_requests,
            Ordering::Relaxed,
        );
        {
            let trust = engine.trust.lock().expect("trust store lock");
            let config = engine.config.clone();
            engine.state.sync_trust(&trust, &config, now);
        }

        // IPC first: the UI should be able to connect and report a problem even if
        // capture or discovery fails.
        {
            let (tx, mut rx) = mpsc::unbounded_channel::<IpcCommand>();
            let wake_ipc = wake.clone();
            tokio::spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    if wake_ipc.send(Wake::Ipc(cmd)).is_err() {
                        return;
                    }
                }
            });
            tokio::spawn(ipc_server.serve(tx));
        }

        engine.spawn_accept_loop();
        if engine.config.network.discovery {
            engine.spawn_discovery(port);
            // Said once, at the point where it becomes relevant: a firewall only
            // matters if this machine is trying to be found. Both a log line and
            // a notice, matching `start_capture` — and unlike that one it is also
            // in the status snapshot, so a UI that connects later still sees it.
            if let Some(warning) = engine.firewall.clone() {
                tracing::warn!("{warning}");
                engine.notice(ipc::NoticeLevel::Warning, warning);
            }
        }
        engine.start_capture();
        engine.seed_static_peers();
        Ok(engine)
    }

    /// Bring up input capture.
    ///
    /// A failure here is reported rather than fatal: this machine can still be
    /// driven by a peer, which is exactly the headless case, and a node that
    /// refuses to start because it cannot capture would be useless as a target.
    fn start_capture(&mut self) {
        let wake = self.wake.clone();
        let sink: wx_platform::CaptureSink = Box::new(move |event| {
            // Must not block: on Windows this runs inside a low-level hook, and
            // exceeding LowLevelHooksTimeout makes the OS silently remove it.
            let _ = wake.send(Wake::Captured(event));
        });
        match self.platform.capture.start(sink) {
            Ok(()) => tracing::info!("input capture running"),
            // Already running, so there is nothing to report and nothing to fix.
            // Both callers are legitimate and can both fire on one launch: the one
            // at boot, and the one on the edge where `CAPTURE_INPUT` is granted. A
            // backend that accepted the sink while its permission dialog was still
            // on screen has already done the work by the time the grant lands.
            Err(PlatformError::AlreadyCapturing) => {
                tracing::debug!("input capture was already running")
            }
            Err(e) => {
                tracing::warn!(error = %e, "input capture unavailable; this node can be driven but cannot drive");
                self.notice(
                    ipc::NoticeLevel::Warning,
                    format!("input capture is unavailable: {e}"),
                );
            }
        }
    }

    fn spawn_accept_loop(&self) {
        let endpoint = Arc::clone(&self.endpoint);
        let identity = Arc::clone(&self.identity);
        let trust = Arc::clone(&self.trust);
        let info = Arc::clone(&self.local_info);
        let pairing = Arc::clone(&self.pairing_open);
        let wake = self.wake.clone();
        tokio::spawn(async move {
            loop {
                // Snapshots taken per connection: a peer paired thirty seconds ago
                // must be admitted now, and a stale trust store would refuse it.
                let trust_snapshot = trust.lock().expect("trust store lock").clone();
                let local_info = info.lock().expect("local info lock").clone();
                let setup = SessionSetup {
                    identity: &identity,
                    trust: &trust_snapshot,
                    local_info,
                    pairing_mode: pairing.load(Ordering::Relaxed),
                };
                match endpoint.accept(&setup).await {
                    None => return,
                    Some(Ok((session, events))) => {
                        let established = Box::new(session.established().clone());
                        let node = established.peer_id();
                        let generation = next_session_generation();
                        if wake
                            .send(Wake::Session(Box::new(NewSession {
                                session,
                                established,
                                initiated_locally: false,
                                generation,
                            })))
                            .is_err()
                        {
                            return;
                        }
                        spawn_pump(node, generation, events, wake.clone());
                    }
                    Some(Err(e)) => {
                        // Anything on the network can open a connection; a refusal
                        // is ordinary and must not be noisy or fatal.
                        tracing::debug!(error = %e, "an inbound connection did not establish");
                    }
                }
            }
        });
    }

    fn spawn_discovery(&self, port: u16) {
        let node = self.local;
        let name = self.config.node.name.clone();
        match Advertiser::start(&node, &name, AGENT_VERSION, port) {
            Ok(advertiser) => {
                // Held for the process lifetime: dropping it withdraws the
                // announcement, and `std::mem::forget` would be the same thing
                // said less clearly.
                Box::leak(Box::new(advertiser));
            }
            Err(e) => tracing::warn!(error = %e, "could not announce on the local network"),
        }

        let wake = self.wake.clone();
        match Browser::start(node) {
            Ok(mut browser) => {
                tokio::spawn(async move {
                    while let Some(event) = browser.next_event().await {
                        if wake.send(Wake::Discovery(event)).is_err() {
                            return;
                        }
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "could not browse for peers"),
        }
    }

    /// Treat configured addresses as if discovery had reported them.
    ///
    /// For networks where multicast does not work. Only paired peers are ever
    /// dialled, so an address here grants nothing on its own.
    fn seed_static_peers(&mut self) {
        let now = Instant::now();
        let addresses: Vec<SocketAddr> = self
            .config
            .network
            .extra_addresses
            .iter()
            .filter_map(|a| match a.parse() {
                Ok(addr) => Some(addr),
                Err(e) => {
                    tracing::warn!(address = %a, error = %e, "ignoring an unparseable peer address");
                    None
                }
            })
            .collect();
        if addresses.is_empty() {
            return;
        }
        // Applied to every known-but-unlocated peer: the config lists addresses,
        // not which key answers at each one, and the handshake settles identity.
        let unlocated: Vec<NodeId> = self
            .state
            .peers()
            .filter(|p| p.is_eligible() && p.addresses.is_empty())
            .map(|p| p.node)
            .collect();
        for node in unlocated {
            let name = self
                .state
                .peer(&node)
                .map(|p| p.advertised_name.clone())
                .unwrap_or_default();
            let peer = self.state.entry(node, &name, now);
            peer.addresses = addresses.clone();
            peer.status = ConnStatus::Discovered;
        }
    }

    async fn handle(&mut self, wake: Wake) {
        match wake {
            Wake::Captured(event) => self.on_captured(event).await,
            Wake::Session(new) => self.on_session(*new).await,
            Wake::Peer {
                node,
                event,
                permit,
            } => {
                self.on_peer_event(node, event).await;
                // Explicit, because it is the whole backpressure mechanism: the
                // pump may only queue another event once this one is dealt with.
                drop(permit);
            }
            Wake::PeerGone {
                node,
                generation,
                reason,
            } => self.on_pump_gone(node, generation, reason).await,
            Wake::DialFailed { node, error } => {
                self.dialing.remove(&node);
                self.state
                    .on_disconnected(node, Some(error.clone()), Instant::now());
                tracing::debug!(peer = %node, error = %error, "dial failed");
                self.publish_peer(node);
            }
            Wake::Discovery(event) => self.on_discovery(event),
            Wake::Ipc(cmd) => {
                let response = self.on_request(cmd.request).await;
                let _ = cmd.reply.send(response);
            }
            Wake::Tick => self.on_tick().await,
            Wake::Probe => self.probe_cursor_owner().await,
            Wake::Shutdown => self.shutting_down = true,
        }
    }

    // -- input ------------------------------------------------------------

    async fn on_captured(&mut self, event: CapturedEvent) {
        let actions = match event {
            CapturedEvent::PointerMotion { dx, dy, position } => {
                self.resync_local_cursor(position);
                self.router.motion(dx, dy)
            }
            CapturedEvent::Button { button, pressed } => {
                self.router.route(InputEvent::Pointer(PointerEvent::Button {
                    button,
                    pressed,
                }))
            }
            CapturedEvent::Scroll { dx, dy, unit } => self
                .router
                .route(InputEvent::Pointer(PointerEvent::Scroll { dx, dy, unit })),
            CapturedEvent::Key(ev) => {
                // Hotkeys are consumed here and never reach the wire: forwarding
                // the chord that locks the cursor would also lock the peer's.
                if let Some(action) = self.config.hotkeys.action_for(&ev) {
                    self.swallow_release = Some(ev.payload.clone());
                    self.run_hotkey(action).await;
                    return;
                }
                if ev.action == KeyAction::Release
                    && self.swallow_release.as_ref() == Some(&ev.payload)
                {
                    // The matching release of a consumed chord. Sending it would
                    // have the peer release a key it never saw pressed.
                    self.swallow_release = None;
                    return;
                }
                self.router.route(InputEvent::Key(ev))
            }
        };
        self.execute(actions, Origin::LocalCapture).await;
    }

    async fn run_hotkey(&mut self, action: HotkeyAction) {
        match action {
            HotkeyAction::ToggleLock => {
                let locked = self.router.toggle_lock();
                tracing::info!(locked, "cursor lock toggled");
                let _ = self.events.send(Event::CursorLockChanged { locked });
                self.sync_cursor();
            }
            HotkeyAction::ReclaimCursor => {
                let local = self.local;
                // Forced: the point of the hotkey is to recover from a peer that
                // still looks alive but has stopped responding, so reachability is
                // deliberately not consulted.
                let actions = reclaim_cursor(&mut self.router, local, |n| n == local);
                self.execute(actions, Origin::Remote).await;
            }
            HotkeyAction::LockAll => {
                let mut unlocked = self.broadcast_optional(
                    Capabilities::SCREENSAVER_SYNC,
                    "lock the session",
                    ControlMsg::LockSession,
                );
                // This machine is asked the same question as a peer, and through the
                // same check, because the notice below names the machines still on
                // screen and this one is no less on screen for being the one the
                // hotkey was pressed on.
                let local = self.local;
                let label = self.peer_label(local);
                // A machine that can lock but just failed to is a different fact from
                // one that cannot lock at all, and only this one carries an error the
                // user can act on, so it is reported separately rather than folded
                // into the list above.
                let mut local_failure = None;
                if !permit_optional(
                    self.advertised_by(local),
                    Capabilities::SCREENSAVER_SYNC,
                    local,
                    &label,
                    "lock this session",
                ) {
                    unlocked.push(label);
                } else if let Err(e) = self.platform.screensaver.lock_session() {
                    tracing::warn!(error = %e, "could not lock this session");
                    local_failure = Some(format!("{label} was left unlocked: {e}"));
                }
                unlocked.sort();
                // The hotkey is invisible by design, so a machine left unlocked would
                // otherwise be discovered by walking over to it and finding the
                // desktop still on screen.
                let mut message = if unlocked.is_empty() {
                    String::new()
                } else if unlocked.len() == 1 {
                    format!(
                        "{} was left unlocked: it cannot lock its own session",
                        unlocked[0]
                    )
                } else {
                    format!(
                        "These machines were left unlocked, because they cannot lock \
                         their own sessions: {}",
                        unlocked.join(", ")
                    )
                };
                if let Some(failure) = local_failure {
                    if !message.is_empty() {
                        message.push_str(". ");
                    }
                    message.push_str(&failure);
                }
                if !message.is_empty() {
                    self.notice(ipc::NoticeLevel::Warning, message);
                }
            }
        }
    }

    /// Carry out a batch of route actions in the order the router gave them.
    async fn execute(&mut self, actions: Vec<RouteAction>, origin: Origin) {
        let from_remote = origin == Origin::Remote;
        for action in actions {
            match action {
                RouteAction::Local { target, event } => {
                    if !should_inject_locally(&event, self.suppressed, from_remote) {
                        continue;
                    }
                    self.inject_local(target, &event);
                }
                RouteAction::Remote { node, frame } => {
                    self.send_to(node, Outbound::Input(frame));
                }
                RouteAction::Handoff { node, .. } | RouteAction::Yield { node } => {
                    if let Some(msg) = action.control_msg() {
                        self.send_to(node, Outbound::Control(msg));
                    }
                }
            }
        }
        self.sync_cursor();
        self.sync_suppression();
    }

    fn inject_local(&mut self, target: MonitorId, event: &InputEvent) {
        if matches!(event, InputEvent::ReleaseControl) {
            if let Err(e) = self.platform.injector.release_all() {
                tracing::warn!(error = %e, "could not release held input");
            }
            return;
        }
        // A frame addressed to a monitor this machine no longer has still carries
        // a keystroke. Falling back to the primary display loses only the
        // position, where dropping the event loses the keystroke.
        let monitor = self
            .state
            .local_monitor(target)
            .or_else(|| self.state.local_monitors().iter().find(|m| m.primary))
            .or_else(|| self.state.local_monitors().first())
            .cloned();
        let Some(monitor) = monitor else {
            tracing::debug!("nothing to inject into: this node has no displays");
            return;
        };
        if let Err(e) = self.platform.injector.inject(&monitor, event) {
            tracing::debug!(error = %e, "injection failed");
        }
    }

    /// Believe the capture backend about where the real pointer is, when it says
    /// something no delta could explain. See [`resync_cursor`].
    fn resync_local_cursor(&mut self, position: Point) {
        resync_cursor(
            &mut self.router,
            self.local,
            self.state.local_monitors(),
            position,
        );
    }

    /// Push the suppression flag to the capture backend when, and only when, it
    /// changes.
    fn sync_suppression(&mut self) {
        let want = self.router.local_cursor_suppressed();
        if want == self.suppressed {
            return;
        }
        match self.platform.capture.set_suppress_local(want) {
            Ok(()) => self.suppressed = want,
            Err(e) => {
                tracing::warn!(error = %e, suppress = want, "could not change input suppression")
            }
        }
    }

    /// Mirror the router's cursor into the state, announcing ownership changes.
    fn sync_cursor(&mut self) {
        let monitor = self.router.cursor().monitor();
        self.state.set_cursor(monitor, self.router.is_locked());
        if monitor.node != self.last_owner {
            self.last_owner = monitor.node;
            begin_liveness_window(
                &mut self.last_heard,
                monitor.node,
                self.local,
                Instant::now(),
            );
            let _ = self.events.send(Event::CursorOwnerChanged {
                node: monitor.node.to_hex(),
                monitor: monitor.monitor.0,
                local: monitor.node == self.local,
            });
        }
    }

    // -- sessions ---------------------------------------------------------

    async fn on_session(&mut self, new: NewSession) {
        let NewSession {
            session,
            established,
            initiated_locally,
            generation,
        } = new;
        let node = established.peer_id();
        self.dialing.remove(&node);

        if node == self.local {
            // Our own announcement dialled back to us, or a peer replaying our
            // key. The handshake already rejects the latter; this is belt and
            // braces against ever routing input to ourselves over the network.
            session.close("self-connection");
            return;
        }
        if self.sessions.contains_key(&node) {
            // Both ends dialled at once. Keeping the older session is arbitrary
            // but deterministic, and dropping the newer one is invisible to the
            // user; keeping both would double every keystroke.
            tracing::debug!(peer = %node, "dropping a duplicate session");
            session.close("duplicate session");
            return;
        }

        let now = Instant::now();
        let trusted = established.peer_was_paired;
        let info = established.peer.clone();
        let name = info.name.clone();
        self.state.on_session(info.clone(), trusted, now);
        {
            let trust = self.trust.lock().expect("trust store lock");
            let config = self.config.clone();
            self.state.sync_trust(&trust, &config, now);
        }

        let out = spawn_sender(session.clone());
        self.sessions.insert(
            node,
            PeerLink {
                session,
                out,
                generation,
            },
        );
        self.gates.insert(node, SequenceGate::new());
        self.last_heard.insert(node, now);

        if trusted {
            tracing::info!(peer = %node, %name, "session established");
            self.on_peer_ready(node, &info).await;
        } else {
            tracing::info!(peer = %node, %name, "session established for pairing only");
            let pending = PendingPairing {
                node,
                name: name.clone(),
                initiated_locally,
                established,
                pairing: None,
                started: now,
            };
            self.pending.insert(node, pending);
            if initiated_locally {
                // This side chose the PIN, so it opens the exchange.
                let Some(pin) = self.offered_pins.claim(node) else {
                    tracing::warn!(peer = %node, "no pairing code was generated");
                    self.abandon_pairing(node, "no pairing code was generated")
                        .await;
                    return;
                };
                if let Some(p) = self.pending.get_mut(&node) {
                    p.pairing = Some(PairingSession::new(&p.established, pin));
                }
                let info = self.local_info.lock().expect("local info lock").clone();
                self.send_to(node, Outbound::Control(ControlMsg::PairRequest { info }));
            }
        }
        self.publish_peer(node);
    }

    /// A trusted session is up: agree on a layout and make sure the peer has a
    /// place in it.
    async fn on_peer_ready(&mut self, node: NodeId, info: &NodeInfo) {
        let mut layout = self.router.layout().clone();
        if needs_placement(&layout, node, &info.monitors)
            && autolayout::apply(&mut layout, node, &info.monitors)
        {
            self.adopt_layout(layout, true);
        }
        let current = self.router.layout().to_layout();
        self.send_to(
            node,
            Outbound::Control(ControlMsg::LayoutUpdate { layout: current }),
        );
        // Asked for as well as sent: whichever side has the higher revision wins,
        // and a reconnecting node with a stale copy needs the other's.
        self.send_to(node, Outbound::Control(ControlMsg::LayoutRequest));

        // A machine with a place in the layout that cannot take input is the exact
        // failure capability negotiation exists to make visible: the cursor crosses
        // onto it and the keyboard goes dead with nothing anywhere to say why. It is
        // a legitimate state during the alpha, because the Linux backends land one
        // capability at a time, so it is said once when the session comes up rather
        // than refused outright.
        if self.peer_supports(node, Capabilities::HAS_DISPLAYS)
            && !self.peer_supports(node, Capabilities::INJECT_INPUT)
        {
            let name = self.peer_label(node);
            tracing::warn!(
                peer = %node,
                machine = %name,
                capabilities = %info.capabilities.describe(),
                "this machine has screens but does not advertise input injection"
            );
            self.notice(
                ipc::NoticeLevel::Warning,
                format!(
                    "{name} has screens in the layout but cannot accept input yet, \
                     so the cursor can reach it and typing will do nothing"
                ),
            );
        }
    }

    async fn on_peer_event(&mut self, node: NodeId, event: SessionEvent) {
        // Any traffic at all counts as a sign of life; see `last_heard`.
        self.last_heard.insert(node, Instant::now());
        match event {
            SessionEvent::Input(frame) => self.on_input_frame(node, frame),
            SessionEvent::Control(msg) => self.on_control(node, msg).await,
        }
    }

    fn on_input_frame(&mut self, node: NodeId, frame: InputFrame) {
        if !self.state.is_reachable(node) {
            // An untrusted or disabled peer must never reach the injector. This is
            // the check that stops a machine on the LAN typing into this one.
            tracing::debug!(peer = %node, "ignoring input from a peer that is not trusted");
            return;
        }
        let gate = self.gates.entry(node).or_default();
        if !gate.accept(&frame) {
            return;
        }
        if matches!(frame.event, InputEvent::ReleaseControl) {
            // The peer is handing this machine back on the input plane rather than
            // with a YieldControl, so the record of who is driving ends here too.
            self.driven_by.let_go(node);
        }
        // Reliability is not consulted here: the gate has already decided, and it
        // deliberately lets late state-latching frames through.
        debug_assert!(
            frame.event.reliability() == Reliability::Reliable
                || matches!(
                    frame.event,
                    InputEvent::Pointer(PointerEvent::MoveTo { .. })
                )
        );
        self.inject_local(frame.target, &frame.event);
    }

    async fn on_control(&mut self, node: NodeId, msg: ControlMsg) {
        let trusted = self.state.is_reachable(node);
        if !trusted && !is_permitted_while_unpaired(&msg) {
            tracing::debug!(peer = %node, "ignoring a control message from an untrusted peer");
            return;
        }

        match msg {
            ControlMsg::TakeControl { target, entry, via } => {
                let monitor = GlobalMonitorId::new(self.local, target);
                match self.router.warp_via(monitor, entry, via) {
                    Ok(actions) => {
                        // From here until it yields or dies, this peer is the one
                        // pressing keys on this machine, and the only record of
                        // that is this one — the router's owner is the local node.
                        if let Some(displaced) = self.driven_by.took_control(node) {
                            // A second peer has taken this machine while the first
                            // still believed it was driving. Nothing else will ever
                            // release what the first one left down, because every
                            // release path from here on tests against the new
                            // driver. See `DrivenBy::took_control`.
                            tracing::warn!(
                                peer = %node,
                                %displaced,
                                "a second machine took control; releasing what the first one held"
                            );
                            if let Err(e) = self.platform.injector.release_all() {
                                tracing::warn!(error = %e, "could not release input held by a displaced peer");
                            }
                        }
                        self.execute(actions, Origin::Remote).await
                    }
                    Err(e) => {
                        // The peer's layout is stale: it is handing the cursor to a
                        // display this machine no longer has. Landing it anywhere
                        // local beats leaving it on the peer.
                        tracing::warn!(peer = %node, error = %e, "handoff named an unknown monitor");
                        let local = self.local;
                        let actions = reclaim_cursor(&mut self.router, local, |n| n == local);
                        self.execute(actions, Origin::Remote).await;
                        let current = self.router.layout().to_layout();
                        self.send_to(
                            node,
                            Outbound::Control(ControlMsg::LayoutUpdate { layout: current }),
                        );
                    }
                }
            }
            ControlMsg::YieldControl => {
                // The peer that was driving this machine has let go. Anything it
                // left held has to come up now, or it stays down forever.
                self.driven_by.let_go(node);
                if let Err(e) = self.platform.injector.release_all() {
                    tracing::warn!(error = %e, "could not release input after a yield");
                }
            }
            ControlMsg::CapabilitiesChanged { capabilities } => {
                // A peer whose portal session was revoked, most likely. Recording it
                // keeps the peer's own account of what it can do current, which is
                // what `publish_peer` below shows the user.
                let before = self.state.peer_capabilities(node);
                if let Some(info) = self.peer_info_mut(node) {
                    if info.capabilities == capabilities {
                        return;
                    }
                    tracing::info!(peer = %node, capabilities = capabilities.0, "a peer's capabilities changed");
                    info.capabilities = capabilities;
                }
                self.publish_peer(node);

                if strands_the_cursor(before, capabilities, self.router.owner() == node) {
                    // The remote half of the rescue `sync_capabilities` performs for
                    // this machine when it loses `CAPTURE_INPUT`. The cursor is on a
                    // machine that has just said it can no longer inject, so every
                    // pointer and key frame from here on lands nowhere, and anything
                    // it is holding down would stay down. Bringing the cursor home is
                    // the same action, and it is also what tells the peer to let go.
                    let name = self.peer_label(node);
                    tracing::warn!(
                        peer = %node,
                        machine = %name,
                        capabilities = %capabilities.describe(),
                        "the machine holding the cursor can no longer receive input; taking it back"
                    );
                    let local = self.local;
                    let actions = reclaim_cursor(&mut self.router, local, |n| n == local);
                    self.execute(actions, Origin::Remote).await;
                    self.notice(
                        ipc::NoticeLevel::Warning,
                        format!("{name} can no longer receive input; control returned here"),
                    );
                }
            }
            ControlMsg::MonitorsChanged { monitors } => {
                if let Some(info) = self.peer_info_mut(node) {
                    info.monitors = monitors.clone();
                }
                // `MonitorsChanged` carries monitors and not capabilities, so the
                // peer's advertised set has to be brought in line here or a machine
                // whose last screen was unplugged keeps `HAS_DISPLAYS` for the rest
                // of the session. Exactly the correction `capabilities_for` makes on
                // this side of the wire when the local displays change, which is why
                // both go through `with_displays`.
                let refreshed =
                    with_displays(self.state.peer_capabilities(node), !monitors.is_empty());
                if self.state.set_peer_capabilities(node, refreshed) {
                    tracing::info!(
                        peer = %node,
                        capabilities = %refreshed.describe(),
                        "a peer changed what it says it can do"
                    );
                }
                let mut layout = self.router.layout().clone();
                if needs_placement(&layout, node, &monitors)
                    && autolayout::apply(&mut layout, node, &monitors)
                {
                    self.adopt_layout(layout, true);
                    self.broadcast_layout();
                }
                self.publish_peer(node);
            }
            ControlMsg::LayoutUpdate { layout } => {
                let current = self.router.layout().to_layout();
                if accept_layout(&current, &layout) {
                    tracing::debug!(peer = %node, revision = layout.revision, "adopting a peer's layout");
                    let mut adopted = GlobalLayout::from_layout(&layout);
                    // This machine's own screens must be present, or the cursor can
                    // never come home. A peer that has not heard of a display we
                    // plugged in five seconds ago is the ordinary case.
                    let monitors = self.state.local_monitors().to_vec();
                    if needs_placement(&adopted, self.local, &monitors) {
                        autolayout::apply(&mut adopted, self.local, &monitors);
                    }
                    let bumped = adopted.revision() > layout.revision;
                    self.adopt_layout(adopted, true);
                    if bumped {
                        // Placing our own screens raised the revision, so the peer's
                        // copy is now stale and it will reject anything we send at
                        // the old one. Sending it back closes that gap immediately
                        // rather than at the next reconnect.
                        self.broadcast_layout();
                    }
                }
            }
            ControlMsg::LayoutRequest => {
                let layout = self.router.layout().to_layout();
                self.send_to(node, Outbound::Control(ControlMsg::LayoutUpdate { layout }));
            }
            ControlMsg::Ping { nonce } => {
                self.send_to(node, Outbound::Control(ControlMsg::Pong { nonce }));
            }
            ControlMsg::Pong { .. } => {}
            ControlMsg::LockSession => {
                if let Err(e) = self.platform.screensaver.lock_session() {
                    tracing::warn!(error = %e, "a peer asked this session to lock, and it would not");
                }
            }
            ControlMsg::PairRequest { info } => self.on_pair_request(node, info).await,
            ControlMsg::PairConfirm { .. } => self.on_pair_confirm(node, msg).await,
            ControlMsg::PairResult { accepted } => {
                if !accepted {
                    self.abandon_pairing(node, "the other machine rejected the code")
                        .await;
                }
            }
            ControlMsg::VideoStart { monitor, .. }
            | ControlMsg::VideoReconfigure { monitor, .. } => {
                // Honest refusal. Advertising VIDEO_SOURCE and then failing would
                // give the UI a button that never works.
                self.send_to(
                    node,
                    Outbound::Control(ControlMsg::VideoUnavailable {
                        monitor,
                        reason: "this agent build does not stream video".into(),
                    }),
                );
            }
            ControlMsg::Goodbye { reason } => {
                tracing::info!(peer = %node, %reason, "peer said goodbye");
                self.on_peer_gone(node, None).await;
            }
            ControlMsg::FileTransferOffer { .. }
            | ControlMsg::FileTransferAccept { .. }
            | ControlMsg::FileTransferDecline { .. }
            | ControlMsg::FileTransferProgress { .. }
            | ControlMsg::FileTransferDone { .. } => {
                // No backend advertises `FILE_TRANSFER`, so a peer sending one of
                // these has ignored the handshake. Said out loud rather than dropped
                // at debug: the peer is waiting on an answer no code path here
                // produces, and that silence is the whole of the failure.
                tracing::warn!(
                    peer = %node,
                    capability = %Capabilities::FILE_TRANSFER.describe(),
                    "ignoring a file transfer: this build never advertised the capability it needs"
                );
            }
            ControlMsg::ClipboardOffer { .. }
            | ControlMsg::ClipboardRequest { .. }
            | ControlMsg::ClipboardData { .. }
            | ControlMsg::ClipboardStale { .. }
            | ControlMsg::VideoStop { .. }
            | ControlMsg::VideoUnavailable { .. } => {
                tracing::debug!(peer = %node, "ignoring a message this build does not handle");
            }
            ControlMsg::Hello { .. }
            | ControlMsg::Welcome { .. }
            | ControlMsg::AuthProof { .. }
            | ControlMsg::Reject { .. } => {
                tracing::warn!(peer = %node, "handshake message arrived after the handshake");
            }
        }
    }

    /// A session pump reported that its connection ended.
    ///
    /// Qualified by generation: a pump whose connection has already been replaced
    /// must not tear down the one that replaced it. That happens for real on two
    /// paths — a duplicate session closed because both ends dialled at once, and
    /// the close-then-redial inside `begin_pairing`.
    async fn on_pump_gone(&mut self, node: NodeId, generation: u64, reason: Option<String>) {
        let current = self.sessions.get(&node).map(|link| link.generation);
        if !teardown_is_current(current, generation) {
            tracing::debug!(
                peer = %node,
                stale = generation,
                current = ?current,
                "ignoring the end of a connection that has already been replaced"
            );
            return;
        }
        self.on_peer_gone(node, reason).await;
    }

    /// A session ended. The cursor may be on the other side of it.
    async fn on_peer_gone(&mut self, node: NodeId, reason: Option<String>) {
        if let Some(link) = self.sessions.remove(&node) {
            link.session.close("closing");
        }
        // If that peer was driving this machine, everything it pushed down is
        // still down: an ungraceful loss sends no YieldControl and no
        // ReleaseControl, so this is the only place left to let go. Doing it before
        // anything else, because a modifier stuck down here outlives the session
        // and the user cannot clear it except by pressing that key themselves.
        if self.driven_by.let_go(node) {
            tracing::info!(peer = %node, "releasing input held by a peer that has gone");
            if let Err(e) = self.platform.injector.release_all() {
                tracing::warn!(error = %e, "could not release input after losing the driving peer");
            }
        }
        self.gates.remove(&node);
        self.last_heard.remove(&node);
        self.pending.remove(&node);
        // Not unconditionally: a code generated for a dial that is still in flight
        // belongs to the *next* connection, not the one that just died. See
        // [`OfferedPins`] — clearing it here broke every restarted pairing.
        self.offered_pins.on_session_ended(node);
        self.state
            .on_disconnected(node, reason.clone(), Instant::now());
        tracing::info!(peer = %node, reason = ?reason, "session ended");

        // The recovery that matters: a cursor left on an unreachable machine
        // cannot be retrieved by moving the mouse, because every delta is being
        // routed to a peer that is not listening.
        if self.router.owner() == node {
            let actions = self.reclaim();
            if !actions.is_empty() {
                tracing::info!(peer = %node, "reclaiming the cursor from a lost peer");
                self.notice(
                    ipc::NoticeLevel::Warning,
                    format!(
                        "{} disconnected while holding the cursor; control returned here",
                        self.peer_label(node)
                    ),
                );
            }
            self.execute(actions, Origin::Remote).await;
        }
        self.publish_peer(node);
    }

    /// Actions needed to bring the cursor back from anything unreachable.
    fn reclaim(&mut self) -> Vec<RouteAction> {
        let local = self.local;
        // Borrowed apart so the closure can consult the state while the router is
        // mutably held.
        let reachable: HashSet<NodeId> = self
            .state
            .peers()
            .filter(|p| self.state.is_reachable(p.node))
            .map(|p| p.node)
            .collect();
        reclaim_cursor(&mut self.router, local, |n| {
            n == local || reachable.contains(&n)
        })
    }

    // -- pairing ----------------------------------------------------------

    async fn on_pair_request(&mut self, node: NodeId, info: NodeInfo) {
        if self
            .trust
            .lock()
            .expect("trust store lock")
            .is_blocked(&node)
        {
            self.send_to(
                node,
                Outbound::Control(ControlMsg::Reject {
                    reason: RejectReason::Blocked,
                }),
            );
            return;
        }
        let name = info.name.clone();
        if let Some(pending) = self.pending.get_mut(&node) {
            pending.name = name.clone();
        }
        tracing::info!(peer = %node, %name, "a peer is asking to pair");
        let _ = self.events.send(Event::PairingRequested {
            node: node.to_hex(),
            name,
        });
    }

    async fn on_pair_confirm(&mut self, node: NodeId, msg: ControlMsg) {
        let Some(pending) = self.pending.get(&node) else {
            tracing::debug!(peer = %node, "a pairing confirmation arrived with no pairing in progress");
            return;
        };
        let Some(pairing) = pending.pairing.as_ref() else {
            // The user has not typed the PIN yet. The exchange is ordered so this
            // does not normally happen; if it does, the peer is out of step and
            // waiting is better than guessing.
            tracing::debug!(peer = %node, "a pairing confirmation arrived before a code was entered");
            return;
        };
        let initiated_locally = pending.initiated_locally;
        match pairing.accepts(&msg) {
            Ok(true) => {
                if initiated_locally {
                    // This side chose the PIN and has now verified the other knows
                    // it. Prove the same in return, then declare success.
                    let confirm = pairing.confirm();
                    self.send_to(node, Outbound::Control(confirm));
                    self.send_to(
                        node,
                        Outbound::Control(ControlMsg::PairResult { accepted: true }),
                    );
                }
                self.finish_pairing(node).await;
            }
            Ok(false) => {
                tracing::warn!(peer = %node, "pairing code did not match");
                self.send_to(
                    node,
                    Outbound::Control(ControlMsg::PairResult { accepted: false }),
                );
                self.abandon_pairing(node, "the pairing code did not match")
                    .await;
            }
            Err(e) => {
                tracing::warn!(peer = %node, error = %e, "malformed pairing confirmation");
                self.abandon_pairing(node, "the peer sent a malformed pairing message")
                    .await;
            }
        }
    }

    /// Record the trust and bring the peer fully into the mesh.
    async fn finish_pairing(&mut self, node: NodeId) {
        let Some(pending) = self.pending.remove(&node) else {
            return;
        };
        {
            let mut trust = self.trust.lock().expect("trust store lock");
            trust.trust(node, pending.name.clone());
            if let Err(e) = trust.save(&self.config_dir) {
                // The pairing worked but will not survive a restart, which the
                // user has to be told about: the alternative is discovering it
                // tomorrow morning.
                tracing::error!(error = %e, "could not persist the trust store");
                self.notice(
                    ipc::NoticeLevel::Error,
                    format!("pairing succeeded but could not be saved: {e}"),
                );
            }
        }
        let now = Instant::now();
        {
            let trust = self.trust.lock().expect("trust store lock");
            let config = self.config.clone();
            self.state.sync_trust(&trust, &config, now);
        }
        self.state.set_status(node, ConnStatus::Connected);
        tracing::info!(peer = %node, name = %pending.name, "paired");
        let _ = self.events.send(Event::PairingFinished {
            node: node.to_hex(),
            accepted: true,
            message: None,
        });

        let info = pending.established.peer.clone();
        self.on_peer_ready(node, &info).await;
        self.publish_peer(node);
    }

    /// Give up on a pairing, telling the UI why.
    async fn abandon_pairing(&mut self, node: NodeId, why: &str) {
        let existed = self.pending.remove(&node).is_some();
        self.offered_pins.discard(node);
        if existed {
            let _ = self.events.send(Event::PairingFinished {
                node: node.to_hex(),
                accepted: false,
                message: Some(why.to_string()),
            });
        }
        // The session was only ever admitted for pairing, so it has no further
        // purpose and must not be left open to an untrusted peer.
        if !self.state.is_reachable(node) {
            if let Some(link) = self.sessions.remove(&node) {
                link.session.close(why);
            }
            self.gates.remove(&node);
        }
        self.publish_peer(node);
    }

    // -- discovery and reconnection ---------------------------------------

    fn on_discovery(&mut self, event: DiscoveryEvent) {
        let now = Instant::now();
        match event {
            DiscoveryEvent::Found(peer) => {
                let node = peer.node;
                {
                    let trust = self.trust.lock().expect("trust store lock");
                    self.state.observe_discovered(&peer, &trust, now);
                }
                let config = self.config.clone();
                {
                    let trust = self.trust.lock().expect("trust store lock");
                    self.state.sync_trust(&trust, &config, now);
                }
                tracing::debug!(peer = %node, "discovered");
                self.publish_peer(node);
            }
            DiscoveryEvent::Lost(node) => {
                self.state.observe_lost(node);
                self.publish_peer(node);
            }
        }
    }

    /// Make sure the machine holding the cursor is still there.
    ///
    /// The failure this exists for, found by killing an agent outright rather than
    /// stopping it cleanly: a machine that loses power sends no close frame, so
    /// QUIC does not declare the connection dead until its idle timeout twenty
    /// seconds later. For those twenty seconds this machine believes a corpse owns
    /// the cursor, keeps routing every keystroke to it, and keeps local input
    /// suppressed — so the user's keyboard and mouse do nothing whatsoever, with no
    /// indication why.
    async fn probe_cursor_owner(&mut self) {
        // Checked first, because it is a different peer and a different failure:
        // "who is typing on this machine" is not "where the cursor is".
        self.probe_driving_peer().await;

        let owner = self.router.owner();
        if owner == self.local {
            return;
        }
        let now = Instant::now();
        let silent_for = self
            .last_heard
            .get(&owner)
            .map(|t| now.saturating_duration_since(*t))
            // No record at all means no session, which is already unreachable.
            .unwrap_or(Duration::MAX);

        if silent_for > CURSOR_LIVENESS {
            tracing::warn!(
                peer = %owner,
                silent_ms = silent_for.as_millis(),
                "the machine holding the cursor has stopped answering"
            );
            // Marked failed first, so the reclaim's reachability test agrees and so
            // the peer is not immediately redialled into the same silence.
            self.state
                .on_disconnected(owner, Some("stopped responding".into()), now);
            let name = self.peer_label(owner);
            self.notice(
                ipc::NoticeLevel::Warning,
                format!("{name} stopped responding; control returned here"),
            );
            let actions = self.reclaim();
            self.execute(actions, Origin::Remote).await;
            // The session is finished as far as this machine is concerned. Closing
            // it explicitly stops the sender queue growing and lets a reconnect
            // start from a clean handshake rather than a half-dead connection.
            if let Some(link) = self.sessions.remove(&owner) {
                link.session.close("stopped responding");
            }
            self.gates.remove(&owner);
            self.last_heard.remove(&owner);
            self.publish_peer(owner);
            return;
        }

        // Nonce is milliseconds of uptime: monotonic, needs no extra state, and
        // identifies the probe in a packet capture. The reply is not matched
        // against it, because any traffic from the peer is proof enough.
        let nonce = self.state.uptime(now).as_millis() as u64;
        self.send_to(owner, Outbound::Control(ControlMsg::Ping { nonce }));
    }

    /// Make sure the machine *driving* this one is still there.
    ///
    /// The mirror of [`Engine::probe_cursor_owner`], and it needs its own path
    /// because the cursor cannot answer the question: while a peer drives this
    /// machine the cursor is here, so the owner is the local node and every
    /// reachability test passes. Left to QUIC's twenty-second idle timeout, a peer
    /// that loses power mid-chord leaves Ctrl and the left mouse button physically
    /// held down here for those twenty seconds — and the button drags.
    ///
    /// The probe is what keeps this safe: a healthy peer that is merely holding a
    /// key without moving the mouse sends nothing, so silence alone would condemn
    /// it. Any answer at all refreshes the deadline.
    async fn probe_driving_peer(&mut self) {
        let Some(driver) = self.driven_by.peer() else {
            return;
        };
        let now = Instant::now();
        let silent_for = self
            .last_heard
            .get(&driver)
            .map(|t| now.saturating_duration_since(*t))
            // No record at all means no session, which is already unreachable.
            .unwrap_or(Duration::MAX);

        if silent_for <= CURSOR_LIVENESS {
            // See `probe_cursor_owner` for why the nonce is uptime and why the
            // reply is not matched against it.
            let nonce = self.state.uptime(now).as_millis() as u64;
            self.send_to(driver, Outbound::Control(ControlMsg::Ping { nonce }));
            return;
        }

        tracing::warn!(
            peer = %driver,
            silent_ms = silent_for.as_millis(),
            "the machine driving this one has stopped answering; releasing what it held"
        );
        self.driven_by.let_go(driver);
        if let Err(e) = self.platform.injector.release_all() {
            tracing::warn!(error = %e, "could not release input held by a peer that stopped answering");
        }
        self.state
            .on_disconnected(driver, Some("stopped responding".into()), now);
        let name = self.peer_label(driver);
        self.notice(
            ipc::NoticeLevel::Warning,
            format!("{name} stopped responding; the keys it was holding were released"),
        );
        if let Some(link) = self.sessions.remove(&driver) {
            link.session.close("stopped responding");
        }
        self.gates.remove(&driver);
        self.last_heard.remove(&driver);
        self.publish_peer(driver);
    }

    /// Notice that what this machine can do has changed, and tell everyone.
    ///
    /// Separate from the display check above, because the two change for unrelated
    /// reasons. A Wayland node loses `CAPTURE_INPUT` and `INJECT_INPUT` the moment
    /// the portal revokes its session — screen locked, dialog dismissed, session
    /// expired — with every monitor still exactly where it was. Left to the display
    /// path, that would go unannounced until somebody unplugged something.
    ///
    /// Polled rather than pushed: the backend that loses a permission is on its own
    /// thread with no route into this loop, and a tick is soon enough for a change
    /// the user just made by hand.
    async fn sync_capabilities(&mut self) {
        let now = capabilities_for(&self.platform, self.state.local_monitors());
        let before = {
            let mut info = self.local_info.lock().expect("local info lock");
            let before = info.capabilities;
            info.capabilities = now;
            before
        };
        if before == now {
            return;
        }

        let lost = Capabilities(before.0 & !now.0);
        let gained = Capabilities(now.0 & !before.0);
        tracing::info!(
            before = before.0,
            now = now.0,
            "local capabilities changed; re-advertising"
        );
        // Only to peers that advertised they understand it. One that did not is
        // left with the capability set it learned at the handshake, which is
        // exactly what a build predating this message has always done.
        self.broadcast_control_capable(
            Capabilities::CAPABILITY_UPDATES,
            ControlMsg::CapabilitiesChanged { capabilities: now },
        );

        if gained.contains(Capabilities::CAPTURE_INPUT) {
            // The permission this machine needs to capture may arrive long after
            // startup — a Wayland consent dialog is answered whenever the user gets
            // to it — and the one `start_capture` at boot may well have failed by
            // then. No guard is needed: this only fires on a bit that was absent and
            // is now present, which is a fresh grant rather than a retry against a
            // refusal, and a backend that took the sink at boot answers with
            // `AlreadyCapturing`, which `start_capture` treats as the no-op it is.
            tracing::info!("this machine can capture input again; starting capture");
            self.start_capture();
        }

        if lost.contains(Capabilities::CAPTURE_INPUT) {
            // Before anything else: if the cursor is out on a peer, this machine
            // has just lost the only way to steer it back, and whatever it was
            // holding down there would stay down forever. Same rescue as the
            // reclaim hotkey, which is also the path that tells the peer to let go.
            let local = self.local;
            let actions = reclaim_cursor(&mut self.router, local, |n| n == local);
            self.execute(actions, Origin::Remote).await;

            // Stopped, not restarted. The usual reason to lose this is a user who
            // refused, and a daemon that answered by asking again would put the
            // consent dialog back on their screen forever.
            if let Err(e) = self.platform.capture.stop() {
                tracing::debug!(error = %e, "input capture was already stopped");
            }
            tracing::warn!("this machine can no longer capture input");
            self.notice(
                ipc::NoticeLevel::Warning,
                "this machine can no longer send input to other machines".to_string(),
            );
        }
        if lost.contains(Capabilities::INJECT_INPUT) {
            // Whatever a peer was holding down here is never coming up on its own.
            if let Err(e) = self.platform.injector.release_all() {
                tracing::debug!(error = %e, "nothing to release");
            }
            self.notice(
                ipc::NoticeLevel::Warning,
                "this machine can no longer be controlled by other machines".to_string(),
            );
        }
    }

    async fn on_tick(&mut self) {
        let now = Instant::now();

        // Displays come and go with docks and lids.
        if let Ok(monitors) = self.platform.displays.monitors() {
            if self.state.set_local_monitors(monitors.clone()) {
                tracing::info!(count = monitors.len(), "local displays changed");
                {
                    // Capabilities are deliberately not touched here: unplugging the
                    // last display drops `HAS_DISPLAYS`, and `sync_capabilities`
                    // below is the one place that notices and tells peers. Setting
                    // them here as well would make that call see no change and stay
                    // quiet.
                    let mut info = self.local_info.lock().expect("local info lock");
                    info.monitors = monitors.clone();
                }
                let mut layout = self.router.layout().clone();
                if needs_placement(&layout, self.local, &monitors)
                    && autolayout::apply(&mut layout, self.local, &monitors)
                {
                    self.adopt_layout(layout, true);
                }
                self.broadcast_control(ControlMsg::MonitorsChanged {
                    monitors: monitors.clone(),
                });
                let _ = self.events.send(Event::MonitorsChanged {
                    monitors: monitors.iter().map(ipc::MonitorSpec::of).collect(),
                });
            }
        }

        self.sync_capabilities().await;

        // Round-trip times, for the UI and for diagnosing a slow link.
        let rtts: Vec<(NodeId, Duration)> = self
            .sessions
            .iter()
            .map(|(node, link)| (*node, link.session.rtt()))
            .collect();
        for (node, rtt) in rtts {
            self.state.set_rtt(node, rtt);
        }

        // Pairings nobody finished. These hold a session from an untrusted peer
        // open, so they are not allowed to linger.
        let stale: Vec<NodeId> = self
            .pending
            .values()
            .filter(|p| now.saturating_duration_since(p.started) > PAIRING_TIMEOUT)
            .map(|p| p.node)
            .collect();
        for node in stale {
            self.abandon_pairing(node, "pairing timed out").await;
        }

        if self.config.network.auto_connect {
            for (node, addresses) in self.state.dial_targets(now) {
                self.dial(node, addresses, false);
            }
        }

        // Belt and braces for held input. The blind spot this covers: while a peer
        // drives *this* machine the router's owner is the local node, which is
        // always reachable, so the cursor check below never fires and nothing else
        // notices that the machine pressing our keys has gone.
        if let Some(driver) = self.driven_by.peer() {
            if !self.state.is_reachable(driver) && self.driven_by.let_go(driver) {
                tracing::info!(peer = %driver, "the peer driving this machine is unreachable; releasing held input");
                if let Err(e) = self.platform.injector.release_all() {
                    tracing::warn!(error = %e, "could not release input held by an unreachable peer");
                }
            }
        }

        // Belt and braces for the cursor: `PeerGone` is the usual path, but a peer
        // that is connected-but-disabled, or one whose session died without a
        // notification, would otherwise keep the cursor.
        if !self.state.is_reachable(self.router.owner()) {
            let actions = self.reclaim();
            if !actions.is_empty() {
                tracing::info!(peer = %self.router.owner(), "cursor was on an unreachable machine");
                self.execute(actions, Origin::Remote).await;
            }
        }
    }

    /// Dial a peer, trying each address in turn.
    fn dial(&mut self, node: NodeId, addresses: Vec<SocketAddr>, pairing_mode: bool) {
        if self.sessions.contains_key(&node) || !self.dialing.insert(node) {
            return;
        }
        self.state.set_status(node, ConnStatus::Connecting);
        let endpoint = Arc::clone(&self.endpoint);
        let identity = Arc::clone(&self.identity);
        let trust = Arc::clone(&self.trust);
        let info = Arc::clone(&self.local_info);
        let wake = self.wake.clone();
        tokio::spawn(async move {
            let trust_snapshot = trust.lock().expect("trust store lock").clone();
            let local_info = info.lock().expect("local info lock").clone();
            let setup = SessionSetup {
                identity: &identity,
                trust: &trust_snapshot,
                local_info,
                pairing_mode,
            };
            let mut last = String::from("no address to try");
            for addr in addresses {
                match endpoint.connect(addr, &setup).await {
                    Ok((session, events)) => {
                        let established = Box::new(session.established().clone());
                        let generation = next_session_generation();
                        let _ = wake.send(Wake::Session(Box::new(NewSession {
                            session,
                            established,
                            initiated_locally: true,
                            generation,
                        })));
                        spawn_pump(node, generation, events, wake);
                        return;
                    }
                    Err(e) => {
                        tracing::debug!(peer = %node, %addr, error = %e, "dial failed");
                        last = e.to_string();
                    }
                }
            }
            let _ = wake.send(Wake::DialFailed { node, error: last });
        });
    }

    // -- layout -----------------------------------------------------------

    /// Install a layout, persist it, and tell the UI.
    fn adopt_layout(&mut self, layout: GlobalLayout, persist: bool) {
        let as_proto = layout.to_layout();
        // `set_layout` rehomes the cursor when its monitor has vanished — a display
        // unplugged, or a peer removed from the layout — and those actions must
        // still be carried out or the cursor stays on a rectangle that no longer
        // exists. Injected unconditionally: a rehome is not an echo of local input,
        // so the physical pointer really does have to be moved.
        let actions = self.router.set_layout(layout);
        for action in actions {
            match action {
                RouteAction::Local { target, event } => self.inject_local(target, &event),
                RouteAction::Remote { node, frame } => self.send_to(node, Outbound::Input(frame)),
                RouteAction::Handoff { node, .. } | RouteAction::Yield { node } => {
                    if let Some(msg) = action.control_msg() {
                        self.send_to(node, Outbound::Control(msg));
                    }
                }
            }
        }
        self.sync_cursor();
        self.sync_suppression();

        if persist {
            self.config.layout = Some(crate::config::SavedLayout::from_layout(&as_proto));
            self.save_config();
        }
        let _ = self.events.send(Event::LayoutChanged {
            layout: crate::config::SavedLayout::from_layout(&as_proto),
        });
    }

    fn broadcast_layout(&mut self) {
        let layout = self.router.layout().to_layout();
        self.broadcast_control(ControlMsg::LayoutUpdate { layout });
    }

    // -- IPC --------------------------------------------------------------

    async fn on_request(&mut self, request: Request) -> Response {
        match request {
            Request::Hello { .. } => Response::Hello {
                node_id: self.local.to_hex(),
                node_name: self.config.node.name.clone(),
                agent_version: AGENT_VERSION.to_string(),
                protocol: wx_proto::PROTOCOL_VERSION,
            },
            Request::Status => Response::Status {
                status: Box::new(ipc::status_snapshot(
                    &self.state,
                    &self.config,
                    &self.router.layout().to_layout(),
                    &self.platform.info,
                    // The live advertisement, not `info.capabilities`: on Wayland
                    // the portal grants input permission long after the backend was
                    // built, and the UI reads this field to decide what this machine
                    // can do.
                    self.advertised_by(self.local),
                    AGENT_VERSION,
                    self.state.uptime(Instant::now()),
                    self.firewall.as_deref(),
                    self.autostart_registered(),
                )),
            },
            Request::ListPeers => Response::Peers {
                peers: self.state.peers().map(ipc::PeerSnapshot::of).collect(),
            },
            Request::GetLayout => Response::Layout {
                layout: crate::config::SavedLayout::from_layout(&self.router.layout().to_layout()),
            },
            Request::GetConfig => Response::Config {
                config: Box::new(self.config.clone()),
            },
            Request::SetLayout { layout } => {
                let mut proto = layout.to_layout();
                // The revision belongs to the agent: two UIs editing at once would
                // otherwise both claim the same one and the peers would disagree
                // about which layout won.
                proto.revision = self.router.layout().revision() + 1;
                self.adopt_layout(GlobalLayout::from_layout(&proto), true);
                self.broadcast_layout();
                Response::Ok
            }
            Request::AutoLayout { node } => {
                let mut layout = self.router.layout().clone();
                let mut changed = false;
                let targets: Vec<NodeId> = match node {
                    Some(hex) => match ipc::parse_node(&hex) {
                        Ok(n) => vec![n],
                        Err(e) => return e,
                    },
                    None => {
                        layout = GlobalLayout::new();
                        layout.set_revision(self.router.layout().revision());
                        let mut all = vec![self.local];
                        all.extend(
                            self.state
                                .peers()
                                .filter(|p| p.is_eligible())
                                .map(|p| p.node),
                        );
                        all
                    }
                };
                for target in targets {
                    let monitors = if target == self.local {
                        self.state.local_monitors().to_vec()
                    } else {
                        self.state
                            .peer(&target)
                            .map(|p| p.monitors().to_vec())
                            .unwrap_or_default()
                    };
                    changed |= autolayout::apply(&mut layout, target, &monitors);
                }
                if changed {
                    self.adopt_layout(layout, true);
                    self.broadcast_layout();
                }
                Response::Layout {
                    layout: crate::config::SavedLayout::from_layout(
                        &self.router.layout().to_layout(),
                    ),
                }
            }
            Request::SetCursorLock { locked } => {
                let locked = match locked {
                    Some(want) => {
                        self.router.set_locked(want);
                        want
                    }
                    None => self.router.toggle_lock(),
                };
                self.sync_cursor();
                let _ = self.events.send(Event::CursorLockChanged { locked });
                Response::CursorLock { locked }
            }
            Request::WarpCursor {
                node,
                monitor,
                x,
                y,
            } => {
                let node = match ipc::parse_node(&node) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                let target = GlobalMonitorId::new(node, MonitorId(monitor));
                match self.router.warp(target, NormPos::new(x, y)) {
                    Ok(actions) => {
                        self.execute(actions, Origin::Remote).await;
                        Response::Ok
                    }
                    Err(e) => Response::error(ErrorCode::BadRequest, e.to_string()),
                }
            }
            Request::LockAll => {
                self.run_hotkey(HotkeyAction::LockAll).await;
                Response::Ok
            }
            Request::BeginPairing { node } => self.begin_pairing(node).await,
            Request::ConfirmPairing { node, pin } => self.confirm_pairing(node, pin).await,
            Request::CancelPairing { node } => {
                let node = match ipc::parse_node(&node) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                self.abandon_pairing(node, "cancelled").await;
                Response::Ok
            }
            Request::ForgetPeer { node } => {
                let node = match ipc::parse_node(&node) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                self.forget_peer(node, false).await;
                Response::Ok
            }
            Request::BlockPeer { node } => {
                let node = match ipc::parse_node(&node) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                self.forget_peer(node, true).await;
                Response::Ok
            }
            Request::SetPeerName { node, name } => {
                let parsed = match ipc::parse_node(&node) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                self.config.peer_mut(&parsed).name = Some(name.clone());
                {
                    let mut trust = self.trust.lock().expect("trust store lock");
                    trust.rename(&parsed, name);
                    let _ = trust.save(&self.config_dir);
                }
                self.save_config();
                self.resync_peers();
                self.publish_peer(parsed);
                Response::Ok
            }
            Request::SetPeerEnabled { node, enabled } => {
                let parsed = match ipc::parse_node(&node) {
                    Ok(n) => n,
                    Err(e) => return e,
                };
                self.config.peer_mut(&parsed).enabled = enabled;
                self.save_config();
                self.resync_peers();
                if !enabled {
                    self.on_peer_gone(parsed, Some("disabled".into())).await;
                }
                self.publish_peer(parsed);
                Response::Ok
            }
            Request::SetNodeName { name } => {
                self.config.node.name = name.clone();
                self.state.set_local_name(name.clone());
                self.local_info.lock().expect("local info lock").name = name;
                self.save_config();
                Response::Ok
            }
            Request::SetAutostart { enabled } => self.set_autostart(enabled).await,
            // Handled by the IPC server itself; reaching the engine means the
            // server's own fast path changed and this is a safe answer.
            Request::Subscribe => Response::Ok,
            Request::Shutdown => {
                self.shutting_down = true;
                Response::Ok
            }
        }
    }

    async fn begin_pairing(&mut self, node: String) -> Response {
        let node = match ipc::parse_node(&node) {
            Ok(n) => n,
            Err(e) => return e,
        };
        let Some(peer) = self.state.peer(&node) else {
            return Response::error(ErrorCode::UnknownPeer, "that machine has not been seen");
        };
        if peer.blocked {
            return Response::error(
                ErrorCode::BadRequest,
                "that machine is blocked; unblock it first",
            );
        }
        let addresses = peer.addresses.clone();
        if addresses.is_empty() {
            return Response::error(
                ErrorCode::NotConnected,
                "no address is known for that machine yet",
            );
        }
        let pin = match Pin::generate() {
            Ok(pin) => pin,
            Err(e) => return Response::error(ErrorCode::Internal, e.to_string()),
        };
        let shown = pin.as_str().to_string();
        // Offered against the dial below rather than against whatever session
        // exists now, so the teardown of the session this is about to close cannot
        // take the code with it. See [`OfferedPins`].
        self.offered_pins.offer(node, pin);
        // A stale session from a previous attempt would carry the wrong nonces, so
        // the exchange always starts from a fresh connection.
        if let Some(link) = self.sessions.remove(&node) {
            link.session.close("restarting pairing");
        }
        self.pending.remove(&node);
        self.dialing.remove(&node);
        self.dial(node, addresses, true);
        Response::PairingStarted {
            node: node.to_hex(),
            pin: shown,
        }
    }

    async fn confirm_pairing(&mut self, node: Option<String>, pin: String) -> Response {
        let node = match node {
            Some(hex) => match ipc::parse_node(&hex) {
                Ok(n) => n,
                Err(e) => return e,
            },
            None => {
                // `--pair <code>` supplies no node. Guessing between two pending
                // pairings could pair the user with the wrong machine, so it is
                // refused instead.
                let mut waiting = self.pending.values().filter(|p| !p.initiated_locally);
                match (waiting.next(), waiting.next()) {
                    (Some(only), None) => only.node,
                    (None, _) => {
                        return Response::error(
                            ErrorCode::NoPairing,
                            "no machine is waiting for a pairing code",
                        )
                    }
                    _ => {
                        return Response::error(
                            ErrorCode::NoPairing,
                            "more than one machine is waiting; name which one",
                        )
                    }
                }
            }
        };
        let pin = match Pin::parse(&pin) {
            Ok(pin) => pin,
            Err(e) => return Response::error(ErrorCode::BadRequest, e.to_string()),
        };
        let Some(pending) = self.pending.get_mut(&node) else {
            return Response::error(ErrorCode::NoPairing, "no pairing is in progress");
        };
        let session = PairingSession::new(&pending.established, pin);
        let confirm = session.confirm();
        pending.pairing = Some(session);
        self.send_to(node, Outbound::Control(confirm));
        Response::Ok
    }

    /// Remove a peer's trust, its layout placements, and its session.
    async fn forget_peer(&mut self, node: NodeId, block: bool) {
        {
            let mut trust = self.trust.lock().expect("trust store lock");
            let name = trust.name_of(&node).unwrap_or("").to_string();
            if block {
                trust.block(node, name);
            } else {
                trust.forget(&node);
            }
            if let Err(e) = trust.save(&self.config_dir) {
                tracing::error!(error = %e, "could not persist the trust store");
            }
        }
        self.resync_peers();
        // The cursor must not be able to walk onto a machine that will now refuse
        // every frame.
        let mut layout = self.router.layout().clone();
        if autolayout::forget_node(&mut layout, node) {
            self.adopt_layout(layout, true);
            self.broadcast_layout();
        }
        self.on_peer_gone(node, Some("no longer paired".into()))
            .await;
    }

    fn resync_peers(&mut self) {
        let now = Instant::now();
        let trust = self.trust.lock().expect("trust store lock");
        let config = self.config.clone();
        self.state.sync_trust(&trust, &config, now);
    }

    // -- plumbing ---------------------------------------------------------

    fn send_to(&mut self, node: NodeId, msg: Outbound) {
        let Some(link) = self.sessions.get(&node) else {
            // Ordinary during a disconnect: the router still had actions for a
            // peer that has just gone. Logged at debug because it is expected.
            tracing::debug!(peer = %node, "no session for this peer");
            return;
        };
        let generation = link.generation;
        match enqueue(&link.out, msg) {
            Queued::Sent => {}
            Queued::ShedMotion => {
                // Only ever an absolute position, which the next one repairs.
                tracing::debug!(peer = %node, "queue full; dropped a superseded pointer position");
            }
            Queued::AlreadyClosed => {
                // The sender task is gone, so the session is dead; the pump task
                // will report it. Nothing useful to do here.
                tracing::debug!(peer = %node, "dropping a message for a closed session");
            }
            Queued::Unresponsive => {
                // Nothing droppable is left, so the choice is to grow without limit
                // or to give up on the peer. Routed through the wake queue rather
                // than handled inline so that the cursor rescue and the UI update
                // run exactly as they do for any other lost peer.
                tracing::warn!(peer = %node, "peer has stopped draining its queue; closing the session");
                let _ = self.wake.send(Wake::PeerGone {
                    node,
                    generation,
                    reason: Some("stopped reading; its send queue filled".into()),
                });
            }
        }
    }

    // -- capability negotiation -------------------------------------------

    /// What a machine says it can do.
    ///
    /// The local node answers from its own advertisement rather than being assumed
    /// capable of everything, so that a feature needing both ends — clipboard sync
    /// needs `CLIPBOARD_TEXT` here as well as there — asks about this machine in
    /// exactly the same way it asks about a peer.
    fn advertised_by(&self, node: NodeId) -> Capabilities {
        if node == self.local {
            self.local_info
                .lock()
                .expect("local info lock")
                .capabilities
        } else {
            self.state.peer_capabilities(node)
        }
    }

    /// Whether a machine advertised a capability.
    ///
    /// The one question an optional feature asks before it does anything. A machine
    /// that has not introduced itself advertises nothing, so the answer is no.
    fn peer_supports(&self, node: NodeId, cap: Capabilities) -> bool {
        self.advertised_by(node).contains(cap)
    }

    /// Label for a machine, for a log line or a notice a user has to act on.
    ///
    /// A hex node id names nothing anyone recognises; the local label is what they
    /// typed into the Devices screen.
    fn peer_label(&self, node: NodeId) -> String {
        if node == self.local {
            return self.state.local_name().to_string();
        }
        self.state
            .peer(&node)
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| node.short())
    }

    /// Send a control message to one peer, but only if the peer advertised what it
    /// needs to act on it. Reports whether the message went.
    ///
    /// The seam every optional feature goes through before it sends. Clipboard,
    /// file transfer, video and session locking all have a capability bit, and
    /// sending one of their messages to a machine that never claimed the bit is how
    /// a feature comes to fail with nothing in any log to attribute it to.
    fn send_optional(
        &mut self,
        node: NodeId,
        cap: Capabilities,
        feature: &str,
        msg: ControlMsg,
    ) -> bool {
        if !permit_optional(
            self.advertised_by(node),
            cap,
            node,
            &self.peer_label(node),
            feature,
        ) {
            return false;
        }
        self.send_to(node, Outbound::Control(msg));
        true
    }

    /// Send to every reachable peer that advertises the capability.
    ///
    /// Returns the machines that were skipped, in a stable order, so that a
    /// user-initiated action can say which of them will not be doing it.
    fn broadcast_optional(
        &mut self,
        cap: Capabilities,
        feature: &str,
        msg: ControlMsg,
    ) -> Vec<String> {
        let peers: Vec<NodeId> = self
            .sessions
            .keys()
            .copied()
            .filter(|n| self.state.is_reachable(*n))
            .collect();
        let mut refused = Vec::new();
        for node in peers {
            if !self.send_optional(node, cap, feature, msg.clone()) {
                refused.push(self.peer_label(node));
            }
        }
        refused.sort();
        refused
    }

    fn broadcast_control(&mut self, msg: ControlMsg) {
        let peers: Vec<NodeId> = self
            .sessions
            .keys()
            .copied()
            .filter(|n| self.state.is_reachable(*n))
            .collect();
        for node in peers {
            self.send_to(node, Outbound::Control(msg.clone()));
        }
    }

    /// Broadcast a message only to the peers that advertised support for it.
    ///
    /// Skipping a peer costs it one piece of news. Sending it anyway costs it the
    /// session: a variant its build does not have is a decode error, and a control
    /// stream that fails to decode is torn down by construction. So a message
    /// added after a peer's build might have been made goes out through here,
    /// keyed to the capability bit that says the peer can decode it.
    ///
    /// [`Engine::peer_supports`] is the predicate rather than
    /// [`Engine::broadcast_optional`] because a peer missing the bit is an older
    /// build rather than a feature being refused, and this fires every time a
    /// portal session comes or goes — the helper's per-peer warning would be
    /// repeated noise about something nobody can act on.
    fn broadcast_control_capable(&mut self, required: Capabilities, msg: ControlMsg) {
        let peers: Vec<NodeId> = self
            .sessions
            .keys()
            .copied()
            .filter(|n| self.state.is_reachable(*n) && self.peer_supports(*n, required))
            .collect();
        for node in peers {
            self.send_to(node, Outbound::Control(msg.clone()));
        }
    }

    /// The `NodeInfo` a peer sent at its handshake, ready to be amended.
    ///
    /// `None` until the peer has actually introduced itself: a control message from
    /// one that has not creates the entry but has nothing to amend.
    fn peer_info_mut(&mut self, node: NodeId) -> Option<&mut NodeInfo> {
        let now = Instant::now();
        let name = self
            .state
            .peer(&node)
            .map(|p| p.advertised_name.clone())
            .unwrap_or_default();
        self.state.entry(node, &name, now).info.as_mut()
    }

    fn publish_peer(&self, node: NodeId) {
        if let Some(peer) = self.state.peer(&node) {
            let _ = self.events.send(Event::PeerChanged {
                peer: ipc::PeerSnapshot::of(peer),
            });
        } else {
            let _ = self.events.send(Event::PeerRemoved {
                node: node.to_hex(),
            });
        }
    }

    fn notice(&self, level: ipc::NoticeLevel, message: String) {
        let _ = self.events.send(Event::Notice { level, message });
    }

    fn save_config(&self) {
        if let Err(e) = self.config.save(&self.config_path) {
            tracing::error!(error = %e, path = %self.config_path.display(), "could not save the configuration");
        }
    }

    /// Whether the OS says the agent starts with the session.
    ///
    /// Asked of the OS on every status request rather than cached, because the
    /// registration is a file or a registry value that anything else on the
    /// machine can remove while this process runs. It is two `stat` calls on
    /// Linux and one registry read on Windows, against a request the UI already
    /// makes at human speed.
    ///
    /// Where the platform cannot answer at all — macOS, which has no mechanism
    /// yet — the config's record is the best available answer, and the honest
    /// one: on such a platform the flag only ever means "the user asked".
    fn autostart_registered(&self) -> bool {
        autostart::is_registered().unwrap_or(self.config.node.autostart)
    }

    /// Register or remove the start-with-the-session entry.
    ///
    /// The config flag is written only after the platform work succeeded, and it
    /// records what the OS was actually told rather than what was asked for. A
    /// `autostart = true` in the config file with no registration behind it is
    /// the same silent-at-every-login failure [`crate::autostart::install`]
    /// exists to avoid, arrived at from the other direction: the UI would report
    /// that the agent starts with the session, and it would not.
    async fn set_autostart(&mut self, enabled: bool) -> Response {
        // On the blocking pool, because registering writes files and then runs
        // `systemctl --user daemon-reload`, which re-reads every unit on the
        // machine — synchronous work that would otherwise occupy a runtime
        // worker thread and stall every other task scheduled on it.
        //
        // It does not make this free. The wake loop is serialized and awaits
        // this call, so a toggle does briefly delay input routing for the peer
        // being driven; `spawn_blocking` moves where the waiting happens, not
        // whether it happens. What keeps that delay bounded is
        // [`crate::autostart::linux_impl::daemon_reload`] giving up on a wedged
        // `systemctl` after a few seconds rather than waiting on it forever.
        let outcome = tokio::task::spawn_blocking(move || {
            if enabled {
                autostart::install().map(|exe| {
                    tracing::info!(path = %exe.display(), "registered to start with this session");
                })
            } else {
                autostart::uninstall().inspect(|()| {
                    tracing::info!("removed the autostart registration");
                })
            }
        })
        .await;
        // A panic in there is this build's bug, not something the user can act
        // on, so it is reported as internal rather than as an autostart error.
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::error!(error = %e, "the autostart task did not finish");
                return Response::error(ErrorCode::Internal, "changing autostart failed");
            }
        };
        match outcome {
            Ok(()) => {
                self.config.node.autostart = enabled;
                self.save_config();
                Response::Ok
            }
            // `Unsupported` is its own code so the UI can say "not on this
            // platform" rather than showing a failure the user could try to fix.
            Err(e @ autostart::AutostartError::Unsupported { .. }) => {
                Response::error(ErrorCode::Unsupported, e.to_string())
            }
            Err(e) => {
                tracing::warn!(error = %e, enabled, "could not change the autostart registration");
                Response::error(ErrorCode::Internal, e.to_string())
            }
        }
    }

    /// Leave the desk in a usable state.
    ///
    /// The order matters: peers are told before the socket closes, and everything
    /// held is released before anything else, because a modifier left down on
    /// another machine outlives this process.
    async fn shutdown(&mut self) {
        tracing::info!("shutting down");
        let actions = self.router.release_all();
        self.execute(actions, Origin::Remote).await;
        self.broadcast_control(ControlMsg::Goodbye {
            reason: "agent shutting down".into(),
        });
        for link in self.sessions.values() {
            // `try_send` rather than awaiting: a peer that has stopped reading must
            // not be able to hold up the shutdown of every other one.
            let _ = link.out.try_send(Outbound::Control(ControlMsg::Goodbye {
                reason: "agent shutting down".into(),
            }));
        }
        if let Err(e) = self.platform.capture.stop() {
            tracing::debug!(error = %e, "input capture was already stopped");
        }
        if let Err(e) = self.platform.injector.release_all() {
            tracing::debug!(error = %e, "nothing to release");
        }
        // Give the per-peer senders a moment to flush the goodbyes, then close.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.endpoint.close();
        self.endpoint.wait_idle().await;
        ipc::EndpointFile::remove(&self.config_dir);
    }
}

/// Capabilities to advertise, given what the platform reported and what displays
/// are actually attached.
///
/// Recomputed on hotplug rather than fixed at startup: a laptop that boots with
/// the lid shut and no external screen would otherwise advertise `HAS_DISPLAYS`
/// for the rest of the session, and peers would place monitors it does not have.
fn capabilities_for(platform: &PlatformBackend, monitors: &[Monitor]) -> Capabilities {
    // The live set, not `info.capabilities`: on Wayland input capture and injection
    // exist only while the portal session does, and the value fixed at startup would
    // keep telling peers this machine can drive them after the user revoked it.
    //
    // `CAPABILITY_UPDATES` is added unconditionally because it describes this
    // build's wire implementation rather than the machine: every node understands
    // the message on every platform, whatever its portal has or has not granted.
    with_displays(
        platform
            .current_capabilities()
            .union(Capabilities::CAPABILITY_UPDATES),
        !monitors.is_empty(),
    )
}

/// `base` with `HAS_DISPLAYS` set to match whether a screen is actually attached.
///
/// Shared by the local advertisement and by the refresh of a peer's set when it
/// reports a hotplug, and that sharing is the point: `ControlMsg::MonitorsChanged`
/// carries monitors and not capabilities, so each side has to derive the bit from
/// the list itself, and two copies of that rule would eventually disagree about
/// whether a machine has a place in the layout.
fn with_displays(base: Capabilities, has_displays: bool) -> Capabilities {
    if has_displays {
        base.union(Capabilities::HAS_DISPLAYS)
    } else {
        // No display, so no place in the layout. Clearing the bit is done by
        // rebuilding the mask, since `Capabilities` has no removal operation.
        Capabilities(base.0 & !Capabilities::HAS_DISPLAYS.0)
    }
}

/// Whether a peer's new capability set leaves the cursor somewhere it cannot be
/// used.
///
/// The mirror of the losing edge in [`Engine::sync_capabilities`]: that one reclaims
/// when *this* machine loses `CAPTURE_INPUT` and so can no longer steer the cursor
/// home; this one reclaims when the machine *holding* the cursor announces it can no
/// longer take input. Both leave the user with a cursor that answers to nothing, and
/// on the remote side the only recovery without this is a hotkey they may not know
/// exists.
///
/// Keyed on the losing edge rather than on the absence of the bit, so that a peer
/// which never advertised injection — a headless forwarder, say — does not have the
/// cursor snatched off it every time it re-advertises anything else.
///
/// Pure, and separate from the engine, so both answers can be tested: losing
/// injection while holding the cursor has to reclaim, and any other change has to
/// leave the cursor where it is.
fn strands_the_cursor(
    before: Capabilities,
    now: Capabilities,
    peer_holds_the_cursor: bool,
) -> bool {
    peer_holds_the_cursor
        && before.contains(Capabilities::INJECT_INPUT)
        && !now.contains(Capabilities::INJECT_INPUT)
}

/// Whether an optional feature may be attempted against a machine, refusing out
/// loud when it may not.
///
/// The enforcement half of capability negotiation. `warn` rather than `debug`, and
/// both the machine and the capability named, because the alternative is what this
/// replaces: a message dropped quietly, a feature that does nothing, and a user
/// with no way to find out which of their machines declined or what it was missing.
/// It is the same principle the video path already follows — a build that cannot
/// stream refuses honestly instead of accepting a start it will never honour.
///
/// Pure, and separate from the engine, so that both answers can be tested: a
/// capability that was advertised has to let the attempt through untouched, and one
/// that was not has to produce a line naming the machine and the bit.
fn permit_optional(
    advertised: Capabilities,
    cap: Capabilities,
    node: NodeId,
    machine: &str,
    feature: &str,
) -> bool {
    if advertised.contains(cap) {
        return true;
    }
    tracing::warn!(
        peer = %node,
        machine,
        capability = %cap.describe(),
        advertises = %advertised.describe(),
        "refusing to {feature}: this machine does not advertise the capability it needs"
    );
    false
}

/// Permits limiting how much of one peer's inbound traffic can be in flight
/// inside the engine at once. See [`INBOUND_QUEUE_DEPTH`].
fn inbound_permits() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(INBOUND_QUEUE_DEPTH))
}

/// Pump one session's inbound events into the engine's queue.
///
/// Per peer rather than shared, so a peer that floods cannot starve the others of
/// their share of the engine's attention.
fn spawn_pump(
    node: NodeId,
    generation: u64,
    mut events: Events,
    wake: mpsc::UnboundedSender<Wake>,
) {
    let permits = inbound_permits();
    tokio::spawn(async move {
        let mut reason = None;
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => {
                    // Taken before the event is queued and released once the engine
                    // has dealt with it. When the engine falls behind — a stalled
                    // injector, or a peer writing faster than input can be applied
                    // — this stops reading rather than growing the queue, and the
                    // stall propagates back through QUIC's flow control to the
                    // sender instead of into this machine's memory.
                    let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                        return;
                    };
                    if wake
                        .send(Wake::Peer {
                            node,
                            event,
                            permit,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(e) => {
                    reason = Some(e.to_string());
                    break;
                }
            }
        }
        let _ = wake.send(Wake::PeerGone {
            node,
            generation,
            reason,
        });
    });
}

/// Serialise everything addressed to one peer through one task.
///
/// Two reasons, both load-bearing. Ordering: the router's action list is only
/// correct if it arrives in order, and a release that overtakes the handoff it was
/// meant to precede strands a modifier. Latency: a peer whose congestion window is
/// full would otherwise block the engine loop, and with it every other machine.
///
/// The queue is bounded ([`OUTBOUND_QUEUE_DEPTH`]) because this task blocks
/// indefinitely on a peer that has stopped reading, and an unbounded queue in
/// front of a blocked writer is unbounded memory growth. What happens when it
/// fills is [`enqueue`]'s decision, not this task's.
fn spawn_sender(session: Session) -> mpsc::Sender<Outbound> {
    let (tx, mut rx) = mpsc::channel::<Outbound>(OUTBOUND_QUEUE_DEPTH);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let result = match &msg {
                Outbound::Input(frame) => session.send_input(frame).await,
                Outbound::Control(control) => session.send_control(control).await,
            };
            if let Err(e) = result {
                // The session is finished; the pump task reports the death, so
                // this only needs to stop trying.
                tracing::debug!(error = %e, "send failed; closing the peer queue");
                return;
            }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{KeyEvent, Modifiers, MouseButton, Placement, Rect, ScrollUnit, SpecialKey};

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    fn gid(n: u8, m: u32) -> GlobalMonitorId {
        GlobalMonitorId::new(node(n), MonitorId(m))
    }

    fn place(id: GlobalMonitorId, x: i32, w: u32) -> Placement {
        Placement {
            monitor: id,
            global_bounds: Rect::new(x, 0, w, 1080),
            cursor_scale: 1.0,
        }
    }

    fn mon(id: u32, x: i32, w: u32) -> Monitor {
        Monitor {
            id: MonitorId(id),
            name: format!("m{id}"),
            local_bounds: Rect::new(x, 0, w, 1080),
            scale: 1.0,
            primary: id == 0,
        }
    }

    /// This machine (node 1) on the left, one peer (node 2) on the right.
    fn two_machines() -> GlobalLayout {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 1920));
        l.insert(place(gid(2, 0), 1920, 1920));
        l
    }

    fn router_with_cursor_on(layout: &GlobalLayout, monitor: GlobalMonitorId) -> InputRouter {
        let cursor = VirtualCursor::at(layout, monitor, NormPos::new(0.5, 0.5)).unwrap();
        InputRouter::new(node(1), layout.clone(), cursor)
    }

    #[test]
    fn a_cursor_that_moved_without_us_seeing_it_is_resynchronised() {
        // The Wayland case in one test: the portal only delivers input while it
        // has capture activated, so the user moves their pointer freely in
        // between. Without this the virtual cursor stays where the last crossing
        // left it, and the next one needs a whole screen's width of travel.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(1, 0));
        let monitors = [mon(0, 0, 1920)];

        assert!(resync_cursor(
            &mut router,
            node(1),
            &monitors,
            Point::new(1900.0, 540.0)
        ));
        assert!(router.owns_cursor());
        // At the right-hand edge now, so the next nudge crosses.
        let actions = router.motion(64.0, 0.0);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, RouteAction::Handoff { .. })),
            "the cursor did not reach the peer: {actions:?}"
        );
    }

    #[test]
    fn ordinary_motion_does_not_drag_the_cursor_back_off_the_edge() {
        // The regression this threshold exists to prevent, and it would be a bad
        // one: on a backend that reports a real position for every event, the
        // physical pointer is pinned at the edge of its own screen while the user
        // pushes onward. Correcting to it on every event would undo exactly the
        // motion that is supposed to carry the cursor onto a peer.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(1, 0));
        let monitors = [mon(0, 0, 1920)];

        // Physically at the edge and staying there; the virtual cursor arrives to
        // meet it and then must be allowed to keep going.
        for _ in 0..40 {
            resync_cursor(&mut router, node(1), &monitors, Point::new(1919.0, 540.0));
            let actions = router.motion(64.0, 0.0);
            if actions
                .iter()
                .any(|a| matches!(a, RouteAction::Handoff { .. }))
            {
                return;
            }
        }
        panic!("the cursor never crossed onto the peer");
    }

    #[test]
    fn a_position_the_cursor_is_already_at_changes_nothing() {
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(1, 0));
        let monitors = [mon(0, 0, 1920)];
        assert!(!resync_cursor(
            &mut router,
            node(1),
            &monitors,
            Point::new(960.0, 540.0)
        ));
    }

    #[test]
    fn a_peer_holding_the_cursor_is_not_dragged_home_by_a_local_pointer() {
        // While a peer owns the cursor the local pointer is parked and suppressed,
        // and where it happens to be says nothing about where the user is
        // pointing. Resynchronising to it would snatch the cursor back.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(2, 0));
        let monitors = [mon(0, 0, 1920)];
        assert!(!resync_cursor(
            &mut router,
            node(1),
            &monitors,
            Point::new(10.0, 10.0)
        ));
        assert_eq!(router.owner(), node(2));
    }

    #[test]
    fn a_position_on_no_local_monitor_is_ignored_rather_than_guessed_at() {
        // A stale monitor list, or a compositor reporting a zone this machine no
        // longer has. Warping to the nearest screen would teleport the pointer.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(1, 0));
        let monitors = [mon(0, 0, 1920)];
        assert!(!resync_cursor(
            &mut router,
            node(1),
            &monitors,
            Point::new(9000.0, 9000.0)
        ));
    }

    #[test]
    fn resynchronising_moves_nothing_by_itself() {
        // It corrects a belief. A local injection here would fight the compositor,
        // which on Wayland has the real pointer pinned at a barrier while capture
        // is active.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(1, 0));
        let monitors = [mon(0, 0, 1920)];
        resync_cursor(&mut router, node(1), &monitors, Point::new(100.0, 100.0));
        // Nothing was handed over and nothing was released: same node, same owner.
        assert!(router.owns_cursor());
        assert!(router.held_keys().is_empty());
    }

    #[test]
    fn a_dead_peer_holding_the_cursor_gives_it_back() {
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(2, 0));
        let actions = reclaim_cursor(&mut router, node(1), |n| n == node(1));
        assert!(router.owns_cursor());
        assert!(
            !actions.is_empty(),
            "nothing was done to recover the cursor"
        );
    }

    #[test]
    fn a_reachable_peer_keeps_the_cursor() {
        // The rescue must not fire on a healthy link, or the cursor would be
        // yanked home every time the tick ran.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(2, 0));
        let actions = reclaim_cursor(&mut router, node(1), |_| true);
        assert!(actions.is_empty());
        assert_eq!(router.owner(), node(2));
    }

    #[test]
    fn reclaiming_releases_everything_the_dead_peer_was_holding() {
        // A modifier left down on a machine that comes back is stuck until
        // someone physically presses that key.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(2, 0));
        router.route(InputEvent::Key(KeyEvent::special(
            SpecialKey::CtrlLeft,
            KeyAction::Press,
            Modifiers::CTRL,
        )));
        router.route(InputEvent::Pointer(PointerEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        }));
        assert_eq!(router.held_keys().len(), 1);
        assert_eq!(router.held_buttons().len(), 1);

        let actions = reclaim_cursor(&mut router, node(1), |n| n == node(1));
        assert!(router.held_keys().is_empty());
        assert!(router.held_buttons().is_empty());

        // The releases are addressed to the peer, and they precede the yield.
        let yield_at = actions
            .iter()
            .position(|a| matches!(a, RouteAction::Yield { .. }))
            .expect("no yield was sent");
        let releases = actions[..yield_at]
            .iter()
            .filter(|a| matches!(a, RouteAction::Remote { .. }))
            .count();
        assert!(releases >= 2, "{actions:?}");
    }

    #[test]
    fn reclaiming_twice_is_harmless() {
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(2, 0));
        reclaim_cursor(&mut router, node(1), |n| n == node(1));
        let again = reclaim_cursor(&mut router, node(1), |n| n == node(1));
        assert!(again.is_empty(), "{again:?}");
        assert!(router.owns_cursor());
    }

    #[test]
    fn a_cursor_already_at_home_is_left_alone() {
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(1, 0));
        assert!(reclaim_cursor(&mut router, node(1), |n| n == node(1)).is_empty());
    }

    #[test]
    fn locally_captured_input_is_not_injected_a_second_time() {
        // The OS has already delivered it to the focused window; injecting the
        // router's matching action as well types everything twice.
        let press = InputEvent::Key(KeyEvent::text("a", KeyAction::Press, Modifiers::NONE));
        assert!(!should_inject_locally(&press, false, false));
        let motion = InputEvent::Pointer(PointerEvent::MoveTo {
            pos: NormPos::new(0.5, 0.5),
        });
        assert!(!should_inject_locally(&motion, false, false));
        let scroll = InputEvent::Pointer(PointerEvent::Scroll {
            dx: 0.0,
            dy: 1.0,
            unit: ScrollUnit::Lines,
        });
        assert!(!should_inject_locally(&scroll, false, false));
    }

    #[test]
    fn a_release_is_always_injected_even_when_it_looks_redundant() {
        // A skipped release latches a modifier or a mouse button down, and the
        // user cannot clear it without pressing that key on the affected machine.
        let key_up = InputEvent::Key(KeyEvent::special(
            SpecialKey::CtrlLeft,
            KeyAction::Release,
            Modifiers::NONE,
        ));
        assert!(should_inject_locally(&key_up, false, false));
        let button_up = InputEvent::Pointer(PointerEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        });
        assert!(should_inject_locally(&button_up, false, false));
        assert!(should_inject_locally(
            &InputEvent::ReleaseControl,
            false,
            false
        ));
    }

    #[test]
    fn everything_is_injected_while_local_input_is_suppressed() {
        // Nothing reached the OS, so the injector is the only path.
        let press = InputEvent::Key(KeyEvent::text("a", KeyAction::Press, Modifiers::NONE));
        assert!(should_inject_locally(&press, true, false));
        assert!(should_inject_locally(&press, false, true));
    }

    #[test]
    fn an_unpaired_peer_may_only_speak_about_pairing() {
        for allowed in [
            ControlMsg::PairRequest {
                info: peer_info(2, vec![]),
            },
            ControlMsg::PairConfirm {
                code_proof: [0u8; 32],
            },
            ControlMsg::PairResult { accepted: true },
            ControlMsg::Ping { nonce: 1 },
            ControlMsg::Goodbye {
                reason: "bye".into(),
            },
        ] {
            assert!(is_permitted_while_unpaired(&allowed), "{allowed:?}");
        }

        // Everything that could move the cursor, change the layout, or read the
        // clipboard is refused until a human has approved the machine.
        for refused in [
            ControlMsg::TakeControl {
                target: MonitorId(0),
                entry: NormPos::new(0.0, 0.5),
                via: wx_proto::Edge::Left,
            },
            ControlMsg::LayoutUpdate {
                layout: Layout::default(),
            },
            ControlMsg::ClipboardRequest {
                format: wx_proto::ClipboardFormat::Utf8Text,
                serial: 1,
            },
            ControlMsg::LockSession,
            ControlMsg::MonitorsChanged { monitors: vec![] },
            ControlMsg::VideoStart {
                monitor: MonitorId(0),
                config: wx_proto::VideoConfig::default(),
            },
        ] {
            assert!(!is_permitted_while_unpaired(&refused), "{refused:?}");
        }
    }

    fn peer_info(n: u8, monitors: Vec<Monitor>) -> NodeInfo {
        NodeInfo {
            id: node(n),
            name: format!("peer{n}"),
            platform: wx_proto::Platform::Linux,
            display_server: wx_proto::DisplayServer::X11,
            capabilities: Capabilities::CAPTURE_INPUT,
            monitors,
            agent_version: "0.1.0".into(),
        }
    }

    #[test]
    fn a_newly_reported_screen_needs_placing_and_a_familiar_one_does_not() {
        let layout = two_machines();
        assert!(!needs_placement(&layout, node(2), &[mon(0, 0, 1920)]));
        assert!(needs_placement(
            &layout,
            node(2),
            &[mon(0, 0, 1920), mon(1, 1920, 1920)]
        ));
        // A screen the OS reports as zero-sized cannot hold a cursor and needs no
        // place in the layout.
        assert!(!needs_placement(&layout, node(2), &[mon(0, 0, 0)]));
    }

    #[test]
    fn the_higher_layout_revision_wins() {
        let a = Layout {
            revision: 4,
            placements: vec![place(gid(1, 0), 0, 1920)],
        };
        let b = Layout {
            revision: 5,
            placements: vec![],
        };
        assert!(accept_layout(&a, &b));
        assert!(!accept_layout(&b, &a));
    }

    #[test]
    fn two_machines_that_each_placed_themselves_first_converge_on_one_layout() {
        // The bug this exists for, found by running two agents: both pair, both
        // reach revision 2 with two placements, and both put themselves on the
        // left. Whatever the rule is, it must make exactly one of them give way.
        let a_view = Layout {
            revision: 2,
            placements: vec![place(gid(1, 0), 0, 1920), place(gid(2, 0), 1920, 1920)],
        };
        let b_view = Layout {
            revision: 2,
            placements: vec![place(gid(2, 0), 0, 1920), place(gid(1, 0), 1920, 1920)],
        };

        // Exactly one direction is accepted, so the two agree rather than each
        // keeping its own and the desk behaving like a ring.
        assert_ne!(
            accept_layout(&a_view, &b_view),
            accept_layout(&b_view, &a_view),
            "both machines kept their own layout"
        );

        // And having agreed, the exchange stops.
        let winner = if accept_layout(&a_view, &b_view) {
            b_view.clone()
        } else {
            a_view.clone()
        };
        assert!(!accept_layout(&winner, &a_view.clone()) || winner == a_view);
        assert!(!accept_layout(&winner, &winner.clone()));
    }

    #[test]
    fn the_tie_break_does_not_depend_on_the_order_placements_arrive_in() {
        // The two ends serialise their placements in whatever order their layouts
        // happen to iterate. If the comparison saw that order, they would disagree
        // about who won and oscillate forever.
        let forwards = Layout {
            revision: 1,
            placements: vec![place(gid(1, 0), 0, 1920), place(gid(2, 0), 1920, 1920)],
        };
        let backwards = Layout {
            revision: 1,
            placements: vec![place(gid(2, 0), 1920, 1920), place(gid(1, 0), 0, 1920)],
        };
        assert!(!accept_layout(&forwards, &backwards));
        assert!(!accept_layout(&backwards, &forwards));
    }

    #[test]
    fn a_tie_is_broken_by_completeness_so_two_bootstraps_converge() {
        // Both machines start at revision 1 with only their own screens. Without a
        // tie-break they would push layouts at each other forever.
        let mine = Layout {
            revision: 1,
            placements: vec![place(gid(1, 0), 0, 1920)],
        };
        let fuller = Layout {
            revision: 1,
            placements: vec![place(gid(1, 0), 0, 1920), place(gid(2, 0), 1920, 1920)],
        };
        assert!(accept_layout(&mine, &fuller));
        assert!(!accept_layout(&fuller, &mine));
        // And an identical layout is never re-adopted, so the exchange terminates.
        assert!(!accept_layout(&fuller, &fuller.clone()));
    }

    /// Capture what `tracing` emitted while `body` ran, as plain text.
    ///
    /// The refusal is a log line and nothing else — it deliberately sends no
    /// message and returns no error — so the only way to test that it names the
    /// machine and the capability is to read what was written. The subscriber is
    /// installed per thread, so tests running in parallel do not see each other's.
    fn captured_logs(body: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("log buffer lock")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
            type Writer = Buffer;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        let bytes = buffer.0.lock().expect("log buffer lock").clone();
        String::from_utf8(bytes).expect("log output is text")
    }

    #[test]
    fn a_feature_the_peer_advertises_is_attempted() {
        let advertised = Capabilities::INJECT_INPUT | Capabilities::SCREENSAVER_SYNC;
        let mut allowed = false;
        let logs = captured_logs(|| {
            allowed = permit_optional(
                advertised,
                Capabilities::SCREENSAVER_SYNC,
                node(2),
                "workshop-mac",
                "lock the session",
            );
        });
        assert!(allowed, "a capability the peer advertised was refused");
        // Nothing is said about a feature that simply works; a warning per lock
        // would train the user to ignore the ones that matter.
        assert!(logs.is_empty(), "{logs}");
    }

    #[test]
    fn a_feature_the_peer_does_not_advertise_is_refused_and_named() {
        // The failure this replaces: the message went out, the other machine
        // ignored it, and nothing anywhere said which machine or what was missing.
        let advertised = Capabilities::INJECT_INPUT | Capabilities::HAS_DISPLAYS;
        let mut allowed = true;
        let logs = captured_logs(|| {
            allowed = permit_optional(
                advertised,
                Capabilities::SCREENSAVER_SYNC,
                node(2),
                "workshop-mac",
                "lock the session",
            );
        });
        assert!(
            !allowed,
            "a capability the peer never claimed was attempted"
        );
        assert!(logs.contains("WARN"), "refused at the wrong level: {logs}");
        assert!(
            logs.contains("SCREENSAVER_SYNC"),
            "the capability was not named: {logs}"
        );
        assert!(
            logs.contains("workshop-mac") && logs.contains(&node(2).short()),
            "the machine was not named: {logs}"
        );
    }

    #[test]
    fn a_machine_that_advertises_nothing_refuses_everything() {
        // The state every peer is in before its handshake, and the state the
        // skeleton backends — macOS, X11, evdev — are in for the whole of the alpha.
        let mut allowed = true;
        let logs = captured_logs(|| {
            allowed = permit_optional(
                Capabilities::NONE,
                Capabilities::CLIPBOARD_TEXT,
                node(3),
                "pi",
                "sync the clipboard",
            );
        });
        assert!(!allowed);
        assert!(logs.contains("CLIPBOARD_TEXT"), "{logs}");
        // And says so: an empty set has to read as a claim, not as a missing field.
        assert!(logs.contains("nothing"), "{logs}");
    }

    #[test]
    fn a_peer_that_loses_injection_while_holding_the_cursor_gives_it_back() {
        // A Wayland peer whose portal session is revoked announces the loss over
        // `CapabilitiesChanged` while the cursor is sitting on it. Every frame sent
        // from here on would land on a machine that cannot inject it, and the user
        // would be left with a cursor that answers to nothing.
        let has_input = Capabilities::HAS_DISPLAYS | Capabilities::INJECT_INPUT;
        let revoked = Capabilities::HAS_DISPLAYS;
        assert!(strands_the_cursor(has_input, revoked, true));

        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(2, 0));
        let actions = reclaim_cursor(&mut router, node(1), |n| n == node(1));
        assert!(router.owns_cursor());
        assert!(!actions.is_empty(), "the cursor was left on the peer");
    }

    #[test]
    fn an_unrelated_capability_change_leaves_the_cursor_where_it_is() {
        // The guard on the rescue: it has to fire on the losing edge of injection and
        // on nothing else, or the cursor would be yanked home whenever a peer plugged
        // in a monitor or its clipboard support landed.
        let base = Capabilities::HAS_DISPLAYS | Capabilities::INJECT_INPUT;
        assert!(!strands_the_cursor(
            base,
            base | Capabilities::CLIPBOARD_TEXT,
            true
        ));
        assert!(!strands_the_cursor(base, Capabilities::INJECT_INPUT, true));
        // Losing it while the cursor is elsewhere is somebody else's problem.
        assert!(!strands_the_cursor(base, Capabilities::HAS_DISPLAYS, false));
        // And a peer that never advertised injection has nothing to lose, so a
        // re-advertisement must not read as a revocation.
        let never = Capabilities::HAS_DISPLAYS;
        assert!(!strands_the_cursor(never, never, true));
        assert!(!strands_the_cursor(
            never,
            never | Capabilities::CLIPBOARD_TEXT,
            true
        ));
    }

    #[test]
    fn a_peers_displays_and_its_advertised_set_are_derived_the_same_way() {
        // Both sides of the wire turn a monitor list into `HAS_DISPLAYS`, because
        // `MonitorsChanged` carries the list and not the bit. Two rules would drift.
        let base = Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT;
        assert!(with_displays(base, true).contains(Capabilities::HAS_DISPLAYS));
        assert!(!with_displays(base, false).contains(Capabilities::HAS_DISPLAYS));
        // A peer that unplugs its last screen loses the bit and keeps the rest.
        let dropped = with_displays(base.union(Capabilities::HAS_DISPLAYS), false);
        assert!(!dropped.contains(Capabilities::HAS_DISPLAYS));
        assert!(dropped.contains(Capabilities::INJECT_INPUT));
    }

    #[test]
    fn a_headless_node_does_not_claim_to_have_displays() {
        // Advertising HAS_DISPLAYS with no screens makes peers place monitors that
        // cannot be reached.
        let platform = wx_platform::current_platform().unwrap();
        let caps = capabilities_for(&platform, &[]);
        assert!(!caps.contains(Capabilities::HAS_DISPLAYS));
        let caps = capabilities_for(&platform, &[mon(0, 0, 1920)]);
        assert!(caps.contains(Capabilities::HAS_DISPLAYS));
    }

    #[test]
    fn what_a_node_advertises_follows_the_permission_it_holds_now() {
        // On Wayland input permission arrives — and vanishes — long after startup,
        // so what peers are told has to come from the live set rather than from the
        // `PlatformInfo` fixed when the backend was built. Reading the startup value
        // here would leave a node whose portal session was revoked still inviting
        // peers to push their cursor onto it.
        let platform = wx_platform::current_platform().unwrap();
        let screens = [mon(0, 0, 1920)];

        platform
            .live_capabilities
            .set(Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT);
        let granted = capabilities_for(&platform, &screens);
        assert!(granted.contains(Capabilities::CAPTURE_INPUT));
        assert!(granted.contains(Capabilities::INJECT_INPUT));

        platform.live_capabilities.set(Capabilities::NONE);
        let revoked = capabilities_for(&platform, &screens);
        assert!(!revoked.contains(Capabilities::CAPTURE_INPUT));
        assert!(!revoked.contains(Capabilities::INJECT_INPUT));
        assert!(
            revoked.contains(Capabilities::HAS_DISPLAYS),
            "screens are not a portal permission and must survive a revocation"
        );
    }

    #[test]
    fn every_node_advertises_that_it_understands_capability_updates() {
        // The bit peers gate `CapabilitiesChanged` on. It describes this build's
        // wire implementation, not the machine, so it has to be advertised on every
        // platform, with or without displays, and whatever the portal has granted —
        // a node that dropped it while its permission was gone would stop being
        // told about its peers' permissions too.
        let platform = wx_platform::current_platform().unwrap();

        platform
            .live_capabilities
            .set(Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT);
        assert!(capabilities_for(&platform, &[mon(0, 0, 1920)])
            .contains(Capabilities::CAPABILITY_UPDATES));

        platform.live_capabilities.set(Capabilities::NONE);
        assert!(
            capabilities_for(&platform, &[]).contains(Capabilities::CAPABILITY_UPDATES),
            "a headless node with no permission still understands the message"
        );
    }

    #[test]
    fn a_full_reclaim_path_routes_input_locally_again() {
        // End to end over the pure pieces: the cursor is on a peer, the peer dies,
        // and the next mouse movement has to reach this machine.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(2, 0));
        let motion = router.motion(5.0, 0.0);
        assert!(
            motion
                .iter()
                .all(|a| matches!(a, RouteAction::Remote { .. })),
            "{motion:?}"
        );

        reclaim_cursor(&mut router, node(1), |n| n == node(1));
        let motion = router.motion(5.0, 0.0);
        assert!(
            motion
                .iter()
                .any(|a| matches!(a, RouteAction::Local { .. })),
            "{motion:?}"
        );
        assert!(!router.local_cursor_suppressed());
    }

    #[test]
    fn crossing_back_from_a_peer_warps_the_physical_pointer() {
        // The pointer was parked while the peer had control, so the MoveTo that
        // follows the crossing has to be injected even though it is local.
        let layout = two_machines();
        let mut router = router_with_cursor_on(&layout, gid(2, 0));
        let actions = router.motion(-2000.0, 0.0);
        let crossed = actions.iter().any(|a| {
            matches!(
                a,
                RouteAction::Local {
                    event: InputEvent::Pointer(PointerEvent::MoveTo { .. }),
                    ..
                }
            )
        });
        assert!(crossed, "{actions:?}");
        // While suppressed, that action is injected; the suppression flag has not
        // been recomputed yet at the moment the batch runs.
        for action in &actions {
            if let RouteAction::Local { event, .. } = action {
                assert!(should_inject_locally(event, true, false), "{event:?}");
            }
        }
    }

    #[test]
    fn taking_control_of_an_idle_peer_does_not_declare_it_dead() {
        // The bug, found by warping onto a peer that had been sitting idle: a
        // healthy peer sends nothing while it is not being driven, so its
        // last-heard timestamp is minutes old by the time the cursor arrives. If
        // the deadline were measured from that, the first probe would reclaim the
        // cursor immediately and it would appear to bounce off the seam.
        let mut last_heard = HashMap::new();
        let now = Instant::now();
        let ancient = now - Duration::from_secs(600);
        last_heard.insert(node(2), ancient);

        begin_liveness_window(&mut last_heard, node(2), node(1), now);
        let silence = now.saturating_duration_since(last_heard[&node(2)]);
        assert!(
            silence < CURSOR_LIVENESS,
            "an idle but healthy peer looks dead after {silence:?}"
        );
    }

    #[test]
    fn the_liveness_clock_is_not_started_for_this_machine() {
        // Nothing to probe: this machine cannot stop answering itself, and an entry
        // keyed by the local node would be a permanent lie in the peer map.
        let mut last_heard = HashMap::new();
        begin_liveness_window(&mut last_heard, node(1), node(1), Instant::now());
        assert!(last_heard.is_empty());
    }

    #[test]
    fn the_cursor_liveness_deadline_is_useful_at_both_ends() {
        // These three constants only work as a set, and tuning one in isolation
        // silently breaks the guarantee. The failure at each end:
        //
        // Too long, and a dead peer keeps the cursor while local input stays
        // suppressed — the user's keyboard does nothing and they have no idea why.
        // QUIC's own idle timeout is 20s, and being no better than that makes the
        // whole probe pointless.
        assert!(
            CURSOR_LIVENESS < Duration::from_secs(5),
            "a dead peer would hold the cursor for {CURSOR_LIVENESS:?}"
        );

        // Too short, and a peer that is merely busy loses the cursor mid-gesture.
        // At least two probes must fit inside the deadline, or a single dropped
        // datagram is enough to declare a healthy machine dead.
        assert!(
            CURSOR_LIVENESS >= CURSOR_PROBE * 2,
            "one lost probe would strand the cursor"
        );

        // And the probe must be frequent enough that the deadline is reached
        // promptly once it does expire.
        assert!(CURSOR_PROBE < CURSOR_LIVENESS);
        assert!(
            CURSOR_PROBE < TICK,
            "housekeeping is not a substitute for probing the cursor owner"
        );
    }

    #[test]
    fn losing_the_peer_that_was_driving_this_machine_demands_a_release() {
        // The stuck-key bug this exists for: A drives B, the user is holding Ctrl
        // and dragging, and A loses power. There is no Goodbye and no YieldControl,
        // and B's router owner is B itself for the whole session — so nothing about
        // the cursor reveals that the machine pressing B's keys has gone. Only this
        // record does, and if it says nothing then Ctrl and the mouse button stay
        // physically down on B until someone presses them there.
        let mut driving = DrivenBy::default();
        assert_eq!(driving.took_control(node(2)), None);
        assert_eq!(driving.peer(), Some(node(2)));
        assert!(
            driving.let_go(node(2)),
            "the driving peer disappearing must release what it was holding"
        );
    }

    #[test]
    fn losing_an_unrelated_peer_releases_nothing() {
        // A spurious release_all while a peer is legitimately mid-chord would drop
        // the keys it is deliberately holding, which is its own visible bug.
        let mut driving = DrivenBy::default();
        assert_eq!(driving.took_control(node(2)), None);
        assert!(!driving.let_go(node(3)));
        assert_eq!(driving.peer(), Some(node(2)));
    }

    #[test]
    fn a_peer_that_yielded_is_not_released_again_when_it_later_disconnects() {
        let mut driving = DrivenBy::default();
        assert_eq!(driving.took_control(node(2)), None);
        assert!(driving.let_go(node(2)), "the yield itself releases");
        assert!(
            !driving.let_go(node(2)),
            "a disconnect after a clean yield must not release input again"
        );
        assert_eq!(driving.peer(), None);
    }

    #[test]
    fn nothing_is_released_when_no_peer_has_ever_taken_control() {
        let mut driving = DrivenBy::default();
        assert!(!driving.let_go(node(2)));
    }

    #[test]
    fn a_second_machine_taking_control_reports_the_one_it_displaced() {
        // The orphaned-modifier bug: each agent's router owns its own cursor, so A
        // and B can both believe they are driving this machine. A pushes Ctrl and
        // the left button down; B then crosses the seam and takes control. With the
        // record simply overwritten, every later release path tested against B, so
        // A's Ctrl and its held button never came up — and a held left button is a
        // drag. Naming the displaced peer here is what lets the takeover release it.
        let mut driving = DrivenBy::default();
        assert_eq!(driving.took_control(node(2)), None);
        assert_eq!(
            driving.took_control(node(3)),
            Some(node(2)),
            "a competing takeover must surrender what the previous driver held"
        );
        assert_eq!(driving.peer(), Some(node(3)));
    }

    #[test]
    fn the_driving_peer_taking_control_again_releases_nothing() {
        // A peer re-taking a machine it already drives is ordinary: it happens on
        // every crossing back and forth across a split edge. Releasing there would
        // drop the modifiers it is deliberately holding mid-chord, which is its own
        // visible bug, so only a *different* peer counts as a displacement.
        let mut driving = DrivenBy::default();
        assert_eq!(driving.took_control(node(2)), None);
        assert_eq!(driving.took_control(node(2)), None);
        assert_eq!(driving.peer(), Some(node(2)));
    }

    #[test]
    fn a_displaced_peer_is_no_longer_treated_as_the_driver() {
        // The other half: once B has taken over and A's keys have been released, A
        // disconnecting must not trigger a second release_all that would drop the
        // chord B is legitimately holding.
        let mut driving = DrivenBy::default();
        let _ = driving.took_control(node(2));
        let _ = driving.took_control(node(3));
        assert!(!driving.let_go(node(2)));
        assert!(driving.let_go(node(3)));
    }

    #[test]
    fn a_duplicate_sessions_death_does_not_close_the_session_that_was_kept() {
        // Both ends dial at once: the first session is kept, the second is closed as
        // a duplicate, and the second pump then reports its connection gone. Keyed
        // only by node id, that report closed the healthy session that was retained.
        let kept = 1u64;
        let duplicate = 2u64;
        assert!(!teardown_is_current(Some(kept), duplicate));
        assert!(teardown_is_current(Some(kept), kept));
    }

    #[test]
    fn a_replaced_sessions_death_does_not_close_the_session_that_replaced_it() {
        // The deterministic case: `begin_pairing` closes the current session and
        // redials immediately, so the old pump's report arrives after the new
        // session is installed. Unqualified, it tore down the pairing that had just
        // started, every time.
        let old = 7u64;
        let new = next_session_generation();
        assert!(!teardown_is_current(Some(new), old));
    }

    #[test]
    fn an_ordinary_disconnect_is_still_honoured_when_no_session_remains() {
        // The common path must not be lost to the generation check: the cursor still
        // has to be rescued from a peer whose link has already been removed.
        assert!(teardown_is_current(None, 42));
    }

    #[test]
    fn session_generations_are_never_reused() {
        let a = next_session_generation();
        let b = next_session_generation();
        assert_ne!(a, b);
    }

    #[test]
    fn a_restarted_pairings_code_outlives_the_session_it_replaced() {
        // The deterministic failure: the other machine dialled first (which it does
        // whenever it is in pairing mode), so a session already exists when the user
        // presses Pair. `begin_pairing` shows a code and closes that session, and the
        // close is local — its teardown reaches the wake queue microseconds later,
        // while the redial needs a whole QUIC plus application handshake. Clearing
        // the code on the teardown meant the redialled session always found none and
        // abandoned the pairing with "no pairing code was generated", after the user
        // had already read the code off the screen.
        let mut pins = OfferedPins::default();
        pins.offer(node(2), Pin::parse("123456").expect("six digits is a PIN"));
        pins.on_session_ended(node(2));
        assert!(
            pins.claim(node(2)).is_some(),
            "the connection the code was generated for must still find it"
        );
    }

    #[test]
    fn a_code_does_not_survive_the_connection_it_was_used_on() {
        // The other half: once the code has been bound to a session, a later
        // teardown must leave nothing behind for an unrelated future session to
        // claim and quietly show a peer a code the user cannot see any more.
        let mut pins = OfferedPins::default();
        pins.offer(node(2), Pin::parse("123456").expect("six digits is a PIN"));
        assert!(pins.claim(node(2)).is_some());
        pins.on_session_ended(node(2));
        assert!(pins.claim(node(2)).is_none());
    }

    #[test]
    fn cancelling_a_pairing_discards_the_code_even_before_the_dial_lands() {
        // `abandon_pairing` is the user saying no, or a timeout, or a mismatch. The
        // code must go whatever the dial is doing, or the next connection would
        // resurrect a pairing that has already been refused.
        let mut pins = OfferedPins::default();
        pins.offer(node(2), Pin::parse("123456").expect("six digits is a PIN"));
        pins.discard(node(2));
        assert!(pins.claim(node(2)).is_none());
    }

    #[test]
    fn a_teardown_for_one_peer_does_not_touch_another_peers_code() {
        let mut pins = OfferedPins::default();
        pins.offer(node(2), Pin::parse("111111").expect("six digits is a PIN"));
        pins.discard(node(2));
        pins.offer(node(3), Pin::parse("222222").expect("six digits is a PIN"));
        pins.on_session_ended(node(2));
        assert!(pins.claim(node(3)).is_some());
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_sheds_motion_and_is_then_given_up_on() {
        // The failure: this queue was unbounded, so a peer that kept its connection
        // alive without draining it grew the queue without limit — every scroll,
        // every key, and whole clipboard payloads. Both halves of the policy matter.
        // Shedding absolute motion is free, because the next position corrects it;
        // shedding a key transition is not, so once nothing droppable is left the
        // session has to be given up rather than allowed to grow.
        let (tx, _rx) = mpsc::channel::<Outbound>(OUTBOUND_QUEUE_DEPTH);
        let motion = || {
            Outbound::Input(InputFrame::new(
                1,
                MonitorId(0),
                InputEvent::Pointer(PointerEvent::MoveTo {
                    pos: NormPos::new(0.5, 0.5),
                }),
            ))
        };
        for _ in 0..OUTBOUND_QUEUE_DEPTH {
            assert_eq!(enqueue(&tx, motion()), Queued::Sent);
        }
        assert_eq!(
            enqueue(&tx, motion()),
            Queued::ShedMotion,
            "a full queue must drop superseded motion rather than grow"
        );

        let key = Outbound::Input(InputFrame::new(
            2,
            MonitorId(0),
            InputEvent::Key(KeyEvent::text("a", KeyAction::Release, Modifiers::NONE)),
        ));
        assert_eq!(
            enqueue(&tx, key),
            Queued::Unresponsive,
            "a key release must never be silently dropped"
        );
        assert_eq!(
            enqueue(&tx, Outbound::Control(ControlMsg::LayoutRequest)),
            Queued::Unresponsive
        );
    }

    #[tokio::test]
    async fn a_dead_send_queue_is_reported_rather_than_treated_as_a_stall() {
        // A closed queue means the session is already finished and its pump is about
        // to say so. Confusing it with an unresponsive peer would close sessions
        // twice on every ordinary disconnect.
        let (tx, rx) = mpsc::channel::<Outbound>(4);
        drop(rx);
        assert_eq!(
            enqueue(&tx, Outbound::Control(ControlMsg::LayoutRequest)),
            Queued::AlreadyClosed
        );
    }

    #[tokio::test]
    async fn one_peers_inbound_backlog_cannot_grow_past_the_cap() {
        // The pump used to forward into an unbounded queue, so the transport's own
        // 1024-deep cap bought nothing: a peer writing faster than input can be
        // injected grew memory without limit. A permit per queued event is what
        // makes the pump stop reading instead.
        let permits = inbound_permits();
        let mut in_flight: Vec<_> = (0..INBOUND_QUEUE_DEPTH)
            .map(|_| {
                Arc::clone(&permits)
                    .try_acquire_owned()
                    .expect("the cap must allow a full window of events")
            })
            .collect();
        assert!(
            Arc::clone(&permits).try_acquire_owned().is_err(),
            "the pump would have queued a {}th event with the engine still behind",
            INBOUND_QUEUE_DEPTH + 1
        );
        // And the engine finishing with one event lets exactly one more through.
        in_flight.pop();
        assert!(Arc::clone(&permits).try_acquire_owned().is_ok());
    }

    #[test]
    fn a_bootstrapped_layout_puts_the_cursor_on_this_machine() {
        let layout = autolayout::bootstrap(node(1), &[mon(0, 0, 1920)]);
        let cursor = VirtualCursor::anywhere(&layout).expect("no cursor could be placed");
        assert_eq!(cursor.monitor().node, node(1));
    }
}
