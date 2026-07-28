//! The connection handshake, as a pure state machine.
//!
//! No sockets, no async, no clock: `Handshake` consumes [`ControlMsg`] values and
//! produces the messages to send plus a verdict. Every interesting case here is a
//! security case — a peer claiming someone else's identity, echoing our own
//! nonce, or arriving unpaired — and none of them are reachable in a test that
//! needs two machines and a network. Keeping the logic pure is what makes them
//! testable at all.
//!
//! Sequence, once the QUIC connection and its control stream exist:
//!
//! ```text
//! initiator                          responder
//!   Hello{proto, info, nonce_i}  ->
//!                                <-  Welcome{proto, info, nonce_r}
//!                                <-  AuthProof{sign(nonce_i)}
//!   AuthProof{sign(nonce_r)}     ->
//! ```
//!
//! Each side signs the *other* side's nonce, so neither can produce a proof
//! ahead of time and neither can replay one from an earlier connection.
//!
//! Each signature also covers a [`ChannelBinding`] derived from the TLS session
//! the messages are travelling over. That is not decoration: without it the whole
//! exchange can be relayed verbatim by a machine in the middle, because the
//! certificate on either side is not checked against anything. The binding is
//! never transmitted — both ends compute it from their own session — so a relay
//! cannot make its two sessions agree. See [`crate::identity`].

use wx_proto::PROTOCOL_VERSION;
use wx_proto::{check_version, ControlMsg, NodeId, NodeInfo, RejectReason, VersionCheck};

use crate::identity::{verify_handshake, ChannelBinding, Identity, TrustStore};
use crate::random_bytes;

/// Which end of the connection this is.
///
/// Not a privilege level — either node may drive the other once connected. It
/// exists because several values have to be ordered or tagged consistently on
/// both machines (see [`crate::pairing`]), and "who dialled" is the only ordering
/// both ends already agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Opened the QUIC connection and sent `Hello`.
    Initiator,
    /// Accepted the QUIC connection.
    Responder,
}

impl Role {
    /// Byte used to bind a derived value to one direction of the connection.
    pub const fn tag(self) -> u8 {
        match self {
            Role::Initiator => 0x01,
            Role::Responder => 0x02,
        }
    }

    pub const fn peer(self) -> Self {
        match self {
            Role::Initiator => Role::Responder,
            Role::Responder => Role::Initiator,
        }
    }
}

/// A fresh 32-byte handshake nonce from the OS CSPRNG.
///
/// Freshness is load-bearing: the nonce is what makes the peer's signature
/// unforgeable for this connection only. A repeated nonce turns a recorded
/// handshake into a valid one.
pub fn fresh_nonce() -> Result<[u8; 32], getrandom::Error> {
    random_bytes::<32>()
}

/// A protocol violation by the peer, as opposed to a policy refusal.
///
/// These mean the connection is unusable and should be dropped: there is nothing
/// to negotiate with a peer that sends `AuthProof` before `Hello`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandshakeError {
    #[error("peer sent {got} while the handshake was waiting for {expected}")]
    OutOfOrder {
        expected: &'static str,
        got: &'static str,
    },
    #[error("the handshake has already finished")]
    AlreadyFinished,
}

/// What a handshake step concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// More messages are expected.
    Continue,
    /// Both sides are authenticated; the session may begin.
    Established(Box<Established>),
    /// This side refuses the peer. The [`ControlMsg::Reject`] to send is in
    /// [`Step::send`]; the connection closes after it goes out.
    Refused(RejectReason),
    /// The peer refused this side.
    RefusedByPeer(RejectReason),
}

/// Result of feeding one message to the state machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Messages to write to the control stream, in order.
    pub send: Vec<ControlMsg>,
    pub outcome: Outcome,
}

/// A completed, authenticated handshake.
#[derive(Debug, Clone, PartialEq)]
pub struct Established {
    pub role: Role,
    pub peer: NodeInfo,
    /// Wire format this connection settled on: `min` of the two advertised
    /// [`PROTOCOL_VERSION`]s, and equal to what the peer computed for itself.
    ///
    /// Usually just this build's own version. It differs when the peer is newer,
    /// which is the case this field exists for — it is what a future encoding
    /// change must branch on, and what makes a mixed-version session legible in
    /// the log rather than something to guess at from two machines' build
    /// numbers.
    pub protocol: u16,
    /// The peer's own newest version, as it advertised it.
    ///
    /// Kept alongside the negotiated one purely so a log line can show both.
    /// Nothing should branch on it: `protocol` is the agreed answer, and a peer
    /// ahead of us is precisely the case where its number is not one we can act
    /// on.
    pub peer_protocol: u16,
    /// Nonce this side chose. Needed after the handshake to derive the pairing
    /// proof, which binds the PIN to this specific connection.
    pub local_nonce: [u8; 32],
    pub peer_nonce: [u8; 32],
    /// Whether the peer was already in the trust store.
    ///
    /// False only when the handshake ran in pairing mode: the caller must then
    /// run the pairing exchange and must not treat the session as trusted yet.
    pub peer_was_paired: bool,
}

