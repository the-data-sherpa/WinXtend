//! Peer discovery over mDNS / DNS-SD.
//!
//! The point is that a user never types an IP address. Machines announce
//! themselves on the local link and the UI lists what it finds, split into
//! already-paired peers (connect straight away) and strangers (offer to pair).
//!
//! Discovery is a *hint*, never an authority. Every field below arrives in an
//! unauthenticated multicast packet that anyone on the LAN can forge, including
//! the advertised node id. Nothing here grants trust: the id is only used to
//! decide which peers to show and which address to dial, and the real check is
//! the signature in [`crate::handshake`]. A forged announcement therefore costs
//! an attacker one refused connection.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use wx_proto::{NodeId, DISCOVERY_SERVICE_TYPE};

use crate::identity::TrustStore;

/// TXT key holding the advertising node's id, hex encoded.
pub const TXT_NODE_ID: &str = "nid";
/// TXT key holding the agent build version, for diagnosing mixed-version meshes.
pub const TXT_AGENT_VERSION: &str = "ver";
/// TXT key holding the node's own display name.
pub const TXT_NAME: &str = "name";

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("announcement has no {TXT_NODE_ID} TXT record")]
    MissingNodeId,
    #[error("announcement carries a malformed node id: {0}")]
    MalformedNodeId(String),
    #[error("mDNS failure: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

/// A DNS-SD announcement, reduced to what this crate cares about.
///
/// A plain data struct rather than the mDNS library's own type so that the
/// interesting logic — self-filtering, malformed ids, re-announcements — is
/// testable without a network interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRecord {
    /// Unique instance name, e.g. `wx-1a2b3c4d._winxtend._udp.local.`. This is
    /// the only handle a removal event carries, so peers are indexed by it.
    pub fullname: String,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
    pub txt: Vec<(String, String)>,
}

impl ServiceRecord {
    fn txt_value(&self, key: &str) -> Option<&str> {
        // Case-insensitive: DNS-SD TXT keys are, and a peer built against a
        // different library may well capitalise differently.
        self.txt
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// A peer seen on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    pub node: NodeId,
    /// Name the peer advertises for itself. Only a default: a locally chosen name
    /// in the trust store wins, or the user's rename would appear not to stick.
    pub advertised_name: String,
    pub agent_version: String,
    /// Every address the peer announced, in announcement order. More than one is
    /// normal (Wi-Fi plus Ethernet, IPv4 plus IPv6) and a caller should try them
    /// in turn rather than assuming the first one routes.
    pub addresses: Vec<SocketAddr>,
    pub fullname: String,
}

impl DiscoveredPeer {
    /// How the UI should present this peer.
    pub fn trust(&self, store: &TrustStore) -> PeerTrust {
        if store.is_blocked(&self.node) {
            PeerTrust::Blocked
        } else if store.is_paired(&self.node) {
            PeerTrust::Paired {
                name: store
                    .name_of(&self.node)
                    .unwrap_or(&self.advertised_name)
                    .to_string(),
            }
        } else {
            PeerTrust::Unpaired
        }
    }

    /// Name to show: the locally assigned one if there is one.
    pub fn display_name(&self, store: &TrustStore) -> String {
        store
            .name_of(&self.node)
            .unwrap_or(&self.advertised_name)
            .to_string()
    }
}

/// Whether a discovered peer can be connected to, needs pairing, or is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTrust {
    Paired {
        name: String,
    },
    /// Seen but never paired: the UI offers to start pairing.
    Unpaired,
    /// Explicitly refused by the user; hidden or greyed out rather than offered.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    /// A peer appeared, or an existing peer's details changed.
    Found(DiscoveredPeer),
    Lost(NodeId),
}

/// Tracks which peers are currently visible.
///
/// Separate from the mDNS plumbing so that the awkward cases have tests: our own
/// announcement coming back to us, a peer re-announcing unchanged every minute,
/// two instance names claiming one node id, and removals for instances we never
/// resolved.
#[derive(Debug)]
pub struct DiscoveryState {
    self_id: NodeId,
    by_fullname: HashMap<String, NodeId>,
    peers: HashMap<NodeId, DiscoveredPeer>,
}

