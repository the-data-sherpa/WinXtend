//! The daemon's live view of the mesh.
//!
//! Everything the agent knows that is not the layout or the cursor: which peers
//! exist, which are paired, which are reachable right now, when each may next be
//! dialled, and where the cursor currently is. Deliberately free of I/O and of
//! the clock — `now` is always passed in — so that reconnection policy, backoff,
//! and the disconnect-recovery rules can be unit-tested rather than observed by
//! unplugging a network cable.
//!
//! The layout and the authoritative cursor live in
//! [`wx_core::InputRouter`](wx_core::InputRouter) instead. Keeping one copy of
//! each avoids the failure that would follow from two: a state struct and a
//! router disagreeing about which machine owns the cursor means input goes to one
//! machine while the UI shows another.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use wx_core::layout::GlobalLayout;
use wx_net::{DiscoveredPeer, TrustStore};
use wx_proto::{Capabilities, GlobalMonitorId, Monitor, NodeId, NodeInfo, Rect};

/// Longest wait between reconnection attempts.
///
/// A KVM has to come back on its own after a peer reboots or a Wi-Fi drop, so
/// backoff has to be capped: exponential growth without a ceiling means a machine
/// that was briefly away is not retried for hours, and the user concludes the
/// software is broken.
pub const MAX_DIAL_BACKOFF: Duration = Duration::from_secs(30);

/// First retry delay, doubled per consecutive failure.
pub const BASE_DIAL_BACKOFF: Duration = Duration::from_millis(500);

/// How long the node with the higher id waits before dialling anyway.
///
/// Both ends of a paired link want to connect, and two simultaneous dials produce
/// two sessions where one is needed. The lower node id dials immediately and the
/// higher one holds off — but only for a while, because discovery is not
/// symmetric: mDNS can easily reach one machine and not the other, and a rule
/// with no fallback would leave that pair permanently unconnected.
pub const DIAL_GRACE: Duration = Duration::from_secs(3);

/// Where a peer stands right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnStatus {
    /// Paired, but nothing has been seen of it since the agent started.
    Offline,
    /// Announced itself on the network; no session yet.
    Discovered,
    /// A dial is in flight.
    Connecting,
    /// A session exists but the peer is not trusted, so it may send nothing but
    /// pairing messages. Kept distinct from `Connected` because the difference is
    /// exactly what stops an unpaired machine injecting input.
    Pairing,
    Connected,
    /// The last attempt failed, with the reason worth showing a user.
    Failed(String),
}

impl ConnStatus {
    /// Whether input may be exchanged with this peer.
    pub fn is_usable(&self) -> bool {
        matches!(self, ConnStatus::Connected)
    }

    /// Whether a session object exists, trusted or not.
    pub fn has_session(&self) -> bool {
        matches!(self, ConnStatus::Connected | ConnStatus::Pairing)
    }
}

/// Motion datagrams discarded on the receiving side of one session.
///
/// **This is not network loss, and reading it as network loss inverts the
/// diagnosis.** A datagram the network dropped never reaches
/// [`wx_net::Session`] and cannot be counted anywhere; the only thing that
/// increments the counter behind this is a datagram that *arrived*, decoded, and
/// was thrown away because this machine's input queue was already full. Loss on
/// the wire is the design working as intended — motion is `BestEffort` precisely
/// so a lost position is superseded rather than retransmitted — whereas a
/// non-zero figure here means the consumer has fallen behind, which is a defect.
/// That is the whole reason the number is worth surfacing: without it both
/// present only as "feels laggy".
///
/// **A zero is much weaker evidence than a non-zero figure.** It says only that
/// nothing was discarded on the datagram plane, which is not the same as saying
/// the input loop kept up. `wx_net`'s sender falls back to the reliable stream
/// when the path MTU is too small or the peer never enabled datagrams, and the
/// stream reader waits on a full queue rather than discarding anything — so on
/// such a session motion never travels as a datagram, a backed-up consumer shows
/// up as latency instead, and this stays at zero however far behind the input
/// loop falls. Treat a moving count as evidence and a zero as the absence of
/// evidence rather than evidence of absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DroppedDatagrams {
    /// Total for the session that is up now.
    ///
    /// Not a lifetime figure: the counter lives on the session, so a reconnect
    /// starts a new one at zero. Said plainly here because a total that silently
    /// resets is worse than one that is known to.
    pub total: u64,
    /// How many of those arrived since the previous sample.
    ///
    /// The figure that answers "is this happening right now". A running total can
    /// only ever say that something happened at some point, which is not a
    /// question anyone asks while watching a test.
    pub recent: u64,
    /// The interval `recent` was counted over.
    ///
    /// Measured between this sample and the one before it, not the agent's tick
    /// constant. The two are not the same figure when it matters: the sampling
    /// tick shares one serial loop with the peer events the counter is a symptom
    /// of, so the tick that reads a non-zero count is by construction a late one,
    /// and labelling a long window with a short constant overstates the rate at
    /// exactly the moment someone reads it as a measurement.
    ///
    /// The first sample of a session is measured from when that session came up,
    /// which is when its counter started at zero — so `recent`, `total` and
    /// `window` describe the same interval on every sample including the first.
    ///
    /// Carried with the reading rather than assumed by whatever renders it, so a
    /// sample taken on a different cadence cannot be read as a rate it is not.
    pub window: Duration,
}

/// One peer, as the agent currently understands it.
#[derive(Debug, Clone)]
pub struct PeerState {
    pub node: NodeId,
    /// Name the peer announces for itself; the fallback label.
    pub advertised_name: String,
    /// Name the user assigned locally, which wins over the advertised one.
    pub configured_name: Option<String>,
    pub paired: bool,
    pub blocked: bool,
    /// Disabled in config: still trusted, but excluded from the mesh.
    pub enabled: bool,
    pub status: ConnStatus,
    /// Every address the peer announced, tried in order. More than one is normal
    /// (Wi-Fi and Ethernet, IPv4 and IPv6) and the first may not route.
    pub addresses: Vec<SocketAddr>,
    /// Handshake details, once a session has been established.
    pub info: Option<NodeInfo>,
    pub rtt: Option<Duration>,
    /// Motion datagrams from this peer that this machine took off the wire and
    /// threw away, sampled once a tick. See [`DroppedDatagrams`] before drawing a
    /// conclusion from it — it says nothing about the network.
    ///
    /// `None` while no session exists, for the same reason `rtt` is: the counter
    /// belongs to a connection, and the figure from the last one says nothing
    /// about a peer that is offline now.
    pub dropped_datagrams: Option<DroppedDatagrams>,
    /// When `dropped_datagrams` was last read, so the next reading can say what
    /// interval it covers instead of assuming the tick ran on time. Reset to the
    /// moment a session comes up, because that is when its counter starts at zero.
    ///
    /// Not public: it is the clock half of a reading nobody outside this module
    /// takes, and a reader that could see it without the count beside it could
    /// only misuse it.
    dropped_sampled_at: Instant,
    /// Consecutive failed dials, driving the backoff.
    pub dial_failures: u32,
    /// Earliest time another dial is worth attempting.
    pub next_dial: Option<Instant>,
    /// When this peer was first seen, for the dial tie-break grace period.
    pub first_seen: Instant,
    pub last_seen: Instant,
}

