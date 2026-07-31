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
//! Blocking clipboard work is the other exception, for the same reason and with a
//! worse worst case. Reading the clipboard on Wayland is a portal round trip and a
//! pipe transfer with a ten-second ceiling, and zstd over tens of megabytes is not
//! free either; run here, the cost is the user's keyboard doing nothing for the
//! duration, which on a KVM is the worst failure this program has. It runs on a
//! thread that owns the platform backend and no engine state at all, and reports
//! back as an ordinary [`Wake`] — so what the single-loop design actually promises
//! is kept: every piece of engine state is owned by this task, with no lock around
//! any of it. See [`spawn_clipboard_worker`].
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
use wx_proto::codec::MAX_CLIPBOARD_BYTES;
use wx_proto::{
    Capabilities, ClipboardFormat, Compression, ControlMsg, GlobalMonitorId, InputEvent,
    InputFrame, KeyAction, KeyPayload, Layout, Monitor, MonitorId, NodeId, NodeInfo, NormPos,
    Point, PointerEvent, RejectReason, Reliability, SequenceGate,
};

use crate::clipboard::{self, ClipboardSync, LocalChange, Serve};
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

/// How often the local clipboard is checked for a change.
///
/// Faster than [`TICK`] because of what the delay is measured against: the user
/// presses Ctrl-C on one machine and Ctrl-V on the other, and anything they can
/// feel between the two reads as the feature not working. Two seconds is easily
/// long enough to lose that race.
///
/// Affordable at this rate only because
/// [`ClipboardAccess::change_serial`](wx_platform::traits::ClipboardAccess::change_serial)
/// exists: it is an atomic load on Wayland and one Win32 call on Windows. Polling
/// `read` and hashing instead would pull the clipboard's whole contents across a
/// process boundary twice a second to learn that nothing happened.
const CLIPBOARD_POLL: Duration = Duration::from_millis(400);

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

/// How many clipboard messages may be waiting for one peer.
///
/// Its own queue, feeding its own task and its own QUIC stream, for the reason
/// given at [`wx_net`]'s clipboard stream tag: a payload of tens of megabytes must
/// not be able to hold up a handoff. The queue depth is the other half of that.
/// Left in [`OUTBOUND_QUEUE_DEPTH`] a clipboard payload would be a message like
/// any other, and [`Outbound::is_sheddable`] says no control message may be
/// dropped — so 256 of them arriving during one slow transfer would return
/// [`Queued::Unresponsive`] and **close a session that was working perfectly**.
///
/// Shallow because the traffic is: one offer per copy, one request and one reply
/// per transfer. Anything past this is a peer that has stopped reading its
/// clipboard stream, and the right answer is then to drop the clipboard message
/// and keep the session — which is exactly the trade the input plane cannot make
/// and this one can. Losing an offer costs one paste; losing the session costs the
/// user their cursor.
const CLIPBOARD_QUEUE_DEPTH: usize = 8;

/// How many blocking clipboard jobs may be waiting for the worker thread.
///
/// Shallow for the same reason [`CLIPBOARD_QUEUE_DEPTH`] is, and one step further:
/// a full queue here means the OS clipboard itself has stopped answering, and the
/// useful response is to drop the job rather than to hold megabytes of payload
/// waiting for a portal that may never reply. Deep enough that one peer's transfer
/// in flight does not cost the next peer's, shallow enough that a wedged backend
/// cannot accumulate work.
const CLIPBOARD_JOB_DEPTH: usize = 4;

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

/// Dials outstanding, for the same reason sessions carry a generation.
static NEXT_DIAL: AtomicU64 = AtomicU64::new(1);

fn next_dial_id() -> u64 {
    NEXT_DIAL.fetch_add(1, Ordering::Relaxed)
}

/// Whether a dial's failure describes the attempt this machine is still waiting on.
///
/// [`teardown_is_current`] one layer down, and needed for the same reason:
/// `begin_pairing` clears `dialing` so a second attempt may start while the first
/// task is still working through its addresses, which takes seconds of connect
/// timeouts. The first task's failure would otherwise be read as the second's —
/// discarding the code the user is looking at right now and failing its card for a
/// machine that is answering.
///
/// Unlike a teardown, a failure with nothing outstanding is *not* honoured. The
/// only thing that removes the entry without replacing it is a session being
/// installed for that peer, and a superseded dial may neither mark a connected
/// peer down nor take the code the live attempt is waiting to bind.
fn dial_is_current(current: Option<u64>, reported: u64) -> bool {
    current == Some(reported)
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
        /// Which attempt failed. See [`dial_is_current`].
        dial: u64,
        error: String,
        /// Whether this was the dial a pairing was waiting on. A reconnect dial
        /// that fails while a pairing dial to the same peer is in flight must not
        /// be mistaken for the pairing's own failure; see [`OfferedPins`].
        pairing: bool,
    },
    Discovery(DiscoveryEvent),
    Ipc(IpcCommand),
    Tick,
    /// Check that the machine holding the cursor is still answering.
    Probe,
    /// Check whether anything was copied on this machine.
    ClipboardPoll,
    /// Blocking clipboard work finished. See [`spawn_clipboard_worker`].
    Clipboard(ClipboardDone),
    /// Ctrl-C, a service stop, or an IPC shutdown request.
    Shutdown,
}

/// Clipboard work that must not run on the engine loop.
///
/// Everything here either blocks on the OS or spends real CPU on a payload of up
/// to [`MAX_CLIPBOARD_BYTES`]. Each job carries the decision the loop already made
/// and nothing else — no engine state crosses to the worker, and none comes back
/// except as a [`ClipboardDone`] the loop applies itself.
enum ClipboardJob {
    /// Sample the clipboard, reading the write-back format only if telling this
    /// machine's own write from a real copy still needs the bytes.
    Poll {
        /// The serial the loop has already accounted for.
        seen: Option<u64>,
        /// The write-back guard, from [`ClipboardSync::armed`].
        armed: Option<(ClipboardFormat, u64)>,
    },
    /// Read and pack the content a peer asked for.
    Serve {
        node: NodeId,
        format: ClipboardFormat,
        serial: u64,
    },
    /// Unpack a peer's payload and put it on this machine's clipboard.
    Accept {
        node: NodeId,
        format: ClipboardFormat,
        compression: Compression,
        data: Vec<u8>,
    },
}

impl ClipboardJob {
    fn kind(&self) -> ClipboardJobKind {
        match self {
            ClipboardJob::Poll { .. } => ClipboardJobKind::Poll,
            ClipboardJob::Serve { .. } => ClipboardJobKind::Serve,
            ClipboardJob::Accept { .. } => ClipboardJobKind::Accept,
        }
    }
}

/// Which kind of job a dispatch or a report belongs to.
///
/// Separate from the job itself so that [`ClipboardTraffic`] can account for one
/// without holding a payload, and so the accounting is the same value on the way
/// out and on the way back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardJobKind {
    Poll,
    Serve,
    Accept,
}

/// What the loop has outstanding with the clipboard worker.
///
/// # The rule this exists to keep
///
/// **A poll must never reach the worker between a write being armed and the
/// serial that write produced coming back.** The worker is FIFO and owns no
/// engine state, so a `Poll` job carries the write-back guard
/// ([`ClipboardSync::armed`]) as it stood when the loop *dispatched* it. Dispatch
/// a poll while a write is still with the worker and that snapshot is `None`,
/// because the loop has not applied [`ClipboardDone::Writing`] yet — and the
/// worker then runs the write first and samples afterwards, with nothing telling
/// it to read the format back.
///
/// That is not a cosmetic ordering. Usually the poll lands on the same serial the
/// write reported and [`LocalChange::Echo`] catches it anyway; but the Wayland
/// portal moves the serial a second time when it echoes `SelectionOwnerChanged`,
/// and if that lands after the write's own `change_serial` the poll sees a serial
/// the guard does not name, has no digest to check it against, clears the guard
/// and offers the peer its own payload straight back — tens of megabytes of it.
/// Holding the poll until every write has reported keeps every `armed` snapshot
/// current, which is the whole of the fix.
///
/// Pure and separate from the engine so that rule is stated once and can be
/// tested without a desktop.
#[derive(Debug, Default)]
struct ClipboardTraffic {
    /// A poll is with the worker and has not reported.
    polling: bool,
    /// Writes dispatched and not yet reported. A count rather than a flag because
    /// two peers can each be having their payload written at the same time.
    writes: usize,
    /// The worker is gone and nothing will report again. See
    /// [`ClipboardDone::WorkerGone`].
    lost: bool,
}

impl ClipboardTraffic {
    /// Whether a fresh poll may be sent right now.
    fn may_poll(&self) -> bool {
        !self.lost && !self.polling && self.writes == 0
    }

    fn dispatched(&mut self, kind: ClipboardJobKind) {
        match kind {
            ClipboardJobKind::Poll => self.polling = true,
            ClipboardJobKind::Accept => self.writes += 1,
            ClipboardJobKind::Serve => {}
        }
    }

    fn settled(&mut self, kind: ClipboardJobKind) {
        match kind {
            ClipboardJobKind::Poll => self.polling = false,
            ClipboardJobKind::Accept => self.writes = self.writes.saturating_sub(1),
            ClipboardJobKind::Serve => {}
        }
    }

    fn is_lost(&self) -> bool {
        self.lost
    }

    /// The worker will never report again.
    ///
    /// Clears what was outstanding as well as latching the fact, because those
    /// jobs died with it: a `polling` flag left standing would make [`may_poll`]
    /// false for the rest of the run, and local copies would then stop being
    /// offered with nothing anywhere to say why.
    ///
    /// [`may_poll`]: ClipboardTraffic::may_poll
    fn worker_lost(&mut self) {
        self.polling = false;
        self.writes = 0;
        self.lost = true;
    }
}

/// A payload read from the clipboard and made ready for the wire.
struct Packed {
    compression: Compression,
    payload: Vec<u8>,
    /// Size before compression, for the log line that says what the transfer cost.
    read: usize,
}

/// What the clipboard worker hands back to the loop.
enum ClipboardDone {
    /// The poll found nothing for the loop to decide: either the clipboard could
    /// not be sampled — ordinary on a Wayland session whose clipboard grant was
    /// refused — or the serial has not moved, which is the usual answer.
    NothingNew,
    Polled {
        serial: u64,
        formats: Vec<ClipboardFormat>,
        /// Fingerprint of the write-back format, if it was worth reading.
        digest: Option<u64>,
    },
    /// `None` for anything that could not be served — an unreadable clipboard, or
    /// content too large for the protocol. Both are answered `ClipboardStale`.
    Served {
        node: NodeId,
        format: ClipboardFormat,
        serial: u64,
        packed: Option<Packed>,
    },
    /// A peer's payload is unpacked and the write is about to start.
    ///
    /// Sent *before* the write rather than with its result, and that ordering is
    /// half of the echo suppression: the guard has to be armed before the change it
    /// suppresses can be observed, and both this and [`Wake::ClipboardPoll`] arrive
    /// on the one wake queue in send order.
    ///
    /// The other half is [`ClipboardTraffic`], and it is not optional. Send order
    /// alone only settles when the loop *handles* a poll; what decides what that
    /// poll can see is the guard snapshot taken when the loop *dispatched* it, and
    /// a poll dispatched while this write was still in flight carries none.
    Writing {
        format: ClipboardFormat,
        digest: u64,
    },
    Wrote {
        node: NodeId,
        format: ClipboardFormat,
        bytes: usize,
        /// The serial the write produced, where the backend could report one.
        serial: Option<u64>,
    },
    /// The payload never reached the clipboard, so there is nothing of ours on it.
    NotWritten {
        /// Whether [`ClipboardDone::Writing`] was already sent for this payload,
        /// which decides whether a guard is standing that must now be cleared.
        armed: bool,
    },
    /// The worker thread has stopped, and nothing it was holding will report.
    ///
    /// Sent by a `Drop` guard rather than at the end of the loop, so that it is
    /// sent on the unwind path too: a panic inside a platform backend leaves the
    /// worker's queued jobs unreported, and the loop would otherwise go on
    /// believing a poll was still in flight and never send another.
    WorkerGone,
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
    /// transitions, relative motion, handoff — and dropping one is a stuck
    /// modifier or a lost keystroke.
    ///
    /// Clipboard messages are not on this queue at all, and that is deliberate:
    /// they are the one kind of traffic large enough to fill it, and the only kind
    /// whose loss is survivable. They go through [`CLIPBOARD_QUEUE_DEPTH`] instead,
    /// where being dropped is an option.
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
    /// The clipboard's own queue. See [`CLIPBOARD_QUEUE_DEPTH`].
    clipboard: mpsc::Sender<ControlMsg>,
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
    awaiting_dial: HashMap<NodeId, AwaitingDial>,
}

/// A pairing that exists only as a code on screen so far.
///
/// Carried because such an exchange is in no session and in no `pending` entry,
/// and a window still has to be able to draw it and to tell when it is over: see
/// [`pending_pairing_snapshots`].
struct AwaitingDial {
    name: String,
    since: Instant,
}

impl OfferedPins {
    /// Record a code for a connection that is about to be dialled.
    fn offer(&mut self, node: NodeId, pin: Pin, name: String, since: Instant) {
        self.pins.insert(node, pin);
        self.awaiting_dial
            .insert(node, AwaitingDial { name, since });
    }

    /// Take the code for a session that has just come up.
    ///
    /// Only a code that is still waiting for its own dial, which is the same test
    /// [`OfferedPins::discard_awaiting_dial`] makes and for the same reason. A code
    /// left behind by an exchange that has already ended is not the one the user is
    /// reading off the screen, and opening a pairing with it would ask the other
    /// machine for digits nobody is being shown.
    fn claim(&mut self, node: NodeId) -> Option<Pin> {
        if self.awaiting_dial.remove(&node).is_none() {
            self.pins.remove(&node);
            return None;
        }
        self.pins.remove(&node)
    }

    /// Forget the code: the pairing is over, cancelled, or refused.
    fn discard(&mut self, node: NodeId) {
        self.awaiting_dial.remove(&node);
        self.pins.remove(&node);
    }

    /// Forget a code whose own connection was never made, reporting whether
    /// there was one. Narrower than [`OfferedPins::discard`] on purpose: only a
    /// code still waiting for its dial can belong to a dial that just failed.
    fn discard_awaiting_dial(&mut self, node: NodeId) -> bool {
        if self.awaiting_dial.remove(&node).is_none() {
            return false;
        }
        self.pins.remove(&node);
        true
    }