impl DiscoveryState {
    pub fn new(self_id: NodeId) -> Self {
        Self {
            self_id,
            by_fullname: HashMap::new(),
            peers: HashMap::new(),
        }
    }

    /// Fold in a resolved announcement.
    ///
    /// `Ok(None)` means "nothing for the UI": either the announcement was our
    /// own, or it repeats what we already knew. mDNS re-announces on a timer, so
    /// suppressing unchanged repeats is what keeps a peer list from flickering.
    pub fn on_resolved(
        &mut self,
        record: &ServiceRecord,
    ) -> Result<Option<DiscoveryEvent>, DiscoveryError> {
        let hex = record
            .txt_value(TXT_NODE_ID)
            .ok_or(DiscoveryError::MissingNodeId)?;
        let node =
            NodeId::from_hex(hex).map_err(|_| DiscoveryError::MalformedNodeId(hex.to_string()))?;

        // Our own announcement always comes back to us on the same socket.
        // Without this the agent dials itself, and the handshake refuses the
        // connection as "peer advertises this node's own identity" forever.
        if node == self.self_id {
            return Ok(None);
        }

        let peer = DiscoveredPeer {
            node,
            advertised_name: record
                .txt_value(TXT_NAME)
                .unwrap_or(instance_label(&record.fullname))
                .to_string(),
            agent_version: record
                .txt_value(TXT_AGENT_VERSION)
                .unwrap_or("unknown")
                .to_string(),
            addresses: record
                .addresses
                .iter()
                .map(|ip| SocketAddr::new(*ip, record.port))
                .collect(),
            fullname: record.fullname.clone(),
        };

        self.by_fullname.insert(record.fullname.clone(), node);
        if self.peers.get(&node) == Some(&peer) {
            return Ok(None);
        }
        self.peers.insert(node, peer.clone());
        Ok(Some(DiscoveryEvent::Found(peer)))
    }

    /// Fold in a removal. `None` when the instance was never resolved, which is
    /// routine: browsing sees goodbye packets for instances it never got a TXT
    /// or address record for.
    pub fn on_removed(&mut self, fullname: &str) -> Option<DiscoveryEvent> {
        let node = self.by_fullname.remove(fullname)?;
        // A node re-announcing under a new instance name would otherwise be
        // dropped from the list by the old name's removal arriving afterwards.
        match self.peers.get(&node) {
            Some(peer) if peer.fullname == fullname => {
                self.peers.remove(&node);
                Some(DiscoveryEvent::Lost(node))
            }
            _ => None,
        }
    }

    pub fn peers(&self) -> impl Iterator<Item = &DiscoveredPeer> {
        self.peers.values()
    }

    pub fn peer(&self, node: &NodeId) -> Option<&DiscoveredPeer> {
        self.peers.get(node)
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

/// The instance label from a full DNS-SD name, used as a fallback display name.
fn instance_label(fullname: &str) -> &str {
    fullname.split('.').next().unwrap_or(fullname)
}

/// DNS-SD instance name for a node.
///
/// Only the short id, because a DNS label caps at 63 bytes and a full 64-char hex
/// node id does not fit. The label is cosmetic: identity comes from the TXT
/// record, and ultimately from the handshake signature, so a collision between
/// two nodes sharing four leading bytes costs nothing but a confusing log line.
pub fn instance_name(node: &NodeId) -> String {
    format!("wx-{}", node.short())
}

/// Publishes this node on the local network for as long as it is alive.
pub struct Advertiser {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    /// Announce `node` on `port`.
    pub fn start(
        node: &NodeId,
        display_name: &str,
        agent_version: &str,
        port: u16,
    ) -> Result<Self, DiscoveryError> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let instance = instance_name(node);
        let mut txt = HashMap::new();
        txt.insert(TXT_NODE_ID.to_string(), node.to_hex());
        txt.insert(TXT_AGENT_VERSION.to_string(), agent_version.to_string());
        txt.insert(TXT_NAME.to_string(), display_name.to_string());

        let info = mdns_sd::ServiceInfo::new(
            DISCOVERY_SERVICE_TYPE,
            &instance,
            &format!("{instance}.local."),
            (),
            port,
            txt,
        )?
        // Addresses are filled in by the daemon and kept current as interfaces
        // come and go: a laptop moving from Ethernet to Wi-Fi must not keep
        // advertising an address that no longer answers.
        .enable_addr_auto();

        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }

    pub fn fullname(&self) -> &str {
        &self.fullname
    }

    /// Withdraw the announcement.
    ///
    /// Worth calling on a clean shutdown: without the goodbye packet, peers keep
    /// the entry until its TTL expires and offer to connect to a machine that has
    /// gone away.
    pub fn shutdown(self) -> Result<(), DiscoveryError> {
        self.daemon.unregister(&self.fullname)?;
        self.daemon.shutdown()?;
        Ok(())
    }
}

/// Browses for peers, turning mDNS traffic into [`DiscoveryEvent`]s.
pub struct Browser {
    daemon: mdns_sd::ServiceDaemon,
    events: mdns_sd::Receiver<mdns_sd::ServiceEvent>,
    state: DiscoveryState,
}

impl Browser {
    pub fn start(self_id: NodeId) -> Result<Self, DiscoveryError> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let events = daemon.browse(DISCOVERY_SERVICE_TYPE)?;
        Ok(Self {
            daemon,
            events,
            state: DiscoveryState::new(self_id),
        })
    }