impl Established {
    /// The initiator's nonce, whichever side is asking.
    ///
    /// Both machines must feed the two nonces to a hash in the same order or
    /// they compute different pairing proofs and pairing silently never succeeds.
    /// Ordering by role is the only ordering both ends agree on.
    pub fn initiator_nonce(&self) -> [u8; 32] {
        match self.role {
            Role::Initiator => self.local_nonce,
            Role::Responder => self.peer_nonce,
        }
    }

    pub fn responder_nonce(&self) -> [u8; 32] {
        match self.role {
            Role::Initiator => self.peer_nonce,
            Role::Responder => self.local_nonce,
        }
    }

    pub fn peer_id(&self) -> NodeId {
        self.peer.id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Responder before any traffic; initiator before [`Handshake::start`].
    AwaitingHello,
    AwaitingWelcome,
    /// Peer info is in hand; only its `AuthProof` is missing.
    AwaitingProof,
    Finished,
}

impl State {
    fn expected(self) -> &'static str {
        match self {
            State::AwaitingHello => "Hello",
            State::AwaitingWelcome => "Welcome",
            State::AwaitingProof => "AuthProof",
            State::Finished => "nothing",
        }
    }
}

/// Drives one connection's handshake.
#[derive(Debug)]
pub struct Handshake {
    role: Role,
    local_info: NodeInfo,
    local_nonce: [u8; 32],
    /// The session this handshake is allowed to authenticate, and no other.
    binding: ChannelBinding,
    state: State,
    peer: Option<NodeInfo>,
    peer_nonce: Option<[u8; 32]>,
    /// Effective wire version and the peer's advertised one, once it has spoken.
    negotiated: Option<(u16, u16)>,
    pairing_mode: bool,
}

impl Handshake {
    /// The binding is a constructor argument rather than an option deliberately:
    /// a handshake that could be built without one would be a handshake that
    /// authenticates a peer without tying it to a connection, which is the defect
    /// this design exists to prevent.
    pub fn new(
        role: Role,
        local_info: NodeInfo,
        local_nonce: [u8; 32],
        binding: ChannelBinding,
    ) -> Self {
        Self {
            role,
            local_info,
            local_nonce,
            binding,
            state: match role {
                Role::Initiator => State::AwaitingWelcome,
                Role::Responder => State::AwaitingHello,
            },
            peer: None,
            peer_nonce: None,
            negotiated: None,
            pairing_mode: false,
        }
    }