    /// A session for `node` ended. Keeps a code whose own connection has not
    /// happened yet; see the type's documentation for why that matters.
    fn on_session_ended(&mut self, node: NodeId) {
        if !self.awaiting_dial.contains_key(&node) {
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

/// Drop a pairing that cannot go on, and tell the UI it is over.
///
/// Free rather than a method on the engine so that both of its callers are
/// obliged to use the same rule, and so that a test can reach it: an engine
/// needs a platform backend, a config directory and the network to exist, and
/// the behaviour worth pinning here is one map removal and one event.
///
/// The event is the UI's only expiry. `store.pairing` in the frontend is set by
/// `pairingRequested` and cleared by nothing else, so a pairing the agent has
/// silently forgotten leaves that window holding a card it will never take down
/// — and, because it holds one, dropping every later request. Every path that
/// removes a `pending` entry must therefore come through here, bar two that are
/// covered on their own terms: [`Engine::finish_pairing`], which removes the
/// entry and emits `PairingFinished { accepted: true }` itself, and
/// [`Engine::begin_pairing`], which removes it deliberately silently because the
/// same pairing is starting over rather than ending. Anything else that reaches
/// for `pending.remove` is a card left standing.
fn end_pending_pairing(
    pending: &mut HashMap<NodeId, PendingPairing>,
    events: &broadcast::Sender<Event>,
    node: NodeId,
    why: &str,
) -> bool {
    if pending.remove(&node).is_none() {
        return false;
    }
    let _ = events.send(Event::PairingFinished {
        node: node.to_hex(),
        accepted: false,
        message: Some(why.to_string()),
    });
    true
}

/// Give up a pairing whose dial never reached the other machine.
///
/// The counterpart to [`end_pending_pairing`] one step earlier in the exchange:
/// nothing is put into `pending` until a session comes up, so a dial that never
/// produces one leaves the card the window raised on the `pairingStarted` answer
/// with nothing that could ever end it — not this function's sibling, and not the
/// stale-pairing sweep, which only walks `pending`. Free, and returning whether it
/// announced anything, for the same reasons as [`end_pending_pairing`].
fn end_undialled_pairing(
    offered_pins: &mut OfferedPins,
    events: &broadcast::Sender<Event>,
    node: NodeId,
    why: &str,
) -> bool {
    if !offered_pins.discard_awaiting_dial(node) {
        return false;
    }
    let _ = events.send(Event::PairingFinished {
        node: node.to_hex(),
        accepted: false,
        message: Some(why.to_string()),
    });
    true
}

/// What a UI needs in order to draw the pairings already under way.
///
/// Free for the same reason as [`end_pending_pairing`]: it is the whole of what
/// [`ipc::StatusSnapshot::pairings`] is, and it is reachable from a test.
///
/// Every pairing this agent is holding, not only those that reached a session.
/// `begin_pairing` answers with a code and dials, and until that dial lands the
/// exchange lives in [`OfferedPins::awaiting_dial`] alone — a window told about
/// the code but not about this would have a card the snapshot never mentions,
/// and so no way to tell that a dial which never landed is over. That is the
/// reported symptom seen from the initiator's side, and it is the one thing the
/// list has to describe for a window without events to reconcile against.
fn pending_pairing_snapshots(
    pending: &HashMap<NodeId, PendingPairing>,
    offered: &OfferedPins,
) -> Vec<ipc::PendingPairingSnapshot> {
    let mut by_age: Vec<(Instant, NodeId, ipc::PendingPairingSnapshot)> = pending
        .values()
        .map(|p| {
            (
                p.started,
                p.node,
                ipc::PendingPairingSnapshot {
                    node: p.node.to_hex(),
                    name: p.name.clone(),
                    initiated_locally: p.initiated_locally,
                    // Only for the side that generated it. The responder also holds
                    // a `PairingSession` once the user has typed something, and that
                    // is the user's own guess — echoing it back as "the code" would
                    // show a second window a number nobody should be typing
                    // anywhere.
                    pin: p
                        .pairing
                        .as_ref()
                        .filter(|_| p.initiated_locally)
                        .map(|s| s.pin().as_str().to_string()),
                },
            )
        })
        .collect();
    by_age.extend(
        offered
            .awaiting_dial
            .iter()
            // A session for that peer got in first, and its entry is the same
            // exchange one step further on. Listing both would offer a window two
            // cards for one pairing.
            .filter(|(node, _)| !pending.contains_key(node))
            .map(|(node, dial)| {
                (
                    dial.since,
                    *node,
                    ipc::PendingPairingSnapshot {
                        node: node.to_hex(),
                        name: dial.name.clone(),
                        // Only this machine puts a code into `awaiting_dial`; the
                        // responder never has one before a session exists.
                        initiated_locally: true,
                        pin: offered.pins.get(node).map(|p| p.as_str().to_string()),
                    },
                )
            }),
    );
    // Oldest first, so a window that attaches late adopts the same pairing the
    // events would have offered it first, and so two windows agree. The instants
    // can tie between two exchanges admitted in the same moment; the node id
    // breaks it only to keep the order stable across status requests.
    by_age.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    by_age
        .into_iter()
        .map(|(_, _, snapshot)| snapshot)
        .collect()
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

/// Which edges of this machine's own screens the cursor can leave by.
///
/// The join between the two halves of the answer: the layout knows what is beyond
/// each screen, and only this machine knows where its screens are in its *own*
/// desktop space — which is the space a capture backend's barriers live in. A
/// monitor the layout does not place gets no exits at all, which is the same
/// answer the router gives it: `GlobalLayout::resolve_move` refuses to resolve any
/// crossing from a monitor it has never heard of, so an edge armed there could
/// only ever pin the pointer with nowhere to send it.
pub fn local_exits(
    layout: &GlobalLayout,
    node: NodeId,
    monitors: &[Monitor],
) -> Vec<wx_platform::ScreenExits> {
    monitors
        .iter()
        .map(|m| wx_platform::ScreenExits {
            bounds: m.local_bounds,
            edges: layout.exit_edges(GlobalMonitorId::new(node, m.id)),
        })
        .collect()
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
    /// Peers being dialled, against the id of the attempt in flight. Keyed by the
    /// attempt and not merely by the peer, because a second attempt may start
    /// while the first is still running; see [`dial_is_current`].
    dialing: HashMap<NodeId, u64>,
    events: broadcast::Sender<Event>,
    wake: mpsc::UnboundedSender<Wake>,
    /// Last value pushed to the capture backend, so it is only changed on a real
    /// transition — flipping the suppression flag loses events.
    suppressed: bool,
    /// Last exits pushed to the capture backend, for the same reason: on Wayland
    /// obeying one is a portal round trip that briefly disarms every barrier.
    exits: Vec<wx_platform::ScreenExits>,
    /// Key payload whose release must be swallowed because its press was consumed
    /// as a hotkey. Without it the peer sees a release for a key it never saw
    /// pressed.
    swallow_release: Option<KeyPayload>,
    /// The peer currently driving this machine, if any. See [`DrivenBy`].
    driven_by: DrivenBy,
    /// Clipboard offer/request state. See [`crate::clipboard`].
    clipboard: ClipboardSync,
    /// Where blocking clipboard work goes. See [`spawn_clipboard_worker`].
    clipboard_jobs: std::sync::mpsc::SyncSender<ClipboardJob>,
    /// What is outstanding with that worker, and the one ordering rule the loop
    /// has to keep around it. See [`ClipboardTraffic`].
    clipboard_traffic: ClipboardTraffic,
    last_owner: NodeId,
    /// Host firewall warning, decided once at startup and then reported for the
    /// life of the run. Held rather than recomputed per status request because
    /// changing it means the user ran `ufw` since, at which point they know.
    firewall: Option<String>,
    /// Why the last display enumeration failed, or `None` if it worked.
    ///
    /// Kept so that an empty monitor list can be told apart from a backend that
    /// could not answer — see [`ipc::StatusSnapshot::displays_error`], which is
    /// where it is reported and where the reasoning lives. Held on the engine
    /// rather than re-derived per status request because the failing call is the
    /// housekeeping poll, and asking the backend again to answer a status request
    /// would be a second chance to succeed and a second chance to hang.
    displays_error: Option<String>,
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
            let mut ticker = tokio::time::interval(CLIPBOARD_POLL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if wake.send(Wake::ClipboardPoll).is_err() {
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
        let mut platform = wx_platform::current_platform_in(&config_dir)?;
        // Moved out of the backend at once, because from here on the clipboard is
        // only ever touched by the worker thread. See [`spawn_clipboard_worker`].
        let clipboard_jobs = spawn_clipboard_worker(platform.take_clipboard(), wake.clone())?;
        // Not fatal. A node with no readable displays still forwards input to its
        // peers, and taking the whole machine out of the mesh over a transient
        // enumeration failure would be worse. But the failure is *kept*, not
        // collapsed into an empty list: "there are no displays" and "I cannot tell
        // you about displays" are different answers, and reporting the second as
        // the first is how a machine with a monitor attached came to describe
        // itself as headless.
        let (monitors, displays_error) = match platform.displays.monitors() {
            Ok(monitors) => (monitors, None),
            Err(e) => {
                tracing::warn!(error = %e, "could not enumerate displays");
                (Vec::new(), Some(e.to_string()))
            }
        };

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
            dialing: HashMap::new(),
            events,
            wake: wake.clone(),
            suppressed: false,
            exits: Vec::new(),
            swallow_release: None,
            driven_by: DrivenBy::default(),
            clipboard: ClipboardSync::new(),
            clipboard_jobs,
            clipboard_traffic: ClipboardTraffic::default(),
            last_owner: local,
            // Against the port actually bound, not the configured one: the two
            // differ when the config asks for port 0, and advice naming a port
            // nothing is listening on is worse than none.
            firewall: crate::firewall::warning(port),
            displays_error,
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
        // Before anything can cross an edge. A backend told nothing arms nothing,
        // which is the safe direction but would leave this machine unable to drive
        // until the first layout change.
        engine.sync_exits();
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
            Wake::DialFailed {
                node,
                dial,
                error,
                pairing,
            } => {
                if !dial_is_current(self.dialing.get(&node).copied(), dial) {
                    tracing::debug!(
                        peer = %node,
                        stale = dial,
                        current = ?self.dialing.get(&node),
                        error = %error,
                        "ignoring the failure of a dial that has already been superseded"
                    );
                    return;
                }
                self.dialing.remove(&node);
                if pairing {
                    end_undialled_pairing(
                        &mut self.offered_pins,
                        &self.events,
                        node,
                        "could not reach that machine",
                    );
                }
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
            Wake::ClipboardPoll => self.poll_clipboard(),
            Wake::Clipboard(done) => self.on_clipboard_done(done),
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

    /// Tell the capture backend which screen edges have a machine beyond them,
    /// when and only when the answer changes.
    ///
    /// The backend cannot work this out: adjacency is a fact about the *global*
    /// layout, and a platform backend sees no further than this machine's own
    /// screens. A backend that grabs the cursor at an edge — on Wayland a pointer
    /// barrier is exactly that — must therefore be told, or it grabs at every edge
    /// and the pointer is taken on three sides of a two-machine mesh.
    ///
    /// Pushed on a change rather than on a schedule, and the whole set rather than
    /// a delta, because "which edges are live" is derived state with one owner: any
    /// layout event, any display change, recompute and compare. On Wayland
    /// obeying it is a portal round trip, which is why it must not be pushed
    /// unchanged.
    fn sync_exits(&mut self) {
        let want = local_exits(
            self.router.layout(),
            self.local,
            self.state.local_monitors(),
        );
        if want == self.exits {
            return;
        }
        match self.platform.capture.set_exits(&want) {
            Ok(()) => self.exits = want,
            Err(e) => tracing::warn!(
                error = %e,
                "could not tell the capture backend which screen edges lead to another machine"
            ),
        }
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
        // Reported on every session, not only mismatched ones. "Which wire format
        // were these two actually speaking" is the first question a two-machine
        // bug report has to answer, and it cannot be inferred from the build
        // numbers — the point of negotiation is that the version in use may be
        // neither machine's newest. Both numbers, so a negotiated-down session is
        // visible as such rather than looking like a matched pair.
        let protocol = established.protocol;
        let peer_protocol = established.peer_protocol;
        self.state.on_session(info.clone(), trusted, now);
        {
            let trust = self.trust.lock().expect("trust store lock");
            let config = self.config.clone();
            self.state.sync_trust(&trust, &config, now);
        }

        let out = spawn_sender(session.clone());
        let clipboard = spawn_clipboard_sender(session.clone());
        self.sessions.insert(
            node,
            PeerLink {
                session,
                out,
                clipboard,
                generation,
            },
        );
        self.gates.insert(node, SequenceGate::new());
        self.last_heard.insert(node, now);

        if trusted {
            tracing::info!(peer = %node, %name, protocol, peer_protocol, "session established");
            self.on_peer_ready(node, &info).await;
        } else {
            tracing::info!(
                peer = %node,
                %name,
                protocol,
                peer_protocol,
                "session established for pairing only"
            );
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

        // Say what this machine can do *now*, not what it could when the handshake
        // snapshot was taken.
        //
        // The two are routinely different, and the gap is not small. The accept loop
        // clones `local_info` at the top of each iteration — before it awaits the
        // next connection — so the `NodeInfo` a peer receives can be arbitrarily old,
        // and on Wayland it is nearly always older than the portal grant: the consent
        // dialog is answered seconds or minutes after the process starts, and the
        // first peer usually connects in that window. `sync_capabilities` does not
        // close it, because by the time the session exists the local capabilities
        // have already finished changing and it only speaks on a transition.
        //
        // Measured on this desktop: alpha's grant landed two seconds before bravo
        // connected, so bravo spent the whole session believing alpha could not take
        // clipboard content, and refused to offer it any.
        //
        // Sent unconditionally rather than only when it looks stale, because there
        // is nothing here to compare it against: `info` is what the *peer* said
        // about itself, and the snapshot this machine actually handed over is a
        // clone the accept loop made and nobody kept. Comparing against the peer's
        // own set is worse than not comparing at all — on two identically configured
        // machines it matches, the correction is skipped, and the stale snapshot
        // stands for the whole session. The cost of always sending is one small
        // control message per session: the receiver's `set_peer_capabilities` is a
        // no-op when nothing changed, so a peer whose snapshot was current logs
        // nothing.
        //
        // Only to a peer that can decode it, which is what `CAPABILITY_UPDATES`
        // says: a variant a build does not have is a decode error, and a decode
        // error on the control stream closes the session.
        let local = self.advertised_by(self.local);
        if let Some(correction) = capability_correction(self.advertised_by(node), local) {
            tracing::debug!(
                peer = %node,
                local = %local.describe(),
                "telling a peer what this machine can do now"
            );
            self.send_to(node, Outbound::Control(correction));
        }

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
                self.on_peer_gone(node, None, None).await;
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
            ControlMsg::ClipboardOffer { formats, serial } => {
                self.on_clipboard_offer(node, formats, serial)
            }
            ControlMsg::ClipboardRequest { format, serial } => {
                self.on_clipboard_request(node, format, serial)
            }
            ControlMsg::ClipboardData {
                format,
                serial,
                compression,
                data,
            } => self.on_clipboard_data(node, format, serial, compression, data),
            ControlMsg::ClipboardStale { serial } => {
                // The content moved on before the request landed. Nothing to do
                // but stop waiting for it: the peer's next copy produces a fresh
                // offer, and asking again for a serial it has already declared gone
                // would loop.
                tracing::debug!(peer = %node, serial, "a peer's clipboard content was already superseded");
                self.clipboard.settled(node);
            }
            ControlMsg::VideoStop { .. } | ControlMsg::VideoUnavailable { .. } => {
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
        // No user-facing copy: a pump's reason is whatever `wx_net` wrote about the
        // stream, which belongs in the log and in the peer's state and nowhere a
        // person reads.
        self.on_peer_gone(node, reason, None).await;
    }

    /// A session ended. The cursor may be on the other side of it.
    ///
    /// `reason` is for logs and peer state and may be a raw transport error.
    /// `told_to_user` is the separate channel for copy somebody wrote for a person
    /// to read, and is the only one a card is allowed to show: "reading from a
    /// stream: connection lost" is a diagnostic, not an explanation. Callers that
    /// have no such copy pass `None` and get the written sentence below.
    async fn on_peer_gone(
        &mut self,
        node: NodeId,
        reason: Option<String>,
        told_to_user: Option<&str>,
    ) {
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
        // Announced, not merely dropped: the exchange is over for the UI too, and
        // it has no other way to find that out. See [`end_pending_pairing`]. The
        // caller's own copy wins where there is any: a peer the user disabled
        // or unpaired themselves did not lose its connection, and saying so sends
        // them looking for a network fault they do not have.
        let ended_a_pairing = end_pending_pairing(
            &mut self.pending,
            &self.events,
            node,
            told_to_user.unwrap_or("the connection was lost"),
        );
        // The request outstanding with that peer dies with the session, and leaving
        // it recorded is not merely untidy. It would let the peer's *next* session
        // set this machine's clipboard with no offer in front of it, because a
        // `ClipboardData` matching the remembered (serial, format) still answers a
        // request nothing is waiting for. It also strands the reverse case: the
        // change serial is process-local and restarts at zero, so a restarted peer
        // re-offering the same pair would be deduplicated against a request that no
        // longer exists and never asked about again.
        self.clipboard.settled(node);
        // Not unconditionally: a code generated for a dial that is still in flight
        // belongs to the *next* connection, not the one that just died. See
        // [`OfferedPins`] — clearing it here broke every restarted pairing.
        //
        // Unless the exchange that code belongs to is the one that just ended: a
        // code that outlives its own pairing is published as a pairing still under
        // way, and nothing can ever take it back out of the list again — the card
        // the window raises from it then blocks every later request. That is the
        // failure this trade buys off. `begin_pairing`'s restart is not this case:
        // it takes its entry out of `pending` before closing the session, so the
        // teardown ends nothing and the code survives for the redial as before.
        //
        // The trade, stated honestly, because a future change will read this: the
        // discard is keyed on a pairing having ended, which is not the same as this
        // code having no owner left, and one path can still have an owner. When the
        // peer's own session lands first — the cross-initiation case
        // [`OfferedPins`] calls normal — `on_session` clears `dialing` and inserts a
        // `pending` entry that never claims the code, because only the
        // `initiated_locally` branch claims. Our dial is still working through its
        // addresses. If that peer-initiated session then drops, this discards a code
        // that dial would have claimed, and the session it eventually brings up
        // abandons the pairing with "no pairing code was generated" after the user
        // has already read the digits off the screen. It ends with a reason and the
        // user can press Pair again, which is why it is preferred to a card nothing
        // can end; telling the two apart needs a dial's own resolution tracked
        // separately from session installation, which belongs with the deferred
        // question of who owns a pairing card's lifetime rather than here.
        if ended_a_pairing {
            self.offered_pins.discard(node);
        } else {
            self.offered_pins.on_session_ended(node);
        }
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
        // The exchange is over, so no code for it may outlive it: one still held
        // here is published as a pairing under way, and there is nothing left that
        // could ever take it back out of the list. Matches `abandon_pairing`, which
        // is the same transition with the other outcome.
        self.offered_pins.discard(node);
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
        end_pending_pairing(&mut self.pending, &self.events, node, why);
        self.offered_pins.discard(node);
        // The session was only ever admitted for pairing, so it has no further
        // purpose and must not be left open to an untrusted peer.
        if !self.state.is_reachable(node) {
            if let Some(link) = self.sessions.remove(&node) {
                link.session.close(why);
                self.state.clear_dropped_datagrams(node);
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
        let monitors = match self.platform.displays.monitors() {
            Ok(monitors) => {
                if self.displays_error.take().is_some() {
                    tracing::info!("display enumeration is answering again");
                }
                Some(monitors)
            }
            Err(e) => {
                // Recorded once per distinct reason rather than on every tick. A
                // backend that cannot enumerate generally stays that way, and this
                // poll runs on a timer — the same unbounded-warning shape that made
                // suppression's refusal fill the log.
                let reason = e.to_string();
                if self.displays_error.as_deref() != Some(reason.as_str()) {
                    tracing::warn!(error = %e, "could not enumerate displays");
                    self.displays_error = Some(reason);
                }
                // The monitor list is deliberately left as it was. "I cannot tell
                // you" is not "there are none", and clearing it would drop this
                // machine out of every peer's layout over one failed poll — taking
                // the cursor with it if it happened to be here.
                None
            }
        };
        if let Some(monitors) = monitors {
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

        // Not only inside the `set_local_monitors` branch above: `adopt_layout` is
        // reached from there only when a placement is missing, so a display that
        // was unplugged or resized leaves the recorded exits describing a screen
        // that is no longer there. Recomputed here, where the monitor list is
        // freshest; unchanged answers cost nothing.
        self.sync_exits();

        self.sync_capabilities().await;

        // Round-trip times, for the UI and for diagnosing a slow link, and the
        // discarded-datagram counters beside them: both are cheap reads off a live
        // session, and sampling them together keeps the window they describe the
        // same one.
        let readings: Vec<(NodeId, Duration, u64)> = self
            .sessions
            .iter()
            .map(|(node, link)| (*node, link.session.rtt(), link.session.dropped_datagrams()))
            .collect();
        for (node, rtt, dropped) in readings {
            self.state.set_rtt(node, rtt);
            self.note_dropped_datagrams(node, dropped, now);
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

    /// Sample one session's discarded-datagram counter, and say so in the log for
    /// as long as it is moving.
    ///
    /// The snapshot field alone would not do. A monotonic total tells a reader that
    /// something happened at some point, and the question during a test run is
    /// whether it is happening *now* and whether it is getting worse — which needs
    /// either a delta or a rate, observed live. So this emits a line per tick while
    /// drops are arriving and one when they stop, and is otherwise silent: an
    /// episode has visible edges in a `journalctl -f` tail, and a quiet log means
    /// no drops rather than nobody looking.
    ///
    /// The wording names this machine's input queue and not the network on purpose.
    /// The counter cannot see network loss at all — see
    /// [`crate::state::DroppedDatagrams`] — so a line that said "packets lost"
    /// would point whoever read it at the opposite of the cause.
    ///
    /// `warn` rather than `info` because the condition is a defect by construction:
    /// dropping motion because the wire lost it is the design, dropping motion that
    /// already arrived is the input loop falling behind.
    ///
    /// `window_ms` is the interval the state measured between this sample and the
    /// last, not [`TICK`]. This loop is the one place the difference bites: a tick
    /// is dispatched from the same serial wake channel as the peer events whose
    /// backlog causes the drops, so the tick that finds a non-zero count is one
    /// that queued behind that backlog and covers longer than `TICK`. Printing the
    /// constant would report a rate up to half again what was measured, on the one
    /// line somebody is reading as a measurement.
    fn note_dropped_datagrams(&mut self, node: NodeId, total: u64, now: Instant) {
        let was_dropping = self
            .state
            .peer(&node)
            .and_then(|p| p.dropped_datagrams)
            .is_some_and(|d| d.recent > 0);
        let Some(reading) = self.state.sample_dropped_datagrams(node, total, now) else {
            return;
        };
        if reading.recent > 0 {
            tracing::warn!(
                peer = %node,
                dropped = reading.recent,
                window_ms = reading.window.as_millis() as u64,
                session_total = reading.total,
                "input datagrams from a peer are being discarded; this machine's input queue is full"
            );
        } else if was_dropping {
            tracing::info!(
                peer = %node,
                session_total = reading.total,
                "input datagrams from a peer are no longer being discarded"
            );
        }
    }

    /// Dial a peer, trying each address in turn.
    fn dial(&mut self, node: NodeId, addresses: Vec<SocketAddr>, pairing_mode: bool) {
        if self.sessions.contains_key(&node) || self.dialing.contains_key(&node) {
            return;
        }
        let dial = next_dial_id();
        self.dialing.insert(node, dial);
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
            let _ = wake.send(Wake::DialFailed {
                node,
                dial,
                error: last,
                pairing: pairing_mode,
            });
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
        self.sync_exits();

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
                    pending_pairing_snapshots(&self.pending, &self.offered_pins),
                    self.displays_error.as_deref(),
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
                    self.on_peer_gone(
                        parsed,
                        Some("disabled".into()),
                        Some("that machine was disabled here"),
                    )
                    .await;
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
        let name = peer.display_name().to_string();
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
        self.offered_pins.offer(node, pin, name, Instant::now());
        // A stale session from a previous attempt would carry the wrong nonces, so
        // the exchange always starts from a fresh connection.
        if let Some(link) = self.sessions.remove(&node) {
            link.session.close("restarting pairing");
            // This path deliberately leaves the peer's status and backoff alone —
            // the same pairing is starting again, not a connection ending — but the
            // drop reading belonged to the session just closed, and left behind it
            // reports a window of loss on a connection that is gone.
            self.state.clear_dropped_datagrams(node);
        }
        // Silently, unlike every other removal: this is the same pairing starting
        // again, not one ending. A `PairingFinished` here would race the
        // `pairingStarted` answer below and put the window's fresh card into the
        // failed state. See [`end_pending_pairing`].
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
        // The pairing card's "Block this machine" button reaches this while the
        // exchange is still pending, so the machine on the other end of the
        // announcement was never paired at all: telling it that it was unpaired
        // here is a sentence a CLI client or a second window has no way to correct.
        self.on_peer_gone(
            node,
            Some("no longer paired".into()),
            Some(if block {
                "that machine was blocked here"
            } else {
                "that machine was unpaired here"
            }),
        )
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

    // -- clipboard --------------------------------------------------------

    /// Hand one clipboard message to a peer's clipboard queue.
    ///
    /// The one thing this must never do is what [`Engine::send_to`] does when the
    /// main queue fills: report the peer unresponsive and close the session. A
    /// clipboard transfer is the largest thing this agent sends and the only thing
    /// it can afford to lose, so a full queue drops the message and says so.
    fn send_clipboard_to(&mut self, node: NodeId, msg: ControlMsg) -> bool {
        let Some(link) = self.sessions.get(&node) else {
            tracing::debug!(peer = %node, "no session; dropping a clipboard message");
            return false;
        };
        match link.clipboard.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Named at warn: a clipboard that silently stops working is the
                // failure this whole slice is most likely to present as.
                tracing::warn!(
                    peer = %node,
                    machine = %self.peer_label(node),
                    "the clipboard queue for this peer is full; dropping the message rather than the session"
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!(peer = %node, "dropping a clipboard message for a closed session");
                false
            }
        }
    }

    /// Whether this machine will exchange clipboard content with a peer at all.
    ///
    /// Three gates, and each closes a different hole:
    ///
    /// * **Reachable.** [`AgentState::is_reachable`] is false for a peer that is
    ///   only mid-pairing, so an untrusted session can neither be offered content
    ///   nor have any written. [`is_permitted_while_unpaired`] refuses the messages
    ///   on the way in; this refuses them on the way out.
    /// * **Enabled for this peer.** The per-peer `clipboard` flag in
    ///   [`crate::config::PeerConfig`], which the Devices screen writes.
    /// * **A session.** Nothing to send it on otherwise.
    fn clipboard_shared_with(&self, node: NodeId) -> bool {
        clipboard_sharing_permitted(
            self.sessions.contains_key(&node),
            self.state.is_reachable(node),
            self.config.peer(&node).clipboard,
        )
    }

    /// Notice that something was copied on this machine, and tell peers about it.
    ///
    /// Called on [`CLIPBOARD_POLL`]. The overwhelmingly common outcome is that the
    /// serial has not moved and nothing else happens.
    fn poll_clipboard(&mut self) {
        if self.sessions.is_empty() {
            // Nobody to tell, so a machine sitting alone never touches its own
            // clipboard at all. The serial is not sampled either, which means the
            // first poll once a peer appears is a `FirstSighting` — whatever was
            // copied while there was nobody to tell is not pushed at the peer the
            // moment it connects.
            return;
        }
        if !self.clipboard_traffic.may_poll() {
            // One poll in flight at a time, and none at all while a write is: the
            // ticker is faster than a portal round trip on purpose, and a poll sent
            // now would carry a write-back guard that has not been armed yet. See
            // [`ClipboardTraffic`], which is where that rule is stated and tested.
            return;
        }
        // Both operands of the write-back decision are read here, on the loop, and
        // travel with the job; the worker does the I/O and hands the answer back.
        // See [`ClipboardSync::armed`].
        let job = ClipboardJob::Poll {
            seen: self.clipboard.serial(),
            armed: self.clipboard.armed(),
        };
        self.dispatch_clipboard(job);
    }

    /// Say what the worker found on the local clipboard.
    ///
    /// `serial` is the one that was sampled, for the log lines; the variants that
    /// act on it carry their own.
    fn on_local_clipboard_change(&mut self, serial: u64, change: LocalChange) {
        match change {
            LocalChange::Unchanged => {}
            LocalChange::FirstSighting => {
                tracing::debug!(
                    serial,
                    "first look at the local clipboard; nothing to attribute it to"
                );
            }
            LocalChange::Echo => {
                tracing::debug!(
                    serial,
                    "the clipboard changed because this machine wrote what a peer sent"
                );
            }
            LocalChange::NothingToOffer => {
                tracing::debug!(serial, "the clipboard changed to nothing this build syncs");
            }
            LocalChange::Offer { serial, formats } => self.offer_clipboard(serial, &formats),
        }
    }

    /// Apply the result of a blocking clipboard job.
    ///
    /// Every arm is a state change the worker could not make itself, because the
    /// state is here and nowhere else. That is the trade the worker exists to make:
    /// it does the waiting, the loop keeps the ownership. The state changes
    /// themselves are in [`settle_clipboard_report`]; what is left here is the
    /// logging and the sending, which need the rest of the engine.
    fn on_clipboard_done(&mut self, done: ClipboardDone) {
        if let ClipboardDone::Served {
            node,
            format,
            serial,
            packed,
        } = done
        {
            self.clipboard_traffic.settled(ClipboardJobKind::Serve);
            self.on_clipboard_served(node, format, serial, packed);
            return;
        }
        if let ClipboardDone::Wrote {
            node,
            format,
            bytes,
            ..
        } = &done
        {
            tracing::info!(peer = %node, ?format, bytes, "took the clipboard from a peer");
        }
        let was_lost = self.clipboard_traffic.is_lost();
        let change =
            settle_clipboard_report(&mut self.clipboard, &mut self.clipboard_traffic, &done);
        if !was_lost && self.clipboard_traffic.is_lost() {
            self.clipboard_worker_lost();
        }
        if let ClipboardDone::Polled { serial, .. } = done {
            if let Some(change) = change {
                self.on_local_clipboard_change(serial, change);
            }
        }
    }

    /// Hand a blocking clipboard job to the worker. `false` if it never left.
    ///
    /// Never blocks, for the reason the whole worker exists: waiting for room here
    /// would put the stall back on the loop it was moved off.
    ///
    /// The accounting is done from the job itself rather than by the caller, so
    /// that the rule in [`ClipboardTraffic`] cannot be kept at one call site and
    /// forgotten at the next: this is the only way to the worker.
    fn dispatch_clipboard(&mut self, job: ClipboardJob) -> bool {
        let kind = job.kind();
        match self.clipboard_jobs.try_send(job) {
            Ok(()) => {
                self.clipboard_traffic.dispatched(kind);
                true
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                tracing::warn!("the clipboard backend is not keeping up; dropping a clipboard job");
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                if !self.clipboard_traffic.is_lost() {
                    self.clipboard_traffic.worker_lost();
                    self.clipboard_worker_lost();
                }
                false
            }
        }
    }

    /// The clipboard worker has stopped. Say so once, and out loud.
    ///
    /// At error level and with a notice, because what has just happened is that a
    /// feature stopped working for the rest of the run: the serve and write paths
    /// are peer-driven and would at least fall silent visibly, but offering local
    /// copies is the primary direction and its only symptom is nothing happening.
    fn clipboard_worker_lost(&mut self) {
        tracing::error!(
            "the clipboard worker has stopped; clipboard sync is off for the rest of this run"
        );
        self.notice(
            ipc::NoticeLevel::Warning,
            "clipboard sync has stopped working on this machine; restart the agent to get it back"
                .to_string(),
        );
    }

    /// Announce new local content to every peer that could use it.
    ///
    /// Only what each peer can actually take: the formats are filtered per peer, so
    /// a machine advertising only `CLIPBOARD_TEXT` is never offered the PNG. That
    /// is the capability seam doing its job — a peer told about a format it cannot
    /// accept would either ask for it and fail, or ignore it silently, and neither
    /// leaves anything to attribute the missing paste to.
    fn offer_clipboard(&mut self, serial: u64, formats: &[ClipboardFormat]) {
        let local = self.advertised_by(self.local);
        let peers: Vec<NodeId> = self.sessions.keys().copied().collect();
        for node in peers {
            if !self.clipboard_shared_with(node) {
                // Said rather than skipped silently: "why did my copy not reach that
                // machine" has exactly two answers, and this is the one the user
                // chose themselves.
                tracing::debug!(peer = %node, "not offering the clipboard: not sharing with this machine");
                continue;
            }
            let theirs = self.advertised_by(node);
            let offered: Vec<ClipboardFormat> = formats
                .iter()
                .copied()
                .filter(|f| {
                    clipboard::supported_by(local, *f) && clipboard::supported_by(theirs, *f)
                })
                .collect();
            if offered.is_empty() {
                // Through the shared seam so the log line reads the same as every
                // other refused optional feature, and names both the capability and
                // the machine.
                let needed = formats
                    .first()
                    .and_then(|f| clipboard::capability_for(*f))
                    .unwrap_or(Capabilities::CLIPBOARD_TEXT);
                permit_optional(
                    theirs,
                    needed,
                    node,
                    &self.peer_label(node),
                    "offer the clipboard",
                );
                continue;
            }
            tracing::debug!(peer = %node, serial, count = offered.len(), "offering the clipboard");
            self.send_clipboard_to(
                node,
                ControlMsg::ClipboardOffer {
                    formats: offered,
                    serial,
                },
            );
        }
    }

    /// A peer says it has new content.
    fn on_clipboard_offer(&mut self, node: NodeId, formats: Vec<ClipboardFormat>, serial: u64) {
        if !self.clipboard_shared_with(node) {
            tracing::debug!(peer = %node, "ignoring a clipboard offer: not sharing with this machine");
            return;
        }
        let local = self.advertised_by(self.local);
        let theirs = self.advertised_by(node);
        let Some(format) = ClipboardSync::choose(&formats, local, theirs) else {
            tracing::debug!(
                peer = %node,
                serial,
                "a peer offered the clipboard in no format this machine can take"
            );
            return;
        };
        if !self.clipboard.ask(node, serial, format) {
            // The same offer again: a re-advertisement, or a session replaced under
            // us. Asking twice would move the payload twice.
            return;
        }
        tracing::debug!(peer = %node, serial, ?format, "asking a peer for its clipboard");
        if !self.send_clipboard_to(node, ControlMsg::ClipboardRequest { format, serial }) {
            // The request never left, so nothing will answer it. Forgetting it here
            // means a re-offer of the same serial is asked about again instead of
            // being deduplicated against a request that does not exist.
            self.clipboard.settled(node);
        }
    }

    /// A peer wants the content this machine offered.
    fn on_clipboard_request(&mut self, node: NodeId, format: ClipboardFormat, serial: u64) {
        // Refused rather than ignored, in every branch below. A request that is
        // dropped on the floor leaves the peer waiting for an answer that never
        // comes, which is the failure `ClipboardStale` exists to make impossible.
        if !self.clipboard_shared_with(node) {
            tracing::info!(
                peer = %node,
                machine = %self.peer_label(node),
                "refusing a clipboard request: sharing is off for this machine"
            );
            let _ = self.send_clipboard_to(node, ControlMsg::ClipboardStale { serial });
            return;
        }
        let theirs = self.advertised_by(node);
        if !clipboard::supported_by(theirs, format) {
            let needed = clipboard::capability_for(format).unwrap_or(Capabilities::CLIPBOARD_TEXT);
            permit_optional(
                theirs,
                needed,
                node,
                &self.peer_label(node),
                "serve the clipboard",
            );
            let _ = self.send_clipboard_to(node, ControlMsg::ClipboardStale { serial });
            return;
        }

        if self.clipboard.serve(serial, format) == Serve::Stale {
            tracing::debug!(peer = %node, serial, "a clipboard request names content that is gone");
            let _ = self.send_clipboard_to(node, ControlMsg::ClipboardStale { serial });
            return;
        }

        // The read and the compression happen on the worker; what comes back is
        // handled by `on_clipboard_served`. A job that never left is refused here
        // rather than left unanswered, like every other branch above.
        if !self.dispatch_clipboard(ClipboardJob::Serve {
            node,
            format,
            serial,
        }) {
            let _ = self.send_clipboard_to(node, ControlMsg::ClipboardStale { serial });
        }
    }

    /// The worker has been to the clipboard on a peer's behalf.
    fn on_clipboard_served(
        &mut self,
        node: NodeId,
        format: ClipboardFormat,
        serial: u64,
        packed: Option<Packed>,
    ) {
        // Re-checked after the read, not only before it. The read is not atomic
        // with the check that authorised it, and content that changed underneath
        // would otherwise be sent as an answer to a request for something else —
        // the peer pasting content it was never offered.
        if self.clipboard.serial() != Some(serial) {
            tracing::debug!(peer = %node, serial, "the clipboard changed while it was being read");
            let _ = self.send_clipboard_to(node, ControlMsg::ClipboardStale { serial });
            return;
        }
        // And re-checked for permission, because the read is not atomic with that
        // either: the user can switch this peer's clipboard off, or unpair it,
        // while the payload is being lifted off the OS.
        if !self.clipboard_shared_with(node) {
            tracing::info!(peer = %node, "not serving the clipboard: sharing is off for this machine");
            let _ = self.send_clipboard_to(node, ControlMsg::ClipboardStale { serial });
            return;
        }
        let Some(packed) = packed else {
            // Unreadable, or larger than the protocol carries. The worker named
            // which; what matters here is that the peer is told rather than left
            // waiting.
            let _ = self.send_clipboard_to(node, ControlMsg::ClipboardStale { serial });
            return;
        };

        tracing::debug!(
            peer = %node,
            serial,
            ?format,
            bytes = packed.read,
            sent = packed.payload.len(),
            "serving the clipboard"
        );
        let _ = self.send_clipboard_to(
            node,
            ControlMsg::ClipboardData {
                format,
                serial,
                compression: packed.compression,
                data: packed.payload,
            },
        );
    }

    /// Content arrived from a peer. Write it, and remember that we did.
    fn on_clipboard_data(
        &mut self,
        node: NodeId,
        format: ClipboardFormat,
        serial: u64,
        compression: Compression,
        data: Vec<u8>,
    ) {
        if !self.clipboard.answers(node, serial, format) {
            // Unsolicited. A paired machine may say what it has copied; it may not
            // reach over and set this machine's clipboard unasked.
            tracing::warn!(
                peer = %node,
                serial,
                ?format,
                "ignoring clipboard content this machine never asked for"
            );
            return;
        }
        self.clipboard.settled(node);
        if !self.clipboard_shared_with(node) {
            // The flag can be turned off, or the peer unpaired, between the request
            // and the answer.
            tracing::info!(peer = %node, "discarding clipboard content: sharing is off for this machine");
            return;
        }
        if !clipboard::supported_by(self.advertised_by(self.local), format) {
            tracing::warn!(peer = %node, ?format, "a peer sent a clipboard format this machine does not advertise");
            return;
        }

        // Decompression and the write itself are the worker's, and it reports the
        // write-back guard back here *before* the write lands — see
        // [`ClipboardDone::Writing`], which is where the ordering that makes echo
        // suppression work is spelled out.
        if !self.dispatch_clipboard(ClipboardJob::Accept {
            node,
            format,
            compression,
            data,
        }) {
            // Nothing to answer: the peer sent what it was asked for and is not
            // waiting on a reply. The paste is lost and the session is not.
            tracing::warn!(peer = %node, ?format, "dropping clipboard content this machine cannot write just now");
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

/// What to tell a peer about this machine when its session comes up.
///
/// `local` is this machine's current set, and it is the only capability set this
/// message ever carries. Stated as a rule because getting it wrong is invisible:
/// the peer's own set is right there at the call site, it is the same shape, and
/// sending it would look like it worked.
///
/// Unconditional, because there is nothing sound to make it conditional on. What
/// the peer was told at the handshake was a clone of `local_info` the accept loop
/// made before it awaited the connection, and nothing keeps it; the peer's own
/// advertised set answers a different question entirely. Skipping the message when
/// the two machines happen to agree with each other leaves the peer holding a
/// snapshot that may predate this machine's portal grant for the rest of the
/// session.
///
/// `None` only for a peer that never claimed to understand the message: a variant
/// a build does not have is a decode error, and a decode error on the control
/// stream closes the session rather than informing it. See
/// [`Engine::broadcast_control_capable`].
fn capability_correction(peer: Capabilities, local: Capabilities) -> Option<ControlMsg> {
    peer.contains(Capabilities::CAPABILITY_UPDATES)
        .then_some(ControlMsg::CapabilitiesChanged {
            capabilities: local,
        })
}

/// Whether clipboard content may cross to a machine at all.
///
/// Pulled out of [`Engine::clipboard_shared_with`] so that the rule can be stated
/// once and tested, because two of the three conditions are security properties
/// rather than preferences and the cost of getting either wrong is a clipboard
/// served to a machine nobody approved.
///
/// `reachable` is the pairing gate: [`AgentState::is_reachable`] is false for a
/// session that has been admitted for pairing but not yet approved by a human, and
/// false for a peer the user disabled. It is the same condition
/// [`is_permitted_while_unpaired`] enforces on the inbound side, applied to what
/// this machine sends.
fn clipboard_sharing_permitted(has_session: bool, reachable: bool, enabled_for_peer: bool) -> bool {
    has_session && reachable && enabled_for_peer
}

/// Apply a worker report to the clipboard state machine and the traffic record.
///
/// Free rather than inlined in [`Engine::on_clipboard_done`] so that the loop's
/// half of the handoff can be driven in a test against the worker's own
/// functions, with no desktop and no engine — the ordering between the two is the
/// part that is easy to get wrong and impossible to see in either half alone.
///
/// Returns the verdict on a poll, which is the only report the loop has to act on
/// beyond recording it.
fn settle_clipboard_report(
    sync: &mut ClipboardSync,
    traffic: &mut ClipboardTraffic,
    done: &ClipboardDone,
) -> Option<LocalChange> {
    match done {
        ClipboardDone::NothingNew => {
            traffic.settled(ClipboardJobKind::Poll);
            None
        }
        ClipboardDone::Polled {
            serial,
            formats,
            digest,
        } => {
            traffic.settled(ClipboardJobKind::Poll);
            // The fingerprint is already taken, so the closure only hands it over.
            // The state machine still decides whether it mattered.
            Some(sync.observe(*serial, formats, |_| *digest))
        }
        ClipboardDone::Served { .. } => {
            traffic.settled(ClipboardJobKind::Serve);
            None
        }
        ClipboardDone::Writing { format, digest } => {
            sync.writing(*format, *digest);
            None
        }
        ClipboardDone::Wrote { serial, .. } => {
            traffic.settled(ClipboardJobKind::Accept);
            if let Some(serial) = serial {
                sync.wrote(*serial);
            }
            None
        }
        ClipboardDone::NotWritten { armed } => {
            traffic.settled(ClipboardJobKind::Accept);
            if *armed {
                // The guard was put up for a write that never happened, so there is
                // nothing of this machine's on the clipboard to suppress and leaving
                // it standing would swallow the user's next copy of the same content.
                sync.write_failed();
            }
            None
        }
        ClipboardDone::WorkerGone => {
            traffic.worker_lost();
            None
        }
    }
}

/// Says so if the clipboard worker ever stops, however it stops.
///
/// A panic inside a platform backend unwinds straight out of the worker's loop,
/// taking whatever it was holding with it and reporting nothing. `Drop` runs on
/// that path as well as the ordinary one, so this is the one report that cannot be
/// skipped — and without it the loop would go on believing a poll was still in
/// flight, and would never send another for the life of the process.
struct WorkerObituary(mpsc::UnboundedSender<Wake>);

impl Drop for WorkerObituary {
    fn drop(&mut self) {
        let _ = self.0.send(Wake::Clipboard(ClipboardDone::WorkerGone));
    }
}

/// Run the blocking half of clipboard sync on a thread of its own.
///
/// # Why a thread, and why it owns the backend
///
/// [`ClipboardAccess`](wx_platform::traits::ClipboardAccess) is a blocking trait
/// and the Wayland implementation means it: a read is a portal request with a
/// ten-second ceiling followed by a pipe transfer of up to
/// [`MAX_CLIPBOARD_BYTES`]. Left on the engine loop, serving one large image stops
/// the user's keyboard, and a portal that stops answering stops it for ten
/// seconds. That is not a slow paste; it is a dead KVM.
///
/// It is a thread rather than `spawn_blocking` because the trait is `Send` and not
/// `Sync`, so somebody has to own the backend, and because one worker gives the
/// serialisation the state machine already assumes — the OS clipboard is a single
/// resource and two reads racing on it answer questions nobody asked.
///
/// # What it deliberately does not have
///
/// No engine state, no `Arc`, no lock. It holds the platform backend and a channel
/// back to the loop, and every decision it makes is one the loop already made and
/// put in the job. That is what keeps the single-owner property in this module's
/// docs true while the bytes move somewhere else.
fn spawn_clipboard_worker(
    access: Box<dyn wx_platform::traits::ClipboardAccess>,
    wake: mpsc::UnboundedSender<Wake>,
) -> anyhow::Result<std::sync::mpsc::SyncSender<ClipboardJob>> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<ClipboardJob>(CLIPBOARD_JOB_DEPTH);
    std::thread::Builder::new()
        .name("wx-clipboard".into())
        .spawn(move || {
            // Declared first so it is dropped last, and so it outlives every path
            // out of the loop below — including the one a panic takes.
            let _obituary = WorkerObituary(wake.clone());
            let report = |done: ClipboardDone| wake.send(Wake::Clipboard(done)).is_ok();
            for job in rx {
                let carried_on = match job {
                    ClipboardJob::Poll { seen, armed } => {
                        report(sample_clipboard(&*access, seen, armed))
                    }
                    ClipboardJob::Serve {
                        node,
                        format,
                        serial,
                    } => report(serve_clipboard(&*access, node, format, serial)),
                    ClipboardJob::Accept {
                        node,
                        format,
                        compression,
                        data,
                    } => accept_clipboard(&*access, node, format, compression, &data, &report),
                };
                if !carried_on {
                    // The loop is gone, so there is nobody to report to and nothing
                    // left worth doing to the clipboard.
                    return;
                }
            }
        })?;
    Ok(tx)
}

/// Look at the clipboard for [`Engine::poll_clipboard`].
///
/// The write-back format is read only when the answer could still change the
/// verdict — the guard is armed, the serial has actually moved past the one the
/// write produced, and the format is still on the clipboard — so the steady state
/// costs one `change_serial` and nothing else.
fn sample_clipboard(
    access: &dyn wx_platform::traits::ClipboardAccess,
    seen: Option<u64>,
    armed: Option<(ClipboardFormat, u64)>,
) -> ClipboardDone {
    let serial = match access.change_serial() {
        Ok(serial) => serial,
        Err(e) => {
            tracing::trace!(error = %e, "the clipboard cannot be polled");
            return ClipboardDone::NothingNew;
        }
    };
    if seen == Some(serial) {
        return ClipboardDone::NothingNew;
    }
    let formats = access.available_formats().unwrap_or_else(|e| {
        tracing::debug!(error = %e, "could not list clipboard formats");
        Vec::new()
    });
    let digest = armed
        .filter(|(format, written)| *written != serial && formats.contains(format))
        .and_then(|(format, _)| access.read(format).ok())
        .map(|bytes| clipboard::fingerprint(&bytes));
    ClipboardDone::Polled {
        serial,
        formats,
        digest,
    }
}

/// Read and compress the content a peer asked for.
fn serve_clipboard(
    access: &dyn wx_platform::traits::ClipboardAccess,
    node: NodeId,
    format: ClipboardFormat,
    serial: u64,
) -> ClipboardDone {
    let refused = ClipboardDone::Served {
        node,
        format,
        serial,
        packed: None,
    };
    let data = match access.read(format) {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!(peer = %node, error = %e, ?format, "could not read the clipboard for a peer");
            return refused;
        }
    };
    if data.len() > MAX_CLIPBOARD_BYTES {
        // The frame writer would refuse this and, on the shared control stream,
        // take the session with it. Said out loud and answered honestly instead.
        tracing::warn!(
            peer = %node,
            bytes = data.len(),
            limit = MAX_CLIPBOARD_BYTES,
            ?format,
            "the clipboard holds more than the protocol carries; refusing to send it"
        );
        return refused;
    }
    let read = data.len();
    let (compression, payload) = clipboard::compress(format, &data);
    ClipboardDone::Served {
        node,
        format,
        serial,
        packed: Some(Packed {
            compression,
            payload,
            read,
        }),
    }
}

/// Unpack a peer's payload and write it to this machine's clipboard.
///
/// Two reports rather than one, and the order is load-bearing: see
/// [`ClipboardDone::Writing`]. Returns whether the loop is still listening.
fn accept_clipboard(
    access: &dyn wx_platform::traits::ClipboardAccess,
    node: NodeId,
    format: ClipboardFormat,
    compression: Compression,
    data: &[u8],
    report: &impl Fn(ClipboardDone) -> bool,
) -> bool {
    let payload = match clipboard::decompress(compression, data) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!(peer = %node, error = %e, "discarding a clipboard payload");
            return report(ClipboardDone::NotWritten { armed: false });
        }
    };
    if !report(ClipboardDone::Writing {
        format,
        digest: clipboard::fingerprint(&payload),
    }) {
        return false;
    }
    match access.write(format, &payload) {
        Ok(()) => report(ClipboardDone::Wrote {
            node,
            format,
            bytes: payload.len(),
            serial: access.change_serial().ok(),
        }),
        Err(e) => {
            tracing::warn!(peer = %node, error = %e, ?format, "could not write the clipboard");
            report(ClipboardDone::NotWritten { armed: true })
        }
    }
}

/// Serialise one peer's clipboard traffic through a task of its own.
///
/// Separate from [`spawn_sender`] so that the two cannot block each other. That
/// task awaits `write_all` on the control stream; this one awaits it on the
/// clipboard stream, and QUIC flow-controls the two independently — so a peer that
/// has stopped reading twenty megabytes of image stalls this task and leaves the
/// cursor moving.
///
/// A framing failure is logged and skipped rather than ending the task: the size
/// checks upstream make it unreachable, and if one ever were reachable, silently
/// disabling the clipboard for the rest of the session is not the way to find out.
fn spawn_clipboard_sender(session: Session) -> mpsc::Sender<ControlMsg> {
    let (tx, mut rx) = mpsc::channel::<ControlMsg>(CLIPBOARD_QUEUE_DEPTH);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match session.send_clipboard(&msg).await {
                Ok(()) => {}
                Err(wx_net::TransportError::Codec(e)) => {
                    tracing::warn!(error = %e, "a clipboard message would not encode; dropping it");
                }
                Err(e) => {
                    // The session is finished; the pump task reports the death, so
                    // this only needs to stop trying.
                    tracing::debug!(error = %e, "clipboard send failed; closing the peer queue");
                    return;
                }
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
            ControlMsg::ClipboardOffer {
                formats: vec![wx_proto::ClipboardFormat::Utf8Text],
                serial: 1,
            },
            ControlMsg::ClipboardData {
                format: wx_proto::ClipboardFormat::Utf8Text,
                serial: 1,
                compression: Compression::None,
                data: b"secret".to_vec(),
            },
            ControlMsg::ClipboardStale { serial: 1 },
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
    fn exits_are_reported_in_this_machines_own_desktop_space() {
        // The two coordinate spaces are the point of this function. A capture
        // backend's barriers live in the *local* space — `cowen-ubuntu`'s screen is
        // at the origin as far as its own compositor is concerned — while the
        // adjacency that decides which of them to arm is a fact about the global
        // one, where the same screen sits at x=6512. Reporting the global rectangle
        // would leave the backend unable to match any of its own screens, and a
        // screen it cannot match arms nothing at all.
        let mut layout = GlobalLayout::new();
        layout.insert(Placement {
            monitor: gid(1, 0),
            global_bounds: Rect::new(6512, 5, 3072, 1728),
            cursor_scale: 1.0,
        });
        layout.insert(Placement {
            monitor: gid(2, 0),
            global_bounds: Rect::new(3072, 144, 3440, 1440),
            cursor_scale: 1.0,
        });
        let local = Monitor {
            id: MonitorId(0),
            name: "DP-1".into(),
            local_bounds: Rect::new(0, 0, 3072, 1728),
            scale: 1.25,
            primary: true,
        };

        let exits = local_exits(&layout, node(1), std::slice::from_ref(&local));
        assert_eq!(exits.len(), 1);
        assert_eq!(exits[0].bounds, local.local_bounds);
        // The peer meets this screen's left-hand edge and nothing meets the other
        // three. This is the reported bug, at the layer that answers it.
        assert_eq!(exits[0].edges, vec![wx_proto::Edge::Left]);
    }

    #[test]
    fn a_screen_missing_from_the_layout_is_offered_no_exits() {
        // A display plugged in a moment ago and not yet placed. Arming it would
        // grab the pointer for a crossing the router then refuses to resolve.
        let exits = local_exits(
            &two_machines(),
            node(1),
            &[mon(0, 0, 1920), mon(7, 5000, 1920)],
        );
        assert_eq!(exits.len(), 2);
        assert_eq!(exits[0].edges, vec![wx_proto::Edge::Right]);
        assert!(exits[1].edges.is_empty());
    }

    #[test]
    fn a_machine_alone_in_the_layout_is_offered_no_exits() {
        let mut alone = GlobalLayout::new();
        alone.insert(place(gid(1, 0), 0, 1920));
        let exits = local_exits(&alone, node(1), &[mon(0, 0, 1920)]);
        assert_eq!(exits.len(), 1);
        assert!(
            exits[0].edges.is_empty(),
            "a machine with no neighbours must capture nowhere"
        );
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
    fn a_peer_is_told_what_this_machine_can_do_and_not_what_it_said_itself() {
        // Measured on a real Wayland desktop while proving this slice out: the accept
        // loop clones `local_info` at the top of each iteration, *before* it awaits
        // the next connection, so the `NodeInfo` a peer receives can predate the
        // portal grant by any amount. `sync_capabilities` does not close the gap
        // either, because it only speaks on a transition and the transition happened
        // before the session existed. The peer then spends the whole session
        // believing this machine cannot take clipboard content, and refuses to offer
        // it any — which is what actually happened, in both directions.
        let local = Capabilities::HAS_DISPLAYS
            | Capabilities::CLIPBOARD_TEXT
            | Capabilities::CAPABILITY_UPDATES;
        let peer = Capabilities::HAS_DISPLAYS | Capabilities::CAPABILITY_UPDATES;

        // What the message carries is this machine's set. The peer's is the wrong
        // operand and the same shape, so nothing but an assertion catches the swap.
        assert_eq!(
            capability_correction(peer, local),
            Some(ControlMsg::CapabilitiesChanged {
                capabilities: local
            })
        );

        // And it is sent even when the two machines agree, which is the case the
        // comparison this replaced got wrong: two identically configured machines
        // match each other while both hold a snapshot older than their own grants.
        assert_eq!(
            capability_correction(local, local),
            Some(ControlMsg::CapabilitiesChanged {
                capabilities: local
            })
        );

        // A peer that never claimed it understands the message is never sent one: a
        // variant its build lacks is a decode error, which closes the session.
        assert_eq!(
            capability_correction(Capabilities::HAS_DISPLAYS, local),
            None
        );
    }

    #[test]
    fn the_clipboard_is_never_shared_with_a_machine_no_human_has_approved() {
        // The gate that matters most in this slice. A session admitted for pairing
        // has proved possession of a key and nothing else, and `is_reachable` is
        // what says a human has since approved it.
        assert!(clipboard_sharing_permitted(true, true, true));
        assert!(
            !clipboard_sharing_permitted(true, false, true),
            "an unpaired or mid-pairing machine was offered the clipboard"
        );
    }

    #[test]
    fn the_per_peer_clipboard_switch_is_honoured_in_both_directions() {
        // `PeerConfig::clipboard`, which the Devices screen writes. The same
        // predicate guards offering, serving and accepting, so switching it off
        // stops content in both directions rather than only outbound.
        assert!(!clipboard_sharing_permitted(true, true, false));
        // And with no session there is nothing to share over.
        assert!(!clipboard_sharing_permitted(false, true, true));
    }

    #[test]
    fn a_machine_that_advertises_nothing_refuses_everything() {
        // The state every peer is in before its handshake, and the state the
        // skeleton backends — macOS, evdev — are in for the whole of the alpha.
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
    fn a_superseded_dials_failure_does_not_kill_the_attempt_that_replaced_it() {
        // Pressing Pair twice is the ordinary way to reach this: the first dial is
        // still working through its addresses, `begin_pairing` clears `dialing` so
        // a second may start, and the first then fails. Unqualified, that failure
        // discarded the code the second attempt had just put on screen and failed
        // its card for a machine that was answering.
        let first = next_dial_id();
        let second = next_dial_id();
        assert!(!dial_is_current(Some(second), first));
        assert!(dial_is_current(Some(second), second));
    }

    #[test]
    fn a_dial_that_was_overtaken_by_a_session_reports_nothing() {
        // The other way the entry goes: the peer dialled this machine at the same
        // time and its session was installed, which clears `dialing`. The losing
        // dial must not then mark a connected peer down.
        assert!(!dial_is_current(None, 42));
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
        pins.offer(
            node(2),
            Pin::parse("123456").expect("six digits is a PIN"),
            "workhorse".to_string(),
            Instant::now(),
        );
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
        pins.offer(
            node(2),
            Pin::parse("123456").expect("six digits is a PIN"),
            "workhorse".to_string(),
            Instant::now(),
        );
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
        pins.offer(
            node(2),
            Pin::parse("123456").expect("six digits is a PIN"),
            "workhorse".to_string(),
            Instant::now(),
        );
        pins.discard(node(2));
        assert!(pins.claim(node(2)).is_none());
    }

    #[test]
    fn a_teardown_for_one_peer_does_not_touch_another_peers_code() {
        let mut pins = OfferedPins::default();
        pins.offer(
            node(2),
            Pin::parse("111111").expect("six digits is a PIN"),
            "workhorse".to_string(),
            Instant::now(),
        );
        pins.discard(node(2));
        pins.offer(
            node(3),
            Pin::parse("222222").expect("six digits is a PIN"),
            "laptop".to_string(),
            Instant::now(),
        );
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
    async fn a_full_clipboard_queue_costs_a_paste_and_not_the_session() {
        // The other half of the policy above, and the reason the clipboard is not on
        // that queue at all. `Outbound::is_sheddable` is false for every control
        // message, so a clipboard payload that filled the main queue would return
        // `Queued::Unresponsive` and close a session that was working — a 20MB image
        // costing the user their cursor.
        let payload = || ControlMsg::ClipboardData {
            format: ClipboardFormat::Png,
            serial: 1,
            compression: Compression::None,
            data: vec![0u8; 64],
        };

        let (clipboard, _held) = mpsc::channel::<ControlMsg>(CLIPBOARD_QUEUE_DEPTH);
        for _ in 0..CLIPBOARD_QUEUE_DEPTH {
            clipboard.try_send(payload()).expect("queue should accept");
        }
        // Refused, and refusal on this path is `send_clipboard_to` dropping the
        // message with a warning. There is no route from here to a teardown.
        assert!(matches!(
            clipboard.try_send(payload()),
            Err(mpsc::error::TrySendError::Full(_))
        ));

        // And this is what the same payload does on the main queue, which is what
        // the separate one exists to avoid.
        let (main, _main_held) = mpsc::channel::<Outbound>(OUTBOUND_QUEUE_DEPTH);
        for _ in 0..OUTBOUND_QUEUE_DEPTH {
            assert_eq!(
                enqueue(&main, Outbound::Control(ControlMsg::Ping { nonce: 0 })),
                Queued::Sent
            );
        }
        assert_eq!(
            enqueue(&main, Outbound::Control(payload())),
            Queued::Unresponsive,
            "this is the teardown the clipboard's own queue exists to prevent"
        );
    }

    /// A clipboard that moves its change serial the way the Wayland portal does.
    ///
    /// Writing bumps it once, and the compositor's own `SelectionOwnerChanged`
    /// echo bumps it again some time later — [`portal_echo`] is that second bump,
    /// fired by the test exactly where the race puts it: after the write has
    /// already reported the serial it produced.
    ///
    /// [`portal_echo`]: DoubleBumpClipboard::portal_echo
    struct DoubleBumpClipboard {
        serial: std::cell::Cell<u64>,
        held: std::cell::RefCell<Option<(ClipboardFormat, Vec<u8>)>>,
    }

    impl DoubleBumpClipboard {
        fn new() -> Self {
            Self {
                serial: std::cell::Cell::new(0),
                held: std::cell::RefCell::new(None),
            }
        }

        fn bump(&self) {
            self.serial.set(self.serial.get() + 1);
        }

        fn copied_by_hand(&self, format: ClipboardFormat, data: &[u8]) {
            *self.held.borrow_mut() = Some((format, data.to_vec()));
            self.bump();
        }

        fn portal_echo(&self) {
            self.bump();
        }
    }

    impl wx_platform::traits::ClipboardAccess for DoubleBumpClipboard {
        fn available_formats(&self) -> wx_platform::Result<Vec<ClipboardFormat>> {
            Ok(self.held.borrow().iter().map(|(f, _)| *f).collect())
        }

        fn read(&self, format: ClipboardFormat) -> wx_platform::Result<Vec<u8>> {
            match &*self.held.borrow() {
                Some((held, data)) if *held == format => Ok(data.clone()),
                _ => Err(PlatformError::Unsupported {
                    operation: "reading a format the clipboard does not hold",
                    backend: "test",
                }),
            }
        }

        fn write(&self, format: ClipboardFormat, data: &[u8]) -> wx_platform::Result<()> {
            *self.held.borrow_mut() = Some((format, data.to_vec()));
            self.bump();
            Ok(())
        }

        fn change_serial(&self) -> wx_platform::Result<u64> {
            Ok(self.serial.get())
        }
    }

    /// One turn of the loop's clipboard poll: check the rule, dispatch, let the
    /// worker run the job, apply what comes back. The two halves are the real
    /// ones — only the queue between them is collapsed.
    fn poll_once(
        access: &DoubleBumpClipboard,
        sync: &mut ClipboardSync,
        traffic: &mut ClipboardTraffic,
    ) -> Option<LocalChange> {
        assert!(traffic.may_poll(), "the loop would not have polled here");
        traffic.dispatched(ClipboardJobKind::Poll);
        let done = sample_clipboard(access, sync.serial(), sync.armed());
        settle_clipboard_report(sync, traffic, &done)
    }

    #[test]
    fn a_poll_may_not_straddle_a_write_and_offer_a_peer_its_own_payload_back() {
        // The dispatch ordering, driven end to end against the worker's own
        // functions. A pure test of `ClipboardSync` cannot see this: the state
        // machine absorbs the write-back correctly in every case, and what used to
        // break it was the loop handing it a `armed` snapshot taken before the
        // write was armed.
        let access = DoubleBumpClipboard::new();
        let mut sync = ClipboardSync::new();
        let mut traffic = ClipboardTraffic::default();

        access.copied_by_hand(ClipboardFormat::Utf8Text, b"whatever was there first");
        assert_eq!(
            poll_once(&access, &mut sync, &mut traffic),
            Some(LocalChange::FirstSighting)
        );

        // A peer's payload arrives and the loop sends the write to the worker.
        traffic.dispatched(ClipboardJobKind::Accept);

        // The ticker fires while that write is still with the worker. This is the
        // poll that used to slip through: the loop has not applied `Writing` yet,
        // so it would carry `armed: None`, and the FIFO worker would run it *after*
        // the write with nothing telling it to read the format back.
        assert!(
            !traffic.may_poll(),
            "a poll sent now reaches the worker after the write and carries no guard"
        );

        // The worker runs the write and reports in its own order; the loop applies
        // each report as it arrives.
        let reports = std::cell::RefCell::new(Vec::new());
        assert!(accept_clipboard(
            &access,
            node(1),
            ClipboardFormat::Utf8Text,
            Compression::None,
            b"the peer's clipboard",
            &|done| {
                reports.borrow_mut().push(done);
                true
            },
        ));
        for done in reports.borrow().iter() {
            settle_clipboard_report(&mut sync, &mut traffic, done);
        }

        // The portal's second bump, landing after the write reported its own
        // serial. This is the move no serial-counting guard can catch, and the one
        // that turns a stale `armed` into a full payload sent straight back.
        access.portal_echo();

        assert_eq!(
            poll_once(&access, &mut sync, &mut traffic),
            Some(LocalChange::Echo),
            "the peer's own payload was offered straight back to it"
        );
    }

    #[test]
    fn a_guard_snapshot_taken_before_the_write_is_mistaken_for_a_fresh_copy() {
        // Why the rule above is not cosmetic, written as the failure it prevents so
        // that nobody removes the latch on the grounds that it looks unnecessary.
        // Same clipboard, same payload, same second bump as the test above — the
        // only difference is that the poll carries the snapshot as it stood before
        // the write was armed, which is exactly what dispatching mid-write does.
        let access = DoubleBumpClipboard::new();
        let mut sync = ClipboardSync::new();
        let mut traffic = ClipboardTraffic::default();

        access.copied_by_hand(ClipboardFormat::Utf8Text, b"whatever was there first");
        poll_once(&access, &mut sync, &mut traffic);

        let reports = std::cell::RefCell::new(Vec::new());
        traffic.dispatched(ClipboardJobKind::Accept);
        accept_clipboard(
            &access,
            node(1),
            ClipboardFormat::Utf8Text,
            Compression::None,
            b"the peer's clipboard",
            &|done| {
                reports.borrow_mut().push(done);
                true
            },
        );
        for done in reports.borrow().iter() {
            settle_clipboard_report(&mut sync, &mut traffic, done);
        }
        access.portal_echo();

        traffic.dispatched(ClipboardJobKind::Poll);
        let stale = sample_clipboard(&access, sync.serial(), None);
        assert!(
            matches!(
                settle_clipboard_report(&mut sync, &mut traffic, &stale),
                Some(LocalChange::Offer { .. })
            ),
            "without the latch this is a spurious full-payload transfer, not a hazard on paper"
        );
    }

    #[test]
    fn every_write_has_to_report_before_the_next_poll_goes_out() {
        // Two peers can be having their payloads written at once, and a serve can
        // be in flight alongside either. Only the writes hold the poll off, and all
        // of them have to report before it goes.
        let mut traffic = ClipboardTraffic::default();
        assert!(traffic.may_poll());

        traffic.dispatched(ClipboardJobKind::Accept);
        traffic.dispatched(ClipboardJobKind::Accept);
        traffic.dispatched(ClipboardJobKind::Serve);
        assert!(!traffic.may_poll());

        traffic.settled(ClipboardJobKind::Serve);
        traffic.settled(ClipboardJobKind::Accept);
        assert!(
            !traffic.may_poll(),
            "one write reporting is not all of them reporting"
        );

        traffic.settled(ClipboardJobKind::Accept);
        assert!(traffic.may_poll());
    }

    #[test]
    fn a_write_that_never_landed_still_lets_polling_resume() {
        // A payload that would not decompress reports `NotWritten` and nothing
        // else. Counting that as a write still outstanding would stop this machine
        // offering anything it copied, for good, over one corrupt message.
        let mut sync = ClipboardSync::new();
        let mut traffic = ClipboardTraffic::default();
        traffic.dispatched(ClipboardJobKind::Accept);
        settle_clipboard_report(
            &mut sync,
            &mut traffic,
            &ClipboardDone::NotWritten { armed: false },
        );
        assert!(traffic.may_poll());
    }

    #[test]
    fn a_worker_that_dies_holding_a_job_is_reported_rather_than_latching_silently() {
        // The failure this guards: the worker panics inside a platform backend with
        // a poll in its queue, that job dies unreported, and the loop goes on
        // believing a poll is in flight — so it never sends another, and local
        // copies stop being offered for the rest of the run with nothing in the log.
        // `WorkerGone` is sent by a `Drop` guard, which is the one report a panic
        // cannot skip.
        let mut sync = ClipboardSync::new();
        let mut traffic = ClipboardTraffic::default();
        traffic.dispatched(ClipboardJobKind::Poll);
        traffic.dispatched(ClipboardJobKind::Accept);
        assert!(!traffic.is_lost());

        settle_clipboard_report(&mut sync, &mut traffic, &ClipboardDone::WorkerGone);

        assert!(
            traffic.is_lost(),
            "the loop has to learn this, because saying so once is the whole point"
        );
        assert!(
            !traffic.may_poll(),
            "and then stop asking a worker that is not there rather than warn every tick"
        );
    }

    // -- two machines, one wire -------------------------------------------

    /// What a machine that syncs everything this build syncs advertises.
    ///
    /// `CAPABILITY_UPDATES` because [`capabilities_for`] adds it to every set this
    /// build produces: it describes the binary, not a permission.
    fn full_clipboard() -> Capabilities {
        Capabilities::CLIPBOARD_TEXT
            | Capabilities::CLIPBOARD_IMAGE
            | Capabilities::CAPABILITY_UPDATES
    }

    /// One machine's half of clipboard sync, with a clipboard of its own.
    ///
    /// This is the assertion a single desktop cannot make. Two agents in one
    /// session share one physical selection — the confound recorded in
    /// `AGENTS.md` — so "the content arrived on the *other* machine" is exactly
    /// what a live run on one host cannot separate from the writer seeing its own
    /// copy. Here each machine owns its own [`DoubleBumpClipboard`] and the bytes
    /// cross a real `wx-net` session, on the clipboard stream, between two QUIC
    /// endpoints on loopback.
    ///
    /// Everything that decides anything is the shipping code: the worker
    /// functions the clipboard thread calls ([`sample_clipboard`],
    /// [`serve_clipboard`], [`accept_clipboard`]), the loop's half of the handoff
    /// ([`settle_clipboard_report`]), the state machine in [`crate::clipboard`],
    /// and the format filter offers go through. What is restated is the dispatch
    /// in `Engine::on_clipboard_*`, because reaching it needs an `Engine` and that
    /// needs a platform backend — which on this target means a portal consent
    /// dialog. Each arm below names the method it mirrors.
    struct Machine {
        name: &'static str,
        /// Who this machine is talking to, which is the key every request is
        /// remembered under.
        peer: NodeId,
        caps: Capabilities,
        peer_caps: Capabilities,
        /// The per-peer `clipboard` flag in [`crate::config::PeerConfig`].
        shares_with_peer: bool,
        access: DoubleBumpClipboard,
        sync: ClipboardSync,
        traffic: ClipboardTraffic,
        session: Session,
        events: Events,
    }

    impl Machine {
        /// One turn of [`Engine::poll_clipboard`], as far as the offer that goes on
        /// the wire.
        async fn poll(&mut self) -> Option<LocalChange> {
            let change = poll_once(&self.access, &mut self.sync, &mut self.traffic)?;
            if let LocalChange::Offer { serial, formats } = &change {
                // `Engine::offer_clipboard`: only what both machines advertise.
                let offered: Vec<ClipboardFormat> = formats
                    .iter()
                    .copied()
                    .filter(|f| {
                        clipboard::supported_by(self.caps, *f)
                            && clipboard::supported_by(self.peer_caps, *f)
                    })
                    .collect();
                if self.shares_with_peer && !offered.is_empty() {
                    self.send(ControlMsg::ClipboardOffer {
                        formats: offered,
                        serial: *serial,
                    })
                    .await;
                }
            }
            Some(change)
        }

        async fn send(&self, msg: ControlMsg) {
            self.session
                .send_clipboard(&msg)
                .await
                .expect("the clipboard stream carries this");
        }

        async fn next_message(&mut self) -> ControlMsg {
            match tokio::time::timeout(Duration::from_secs(30), self.events.next()).await {
                Ok(Some(Ok(SessionEvent::Control(msg)))) => msg,
                other => panic!("{} expected a clipboard message, got {other:?}", self.name),
            }
        }

        /// Nothing more from this peer.
        ///
        /// A short wait rather than a look, because what it rules out — the payload
        /// being offered straight back the way it came — is a message that would
        /// already be on the wire.
        async fn heard_nothing_more(&mut self) -> bool {
            tokio::time::timeout(Duration::from_millis(250), self.events.next())
                .await
                .is_err()
        }

        /// Handle one message from the peer, and return the reply that went back.
        async fn handle(&mut self, msg: ControlMsg) -> Option<ControlMsg> {
            match msg {
                // `Engine::on_clipboard_offer`.
                ControlMsg::ClipboardOffer { formats, serial } => {
                    if !self.shares_with_peer {
                        return None;
                    }
                    let format = ClipboardSync::choose(&formats, self.caps, self.peer_caps)?;
                    if !self.sync.ask(self.peer, serial, format) {
                        return None;
                    }
                    let reply = ControlMsg::ClipboardRequest { format, serial };
                    self.send(reply.clone()).await;
                    Some(reply)
                }
                // `Engine::on_clipboard_request`, then `Engine::on_clipboard_served`.
                ControlMsg::ClipboardRequest { format, serial } => {
                    let stale = ControlMsg::ClipboardStale { serial };
                    let reply = if !self.shares_with_peer
                        || !clipboard::supported_by(self.peer_caps, format)
                        || self.sync.serve(serial, format) == Serve::Stale
                    {
                        stale
                    } else {
                        let done = serve_clipboard(&self.access, self.peer, format, serial);
                        settle_clipboard_report(&mut self.sync, &mut self.traffic, &done);
                        match done {
                            // The serial is re-checked after the read as well as
                            // before it; the read is not atomic with the check that
                            // authorised it.
                            ClipboardDone::Served {
                                packed: Some(packed),
                                ..
                            } if self.sync.serial() == Some(serial) => ControlMsg::ClipboardData {
                                format,
                                serial,
                                compression: packed.compression,
                                data: packed.payload,
                            },
                            _ => stale,
                        }
                    };
                    self.send(reply.clone()).await;
                    Some(reply)
                }
                // `Engine::on_clipboard_data`, and the write the worker does for it.
                ControlMsg::ClipboardData {
                    format,
                    serial,
                    compression,
                    data,
                } => {
                    if !self.sync.answers(self.peer, serial, format) {
                        println!(
                            "  {}: refusing clipboard content it never asked for",
                            self.name
                        );
                        return None;
                    }
                    self.sync.settled(self.peer);
                    self.traffic.dispatched(ClipboardJobKind::Accept);
                    let reports = std::cell::RefCell::new(Vec::new());
                    accept_clipboard(
                        &self.access,
                        self.peer,
                        format,
                        compression,
                        &data,
                        &|done| {
                            reports.borrow_mut().push(done);
                            true
                        },
                    );
                    for done in reports.borrow().iter() {
                        settle_clipboard_report(&mut self.sync, &mut self.traffic, done);
                    }
                    None
                }
                ControlMsg::ClipboardStale { .. } => {
                    self.sync.settled(self.peer);
                    None
                }
                // `Engine::on_control`: the peer's own account of what it can do,
                // replacing whatever the handshake snapshot said.
                ControlMsg::CapabilitiesChanged { capabilities } => {
                    self.peer_caps = capabilities;
                    None
                }
                other => panic!("nothing else belongs on the clipboard stream: {other:?}"),
            }
        }

        /// What a user pressing Ctrl-V on this machine would get.
        fn pasted(&self) -> Option<(ClipboardFormat, Vec<u8>)> {
            self.access.held.borrow().clone()
        }
    }

    /// A line of transcript for one message, for the record this test leaves.
    fn describe(msg: &ControlMsg) -> String {
        match msg {
            ControlMsg::ClipboardOffer { formats, serial } => {
                format!("ClipboardOffer  serial {serial}, formats {formats:?}")
            }
            ControlMsg::ClipboardRequest { format, serial } => {
                format!("ClipboardRequest serial {serial}, {format:?}")
            }
            ControlMsg::ClipboardData {
                format,
                serial,
                compression,
                data,
            } => format!(
                "ClipboardData   serial {serial}, {format:?}, {} bytes on the wire ({compression:?})",
                data.len()
            ),
            ControlMsg::ClipboardStale { serial } => {
                format!("ClipboardStale  serial {serial} — refused, nothing sent")
            }
            ControlMsg::CapabilitiesChanged { capabilities } => {
                format!("CapabilitiesChanged  {}", capabilities.describe())
            }
            other => format!("{other:?}"),
        }
    }

    /// Two paired machines on two real QUIC endpoints.
    ///
    /// The endpoints come back with them and must be kept: dropping a
    /// `quinn::Endpoint` closes its socket under the session.
    async fn two_clipboards(
        alpha_caps: Capabilities,
        bravo_caps: Capabilities,
    ) -> (Machine, Machine, (Endpoint, Endpoint)) {
        fn info(id: NodeId, name: &str, capabilities: Capabilities) -> NodeInfo {
            NodeInfo {
                id,
                name: name.into(),
                platform: wx_proto::Platform::Linux,
                display_server: wx_proto::DisplayServer::Wayland,
                capabilities,
                monitors: Vec::new(),
                agent_version: AGENT_VERSION.into(),
            }
        }

        let alpha_id = Identity::generate().unwrap();
        let bravo_id = Identity::generate().unwrap();
        let mut alpha_trust = TrustStore::new();
        alpha_trust.trust(bravo_id.node_id(), "bravo");
        let mut bravo_trust = TrustStore::new();
        bravo_trust.trust(alpha_id.node_id(), "alpha");

        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let bravo_endpoint = Endpoint::bind(loopback).unwrap();
        let alpha_endpoint = Endpoint::bind(loopback).unwrap();
        let bravo_addr = bravo_endpoint.local_addr().unwrap();

        let alpha_setup = SessionSetup {
            identity: &alpha_id,
            trust: &alpha_trust,
            local_info: info(alpha_id.node_id(), "alpha", alpha_caps),
            pairing_mode: false,
        };
        let bravo_setup = SessionSetup {
            identity: &bravo_id,
            trust: &bravo_trust,
            local_info: info(bravo_id.node_id(), "bravo", bravo_caps),
            pairing_mode: false,
        };
        // Both sides at once: each blocks on the other.
        let (accepted, connected) = tokio::join!(
            async { bravo_endpoint.accept(&bravo_setup).await.expect("closed") },
            alpha_endpoint.connect(bravo_addr, &alpha_setup),
        );
        let (alpha_session, alpha_events) = connected.expect("alpha handshake");
        let (bravo_session, bravo_events) = accepted.expect("bravo handshake");

        let machine = |name, peer, caps, peer_caps, session, events| Machine {
            name,
            peer,
            caps,
            peer_caps,
            shares_with_peer: true,
            access: DoubleBumpClipboard::new(),
            sync: ClipboardSync::new(),
            traffic: ClipboardTraffic::default(),
            session,
            events,
        };
        (
            machine(
                "alpha",
                bravo_id.node_id(),
                alpha_caps,
                bravo_caps,
                alpha_session,
                alpha_events,
            ),
            machine(
                "bravo",
                alpha_id.node_id(),
                bravo_caps,
                alpha_caps,
                bravo_session,
                bravo_events,
            ),
            (alpha_endpoint, bravo_endpoint),
        )
    }

    /// Carry the exchange `from` has just started until neither side answers, and
    /// return every message that crossed.
    async fn carry_the_exchange(from: &mut Machine, to: &mut Machine) -> Vec<ControlMsg> {
        let (mut from, mut to) = (from, to);
        let mut transcript = Vec::new();
        loop {
            let msg = to.next_message().await;
            println!("  {} → {}:  {}", from.name, to.name, describe(&msg));
            transcript.push(msg.clone());
            match to.handle(msg).await {
                Some(_) => std::mem::swap(&mut from, &mut to),
                None => return transcript,
            }
        }
    }

    /// Whatever was on a clipboard before the agent existed stays where it is.
    async fn settle_first_sighting(machine: &mut Machine) {
        assert_eq!(machine.poll().await, Some(LocalChange::FirstSighting));
    }

    #[tokio::test]
    async fn a_copy_on_one_machine_is_pasteable_on_the_other() {
        let (mut alpha, mut bravo, _endpoints) =
            two_clipboards(full_clipboard(), full_clipboard()).await;

        alpha.access.copied_by_hand(
            ClipboardFormat::Utf8Text,
            b"from before either agent started",
        );
        settle_first_sighting(&mut alpha).await;
        settle_first_sighting(&mut bravo).await;

        // The user copies something on alpha.
        let copied = "Ship it — 22:04, and the kettle is on ☕";
        println!("alpha: the user copies {copied:?}");
        alpha
            .access
            .copied_by_hand(ClipboardFormat::Utf8Text, copied.as_bytes());
        assert!(matches!(
            alpha.poll().await,
            Some(LocalChange::Offer { .. })
        ));
        carry_the_exchange(&mut alpha, &mut bravo).await;

        let (format, pasted) = bravo.pasted().expect("bravo's clipboard is empty");
        println!(
            "bravo: the user presses Ctrl-V and gets {:?} ({format:?})",
            String::from_utf8_lossy(&pasted)
        );
        assert_eq!(format, ClipboardFormat::Utf8Text);
        assert_eq!(pasted, copied.as_bytes());

        // And it stops there. Bravo's own poll sees the change its write made —
        // twice over, because the portal echoes it — and absorbs both.
        assert_eq!(bravo.poll().await, Some(LocalChange::Echo));
        bravo.access.portal_echo();
        assert_eq!(bravo.poll().await, Some(LocalChange::Echo));
        assert!(
            alpha.heard_nothing_more().await,
            "bravo offered alpha its own payload back"
        );
        // Nothing for alpha either: its own clipboard has not moved, which the
        // worker answers with `NothingNew` and the loop with nothing at all.
        assert_eq!(alpha.poll().await, None);
        println!("bravo: its own write-back is absorbed; nothing goes back to alpha");

        // The other direction, in the richer format, on the same session.
        let html = b"<p>and <em>this</em> came back the other way</p>";
        println!("bravo: the user copies HTML");
        bravo.access.copied_by_hand(ClipboardFormat::Html, html);
        assert!(matches!(
            bravo.poll().await,
            Some(LocalChange::Offer { .. })
        ));
        carry_the_exchange(&mut bravo, &mut alpha).await;
        assert_eq!(
            alpha.pasted(),
            Some((ClipboardFormat::Html, html.to_vec())),
            "alpha did not end up with bravo's HTML"
        );
        println!(
            "alpha: pastes {:?}",
            String::from_utf8_lossy(&alpha.pasted().unwrap().1)
        );

        // Text large enough to be worth compressing, which the short strings above
        // are not: what crosses is zstd, and what is pasted is the original.
        let log: String =
            "2026-07-28T09:14:02Z  wx-agent  peer bravo is reachable\n".repeat(16_384);
        println!("alpha: the user copies {} KiB of log", log.len() / 1024);
        alpha
            .access
            .copied_by_hand(ClipboardFormat::Utf8Text, log.as_bytes());
        alpha.poll().await;
        let transcript = carry_the_exchange(&mut alpha, &mut bravo).await;
        assert!(
            transcript.iter().any(|m| matches!(
                m,
                ControlMsg::ClipboardData {
                    compression: Compression::Zstd,
                    ..
                }
            )),
            "text this repetitive should not have crossed uncompressed"
        );
        assert_eq!(
            bravo.pasted(),
            Some((ClipboardFormat::Utf8Text, log.into_bytes())),
            "the compressed payload did not come back out the way it went in"
        );
        println!(
            "bravo: pastes {} KiB, byte-exact, after decompressing",
            bravo.pasted().unwrap().1.len() / 1024
        );
    }

    #[tokio::test]
    async fn an_image_crosses_byte_exact_and_one_too_large_costs_only_the_paste() {
        let (mut alpha, mut bravo, _endpoints) =
            two_clipboards(full_clipboard(), full_clipboard()).await;
        settle_first_sighting(&mut alpha).await;
        settle_first_sighting(&mut bravo).await;

        // A screenshot-sized PNG: past every QUIC flow-control window, and the size
        // the clipboard's own stream exists for.
        let png = png_of(22 * 1024 * 1024);
        println!(
            "alpha: the user copies a {} MiB PNG",
            png.len() / 1024 / 1024
        );
        alpha.access.copied_by_hand(ClipboardFormat::Png, &png);
        alpha.poll().await;
        let transcript = carry_the_exchange(&mut alpha, &mut bravo).await;

        let (format, pasted) = bravo.pasted().expect("bravo's clipboard is empty");
        assert_eq!(format, ClipboardFormat::Png);
        assert_eq!(pasted, png, "the image did not arrive byte for byte");
        println!(
            "bravo: pastes {} bytes of PNG, byte-exact ({}…)",
            pasted.len(),
            pasted[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(
            transcript.iter().any(|m| matches!(
                m,
                ControlMsg::ClipboardData {
                    compression: Compression::None,
                    ..
                }
            )),
            "a PNG must not be handed to zstd"
        );

        // Now more than the protocol carries. The offer is made — nothing has read
        // the clipboard yet — and the request is answered honestly.
        let too_big = png_of(50 * 1024 * 1024);
        println!(
            "alpha: the user copies a {} MiB PNG, past what the protocol carries",
            too_big.len() / 1024 / 1024
        );
        alpha.access.copied_by_hand(ClipboardFormat::Png, &too_big);
        alpha.poll().await;
        let transcript = carry_the_exchange(&mut alpha, &mut bravo).await;
        assert!(
            matches!(transcript.last(), Some(ControlMsg::ClipboardStale { .. })),
            "an oversized payload must be refused, not sent"
        );
        assert_eq!(
            bravo.pasted().map(|(f, d)| (f, d.len())),
            Some((ClipboardFormat::Png, png.len())),
            "bravo's clipboard must still hold what it had"
        );

        // And the session is still there, which is the half of this that matters:
        // the refusal costs one paste and not the link.
        let after = "still here, still working";
        println!("alpha: the user copies {after:?} on the same session");
        alpha
            .access
            .copied_by_hand(ClipboardFormat::Utf8Text, after.as_bytes());
        alpha.poll().await;
        carry_the_exchange(&mut alpha, &mut bravo).await;
        assert_eq!(
            bravo.pasted(),
            Some((ClipboardFormat::Utf8Text, after.as_bytes().to_vec())),
            "the oversized refusal took the session with it"
        );
        println!(
            "bravo: pastes {:?} — the refusal cost a paste, not the session",
            String::from_utf8_lossy(&bravo.pasted().unwrap().1)
        );
    }

    #[tokio::test]
    async fn a_peer_may_say_what_it_copied_and_may_not_set_this_machines_clipboard() {
        let (alpha, mut bravo, _endpoints) =
            two_clipboards(full_clipboard(), full_clipboard()).await;
        bravo
            .access
            .copied_by_hand(ClipboardFormat::Utf8Text, b"what bravo's user copied");
        settle_first_sighting(&mut bravo).await;

        // No offer, no request: a payload pushed at a machine that never asked.
        println!("alpha: sends ClipboardData that bravo never asked for");
        alpha
            .send(ControlMsg::ClipboardData {
                format: ClipboardFormat::Utf8Text,
                serial: 1,
                compression: Compression::None,
                data: b"alpha decided what you have copied".to_vec(),
            })
            .await;
        let msg = bravo.next_message().await;
        bravo.handle(msg).await;

        assert_eq!(
            bravo.pasted(),
            Some((
                ClipboardFormat::Utf8Text,
                b"what bravo's user copied".to_vec()
            )),
            "a paired peer must not be able to set this machine's clipboard unasked"
        );
        println!("bravo: still pastes its own content — unsolicited content is refused");
    }

    #[tokio::test]
    async fn a_machine_the_user_switched_off_and_a_format_it_cannot_take_are_not_offered() {
        // Both refusals happen before anything is sent, so what proves them is the
        // wire staying silent.
        let (mut alpha, mut bravo, _endpoints) =
            two_clipboards(full_clipboard(), Capabilities::CLIPBOARD_TEXT).await;
        settle_first_sighting(&mut alpha).await;

        println!("alpha: the user copies a PNG; bravo advertises text only");
        alpha
            .access
            .copied_by_hand(ClipboardFormat::Png, &png_of(4 * 1024));
        assert!(matches!(
            alpha.poll().await,
            Some(LocalChange::Offer { .. })
        ));
        assert!(
            bravo.heard_nothing_more().await,
            "a format the peer cannot take must not be offered to it"
        );
        println!("bravo: hears nothing — the capability gate held");

        alpha.shares_with_peer = false;
        println!("alpha: the user turns the clipboard off for bravo, then copies text");
        alpha
            .access
            .copied_by_hand(ClipboardFormat::Utf8Text, b"not for bravo");
        assert!(matches!(
            alpha.poll().await,
            Some(LocalChange::Offer { .. })
        ));
        assert!(
            bravo.heard_nothing_more().await,
            "the per-peer clipboard flag was not honoured"
        );
        println!("bravo: hears nothing — the per-peer switch held");
    }

    #[tokio::test]
    async fn a_portal_grant_that_landed_after_the_handshake_still_reaches_a_peer() {
        // The bug the second half of this change fixes, as the user meets it. The
        // accept loop clones `local_info` before it awaits the next connection, so
        // on Wayland the `NodeInfo` a peer receives routinely predates the consent
        // dialog — and a peer holding that snapshot refuses to offer this machine
        // anything for the rest of the session.
        let (mut alpha, mut bravo, _endpoints) =
            two_clipboards(full_clipboard(), full_clipboard()).await;
        // What bravo was actually handed at the handshake: alpha before its grant.
        bravo.peer_caps = Capabilities::CAPTURE_INPUT | Capabilities::CAPABILITY_UPDATES;
        settle_first_sighting(&mut bravo).await;

        println!("bravo: the user copies text, holding a pre-grant snapshot of alpha");
        bravo
            .access
            .copied_by_hand(ClipboardFormat::Utf8Text, b"copied before the correction");
        assert!(matches!(
            bravo.poll().await,
            Some(LocalChange::Offer { .. })
        ));
        assert!(
            alpha.heard_nothing_more().await,
            "the stale snapshot is what this test is about"
        );
        println!("alpha: hears nothing — this is the failure, measured live on a real desktop");

        // `Engine::on_peer_ready`, which now says what this machine can do *now*.
        let correction = capability_correction(alpha.peer_caps, alpha.caps)
            .expect("bravo advertises CAPABILITY_UPDATES");
        println!("alpha → bravo:  {}", describe(&correction));
        alpha.session.send_control(&correction).await.unwrap();
        let msg = bravo.next_message().await;
        bravo.handle(msg).await;

        println!("bravo: the user copies again");
        bravo
            .access
            .copied_by_hand(ClipboardFormat::Utf8Text, b"copied after the correction");
        bravo.poll().await;
        carry_the_exchange(&mut bravo, &mut alpha).await;
        assert_eq!(
            alpha.pasted(),
            Some((
                ClipboardFormat::Utf8Text,
                b"copied after the correction".to_vec()
            )),
            "the correction did not unblock the clipboard"
        );
        println!(
            "alpha: pastes {:?}",
            String::from_utf8_lossy(&alpha.pasted().unwrap().1)
        );
    }

    /// A PNG-shaped payload of a given size.
    ///
    /// Real magic bytes and a non-repeating body: a run of one byte would compress
    /// away to nothing and measure the wrong thing.
    fn png_of(bytes: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(bytes);
        data.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        data.extend((data.len()..bytes).map(|i| (i % 251) as u8));
        data
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

    // -- pairings the UI has to be told about ------------------------------
    //
    // These exercise the two free functions rather than `Engine::on_peer_gone`
    // and `Request::Status`, because an `Engine` needs a platform backend, a
    // config directory and a live endpoint, and starting one here would put an
    // mDNS advertisement on whatever network the test runs on. The functions are
    // the whole of the behaviour: `on_peer_gone` calls one and the status
    // request builds its list from the other.

    fn established_with(peer: NodeId, role: wx_net::Role) -> Box<Established> {
        Box::new(Established {
            role,
            peer: NodeInfo {
                id: peer,
                name: "workhorse".into(),
                platform: wx_proto::Platform::Linux,
                display_server: wx_proto::DisplayServer::Wayland,
                capabilities: Capabilities::HAS_DISPLAYS,
                monitors: Vec::new(),
                agent_version: "0.1.0".into(),
            },
            protocol: wx_proto::PROTOCOL_VERSION,
            peer_protocol: wx_proto::PROTOCOL_VERSION,
            local_nonce: [3u8; 32],
            peer_nonce: [4u8; 32],
            peer_was_paired: false,
        })
    }

    /// A pairing as the engine holds it. The `PairingSession` is the code this
    /// side generated, which only the initiator has at this point.
    fn pending_pairing(peer: NodeId, initiated_locally: bool, started: Instant) -> PendingPairing {
        let established = established_with(peer, wx_net::Role::Initiator);
        let pairing = initiated_locally.then(|| {
            PairingSession::new(
                &established,
                Pin::parse("123456").expect("a six-digit code"),
            )
        });
        PendingPairing {
            node: peer,
            name: "workhorse".into(),
            initiated_locally,
            established,
            pairing,
            started,
        }
    }

    #[test]
    fn a_pairing_that_dies_with_its_session_is_announced_to_the_ui() {
        // The regression: `on_peer_gone` used to drop the entry with
        // `pending.remove(&node)` and say nothing. The UI sets its pairing card
        // from events alone, so a silent removal left every window holding a card
        // for an exchange the agent had already forgotten — and, because that card
        // suppressed the next request, no pairing could ever be prompted for
        // again without reloading the window.
        let (events, mut rx) = broadcast::channel(8);
        let mut pending = HashMap::new();
        pending.insert(node(2), pending_pairing(node(2), false, Instant::now()));

        let removed =
            end_pending_pairing(&mut pending, &events, node(2), "the connection was lost");

        assert!(removed);
        assert!(pending.is_empty(), "the pairing outlived its session");
        match rx.try_recv().expect("the UI was told nothing") {
            Event::PairingFinished {
                node: hex,
                accepted,
                message,
            } => {
                assert_eq!(hex, node(2).to_hex());
                assert!(!accepted);
                assert_eq!(message.as_deref(), Some("the connection was lost"));
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn losing_a_session_that_was_not_pairing_announces_nothing() {
        // The other half of the rule: `on_peer_gone` runs on every session loss,
        // and a paired peer disconnecting must not make the UI show a failed
        // pairing that never happened.
        let (events, mut rx) = broadcast::channel(8);
        let mut pending = HashMap::new();

        assert!(!end_pending_pairing(
            &mut pending,
            &events,
            node(2),
            "the connection was lost"
        ));
        assert!(rx.try_recv().is_err(), "a pairing nobody started was ended");
    }

    #[test]
    fn a_pairing_whose_dial_never_landed_is_announced_to_the_ui() {
        // The exit path one step before a session exists: `begin_pairing` answers
        // with a code, the window puts a live card on screen, and the dial then
        // fails. Nothing was ever put into `pending`, so neither the sibling
        // function nor the stale-pairing sweep can reach it — without this, the
        // card stayed live and suppressed every later request for the life of the
        // window, which is the reported symptom read from the other side.
        let (events, mut rx) = broadcast::channel(8);
        let mut pins = OfferedPins::default();
        pins.offer(
            node(2),
            Pin::parse("123456").expect("six digits is a PIN"),
            "workhorse".to_string(),
            Instant::now(),
        );

        let ended =
            end_undialled_pairing(&mut pins, &events, node(2), "could not reach that machine");

        assert!(ended);
        assert!(
            pins.claim(node(2)).is_none(),
            "a code was left for a connection that will never be made"
        );
        match rx.try_recv().expect("the UI was told nothing") {
            Event::PairingFinished {
                node: hex,
                accepted,
                message,
            } => {
                assert_eq!(hex, node(2).to_hex());
                assert!(!accepted);
                assert_eq!(message.as_deref(), Some("could not reach that machine"));
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn a_dial_that_fails_after_its_code_was_claimed_announces_nothing() {
        // Why `Wake::DialFailed` may not simply discard: once the code has been
        // bound to a session the pairing is under way, and the only failure left to
        // report belongs to `end_pending_pairing`. Announcing here as well would
        // fail a card the user is still typing into.
        let (events, mut rx) = broadcast::channel(8);
        let mut pins = OfferedPins::default();
        pins.offer(
            node(2),
            Pin::parse("123456").expect("six digits is a PIN"),
            "workhorse".to_string(),
            Instant::now(),
        );
        assert!(pins.claim(node(2)).is_some());

        assert!(!end_undialled_pairing(
            &mut pins,
            &events,
            node(2),
            "could not reach that machine"
        ));
        assert!(rx.try_recv().is_err(), "a pairing in progress was failed");
    }

    #[test]
    fn a_late_window_can_recover_the_pairing_it_never_heard_about() {
        // What a UI attaching after `PairingRequested` was emitted has to be able
        // to read out of the status snapshot: which machine, which way round, and
        // — only on the side that generated it — the code to show the user.
        let now = Instant::now();
        let older = now - Duration::from_secs(5);
        let mut pending = HashMap::new();
        pending.insert(node(2), pending_pairing(node(2), false, now));
        pending.insert(node(3), pending_pairing(node(3), true, older));

        let snapshots = pending_pairing_snapshots(&pending, &OfferedPins::default());

        assert_eq!(snapshots.len(), 2);
        // Oldest first, so every window adopts the same one.
        assert_eq!(snapshots[0].node, node(3).to_hex());
        assert!(snapshots[0].initiated_locally);
        assert_eq!(
            snapshots[0].pin.as_deref(),
            Some("123456"),
            "the side showing the code cannot show it"
        );
        assert_eq!(snapshots[1].node, node(2).to_hex());
        assert!(!snapshots[1].initiated_locally);
        assert_eq!(
            snapshots[1].pin, None,
            "a code was offered to the side that is supposed to type it"
        );
    }

    #[test]
    fn a_code_shown_before_its_dial_lands_is_in_the_snapshot() {
        // The captain's own symptom, from the initiating side: a code on screen and
        // a machine that never answers. Nothing is in `pending` until a session
        // exists, so a snapshot describing only `pending` never mentioned that
        // pairing at all — and a window with no events, which reconciles its card
        // against this list and against nothing else, could neither draw it after a
        // reload nor tell when it was over.
        let now = Instant::now();
        let mut pins = OfferedPins::default();
        pins.offer(
            node(2),
            Pin::parse("123456").expect("six digits is a PIN"),
            "workhorse".to_string(),
            now - Duration::from_secs(5),
        );
        let mut pending = HashMap::new();
        pending.insert(node(3), pending_pairing(node(3), false, now));

        let snapshots = pending_pairing_snapshots(&pending, &pins);

        assert_eq!(snapshots.len(), 2);
        // Ordered against the sessions by the same clock, not appended after them.
        assert_eq!(snapshots[0].node, node(2).to_hex());
        assert_eq!(snapshots[0].name, "workhorse");
        assert!(
            snapshots[0].initiated_locally,
            "only this machine offers a code before a connection exists"
        );
        assert_eq!(
            snapshots[0].pin.as_deref(),
            Some("123456"),
            "a window that reloaded cannot show the user what to type"
        );
        assert_eq!(snapshots[1].node, node(3).to_hex());

        // And once the dial has failed the entry goes, which is how a window
        // without events learns the exchange is over.
        let (events, _rx) = broadcast::channel(8);
        assert!(end_undialled_pairing(
            &mut pins,
            &events,
            node(2),
            "could not reach that machine"
        ));
        let snapshots = pending_pairing_snapshots(&pending, &pins);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].node, node(3).to_hex());
    }

    #[test]
    fn a_code_that_outlived_its_exchange_would_be_a_pairing_nothing_can_end() {
        // Why every terminal transition now clears the code as `abandon_pairing`
        // always did. A code can be left with no dial of its own outstanding — the
        // other machine's session got in first, so nothing claimed it — and while
        // the exchange runs, its `pending` entry hides that. Once the exchange ends,
        // a code still held here is published as a pairing under way that no later
        // snapshot can ever drop: the window raises a card for a machine it has just
        // been shown as paired, and that card suppresses every later request.
        let now = Instant::now();
        let mut pins = OfferedPins::default();
        pins.offer(
            node(2),
            Pin::parse("123456").expect("six digits is a PIN"),
            "workhorse".to_string(),
            now,
        );
        let mut pending = HashMap::new();
        pending.insert(node(2), pending_pairing(node(2), false, now));
        assert_eq!(pending_pairing_snapshots(&pending, &pins).len(), 1);

        // The exchange ends: `finish_pairing` on success, `on_peer_gone` when the
        // session drops, `abandon_pairing` on a refusal.
        pending.remove(&node(2));
        pins.discard(node(2));

        assert!(
            pending_pairing_snapshots(&pending, &pins).is_empty(),
            "a pairing that is over is still being advertised as under way"
        );
    }

    #[test]
    fn a_code_no_dial_is_waiting_on_is_never_turned_into_a_pair_request() {
        // The other half of the same leak: a code with no dial of its own left to
        // claim it is not the code on the user's screen, and opening a pairing with
        // it would ask the other machine for digits nobody is being shown.
        let mut pins = OfferedPins::default();
        pins.pins
            .insert(node(2), Pin::parse("123456").expect("six digits is a PIN"));

        assert!(pins.claim(node(2)).is_none());
        assert!(
            pins.claim(node(2)).is_none(),
            "the stale code was left behind to be found again"
        );
    }

    #[test]
    fn a_pairing_that_reached_a_session_is_listed_once() {
        // The cross-initiation case: this machine has shown a code and dialled while
        // the other machine's dial lands first, so the same exchange is in both
        // places for a moment. The session's entry is the same pairing further on,
        // and two entries would offer a window two cards for one pairing.
        let now = Instant::now();
        let mut pins = OfferedPins::default();
        pins.offer(
            node(2),
            Pin::parse("123456").expect("six digits is a PIN"),
            "workhorse".to_string(),
            now,
        );
        let mut pending = HashMap::new();
        pending.insert(node(2), pending_pairing(node(2), true, now));

        let snapshots = pending_pairing_snapshots(&pending, &pins);

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].node, node(2).to_hex());
    }
}