    /// Wait for the next event worth showing a user.
    ///
    /// Returns `None` when the daemon has stopped. Malformed announcements are
    /// logged and skipped rather than surfaced: a device on the LAN publishing
    /// junk under our service type must not be able to stall discovery.
    pub async fn next_event(&mut self) -> Option<DiscoveryEvent> {
        loop {
            let event = self.events.recv_async().await.ok()?;
            let folded = match event {
                mdns_sd::ServiceEvent::ServiceResolved(resolved) => {
                    let record = ServiceRecord {
                        fullname: resolved.fullname.clone(),
                        port: resolved.port,
                        addresses: resolved
                            .addresses
                            .iter()
                            .map(|scoped| scoped.to_ip_addr())
                            .collect(),
                        txt: resolved
                            .txt_properties
                            .iter()
                            .map(|p| (p.key().to_string(), p.val_str().to_string()))
                            .collect(),
                    };
                    match self.state.on_resolved(&record) {
                        Ok(event) => event,
                        Err(e) => {
                            tracing::debug!(
                                fullname = %record.fullname,
                                error = %e,
                                "ignoring malformed WinXtend announcement"
                            );
                            None
                        }
                    }
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                    self.state.on_removed(&fullname)
                }
                // SearchStarted/SearchStopped/ServiceFound carry no peer detail
                // worth surfacing; the resolved event is the useful one.
                _ => None,
            };
            if let Some(event) = folded {
                return Some(event);
            }
        }
    }

    /// Everything currently visible, for populating a freshly opened UI.
    pub fn peers(&self) -> impl Iterator<Item = &DiscoveredPeer> {
        self.state.peers()
    }

    pub fn shutdown(self) -> Result<(), DiscoveryError> {
        self.daemon.shutdown()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fullname: &str, node: &NodeId, name: &str) -> ServiceRecord {
        ServiceRecord {
            fullname: fullname.to_string(),
            port: 24800,
            addresses: vec![IpAddr::from([192, 168, 1, 20])],
            txt: vec![
                (TXT_NODE_ID.to_string(), node.to_hex()),
                (TXT_AGENT_VERSION.to_string(), "0.1.0".to_string()),
                (TXT_NAME.to_string(), name.to_string()),
            ],
        }
    }

    fn found(event: Option<DiscoveryEvent>) -> DiscoveredPeer {
        match event {
            Some(DiscoveryEvent::Found(peer)) => peer,
            other => panic!("expected a Found event, got {other:?}"),
        }
    }