    /// Admit a peer that is not in the trust store.
    ///
    /// Only for a connection the user opened a pairing dialog for. It is not a
    /// "trust everyone" switch: the peer still has to prove possession of the
    /// key it advertises, an explicitly blocked peer is still refused, and the
    /// resulting [`Established::peer_was_paired`] is false so the caller cannot
    /// mistake the session for a trusted one.
    pub fn pairing_mode(mut self, enabled: bool) -> Self {
        self.pairing_mode = enabled;
        self
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn local_nonce(&self) -> [u8; 32] {
        self.local_nonce
    }

    /// Messages this side must send before it can expect anything back.
    ///
    /// The responder speaks second, so this is empty for it; calling it anyway is
    /// harmless, which keeps the caller free of role-specific branches.
    pub fn start(&self) -> Vec<ControlMsg> {
        match self.role {
            Role::Initiator => vec![ControlMsg::Hello {
                protocol: PROTOCOL_VERSION,
                info: self.local_info.clone(),
                nonce: self.local_nonce,
            }],
            Role::Responder => Vec::new(),
        }
    }

    /// Feed one received control message.
    pub fn on_message(
        &mut self,
        msg: &ControlMsg,
        identity: &Identity,
        trust: &TrustStore,
    ) -> Result<Step, HandshakeError> {
        // A peer may refuse at any point, including before it has said anything
        // else, so this is checked ahead of the state machine.
        if let ControlMsg::Reject { reason } = msg {
            self.state = State::Finished;
            return Ok(Step {
                send: Vec::new(),
                outcome: Outcome::RefusedByPeer(reason.clone()),
            });
        }

        match (self.state, msg) {
            (
                State::AwaitingHello,
                ControlMsg::Hello {
                    protocol,
                    info,
                    nonce,
                },
            ) => {
                if let Some(step) = self.admit(*protocol, info, nonce, identity, trust) {
                    return Ok(step);
                }
                self.state = State::AwaitingProof;
                Ok(Step {
                    send: vec![
                        ControlMsg::Welcome {
                            protocol: PROTOCOL_VERSION,
                            info: self.local_info.clone(),
                            nonce: self.local_nonce,
                        },
                        // Proving ourselves immediately, rather than waiting for
                        // the peer's proof, costs nothing: the value signed is the
                        // peer's nonce over this session's binding, which is
                        // useless to anyone else and on any other connection.
                        ControlMsg::AuthProof {
                            signature: identity.sign_handshake(&self.binding, nonce),
                        },
                    ],
                    outcome: Outcome::Continue,
                })
            }
            (
                State::AwaitingWelcome,
                ControlMsg::Welcome {
                    protocol,
                    info,
                    nonce,
                },
            ) => {
                if let Some(step) = self.admit(*protocol, info, nonce, identity, trust) {
                    return Ok(step);
                }
                self.state = State::AwaitingProof;
                Ok(Step {
                    send: vec![ControlMsg::AuthProof {
                        signature: identity.sign_handshake(&self.binding, nonce),
                    }],
                    outcome: Outcome::Continue,
                })
            }
            (State::AwaitingProof, ControlMsg::AuthProof { signature }) => {
                let peer = self
                    .peer
                    .clone()
                    .expect("peer info is recorded before AwaitingProof");
                let peer_nonce = self
                    .peer_nonce
                    .expect("peer nonce is recorded before AwaitingProof");
                let (protocol, peer_protocol) = self
                    .negotiated
                    .expect("the version is negotiated before AwaitingProof");
                // Verified against *this* session's binding, so a signature that is
                // genuine but was produced on another connection — the relay case —
                // is refused here rather than establishing a session with a machine
                // in the middle.
                if !verify_handshake(&peer.id, &self.binding, &self.local_nonce, signature) {
                    return Ok(self.refuse(RejectReason::Other(
                        "handshake signature did not verify".into(),
                    )));
                }
                let peer_was_paired = trust.is_paired(&peer.id);
                self.state = State::Finished;
                Ok(Step {
                    send: Vec::new(),
                    outcome: Outcome::Established(Box::new(Established {
                        role: self.role,
                        peer,
                        protocol,
                        peer_protocol,
                        local_nonce: self.local_nonce,
                        peer_nonce,
                        peer_was_paired,
                    })),
                })
            }
            (State::Finished, _) => Err(HandshakeError::AlreadyFinished),
            (state, other) => Err(HandshakeError::OutOfOrder {
                expected: state.expected(),
                got: describe(other),
            }),
        }
    }

    /// Version, identity, and trust checks shared by `Hello` and `Welcome`.
    ///
    /// Returns `Some(step)` when the peer must be refused, `None` when it passes
    /// and its details have been recorded.
    fn admit(
        &mut self,
        protocol: u16,
        info: &NodeInfo,
        nonce: &[u8; 32],
        identity: &Identity,
        trust: &TrustStore,
    ) -> Option<Step> {
        // First, and deliberately so: an incompatible peer must not learn whether
        // it is paired with us, and no signature is produced for one.
        //
        // A peer *newer* than this build is no longer refused — it negotiates down
        // to a format both ends speak, so that the first version bump does not sever
        // interop between an updated machine and a stale one. `check_version` carries
        // the argument for why that is sound. [`RejectReason::ProtocolTooNew`] is
        // consequently never sent from here any more, but stays on the wire: it is a
        // postcard variant index, and a peer on a build that predates this change
        // still sends it to us.
        match check_version(protocol) {
            VersionCheck::Compatible { effective } => self.negotiated = Some((effective, protocol)),
            VersionCheck::PeerTooOld { min_supported, .. } => {
                return Some(self.refuse(RejectReason::ProtocolTooOld { min_supported }))
            }
        }

        // A peer claiming our own key is either a loopback mistake or an attempt
        // to get us to accept our own reflected AuthProof as proof of the peer's
        // identity. There is no legitimate case, so refuse before anything else
        // looks at the key.
        if info.id == identity.node_id() {
            return Some(self.refuse(RejectReason::Other(
                "peer advertises this node's own identity".into(),
            )));
        }

        // Same attack from the other direction: if the peer echoes the nonce we
        // chose, then the signature we are about to produce over "its" nonce is
        // exactly the proof it owes us. Refusing a duplicate nonce closes that
        // reflection outright.
        if nonce == &self.local_nonce {
            return Some(self.refuse(RejectReason::Other(
                "peer echoed this connection's nonce".into(),
            )));
        }

        if trust.is_blocked(&info.id) {
            return Some(self.refuse(RejectReason::Blocked));
        }
        if !trust.is_paired(&info.id) && !self.pairing_mode {
            // This does tell an unauthenticated peer whether it is paired with
            // us. That is unavoidable — the answer is why we are hanging up — and
            // it reveals nothing an attacker could not learn by trying to pair.
            return Some(self.refuse(RejectReason::NotPaired));
        }

        self.peer = Some(info.clone());
        self.peer_nonce = Some(*nonce);
        None
    }

    fn refuse(&mut self, reason: RejectReason) -> Step {
        self.state = State::Finished;
        Step {
            send: vec![ControlMsg::Reject {
                reason: reason.clone(),
            }],
            outcome: Outcome::Refused(reason),
        }
    }

    /// Peer details, available once `Hello` or `Welcome` has been accepted.
    pub fn peer(&self) -> Option<&NodeInfo> {
        self.peer.as_ref()
    }

    /// Wire version agreed with the peer, available at the same point as
    /// [`Handshake::peer`] — so a connection that is refused after the version
    /// step can still say what it had settled on.
    pub fn negotiated_protocol(&self) -> Option<u16> {
        self.negotiated.map(|(effective, _)| effective)
    }
}

/// Message name for error messages. Only the handshake-relevant variants need
/// naming; anything else arriving mid-handshake is equally out of order.
fn describe(msg: &ControlMsg) -> &'static str {
    match msg {
        ControlMsg::Hello { .. } => "Hello",
        ControlMsg::Welcome { .. } => "Welcome",
        ControlMsg::AuthProof { .. } => "AuthProof",
        ControlMsg::Reject { .. } => "Reject",
        _ => "a post-handshake message",
    }
}