impl PeerState {
    fn new(node: NodeId, name: String, now: Instant) -> Self {
        Self {
            node,
            advertised_name: name,
            configured_name: None,
            paired: false,
            blocked: false,
            enabled: true,
            status: ConnStatus::Offline,
            addresses: Vec::new(),
            info: None,
            rtt: None,
            dropped_datagrams: None,
            dropped_sampled_at: now,
            dial_failures: 0,
            next_dial: None,
            first_seen: now,
            last_seen: now,
        }
    }

    /// Label to show in the UI and in logs.
    pub fn display_name(&self) -> &str {
        self.configured_name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(&self.advertised_name)
    }

    /// Monitors this peer reported in its handshake, or none if it has not
    /// connected yet. A headless node legitimately reports an empty list.
    pub fn monitors(&self) -> &[Monitor] {
        self.info
            .as_ref()
            .map(|i| i.monitors.as_slice())
            .unwrap_or(&[])
    }

    /// What this peer says it can do, or nothing at all before a session exists.
    ///
    /// Derived from `info` rather than kept beside it. Two copies of one fact is
    /// how a peer's advertised set and the handshake it came from end up
    /// disagreeing, which is the same failure the module docs give for keeping one
    /// copy of the cursor — and here the consequence is a feature attempted against
    /// a machine that withdrew the capability a minute ago. `info` is the peer's own
    /// account of itself and is updated in place as the peer reports changes.
    pub fn capabilities(&self) -> Capabilities {
        self.info
            .as_ref()
            .map(|i| i.capabilities)
            .unwrap_or(Capabilities::NONE)
    }

    /// Whether this peer should participate in the mesh at all.
    pub fn is_eligible(&self) -> bool {
        self.paired && !self.blocked && self.enabled
    }
}

/// Everything the agent knows about the mesh.
#[derive(Debug)]
pub struct AgentState {
    local: NodeId,
    local_name: String,
    local_monitors: Vec<Monitor>,
    peers: HashMap<NodeId, PeerState>,
    /// Monitor currently holding the cursor, mirrored from the router so that a
    /// status snapshot needs no access to it.
    cursor_on: Option<GlobalMonitorId>,
    cursor_locked: bool,
    started: Instant,
}

impl AgentState {
    pub fn new(local: NodeId, local_name: String, now: Instant) -> Self {
        Self {
            local,
            local_name,
            local_monitors: Vec::new(),
            peers: HashMap::new(),
            cursor_on: None,
            cursor_locked: false,
            started: now,
        }
    }

    pub fn local(&self) -> NodeId {
        self.local
    }

    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    pub fn set_local_name(&mut self, name: String) {
        self.local_name = name;
    }

    pub fn local_monitors(&self) -> &[Monitor] {
        &self.local_monitors
    }

    /// Record the local display list, reporting whether it actually changed.
    ///
    /// The answer gates re-laying-out and telling peers, so an unchanged
    /// enumeration — which happens on every poll — must not look like a hotplug.
    pub fn set_local_monitors(&mut self, monitors: Vec<Monitor>) -> bool {
        if self.local_monitors == monitors {
            return false;
        }
        self.local_monitors = monitors;
        true
    }

    /// The local monitor a [`wx_proto::MonitorId`] refers to.
    pub fn local_monitor(&self, id: wx_proto::MonitorId) -> Option<&Monitor> {
        self.local_monitors.iter().find(|m| m.id == id)
    }