    #[test]
    fn a_new_peer_is_reported_with_its_address_and_version() {
        let me = NodeId([1u8; 32]);
        let them = NodeId([2u8; 32]);
        let mut state = DiscoveryState::new(me);
        let peer = found(
            state
                .on_resolved(&record("wx-02020202._winxtend._udp.local.", &them, "pi"))
                .unwrap(),
        );
        assert_eq!(peer.node, them);
        assert_eq!(peer.advertised_name, "pi");
        assert_eq!(peer.agent_version, "0.1.0");
        assert_eq!(
            peer.addresses,
            vec![SocketAddr::from(([192, 168, 1, 20], 24800))]
        );
    }

    #[test]
    fn our_own_announcement_is_ignored() {
        // Multicast comes back on the same socket. Reporting ourselves would have
        // the agent endlessly dial its own port.
        let me = NodeId([1u8; 32]);
        let mut state = DiscoveryState::new(me);
        assert_eq!(
            state
                .on_resolved(&record("wx-01010101._winxtend._udp.local.", &me, "self"))
                .unwrap(),
            None
        );
        assert!(state.is_empty());
    }

    #[test]
    fn an_unchanged_re_announcement_produces_no_event() {
        // mDNS re-announces on a timer; a Found per announcement would make the
        // UI list flicker every minute.
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let rec = record(
            "wx-02020202._winxtend._udp.local.",
            &NodeId([2u8; 32]),
            "pi",
        );
        assert!(state.on_resolved(&rec).unwrap().is_some());
        assert!(state.on_resolved(&rec).unwrap().is_none());
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn a_changed_announcement_is_reported_again() {
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let them = NodeId([2u8; 32]);
        let mut rec = record("wx-02020202._winxtend._udp.local.", &them, "pi");
        state.on_resolved(&rec).unwrap();
        rec.addresses = vec![IpAddr::from([10, 0, 0, 5])];
        let peer = found(state.on_resolved(&rec).unwrap());
        assert_eq!(
            peer.addresses,
            vec![SocketAddr::from(([10, 0, 0, 5], 24800))]
        );
        assert_eq!(state.len(), 1, "the peer must not be duplicated");
    }

    #[test]
    fn an_announcement_without_a_node_id_is_an_error() {
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let mut rec = record("x._winxtend._udp.local.", &NodeId([2u8; 32]), "pi");
        rec.txt.retain(|(k, _)| k != TXT_NODE_ID);
        assert!(matches!(
            state.on_resolved(&rec),
            Err(DiscoveryError::MissingNodeId)
        ));
    }

    #[test]
    fn a_malformed_node_id_is_an_error_not_a_panic() {
        // TXT records come from an unauthenticated multicast packet.
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        for junk in ["", "zzzz", "ab", &"ff".repeat(64)] {
            let mut rec = record("x._winxtend._udp.local.", &NodeId([2u8; 32]), "pi");
            rec.txt = vec![(TXT_NODE_ID.to_string(), junk.to_string())];
            assert!(matches!(
                state.on_resolved(&rec),
                Err(DiscoveryError::MalformedNodeId(_))
            ));
        }
    }

    #[test]
    fn missing_optional_txt_records_fall_back_instead_of_failing() {
        // A peer built from an older or leaner implementation still needs to be
        // connectable; only the node id is mandatory.
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let them = NodeId([2u8; 32]);
        let rec = ServiceRecord {
            fullname: "wx-02020202._winxtend._udp.local.".into(),
            port: 24800,
            addresses: vec![IpAddr::from([127, 0, 0, 1])],
            txt: vec![(TXT_NODE_ID.to_string(), them.to_hex())],
        };
        let peer = found(state.on_resolved(&rec).unwrap());
        assert_eq!(peer.advertised_name, "wx-02020202");
        assert_eq!(peer.agent_version, "unknown");
    }

    #[test]
    fn unknown_txt_keys_are_tolerated() {
        // Forward compatibility: a newer peer will advertise more than we read.
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let mut rec = record("x._winxtend._udp.local.", &NodeId([2u8; 32]), "pi");
        rec.txt.push(("future".into(), "value".into()));
        assert!(state.on_resolved(&rec).unwrap().is_some());
    }

    #[test]
    fn txt_keys_are_matched_case_insensitively() {
        // DNS-SD says TXT keys are case insensitive, and other implementations
        // capitalise differently.
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let them = NodeId([2u8; 32]);
        let rec = ServiceRecord {
            fullname: "wx-02020202._winxtend._udp.local.".into(),
            port: 1,
            addresses: Vec::new(),
            txt: vec![
                ("NID".to_string(), them.to_hex()),
                ("Name".to_string(), "shouty".to_string()),
            ],
        };
        assert_eq!(
            found(state.on_resolved(&rec).unwrap()).advertised_name,
            "shouty"
        );
    }

    #[test]
    fn a_peer_with_no_addresses_is_still_listed() {
        // Address records can arrive after the TXT record. Dropping the peer
        // would mean it never appears at all on some networks.
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let mut rec = record("x._winxtend._udp.local.", &NodeId([2u8; 32]), "pi");
        rec.addresses.clear();
        let peer = found(state.on_resolved(&rec).unwrap());
        assert!(peer.addresses.is_empty());
    }

    #[test]
    fn removal_reports_the_peer_as_lost() {
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let them = NodeId([2u8; 32]);
        let full = "wx-02020202._winxtend._udp.local.";
        state.on_resolved(&record(full, &them, "pi")).unwrap();
        assert_eq!(state.on_removed(full), Some(DiscoveryEvent::Lost(them)));
        assert!(state.is_empty());
    }

    #[test]
    fn removal_of_an_unknown_instance_is_silent() {
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        assert_eq!(state.on_removed("never-seen._winxtend._udp.local."), None);
    }

    #[test]
    fn removal_is_not_reported_twice() {
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let full = "wx-02020202._winxtend._udp.local.";
        state
            .on_resolved(&record(full, &NodeId([2u8; 32]), "pi"))
            .unwrap();
        assert!(state.on_removed(full).is_some());
        assert_eq!(state.on_removed(full), None);
    }

    #[test]
    fn a_stale_instance_removal_does_not_drop_a_re_announced_peer() {
        // A restarting node can reappear under a new instance name before the old
        // name's goodbye arrives. Keying removal on the node id alone would then
        // delete a peer that is present.
        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let them = NodeId([2u8; 32]);
        let old = "wx-old._winxtend._udp.local.";
        let new = "wx-new._winxtend._udp.local.";
        state.on_resolved(&record(old, &them, "pi")).unwrap();
        state.on_resolved(&record(new, &them, "pi")).unwrap();
        assert_eq!(state.on_removed(old), None);
        assert!(state.peer(&them).is_some());
        assert_eq!(state.on_removed(new), Some(DiscoveryEvent::Lost(them)));
    }

    #[test]
    fn paired_blocked_and_unknown_peers_are_distinguishable() {
        let mut store = TrustStore::new();
        let paired = NodeId([2u8; 32]);
        let blocked = NodeId([3u8; 32]);
        store.trust(paired, "owen-mac");
        store.block(blocked, "someone else");

        let mut state = DiscoveryState::new(NodeId([1u8; 32]));
        let a = found(
            state
                .on_resolved(&record("a._winxtend._udp.local.", &paired, "MacBook-Pro"))
                .unwrap(),
        );
        let b = found(
            state
                .on_resolved(&record("b._winxtend._udp.local.", &blocked, "intruder"))
                .unwrap(),
        );
        let c = found(
            state
                .on_resolved(&record(
                    "c._winxtend._udp.local.",
                    &NodeId([4u8; 32]),
                    "new pi",
                ))
                .unwrap(),
        );

        // The locally chosen name wins, or the user's rename looks broken.
        assert_eq!(
            a.trust(&store),
            PeerTrust::Paired {
                name: "owen-mac".into()
            }
        );
        assert_eq!(a.display_name(&store), "owen-mac");
        assert_eq!(b.trust(&store), PeerTrust::Blocked);
        assert_eq!(c.trust(&store), PeerTrust::Unpaired);
        assert_eq!(c.display_name(&store), "new pi");
    }

    #[test]
    fn instance_names_fit_a_dns_label() {
        // Over 63 bytes and the announcement is silently invalid.
        let name = instance_name(&NodeId([0xab; 32]));
        assert!(name.len() <= 63, "instance name is {} bytes", name.len());
        assert_eq!(name, "wx-abababab");
    }
}