/// One end of an in-memory handshake, for [`run_in_memory`].
pub struct Party<'a> {
    pub identity: &'a Identity,
    pub trust: &'a TrustStore,
    pub info: NodeInfo,
    pub nonce: [u8; 32],
    /// What this side believes it is connected over. Two parties with *different*
    /// bindings are exactly the man-in-the-middle case: one TLS session to each
    /// peer, with every message relayed between them unchanged.
    pub binding: ChannelBinding,
    pub pairing_mode: bool,
}

/// Run both ends of a handshake against each other with no transport at all.
///
/// Useful beyond this module's tests — the agent's own tests need to reproduce a
/// handshake outcome without sockets — so it is public rather than test-only.
pub fn run_in_memory(
    initiator: Party<'_>,
    responder: Party<'_>,
) -> (
    Result<Outcome, HandshakeError>,
    Result<Outcome, HandshakeError>,
) {
    let mut a = Handshake::new(
        Role::Initiator,
        initiator.info.clone(),
        initiator.nonce,
        initiator.binding,
    )
    .pairing_mode(initiator.pairing_mode);
    let mut b = Handshake::new(
        Role::Responder,
        responder.info.clone(),
        responder.nonce,
        responder.binding,
    )
    .pairing_mode(responder.pairing_mode);

    let mut to_b = a.start();
    let mut to_a: Vec<ControlMsg> = Vec::new();
    let mut out_a = Outcome::Continue;
    let mut out_b = Outcome::Continue;

    // Bounded so that a state-machine bug fails the test instead of hanging it.
    for _ in 0..8 {
        if let Err(e) = deliver(
            &mut b,
            &responder,
            std::mem::take(&mut to_b),
            &mut to_a,
            &mut out_b,
        ) {
            return (Ok(out_a), Err(e));
        }
        if let Err(e) = deliver(
            &mut a,
            &initiator,
            std::mem::take(&mut to_a),
            &mut to_b,
            &mut out_a,
        ) {
            return (Err(e), Ok(out_b));
        }
        if to_a.is_empty() && to_b.is_empty() {
            break;
        }
    }
    (Ok(out_a), Ok(out_b))
}