    pub fn uptime(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    pub fn cursor_on(&self) -> Option<GlobalMonitorId> {
        self.cursor_on
    }

    /// Node whose machine holds the cursor. Falls back to this one, because with
    /// no layout at all the cursor is unambiguously here.
    pub fn cursor_owner(&self) -> NodeId {
        self.cursor_on.map(|m| m.node).unwrap_or(self.local)
    }

    pub fn set_cursor(&mut self, monitor: GlobalMonitorId, locked: bool) {
        self.cursor_on = Some(monitor);
        self.cursor_locked = locked;
    }

    pub fn cursor_locked(&self) -> bool {
        self.cursor_locked
    }

    pub fn peers(&self) -> impl Iterator<Item = &PeerState> {
        self.peers.values()
    }

    pub fn peer(&self, node: &NodeId) -> Option<&PeerState> {
        self.peers.get(node)
    }

    /// Peer entry, created as unpaired and offline if it is new.
    pub fn entry(&mut self, node: NodeId, name: &str, now: Instant) -> &mut PeerState {
        self.peers
            .entry(node)
            .or_insert_with(|| PeerState::new(node, name.to_string(), now))
    }

    /// What a peer says it can do, or nothing for a machine with no session.
    ///
    /// The one place a peer's advertised set is read from. Whether a feature is
    /// permitted is asked in exactly one spelling, `Engine::peer_supports`, which
    /// answers it from here; a second predicate at this level would be a seam a
    /// caller could reach for that knows nothing about this machine.
    pub fn peer_capabilities(&self, node: NodeId) -> Capabilities {
        self.peers
            .get(&node)
            .map(PeerState::capabilities)
            .unwrap_or(Capabilities::NONE)
    }

    /// Record what a peer now says it can do, reporting whether it changed.
    ///
    /// A capability set is not fixed for the life of a session: a peer whose last
    /// screen is unplugged stops having displays, and during the alpha a peer picks
    /// up injection or clipboard support as its backend lands. Nothing arrives to
    /// re-run the handshake when that happens, so the stored set has to be
    /// refreshed in place or every decision made against it is a minute out of date.
    ///
    /// Does nothing for a peer with no session: there is no advertised set to amend
    /// until the machine has introduced itself.
    pub fn set_peer_capabilities(&mut self, node: NodeId, caps: Capabilities) -> bool {
        let Some(info) = self.peers.get_mut(&node).and_then(|p| p.info.as_mut()) else {
            return false;
        };
        if info.capabilities == caps {
            return false;
        }
        info.capabilities = caps;
        true
    }

    /// Whether a peer can be sent input right now.
    pub fn is_reachable(&self, node: NodeId) -> bool {
        node == self.local
            || self
                .peers
                .get(&node)
                .is_some_and(|p| p.status.is_usable() && p.is_eligible())
    }

    /// Fold in what discovery saw.
    ///
    /// Discovery grants nothing: it only supplies addresses and a default name.
    /// Trust comes from the store, which is why it is re-read here rather than
    /// remembered from whenever the peer was first seen.
    pub fn observe_discovered(
        &mut self,
        peer: &DiscoveredPeer,
        trust: &TrustStore,
        now: Instant,
    ) -> bool {
        let known = self.peers.contains_key(&peer.node);
        let entry = self
            .peers
            .entry(peer.node)
            .or_insert_with(|| PeerState::new(peer.node, peer.advertised_name.clone(), now));
        let changed = entry.addresses != peer.addresses
            || entry.advertised_name != peer.advertised_name
            || !known;
        entry.advertised_name = peer.advertised_name.clone();
        entry.addresses = peer.addresses.clone();
        entry.last_seen = now;
        entry.paired = trust.is_paired(&peer.node);
        entry.blocked = trust.is_blocked(&peer.node);
        if !entry.status.has_session() && !matches!(entry.status, ConnStatus::Connecting) {
            entry.status = ConnStatus::Discovered;
            // A peer that has just reappeared deserves an immediate attempt: it
            // has most likely rebooted, and making the user wait out a backoff
            // that accrued while it was off would look like a hang.
            entry.dial_failures = 0;
            entry.next_dial = None;
        }
        changed
    }

    /// Discovery says the peer has gone away.
    ///
    /// The live session is deliberately not touched. mDNS announcements expire
    /// for reasons that have nothing to do with reachability — a missed multicast
    /// packet, a switch that prunes groups — and tearing down a working
    /// connection because of one would drop the cursor mid-gesture.
    pub fn observe_lost(&mut self, node: NodeId) {
        if let Some(p) = self.peers.get_mut(&node) {
            if !p.status.has_session() {
                p.status = ConnStatus::Offline;
            }
            p.addresses.clear();
        }
    }

    /// Refresh trust and per-peer config for every known peer.
    pub fn sync_trust(&mut self, trust: &TrustStore, config: &crate::config::Config, now: Instant) {
        for (node, entry) in trust.peers() {
            let peer = self.entry(node, &entry.name, now);
            peer.paired = !entry.blocked;
            peer.blocked = entry.blocked;
        }
        for peer in self.peers.values_mut() {
            peer.paired = trust.is_paired(&peer.node);
            peer.blocked = trust.is_blocked(&peer.node);
            let cfg = config.peer(&peer.node);
            peer.enabled = cfg.enabled;
            peer.configured_name = cfg
                .name
                .clone()
                .or_else(|| trust.name_of(&peer.node).map(str::to_string));
        }
    }

    pub fn set_status(&mut self, node: NodeId, status: ConnStatus) {
        if let Some(p) = self.peers.get_mut(&node) {
            p.status = status;
        }
    }

    /// A session came up.
    pub fn on_session(&mut self, info: NodeInfo, trusted: bool, now: Instant) {
        let node = info.id;
        let name = info.name.clone();
        let peer = self.entry(node, &name, now);
        peer.advertised_name = name;
        peer.info = Some(info);
        peer.last_seen = now;
        peer.dial_failures = 0;
        peer.next_dial = None;
        // A new session brings a new counter, starting at zero. Both halves of the
        // reading are reset here so the first sample on it is a delta from zero
        // over the age of this session, rather than a delta from a total that
        // belonged to a connection which is gone and a window that predates it.
        peer.dropped_datagrams = None;
        peer.dropped_sampled_at = now;
        peer.status = if trusted {
            ConnStatus::Connected
        } else {
            ConnStatus::Pairing
        };
    }

    /// A session went away, for any reason.
    ///
    /// Backoff is armed here rather than only on a failed dial, because a peer
    /// that accepts a connection and immediately drops it would otherwise be
    /// redialled in a tight loop.
    pub fn on_disconnected(&mut self, node: NodeId, reason: Option<String>, now: Instant) {
        if let Some(p) = self.peers.get_mut(&node) {
            p.status = match reason {
                Some(r) => ConnStatus::Failed(r),
                None => ConnStatus::Offline,
            };
            p.rtt = None;
            // Cleared with the session it was counted on. Leaving the last
            // reading behind would have a peer that is offline still reporting a
            // window of drops, which reads as something happening now.
            p.dropped_datagrams = None;
            p.dial_failures = p.dial_failures.saturating_add(1);
            p.next_dial = Some(now + backoff(p.dial_failures));
        }
    }

    pub fn set_rtt(&mut self, node: NodeId, rtt: Duration) {
        if let Some(p) = self.peers.get_mut(&node) {
            p.rtt = Some(rtt);
        }
    }

    /// Record a session's current discarded-datagram total, deriving both the
    /// count and the window from the previous reading, and return what was
    /// recorded.
    ///
    /// Returned rather than only stored so the caller can log the change without
    /// reading the peer back out; the log line is the half of this that is usable
    /// while a test is running, and a snapshot field nobody is looking at is not.
    ///
    /// The window is measured from `now` back to the previous sample rather than
    /// taken as the caller's tick interval, because the interval a tick actually
    /// covers and the interval it was scheduled for diverge in precisely the case
    /// that produces a count: the sampling tick and the peer events being dropped
    /// share one serial loop, so a tick reading a non-zero count has waited behind
    /// the backlog that caused it. The constant would name a shorter window than
    /// was measured and overstate the rate. `now` is passed in for the reason
    /// everything else here takes it — see the module docs.
    ///
    /// `saturating_sub` because the total legitimately goes backwards: a session
    /// replaced without a disconnect being seen starts its counter at zero. That
    /// reads as a window of zero — one sample of understatement, and every sample
    /// after it is right again. Wrapping would instead invent an enormous figure
    /// at exactly the point someone is trying to read the log.
    pub fn sample_dropped_datagrams(
        &mut self,
        node: NodeId,
        total: u64,
        now: Instant,
    ) -> Option<DroppedDatagrams> {
        let peer = self.peers.get_mut(&node)?;
        let previous = peer.dropped_datagrams.map_or(0, |d| d.total);
        let reading = DroppedDatagrams {
            total,
            recent: total.saturating_sub(previous),
            window: now.saturating_duration_since(peer.dropped_sampled_at),
        };
        peer.dropped_sampled_at = now;
        peer.dropped_datagrams = Some(reading);
        Some(reading)
    }

    /// Forget a peer's discarded-datagram reading without touching anything else
    /// about its connection.
    ///
    /// For the paths that close a session without going through
    /// [`Self::on_disconnected`] — restarting a pairing, abandoning one. Those
    /// deliberately leave the peer's status and backoff alone, but the reading is
    /// a property of the session that just went away, and left behind it says a
    /// window of drops is happening now on a connection that no longer exists.
    /// The next session re-arms it; this covers the gap, including the case where
    /// the next session never arrives.
    pub fn clear_dropped_datagrams(&mut self, node: NodeId) {
        if let Some(p) = self.peers.get_mut(&node) {
            p.dropped_datagrams = None;
        }
    }

    /// Peers that should be dialled now, with the addresses to try.
    ///
    /// Pure with respect to the clock and to the tie-break rule, so the awkward
    /// cases — a peer with no address, a peer in backoff, the higher-id side of a
    /// pair — are all testable.
    pub fn dial_targets(&self, now: Instant) -> Vec<(NodeId, Vec<SocketAddr>)> {
        let mut out = Vec::new();
        for peer in self.peers.values() {
            if !peer.is_eligible() || peer.status.has_session() {
                continue;
            }
            if matches!(peer.status, ConnStatus::Connecting) || peer.addresses.is_empty() {
                continue;
            }
            if peer.next_dial.is_some_and(|t| now < t) {
                continue;
            }
            // The higher node id defers, so that two paired machines seeing each
            // other at the same moment do not establish two sessions.
            if self.local > peer.node && now.saturating_duration_since(peer.first_seen) < DIAL_GRACE
            {
                continue;
            }
            out.push((peer.node, peer.addresses.clone()));
        }
        // Deterministic order so a log from one run can be compared with another.
        out.sort_by_key(|(node, _)| node.to_hex());
        out
    }
}

/// Delay before the next dial after `failures` consecutive failures.
///
/// Doubling, capped. The cap is the important half: an uncapped exponential means
/// a machine that was switched off overnight is not retried for hours after it
/// comes back.
pub fn backoff(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let shift = (failures - 1).min(16);
    BASE_DIAL_BACKOFF
        .saturating_mul(1u32 << shift)
        .min(MAX_DIAL_BACKOFF)
}

/// Where the cursor must be put when the machine holding it becomes unreachable.
///
/// Returns `None` when nothing needs to happen — the cursor is somewhere still
/// reachable — and otherwise the monitor to warp to.
///
/// The rescue prefers this machine, because that is where the user is sitting: a
/// cursor recovered onto a third machine's screen is no more usable than one
/// stranded on a dead peer. Among local monitors the nearest to where the cursor
/// was wins, so the pointer reappears next to where it disappeared rather than on
/// whichever screen the OS happens to enumerate first.
///
/// The fall back to a reachable peer exists for headless nodes: a machine with no
/// displays of its own has nowhere local to put the cursor, and leaving it on a
/// dead peer would mean losing input entirely until something else changed.
pub fn reclaim_target(
    layout: &GlobalLayout,
    cursor_on: GlobalMonitorId,
    local: NodeId,
    is_reachable: impl Fn(NodeId) -> bool,
) -> Option<GlobalMonitorId> {
    if is_reachable(cursor_on.node) {
        return None;
    }
    let from = layout.rect(cursor_on);

    let nearest = |candidates: Vec<(GlobalMonitorId, Rect)>| -> Option<GlobalMonitorId> {
        let reference = from?;
        candidates
            .into_iter()
            .min_by(|a, b| {
                distance_between(reference, a.1)
                    .total_cmp(&distance_between(reference, b.1))
                    // Ties broken by id so the choice does not vary run to run.
                    .then_with(|| a.0.monitor.0.cmp(&b.0.monitor.0))
            })
            .map(|(id, _)| id)
    };

    let locals: Vec<(GlobalMonitorId, Rect)> = layout
        .monitors_of(local)
        .filter(|p| !p.global_bounds.is_empty())
        .map(|p| (p.monitor, p.global_bounds))
        .collect();
    if !locals.is_empty() {
        // Without a rectangle to measure from — the stranded monitor is no longer
        // in the layout at all — any local screen beats none.
        return nearest(locals.clone()).or_else(|| locals.first().map(|(id, _)| *id));
    }

    let elsewhere: Vec<(GlobalMonitorId, Rect)> = layout
        .iter()
        .filter(|p| {
            p.monitor.node != cursor_on.node
                && !p.global_bounds.is_empty()
                && is_reachable(p.monitor.node)
        })
        .map(|p| (p.monitor, p.global_bounds))
        .collect();
    nearest(elsewhere.clone()).or_else(|| elsewhere.first().map(|(id, _)| *id))
}

/// Squared distance between two rectangles' centres.
///
/// Squared because only the ordering matters and a square root buys nothing.
fn distance_between(a: Rect, b: Rect) -> f64 {
    let cx = |r: Rect| r.x as f64 + r.w as f64 / 2.0;
    let cy = |r: Rect| r.y as f64 + r.h as f64 / 2.0;
    let dx = cx(a) - cx(b);
    let dy = cy(a) - cy(b);
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autolayout;
    use crate::config::Config;
    use wx_proto::{MonitorId, Placement};

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    fn gid(n: u8, m: u32) -> GlobalMonitorId {
        GlobalMonitorId::new(node(n), MonitorId(m))
    }

    fn mon(id: u32, x: i32, y: i32, w: u32, h: u32) -> Monitor {
        Monitor {
            id: MonitorId(id),
            name: format!("m{id}"),
            local_bounds: Rect::new(x, y, w, h),
            scale: 1.0,
            primary: id == 0,
        }
    }

    fn place(id: GlobalMonitorId, x: i32, y: i32, w: u32, h: u32) -> Placement {
        Placement {
            monitor: id,
            global_bounds: Rect::new(x, y, w, h),
            cursor_scale: 1.0,
        }
    }

    fn addr(n: u8) -> SocketAddr {
        format!("10.0.0.{n}:24800").parse().unwrap()
    }

    fn discovered(n: u8) -> DiscoveredPeer {
        DiscoveredPeer {
            node: node(n),
            advertised_name: format!("peer{n}"),
            agent_version: "0.1.0".into(),
            addresses: vec![addr(n)],
            fullname: format!("wx-{n}._winxtend._udp.local."),
        }
    }

    fn info(n: u8, monitors: Vec<Monitor>) -> NodeInfo {
        NodeInfo {
            id: node(n),
            name: format!("peer{n}"),
            platform: wx_proto::Platform::Linux,
            display_server: wx_proto::DisplayServer::X11,
            capabilities: wx_proto::Capabilities::CAPTURE_INPUT
                | wx_proto::Capabilities::INJECT_INPUT
                | wx_proto::Capabilities::HAS_DISPLAYS,
            monitors,
            agent_version: "0.1.0".into(),
        }
    }

    /// This machine (node 1) on the left, one peer (node 2) on the right.
    fn two_machines() -> GlobalLayout {
        let mut l = GlobalLayout::new();
        l.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        l.insert(place(gid(2, 0), 1920, 0, 1920, 1080));
        l
    }

    #[test]
    fn a_reachable_peer_needs_no_rescue() {
        let layout = two_machines();
        assert_eq!(
            reclaim_target(&layout, gid(2, 0), node(1), |_| true),
            None,
            "the cursor was moved for no reason"
        );
    }

    #[test]
    fn a_cursor_on_a_dead_peer_comes_home() {
        let layout = two_machines();
        assert_eq!(
            reclaim_target(&layout, gid(2, 0), node(1), |n| n == node(1)),
            Some(gid(1, 0))
        );
    }

    #[test]
    fn the_cursor_reappears_on_the_local_screen_nearest_where_it_was() {
        // Three local screens in a row with the dead peer beyond the rightmost.
        // Coming back onto the leftmost would be a jump across the whole desk.
        let mut layout = GlobalLayout::new();
        layout.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        layout.insert(place(gid(1, 1), 1920, 0, 1920, 1080));
        layout.insert(place(gid(1, 2), 3840, 0, 1920, 1080));
        layout.insert(place(gid(2, 0), 5760, 0, 1920, 1080));

        assert_eq!(
            reclaim_target(&layout, gid(2, 0), node(1), |n| n == node(1)),
            Some(gid(1, 2))
        );
    }

    #[test]
    fn a_headless_node_recovers_onto_a_peer_that_is_still_alive() {
        // With no local screens there is nowhere else to put the cursor, and
        // leaving it on the dead machine would lose input entirely.
        let mut layout = GlobalLayout::new();
        layout.insert(place(gid(2, 0), 0, 0, 1920, 1080));
        layout.insert(place(gid(3, 0), 1920, 0, 1920, 1080));

        assert_eq!(
            reclaim_target(&layout, gid(2, 0), node(1), |n| n == node(3)),
            Some(gid(3, 0))
        );
    }

    #[test]
    fn nothing_reachable_means_no_target_rather_than_a_wrong_one() {
        let layout = two_machines();
        let mut only_peer = GlobalLayout::new();
        only_peer.insert(place(gid(2, 0), 0, 0, 1920, 1080));
        assert_eq!(
            reclaim_target(&only_peer, gid(2, 0), node(1), |_| false),
            None
        );
        // And with an empty layout there is nowhere at all.
        assert_eq!(
            reclaim_target(&GlobalLayout::new(), gid(2, 0), node(1), |_| false),
            None
        );
        let _ = layout;
    }

    #[test]
    fn a_stranded_monitor_missing_from_the_layout_still_finds_a_way_home() {
        // The peer's display was unplugged and the layout no longer holds its
        // rectangle, so there is nothing to measure distance from.
        let mut layout = GlobalLayout::new();
        layout.insert(place(gid(1, 0), 0, 0, 1920, 1080));
        assert_eq!(
            reclaim_target(&layout, gid(2, 7), node(1), |n| n == node(1)),
            Some(gid(1, 0))
        );
    }

    #[test]
    fn degenerate_local_screens_are_never_chosen_as_a_refuge() {
        // A cursor parked on a zero-extent rectangle can never leave it, so
        // rescuing onto one is worse than not rescuing at all.
        let mut layout = GlobalLayout::new();
        layout.insert(place(gid(1, 0), 0, 0, 0, 0));
        layout.insert(place(gid(1, 1), 0, 0, 1280, 800));
        layout.insert(place(gid(2, 0), 1280, 0, 1920, 1080));
        assert_eq!(
            reclaim_target(&layout, gid(2, 0), node(1), |n| n == node(1)),
            Some(gid(1, 1))
        );
    }

    #[test]
    fn reclaiming_and_routing_agree_about_who_owns_the_cursor() {
        // The end-to-end property the recovery path exists for: after the rescue
        // the router must be routing input locally again.
        let layout = two_machines();
        let cursor =
            wx_core::VirtualCursor::at(&layout, gid(2, 0), wx_proto::NormPos::new(0.5, 0.5))
                .unwrap();
        let mut router = wx_core::InputRouter::new(node(1), layout.clone(), cursor);
        assert!(!router.owns_cursor());

        let target = reclaim_target(&layout, router.cursor().monitor(), node(1), |n| {
            n == node(1)
        })
        .expect("no rescue offered");
        let actions = router
            .warp(target, wx_proto::NormPos::new(0.5, 0.5))
            .unwrap();

        assert!(router.owns_cursor(), "the cursor is still on the dead peer");
        // The dead peer is told to let go, so that if it comes back it does not
        // believe it still owns the cursor.
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, wx_core::RouteAction::Yield { node: n } if *n == node(2))),
            "{actions:?}"
        );
    }

    #[test]
    fn a_locked_cursor_is_still_reclaimed_from_a_dead_peer() {
        // The lock stops accidental crossings, not deliberate recovery. Honouring
        // it here would strand the user with no pointer at all.
        let layout = two_machines();
        let cursor =
            wx_core::VirtualCursor::at(&layout, gid(2, 0), wx_proto::NormPos::new(0.5, 0.5))
                .unwrap();
        let mut router = wx_core::InputRouter::new(node(1), layout.clone(), cursor);
        router.set_locked(true);

        let target = reclaim_target(&layout, gid(2, 0), node(1), |n| n == node(1)).unwrap();
        router
            .warp(target, wx_proto::NormPos::new(0.5, 0.5))
            .unwrap();
        assert!(router.owns_cursor());
        assert!(router.is_locked(), "the lock should survive the rescue");
    }

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        assert_eq!(backoff(0), Duration::ZERO);
        assert_eq!(backoff(1), BASE_DIAL_BACKOFF);
        assert_eq!(backoff(2), BASE_DIAL_BACKOFF * 2);
        assert_eq!(backoff(u32::MAX), MAX_DIAL_BACKOFF);
        // Monotonic, and never past the cap.
        let mut last = Duration::ZERO;
        for n in 0..40 {
            let d = backoff(n);
            assert!(d >= last, "backoff went backwards at {n}");
            assert!(d <= MAX_DIAL_BACKOFF);
            last = d;
        }
    }

    fn state_with_peer(local: u8, peer: u8, now: Instant) -> (AgentState, TrustStore, Config) {
        let mut trust = TrustStore::new();
        trust.trust(node(peer), format!("peer{peer}"));
        let config = Config::default();
        let mut state = AgentState::new(node(local), "me".into(), now);
        state.observe_discovered(&discovered(peer), &trust, now);
        state.sync_trust(&trust, &config, now);
        (state, trust, config)
    }

    #[test]
    fn a_paired_discovered_peer_is_dialled() {
        let now = Instant::now();
        let (state, _, _) = state_with_peer(1, 2, now);
        assert_eq!(state.dial_targets(now), vec![(node(2), vec![addr(2)])]);
    }

    #[test]
    fn an_unpaired_peer_is_never_dialled() {
        // Connecting to a machine the user has not approved would leak this
        // node's monitor list and name to anything on the LAN.
        let now = Instant::now();
        let mut state = AgentState::new(node(1), "me".into(), now);
        let trust = TrustStore::new();
        state.observe_discovered(&discovered(2), &trust, now);
        state.sync_trust(&trust, &Config::default(), now);
        assert!(state.dial_targets(now).is_empty());
    }

    #[test]
    fn a_blocked_peer_is_never_dialled() {
        let now = Instant::now();
        let mut trust = TrustStore::new();
        trust.block(node(2), "peer2");
        let mut state = AgentState::new(node(1), "me".into(), now);
        state.observe_discovered(&discovered(2), &trust, now);
        state.sync_trust(&trust, &Config::default(), now);
        assert!(state.dial_targets(now).is_empty());
    }

    #[test]
    fn a_peer_disabled_in_config_is_left_alone_without_being_unpaired() {
        let now = Instant::now();
        let (mut state, trust, mut config) = state_with_peer(1, 2, now);
        config.peer_mut(&node(2)).enabled = false;
        state.sync_trust(&trust, &config, now);
        assert!(state.dial_targets(now).is_empty());
        assert!(state.peer(&node(2)).unwrap().paired, "trust was discarded");
    }

    #[test]
    fn a_peer_with_no_address_is_not_dialled() {
        let now = Instant::now();
        let mut trust = TrustStore::new();
        trust.trust(node(2), "peer2");
        let mut state = AgentState::new(node(1), "me".into(), now);
        state.sync_trust(&trust, &Config::default(), now);
        // Known from the trust store but never seen on the network.
        assert!(state.dial_targets(now).is_empty());
        assert_eq!(state.peer(&node(2)).unwrap().status, ConnStatus::Offline);
    }

    #[test]
    fn the_higher_node_id_defers_briefly_so_only_one_session_is_opened() {
        let now = Instant::now();
        // Local id is higher than the peer's, so this side waits.
        let (state, _, _) = state_with_peer(9, 2, now);
        assert!(state.dial_targets(now).is_empty());
        // But not forever: discovery is not symmetric, and a rule with no
        // fallback would leave the pair permanently unconnected.
        assert_eq!(
            state.dial_targets(now + DIAL_GRACE + Duration::from_millis(1)),
            vec![(node(2), vec![addr(2)])]
        );
    }

    #[test]
    fn the_lower_node_id_dials_at_once() {
        let now = Instant::now();
        let (state, _, _) = state_with_peer(1, 9, now);
        assert_eq!(state.dial_targets(now).len(), 1);
    }

    #[test]
    fn a_failed_dial_is_not_retried_immediately() {
        let now = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, now);
        state.on_disconnected(node(2), Some("refused".into()), now);
        assert!(state.dial_targets(now).is_empty());
        assert_eq!(
            state.dial_targets(now + MAX_DIAL_BACKOFF).len(),
            1,
            "the peer was never retried"
        );
    }

    #[test]
    fn a_reappearing_peer_skips_its_accrued_backoff() {
        // A machine that has just rebooted is answering now; making the user wait
        // out a backoff earned while it was switched off looks like a hang.
        let now = Instant::now();
        let (mut state, trust, _) = state_with_peer(1, 2, now);
        for _ in 0..6 {
            state.on_disconnected(node(2), Some("down".into()), now);
        }
        assert!(state.dial_targets(now).is_empty());
        state.observe_discovered(&discovered(2), &trust, now);
        assert_eq!(state.dial_targets(now).len(), 1);
    }

    #[test]
    fn a_connected_peer_is_not_dialled_again() {
        let now = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, now);
        state.on_session(info(2, vec![mon(0, 0, 0, 1920, 1080)]), true, now);
        assert!(state.dial_targets(now).is_empty());
        assert!(state.is_reachable(node(2)));
    }

    #[test]
    fn a_peer_that_did_not_advertise_the_message_is_never_sent_it() {
        // An older build has no `CapabilitiesChanged` variant to decode into, and
        // the control stream tears down when a decode fails. Skipping that peer
        // costs it one update; sending it would cost it the session. The bit in the
        // handshake is the only statement it makes about which it is.
        let now = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, now);
        let older = info(2, vec![]);
        assert!(
            !older
                .capabilities
                .contains(Capabilities::CAPABILITY_UPDATES),
            "the older build advertises nothing about this message"
        );
        state.on_session(older.clone(), true, now);

        assert!(
            state.is_reachable(node(2)),
            "an older peer is still a peer, and still gets everything else"
        );
        assert!(!state
            .peer_capabilities(node(2))
            .contains(Capabilities::CAPABILITY_UPDATES));

        // The same machine on a build that does understand the message.
        let newer = NodeInfo {
            capabilities: older.capabilities.union(Capabilities::CAPABILITY_UPDATES),
            ..older
        };
        state.on_session(newer, true, now);
        assert!(state
            .peer_capabilities(node(2))
            .contains(Capabilities::CAPABILITY_UPDATES));
    }

    #[test]
    fn a_peer_that_has_not_introduced_itself_is_sent_nothing_optional() {
        // A peer known only from discovery has advertised nothing, so the answer
        // has to be no rather than an assumption.
        let now = Instant::now();
        let (state, _, _) = state_with_peer(1, 2, now);
        assert!(!state
            .peer_capabilities(node(2))
            .contains(Capabilities::CAPABILITY_UPDATES));
    }

    #[test]
    fn an_unpaired_session_is_not_treated_as_reachable() {
        // A pairing-mode session must not be able to inject input; the whole
        // point of admitting it is to show the user a PIN prompt.
        let now = Instant::now();
        let mut state = AgentState::new(node(1), "me".into(), now);
        state.on_session(info(2, vec![]), false, now);
        assert_eq!(state.peer(&node(2)).unwrap().status, ConnStatus::Pairing);
        assert!(!state.is_reachable(node(2)));
    }

    #[test]
    fn a_peers_advertised_capabilities_survive_the_handshake() {
        // The whole point of storing them: something has to be able to ask what a
        // peer can do without holding the handshake message it arrived in.
        let now = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, now);
        state.on_session(info(2, vec![mon(0, 0, 0, 1920, 1080)]), true, now);

        let caps = state.peer_capabilities(node(2));
        assert!(caps.contains(wx_proto::Capabilities::INJECT_INPUT));
        assert!(caps.contains(wx_proto::Capabilities::HAS_DISPLAYS));
        // Never advertised, so never assumed.
        assert!(!caps.contains(wx_proto::Capabilities::CLIPBOARD_TEXT));
        assert!(!caps.contains(wx_proto::Capabilities::SCREENSAVER_SYNC));
    }

    #[test]
    fn a_machine_that_has_not_introduced_itself_advertises_nothing() {
        // Discovery supplies an address and a name and nothing else. Assuming a
        // capability here would have the agent attempt a feature before the peer
        // has ever said it can do it.
        let now = Instant::now();
        let (state, _, _) = state_with_peer(1, 2, now);
        assert_eq!(
            state.peer_capabilities(node(2)),
            wx_proto::Capabilities::NONE
        );
        assert!(!state
            .peer_capabilities(node(2))
            .contains(wx_proto::Capabilities::INJECT_INPUT));
        // And a machine that is not known at all is not a special case.
        assert_eq!(
            state.peer_capabilities(node(7)),
            wx_proto::Capabilities::NONE
        );
    }

    #[test]
    fn a_peer_that_withdraws_a_capability_is_believed() {
        // A peer whose last screen was unplugged stops having displays. Holding the
        // handshake's answer for the life of the session would keep the agent
        // placing monitors on a machine that no longer has any.
        let now = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, now);
        state.on_session(info(2, vec![mon(0, 0, 0, 1920, 1080)]), true, now);

        let reduced = wx_proto::Capabilities::CAPTURE_INPUT | wx_proto::Capabilities::INJECT_INPUT;
        assert!(state.set_peer_capabilities(node(2), reduced));
        assert!(!state
            .peer_capabilities(node(2))
            .contains(wx_proto::Capabilities::HAS_DISPLAYS));
        // An unchanged set is not reported as a change, so nothing downstream
        // re-renders or re-broadcasts on every tick.
        assert!(!state.set_peer_capabilities(node(2), reduced));
        // A peer with no session has nothing to amend.
        assert!(!state.set_peer_capabilities(node(7), reduced));
    }

    #[test]
    fn this_machine_is_always_reachable() {
        let now = Instant::now();
        let state = AgentState::new(node(1), "me".into(), now);
        assert!(state.is_reachable(node(1)));
    }

    #[test]
    fn losing_an_announcement_does_not_drop_a_working_session() {
        // Multicast is lossy; tearing down a live connection over a missed
        // announcement would drop the cursor mid-gesture.
        let now = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, now);
        state.on_session(info(2, vec![]), true, now);
        state.observe_lost(node(2));
        assert_eq!(state.peer(&node(2)).unwrap().status, ConnStatus::Connected);
    }

    #[test]
    fn an_unchanged_display_list_is_not_reported_as_a_hotplug() {
        let now = Instant::now();
        let mut state = AgentState::new(node(1), "me".into(), now);
        let screens = vec![mon(0, 0, 0, 1920, 1080)];
        assert!(state.set_local_monitors(screens.clone()));
        assert!(!state.set_local_monitors(screens));
        assert!(state.set_local_monitors(vec![mon(0, 0, 0, 2560, 1440)]));
    }

    #[test]
    fn a_locally_renamed_peer_keeps_that_name_over_the_advertised_one() {
        let now = Instant::now();
        let (mut state, trust, mut config) = state_with_peer(1, 2, now);
        config.peer_mut(&node(2)).name = Some("mac mini".into());
        state.sync_trust(&trust, &config, now);
        assert_eq!(state.peer(&node(2)).unwrap().display_name(), "mac mini");
    }

    const TICK: Duration = Duration::from_secs(2);

    #[test]
    fn a_datagram_drop_count_is_reported_as_a_window_and_not_only_a_total() {
        // The whole point of the field: a running total says something happened at
        // some point, and someone watching a test needs to know whether it is
        // happening now and whether it is getting worse.
        let start = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, start);
        state.on_session(info(2, vec![]), true, start);

        let first = state
            .sample_dropped_datagrams(node(2), 4, start + TICK)
            .unwrap();
        assert_eq!((first.total, first.recent), (4, 4));

        // Worse: the window grows even though the total merely climbed.
        let second = state
            .sample_dropped_datagrams(node(2), 17, start + 2 * TICK)
            .unwrap();
        assert_eq!((second.total, second.recent), (17, 13));

        // Over: the total stands still, and the window says so.
        let third = state
            .sample_dropped_datagrams(node(2), 17, start + 3 * TICK)
            .unwrap();
        assert_eq!((third.total, third.recent), (17, 0));
        assert_eq!(third.window, TICK);
    }

    #[test]
    fn the_window_is_the_interval_that_elapsed_and_not_the_interval_intended() {
        // The reason the caller passes a clock rather than its tick constant. The
        // sampling tick shares one serial loop with the peer events being dropped,
        // so the tick that reads a non-zero count is a late one by construction —
        // exactly the sample somebody reads as a rate. Labelling a 3.3s window with
        // a 2s constant overstates that rate by half.
        let start = Instant::now();
        let late = Duration::from_millis(3_300);
        let (mut state, _, _) = state_with_peer(1, 2, start);
        state.on_session(info(2, vec![]), true, start);
        state.sample_dropped_datagrams(node(2), 0, start + TICK);

        let behind = state
            .sample_dropped_datagrams(node(2), 12, start + TICK + late)
            .unwrap();
        assert_eq!(behind.recent, 12);
        assert_eq!(behind.window, late);
        assert_ne!(behind.window, TICK);
    }

    #[test]
    fn the_first_sample_of_a_session_is_measured_from_when_it_came_up() {
        // A session's counter starts at zero when the session does, so the honest
        // window for the first reading is the age of the session — not a full tick
        // it did not cover, and not nothing.
        let start = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, start);
        state.on_session(info(2, vec![]), true, start + Duration::from_secs(10));

        let first = state
            .sample_dropped_datagrams(node(2), 6, start + Duration::from_secs(11))
            .unwrap();
        assert_eq!((first.total, first.recent), (6, 6));
        assert_eq!(first.window, Duration::from_secs(1));
    }

    #[test]
    fn a_reconnect_counts_the_new_sessions_drops_from_zero() {
        // The counter lives on the session, so a reconnect starts a new one at
        // zero. The disconnect clears the reading, so the first sample on the new
        // session is measured against nothing rather than against a total that
        // belonged to a connection which no longer exists.
        let start = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, start);
        state.on_session(info(2, vec![]), true, start);
        state.sample_dropped_datagrams(node(2), 900, start + TICK);

        state.on_disconnected(node(2), None, start + TICK);
        state.on_session(info(2, vec![]), true, start + 2 * TICK);

        let after = state
            .sample_dropped_datagrams(node(2), 3, start + 3 * TICK)
            .unwrap();
        assert_eq!((after.total, after.recent), (3, 3));
        // Measured from the new session coming up, not from the last sample taken
        // on the old one.
        assert_eq!(after.window, TICK);
    }

    #[test]
    fn a_total_that_goes_backwards_understates_rather_than_wrapping() {
        // The backstop for a session replaced without a disconnect being seen.
        // Wrapping would report several billion drops in one tick at exactly the
        // moment someone is reading the log to find out what just happened, so the
        // reset costs one sample of understatement instead, and the sample after
        // it is right again.
        let start = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, start);
        state.on_session(info(2, vec![]), true, start);
        state.sample_dropped_datagrams(node(2), 900, start + TICK);

        let reset = state
            .sample_dropped_datagrams(node(2), 3, start + 2 * TICK)
            .unwrap();
        assert_eq!((reset.total, reset.recent), (3, 0));

        let next = state
            .sample_dropped_datagrams(node(2), 9, start + 3 * TICK)
            .unwrap();
        assert_eq!((next.total, next.recent), (9, 6));
    }

    #[test]
    fn a_peer_that_went_away_stops_reporting_the_drops_of_a_dead_session() {
        // A window figure left behind on an offline peer reads as loss happening
        // now, on a connection that no longer exists.
        let start = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, start);
        state.on_session(info(2, vec![]), true, start);
        state.sample_dropped_datagrams(node(2), 12, start + TICK);
        assert!(state.peer(&node(2)).unwrap().dropped_datagrams.is_some());

        state.on_disconnected(node(2), None, start + TICK);
        assert!(state.peer(&node(2)).unwrap().dropped_datagrams.is_none());
    }

    #[test]
    fn a_session_closed_without_a_disconnect_stops_reporting_its_drops_too() {
        // Restarting a pairing, and abandoning one, both close the session without
        // going through `on_disconnected` — deliberately, because neither is a
        // connection ending and neither should arm backoff. The reading still
        // belonged to the session that just closed, so an offline peer would go on
        // showing a window of drops indefinitely if the redial never landed, and
        // the first sample on the next session would see the total go backwards and
        // announce that drops had stopped when they had never been counted.
        let start = Instant::now();
        let (mut state, _, _) = state_with_peer(1, 2, start);
        state.on_session(info(2, vec![]), true, start);
        state.sample_dropped_datagrams(node(2), 48, start + TICK);

        state.clear_dropped_datagrams(node(2));
        assert!(state.peer(&node(2)).unwrap().dropped_datagrams.is_none());
        // Only the reading: the peer's standing with the mesh is not this
        // function's business, and rewriting it is what calling `on_disconnected`
        // here would have got wrong.
        assert_eq!(state.peer(&node(2)).unwrap().status, ConnStatus::Connected);
        assert_eq!(state.peer(&node(2)).unwrap().dial_failures, 0);
    }

    #[test]
    fn sampling_a_peer_that_is_not_known_reports_nothing() {
        let now = Instant::now();
        let mut state = AgentState::new(node(1), "me".into(), now);
        assert!(state.sample_dropped_datagrams(node(9), 5, now).is_none());
        // And clearing one is a no-op rather than a panic, for the same reason.
        state.clear_dropped_datagrams(node(9));
    }

    #[test]
    fn the_cursor_owner_is_this_machine_before_anything_is_laid_out() {
        let now = Instant::now();
        let state = AgentState::new(node(1), "me".into(), now);
        assert_eq!(state.cursor_owner(), node(1));
    }

    #[test]
    fn the_mirrored_cursor_follows_the_layout_the_agent_built() {
        let now = Instant::now();
        let mut state = AgentState::new(node(1), "me".into(), now);
        let layout = autolayout::bootstrap(node(1), &[mon(0, 0, 0, 1920, 1080)]);
        let cursor = wx_core::VirtualCursor::anywhere(&layout).unwrap();
        state.set_cursor(cursor.monitor(), false);
        assert_eq!(state.cursor_owner(), node(1));
        assert!(!state.cursor_locked());
    }
}