fn deliver(
    hs: &mut Handshake,
    party: &Party<'_>,
    inbox: Vec<ControlMsg>,
    outbox: &mut Vec<ControlMsg>,
    outcome: &mut Outcome,
) -> Result<(), HandshakeError> {
    for msg in inbox {
        // A real transport stops reading once it has refused or finished; feeding
        // the rest of a batch in would report a spurious protocol violation.
        if !matches!(outcome, Outcome::Continue) {
            break;
        }
        let step = hs.on_message(&msg, party.identity, party.trust)?;
        outbox.extend(step.send);
        *outcome = step.outcome;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{Capabilities, DisplayServer, Platform, MIN_COMPATIBLE_VERSION};

    fn info(id: NodeId, name: &str) -> NodeInfo {
        NodeInfo {
            id,
            name: name.into(),
            platform: Platform::Windows,
            display_server: DisplayServer::Windows,
            capabilities: Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT,
            monitors: Vec::new(),
            agent_version: "0.1.0".into(),
        }
    }

    struct Pair {
        alice: Identity,
        bob: Identity,
        alice_trust: TrustStore,
        bob_trust: TrustStore,
    }

    /// Two nodes that have already paired with each other.
    fn paired() -> Pair {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let mut alice_trust = TrustStore::new();
        alice_trust.trust(bob.node_id(), "bob");
        let mut bob_trust = TrustStore::new();
        bob_trust.trust(alice.node_id(), "alice");
        Pair {
            alice,
            bob,
            alice_trust,
            bob_trust,
        }
    }

    /// The binding both parties see when there is nothing between them. Tests
    /// about anything other than channel binding use this on both sides.
    fn same_session() -> ChannelBinding {
        ChannelBinding::from_bytes([0x33u8; 32])
    }

    fn party<'a>(
        identity: &'a Identity,
        trust: &'a TrustStore,
        name: &str,
        nonce: [u8; 32],
        pairing_mode: bool,
    ) -> Party<'a> {
        party_on(same_session(), identity, trust, name, nonce, pairing_mode)
    }

    fn party_on<'a>(
        binding: ChannelBinding,
        identity: &'a Identity,
        trust: &'a TrustStore,
        name: &str,
        nonce: [u8; 32],
        pairing_mode: bool,
    ) -> Party<'a> {
        Party {
            info: info(identity.node_id(), name),
            identity,
            trust,
            nonce,
            binding,
            pairing_mode,
        }
    }

    fn established(outcome: &Outcome) -> Established {
        match outcome {
            Outcome::Established(e) => (**e).clone(),
            other => panic!("expected an established handshake, got {other:?}"),
        }
    }

    #[test]
    fn two_paired_nodes_complete_the_handshake() {
        let p = paired();
        let (a, b) = run_in_memory(
            party(&p.alice, &p.alice_trust, "alice", [1u8; 32], false),
            party(&p.bob, &p.bob_trust, "bob", [2u8; 32], false),
        );
        let a = established(&a.unwrap());
        let b = established(&b.unwrap());
        assert_eq!(a.peer_id(), p.bob.node_id());
        assert_eq!(b.peer_id(), p.alice.node_id());
        assert!(a.peer_was_paired);
        assert!(b.peer_was_paired);
    }

    #[test]
    fn a_handshake_relayed_between_two_tls_sessions_is_refused() {
        // The attack the channel binding exists for, and the one a nonce alone
        // cannot see. Eve terminates TLS towards Alice and towards Bob, then
        // forwards every byte unchanged: both peers verify a *genuine* signature
        // by the right key over their own fresh nonce. What differs is only the
        // session each proof was made on, so covering that is the whole defence.
        // Without it both sides reach Established and Eve reads every keystroke.
        let p = paired();
        let (a, b) = run_in_memory(
            party_on(
                ChannelBinding::from_bytes([0xa1u8; 32]),
                &p.alice,
                &p.alice_trust,
                "alice",
                [1u8; 32],
                false,
            ),
            party_on(
                ChannelBinding::from_bytes([0xb2u8; 32]),
                &p.bob,
                &p.bob_trust,
                "bob",
                [2u8; 32],
                false,
            ),
        );
        assert!(
            matches!(b.unwrap(), Outcome::Refused(RejectReason::Other(_))),
            "the responder accepted a relayed proof"
        );
        assert!(
            !matches!(a.unwrap(), Outcome::Established(_)),
            "the initiator established a session through a man in the middle"
        );
    }

    #[test]
    fn a_relay_is_refused_even_while_pairing() {
        // Pairing is the moment trust is created, so a relay that survived here
        // would be trusted permanently by both machines. The handshake must refuse
        // before either side gets as far as exchanging a PIN proof.
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let empty = TrustStore::new();
        let (a, b) = run_in_memory(
            party_on(
                ChannelBinding::from_bytes([0x10u8; 32]),
                &alice,
                &empty,
                "alice",
                [1u8; 32],
                true,
            ),
            party_on(
                ChannelBinding::from_bytes([0x20u8; 32]),
                &bob,
                &empty,
                "bob",
                [2u8; 32],
                true,
            ),
        );
        assert!(!matches!(a.unwrap(), Outcome::Established(_)));
        assert!(!matches!(b.unwrap(), Outcome::Established(_)));
    }

    #[test]
    fn both_sides_agree_on_which_nonce_belongs_to_which_role() {
        // Pairing proofs are derived from both nonces in role order; if the two
        // ends disagree on that order, pairing fails with no useful diagnostic.
        let p = paired();
        let (a, b) = run_in_memory(
            party(&p.alice, &p.alice_trust, "alice", [11u8; 32], false),
            party(&p.bob, &p.bob_trust, "bob", [22u8; 32], false),
        );
        let a = established(&a.unwrap());
        let b = established(&b.unwrap());
        assert_eq!(a.initiator_nonce(), b.initiator_nonce());
        assert_eq!(a.responder_nonce(), b.responder_nonce());
        assert_eq!(a.initiator_nonce(), [11u8; 32]);
        assert_eq!(a.responder_nonce(), [22u8; 32]);
    }

    #[test]
    fn an_unpaired_peer_is_refused_as_not_paired() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let empty = TrustStore::new();
        let (a, b) = run_in_memory(
            party(&alice, &empty, "alice", [1u8; 32], false),
            party(&bob, &empty, "bob", [2u8; 32], false),
        );
        assert_eq!(b.unwrap(), Outcome::Refused(RejectReason::NotPaired));
        assert_eq!(a.unwrap(), Outcome::RefusedByPeer(RejectReason::NotPaired));
    }

    #[test]
    fn the_initiator_also_refuses_an_unpaired_responder() {
        // Asymmetric trust is a real state: the machine that answers may have
        // been re-imaged. The dialling side must check too, or it hands input to
        // a machine it has never paired with.
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let alice_trust = TrustStore::new();
        let mut bob_trust = TrustStore::new();
        bob_trust.trust(alice.node_id(), "alice");

        let (a, _b) = run_in_memory(
            party(&alice, &alice_trust, "alice", [1u8; 32], false),
            party(&bob, &bob_trust, "bob", [2u8; 32], false),
        );
        assert_eq!(a.unwrap(), Outcome::Refused(RejectReason::NotPaired));
    }

    #[test]
    fn a_blocked_peer_is_refused_even_in_pairing_mode() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let alice_trust = TrustStore::new();
        let mut bob_trust = TrustStore::new();
        bob_trust.block(alice.node_id(), "alice");

        let (_a, b) = run_in_memory(
            party(&alice, &alice_trust, "alice", [1u8; 32], true),
            party(&bob, &bob_trust, "bob", [2u8; 32], true),
        );
        assert_eq!(b.unwrap(), Outcome::Refused(RejectReason::Blocked));
    }

    #[test]
    fn pairing_mode_admits_an_unpaired_peer_but_marks_it_untrusted() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let empty = TrustStore::new();
        let (a, b) = run_in_memory(
            party(&alice, &empty, "alice", [1u8; 32], true),
            party(&bob, &empty, "bob", [2u8; 32], true),
        );
        assert!(!established(&a.unwrap()).peer_was_paired);
        assert!(!established(&b.unwrap()).peer_was_paired);
    }

    #[test]
    fn a_peer_claiming_our_own_identity_is_refused() {
        // Reflection: an attacker replays our Hello back at us so that our own
        // AuthProof satisfies the proof it owes.
        let alice = Identity::generate().unwrap();
        let mut trust = TrustStore::new();
        trust.trust(alice.node_id(), "me");
        let mut hs = Handshake::new(
            Role::Responder,
            info(alice.node_id(), "me"),
            [1u8; 32],
            same_session(),
        );
        let step = hs
            .on_message(
                &ControlMsg::Hello {
                    protocol: PROTOCOL_VERSION,
                    info: info(alice.node_id(), "me"),
                    nonce: [2u8; 32],
                },
                &alice,
                &trust,
            )
            .unwrap();
        assert!(matches!(
            step.outcome,
            Outcome::Refused(RejectReason::Other(_))
        ));
    }

    #[test]
    fn a_peer_echoing_our_nonce_is_refused() {
        let p = paired();
        let mut hs = Handshake::new(
            Role::Responder,
            info(p.bob.node_id(), "bob"),
            [0x5au8; 32],
            same_session(),
        );
        let step = hs
            .on_message(
                &ControlMsg::Hello {
                    protocol: PROTOCOL_VERSION,
                    info: info(p.alice.node_id(), "alice"),
                    // Same value the responder is about to ask the peer to sign.
                    nonce: [0x5au8; 32],
                },
                &p.bob,
                &p.bob_trust,
            )
            .unwrap();
        assert!(matches!(
            step.outcome,
            Outcome::Refused(RejectReason::Other(_))
        ));
    }

    #[test]
    fn a_forged_auth_proof_is_refused() {
        let p = paired();
        // Eve knows Alice's public key but not her private key.
        let eve = Identity::generate().unwrap();
        let mut hs = Handshake::new(
            Role::Responder,
            info(p.bob.node_id(), "bob"),
            [9u8; 32],
            same_session(),
        );
        hs.on_message(
            &ControlMsg::Hello {
                protocol: PROTOCOL_VERSION,
                info: info(p.alice.node_id(), "alice"),
                nonce: [8u8; 32],
            },
            &p.bob,
            &p.bob_trust,
        )
        .unwrap();
        let step = hs
            .on_message(
                &ControlMsg::AuthProof {
                    signature: eve.sign_handshake(&same_session(), &[9u8; 32]),
                },
                &p.bob,
                &p.bob_trust,
            )
            .unwrap();
        assert!(matches!(
            step.outcome,
            Outcome::Refused(RejectReason::Other(_))
        ));
    }

    #[test]
    fn a_proof_over_the_wrong_nonce_is_refused() {
        // A proof captured from another connection: valid signature, wrong nonce.
        let p = paired();
        let mut hs = Handshake::new(
            Role::Responder,
            info(p.bob.node_id(), "bob"),
            [9u8; 32],
            same_session(),
        );
        hs.on_message(
            &ControlMsg::Hello {
                protocol: PROTOCOL_VERSION,
                info: info(p.alice.node_id(), "alice"),
                nonce: [8u8; 32],
            },
            &p.bob,
            &p.bob_trust,
        )
        .unwrap();
        let step = hs
            .on_message(
                &ControlMsg::AuthProof {
                    signature: p.alice.sign_handshake(&same_session(), &[123u8; 32]),
                },
                &p.bob,
                &p.bob_trust,
            )
            .unwrap();
        assert!(matches!(
            step.outcome,
            Outcome::Refused(RejectReason::Other(_))
        ));
    }

    /// Drive a whole handshake against a peer that advertises `peer_protocol`.
    ///
    /// The peer is played by hand rather than by [`run_in_memory`], because the
    /// point is a version this build cannot produce for itself.
    fn handshake_with_peer_at(role: Role, peer_protocol: u16) -> (Step, Step) {
        let p = paired();
        let (local, local_id, remote, remote_id) = match role {
            // `paired()` trusts each of alice and bob to the other, so either may
            // play the local side; the roles just decide who speaks first.
            Role::Responder => (&p.bob, p.bob.node_id(), &p.alice, p.alice.node_id()),
            Role::Initiator => (&p.alice, p.alice.node_id(), &p.bob, p.bob.node_id()),
        };
        let trust = match role {
            Role::Responder => &p.bob_trust,
            Role::Initiator => &p.alice_trust,
        };
        let local_nonce = [1u8; 32];
        let peer_nonce = [2u8; 32];

        let mut hs = Handshake::new(role, info(local_id, "local"), local_nonce, same_session());
        let greeting = match role {
            Role::Responder => ControlMsg::Hello {
                protocol: peer_protocol,
                info: info(remote_id, "remote"),
                nonce: peer_nonce,
            },
            Role::Initiator => ControlMsg::Welcome {
                protocol: peer_protocol,
                info: info(remote_id, "remote"),
                nonce: peer_nonce,
            },
        };
        let first = hs.on_message(&greeting, local, trust).unwrap();
        if !matches!(first.outcome, Outcome::Continue) {
            return (first.clone(), first);
        }
        let second = hs
            .on_message(
                &ControlMsg::AuthProof {
                    signature: remote.sign_handshake(&same_session(), &local_nonce),
                },
                local,
                trust,
            )
            .unwrap();
        (first, second)
    }

    #[test]
    fn a_newer_peer_connects_at_the_older_version_instead_of_being_refused() {
        // The regression that matters. Before newer-peer tolerance this refused
        // outright, which meant the first `PROTOCOL_VERSION` bump would have left
        // an updated machine unable to talk to a stale one at all — strictly worse
        // than carrying on at the older feature level.
        //
        // Checked from both roles: the initiator learns the peer's version from
        // `Welcome` and the responder from `Hello`, so they are separate paths
        // through `admit` and only one of them being tolerant is a bug that only
        // shows up in one dialling direction.
        for role in [Role::Responder, Role::Initiator] {
            for ahead in [1, 7, 1000] {
                let (_, last) = handshake_with_peer_at(role, PROTOCOL_VERSION + ahead);
                let Outcome::Established(e) = last.outcome else {
                    panic!("{role:?} refused a peer {ahead} versions ahead: {last:?}");
                };
                // ...and at a version this build actually speaks, not the peer's.
                assert_eq!(e.protocol, PROTOCOL_VERSION);
                // The peer's own number survives for the log line, unclamped.
                assert_eq!(e.peer_protocol, PROTOCOL_VERSION + ahead);
            }
        }
    }

    #[test]
    fn a_matched_peer_negotiates_the_current_version() {
        for role in [Role::Responder, Role::Initiator] {
            let (_, last) = handshake_with_peer_at(role, PROTOCOL_VERSION);
            let Outcome::Established(e) = last.outcome else {
                panic!("{role:?} refused a matched peer: {last:?}");
            };
            assert_eq!(e.protocol, PROTOCOL_VERSION);
        }
    }

    #[test]
    fn a_below_floor_peer_is_still_refused_from_either_role() {
        // Tolerance is one-directional. A peer under `MIN_COMPATIBLE_VERSION`
        // speaks a format whose code is gone, so it is refused with the number it
        // would have to reach — never admitted to fail confusingly later.
        for role in [Role::Responder, Role::Initiator] {
            let (first, _) = handshake_with_peer_at(role, 0);
            assert_eq!(
                first.outcome,
                Outcome::Refused(RejectReason::ProtocolTooOld {
                    min_supported: MIN_COMPATIBLE_VERSION
                }),
                "{role:?} admitted a below-floor peer"
            );
            assert_eq!(
                first.send,
                vec![ControlMsg::Reject {
                    reason: RejectReason::ProtocolTooOld {
                        min_supported: MIN_COMPATIBLE_VERSION
                    }
                }],
                "{role:?} refused without saying so"
            );
        }
    }

    #[test]
    fn a_version_zero_peer_is_refused_as_too_old() {
        let p = paired();
        let mut hs = Handshake::new(
            Role::Responder,
            info(p.bob.node_id(), "bob"),
            [1u8; 32],
            same_session(),
        );
        let step = hs
            .on_message(
                &ControlMsg::Hello {
                    protocol: 0,
                    info: info(p.alice.node_id(), "alice"),
                    nonce: [2u8; 32],
                },
                &p.bob,
                &p.bob_trust,
            )
            .unwrap();
        assert!(matches!(
            step.outcome,
            Outcome::Refused(RejectReason::ProtocolTooOld { .. })
        ));
    }

    #[test]
    fn a_version_check_happens_before_the_trust_lookup() {
        // An incompatible peer must not be told whether it is paired: that is
        // information leaked for no protocol benefit. Newer-peer tolerance moved
        // which versions are incompatible, not where in `admit` the question is
        // asked, so the below-floor case still has to answer with the version.
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let empty = TrustStore::new();
        let mut hs = Handshake::new(
            Role::Responder,
            info(bob.node_id(), "bob"),
            [1u8; 32],
            same_session(),
        );
        let step = hs
            .on_message(
                &ControlMsg::Hello {
                    protocol: MIN_COMPATIBLE_VERSION - 1,
                    info: info(alice.node_id(), "alice"),
                    nonce: [2u8; 32],
                },
                &bob,
                &empty,
            )
            .unwrap();
        assert!(matches!(
            step.outcome,
            Outcome::Refused(RejectReason::ProtocolTooOld { .. })
        ));
    }

    #[test]
    fn a_newer_peer_is_still_screened_by_every_other_admission_check() {
        // Tolerating a newer version must not turn into tolerating anything else:
        // a peer that is unpaired, blocked, or reflecting our own identity is
        // refused exactly as before, and before any signature goes out.
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let empty = TrustStore::new();
        let mut hs = Handshake::new(
            Role::Responder,
            info(bob.node_id(), "bob"),
            [1u8; 32],
            same_session(),
        );
        let step = hs
            .on_message(
                &ControlMsg::Hello {
                    protocol: PROTOCOL_VERSION + 3,
                    info: info(alice.node_id(), "alice"),
                    nonce: [2u8; 32],
                },
                &bob,
                &empty,
            )
            .unwrap();
        assert_eq!(step.outcome, Outcome::Refused(RejectReason::NotPaired));
        assert!(
            !step
                .send
                .iter()
                .any(|m| matches!(m, ControlMsg::AuthProof { .. })),
            "signed for a peer it was refusing"
        );
    }

    #[test]
    fn a_proof_before_a_hello_is_a_protocol_violation() {
        let p = paired();
        let mut hs = Handshake::new(
            Role::Responder,
            info(p.bob.node_id(), "bob"),
            [1u8; 32],
            same_session(),
        );
        let err = hs
            .on_message(
                &ControlMsg::AuthProof {
                    signature: p.alice.sign_handshake(&same_session(), &[1u8; 32]),
                },
                &p.bob,
                &p.bob_trust,
            )
            .unwrap_err();
        assert_eq!(
            err,
            HandshakeError::OutOfOrder {
                expected: "Hello",
                got: "AuthProof"
            }
        );
    }

    #[test]
    fn a_second_hello_is_a_protocol_violation() {
        let p = paired();
        let mut hs = Handshake::new(
            Role::Responder,
            info(p.bob.node_id(), "bob"),
            [1u8; 32],
            same_session(),
        );
        let hello = ControlMsg::Hello {
            protocol: PROTOCOL_VERSION,
            info: info(p.alice.node_id(), "alice"),
            nonce: [2u8; 32],
        };
        hs.on_message(&hello, &p.bob, &p.bob_trust).unwrap();
        assert!(hs.on_message(&hello, &p.bob, &p.bob_trust).is_err());
    }

    #[test]
    fn post_handshake_traffic_during_the_handshake_is_a_violation() {
        let p = paired();
        let mut hs = Handshake::new(
            Role::Responder,
            info(p.bob.node_id(), "bob"),
            [1u8; 32],
            same_session(),
        );
        assert!(hs
            .on_message(&ControlMsg::LayoutRequest, &p.bob, &p.bob_trust)
            .is_err());
    }

    #[test]
    fn nothing_is_accepted_after_the_handshake_finishes() {
        let p = paired();
        let mut hs = Handshake::new(
            Role::Responder,
            info(p.bob.node_id(), "bob"),
            [1u8; 32],
            same_session(),
        );
        hs.on_message(
            &ControlMsg::Hello {
                protocol: PROTOCOL_VERSION,
                info: info(p.alice.node_id(), "alice"),
                nonce: [2u8; 32],
            },
            &p.bob,
            &p.bob_trust,
        )
        .unwrap();
        hs.on_message(
            &ControlMsg::AuthProof {
                signature: p.alice.sign_handshake(&same_session(), &[1u8; 32]),
            },
            &p.bob,
            &p.bob_trust,
        )
        .unwrap();
        assert_eq!(
            hs.on_message(&ControlMsg::LayoutRequest, &p.bob, &p.bob_trust)
                .unwrap_err(),
            HandshakeError::AlreadyFinished
        );
    }

    #[test]
    fn a_reject_at_any_point_ends_the_handshake() {
        let p = paired();
        let mut hs = Handshake::new(
            Role::Initiator,
            info(p.alice.node_id(), "alice"),
            [1u8; 32],
            same_session(),
        );
        let step = hs
            .on_message(
                &ControlMsg::Reject {
                    reason: RejectReason::Busy,
                },
                &p.alice,
                &p.alice_trust,
            )
            .unwrap();
        assert_eq!(step.outcome, Outcome::RefusedByPeer(RejectReason::Busy));
        assert!(step.send.is_empty(), "must not answer a Reject");
    }

    #[test]
    fn a_refusal_carries_the_reject_to_send() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let empty = TrustStore::new();
        let mut hs = Handshake::new(
            Role::Responder,
            info(bob.node_id(), "bob"),
            [1u8; 32],
            same_session(),
        );
        let step = hs
            .on_message(
                &ControlMsg::Hello {
                    protocol: PROTOCOL_VERSION,
                    info: info(alice.node_id(), "alice"),
                    nonce: [2u8; 32],
                },
                &bob,
                &empty,
            )
            .unwrap();
        assert_eq!(
            step.send,
            vec![ControlMsg::Reject {
                reason: RejectReason::NotPaired
            }]
        );
    }

    #[test]
    fn the_initiator_speaks_first_and_the_responder_waits() {
        let id = Identity::generate().unwrap();
        let a = Handshake::new(
            Role::Initiator,
            info(id.node_id(), "a"),
            [1u8; 32],
            same_session(),
        );
        let b = Handshake::new(
            Role::Responder,
            info(id.node_id(), "b"),
            [1u8; 32],
            same_session(),
        );
        assert!(matches!(a.start().as_slice(), [ControlMsg::Hello { .. }]));
        assert!(b.start().is_empty());
    }

    #[test]
    fn fresh_nonces_differ() {
        assert_ne!(fresh_nonce().unwrap(), fresh_nonce().unwrap());
    }

    #[test]
    fn role_tags_and_peers_are_distinct() {
        assert_ne!(Role::Initiator.tag(), Role::Responder.tag());
        assert_eq!(Role::Initiator.peer(), Role::Responder);
        assert_eq!(Role::Responder.peer(), Role::Initiator);
    }
}
