//! Two real QUIC endpoints on loopback, driven end to end.
//!
//! The unit tests cover the handshake as a pure state machine; these exist to
//! catch what only shows up once quinn, rustls, and the framing are all involved:
//! an ALPN mismatch, a stream tag written but never read, a datagram that
//! silently never arrives because datagrams were not enabled, and a reliable
//! frame that deadlocks behind a lock.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use wx_net::identity::{Identity, TrustStore};
use wx_net::pairing::{PairingSession, Pin};
use wx_net::transport::{Endpoint, Events, Session, SessionEvent, SessionSetup, TransportError};
use wx_proto::{
    Capabilities, ControlMsg, DisplayServer, InputEvent, InputFrame, KeyAction, KeyEvent,
    Modifiers, MonitorId, MouseButton, NodeId, NodeInfo, NormPos, Platform, PointerEvent,
    RejectReason, Reliability, ScrollUnit,
};

type Attempt = Result<(Session, Events), TransportError>;

fn node_info(id: NodeId, name: &str) -> NodeInfo {
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

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// Bring up two endpoints and connect them with whatever trust each side has.
///
/// The endpoints come back in the first element and the caller must keep them
/// bound: dropping a `quinn::Endpoint` closes its socket, which would kill the
/// session halfway through a test and look like a protocol bug.
async fn connect_pair(
    client_identity: &Identity,
    client_trust: &TrustStore,
    server_identity: &Identity,
    server_trust: &TrustStore,
    pairing_mode: bool,
) -> ((Endpoint, Endpoint), Attempt, Attempt) {
    let server = Endpoint::bind(loopback()).unwrap();
    let client = Endpoint::bind(loopback()).unwrap();
    let server_addr = server.local_addr().unwrap();

    let server_setup = SessionSetup {
        identity: server_identity,
        trust: server_trust,
        local_info: node_info(server_identity.node_id(), "server"),
        pairing_mode,
    };
    let client_setup = SessionSetup {
        identity: client_identity,
        trust: client_trust,
        local_info: node_info(client_identity.node_id(), "client"),
        pairing_mode,
    };

    // Both sides have to be driven at once: each blocks waiting on the other.
    let (accepted, connected) = tokio::join!(
        async { server.accept(&server_setup).await.expect("endpoint closed") },
        client.connect(server_addr, &client_setup),
    );
    ((server, client), connected, accepted)
}

/// Two identities that have already paired with each other.
fn paired() -> (Identity, TrustStore, Identity, TrustStore) {
    let client = Identity::generate().unwrap();
    let server = Identity::generate().unwrap();
    let mut client_trust = TrustStore::new();
    client_trust.trust(server.node_id(), "server");
    let mut server_trust = TrustStore::new();
    server_trust.trust(client.node_id(), "client");
    (client, client_trust, server, server_trust)
}

async fn next_event(events: &mut Events) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(10), events.next())
        .await
        .expect("timed out waiting for an event")
        .expect("session ended early")
        .expect("session error")
}

fn expect_input(event: SessionEvent) -> InputFrame {
    match event {
        SessionEvent::Input(frame) => frame,
        other => panic!("expected an input frame, got {other:?}"),
    }
}

#[tokio::test]
async fn paired_peers_complete_a_handshake_over_loopback() {
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (server, _se) = server.expect("server handshake");

    assert_eq!(client.peer().id, server_id.node_id());
    assert_eq!(server.peer().id, client_id.node_id());
    assert_eq!(client.peer().name, "server");
    assert_eq!(server.peer().name, "client");
    assert!(client.established().peer_was_paired);
    assert!(server.established().peer_was_paired);
    // Each side signed the other's nonce, so the two views must not agree on
    // which nonce was local.
    assert_eq!(
        client.established().local_nonce,
        server.established().peer_nonce
    );
}

#[tokio::test]
async fn a_motion_datagram_and_a_control_message_both_round_trip() {
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, mut client_events) = client.expect("client handshake");
    let (server, mut server_events) = server.expect("server handshake");

    // Best-effort, so this must travel as a datagram.
    let motion = InputEvent::Pointer(PointerEvent::MoveTo {
        pos: NormPos::new(0.25, 0.75),
    });
    assert_eq!(motion.reliability(), Reliability::BestEffort);
    let frame = InputFrame::new(1, MonitorId(3), motion);
    client.send_input(&frame).await.unwrap();
    assert_eq!(expect_input(next_event(&mut server_events).await), frame);

    // Control goes the other way, on the reliable stream.
    let msg = ControlMsg::Ping { nonce: 0xfeed };
    server.send_control(&msg).await.unwrap();
    assert_eq!(
        next_event(&mut client_events).await,
        SessionEvent::Control(msg)
    );
}

#[tokio::test]
async fn a_key_event_arrives_on_the_reliable_plane() {
    // Keys are Reliable, so they take the input stream. A dropped release strands
    // a modifier down, which the user cannot recover from.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    // Non-ASCII on purpose: keys cross the wire as sender-resolved text.
    let event = InputEvent::Key(KeyEvent::text("å", KeyAction::Press, Modifiers::NONE));
    assert_eq!(event.reliability(), Reliability::Reliable);
    let frame = InputFrame::new(7, MonitorId(0), event);
    client.send_input(&frame).await.unwrap();
    assert_eq!(expect_input(next_event(&mut server_events).await), frame);
}

#[tokio::test]
async fn a_revoked_portal_session_reaches_the_peer_over_a_real_control_stream() {
    // `CapabilitiesChanged` is the one variant appended to `ControlMsg` after the
    // format was already in use, and the wire format is positional. Getting that
    // wrong does not fail politely: a control message that will not decode tears
    // the stream down, so the Wayland machine that just lost its portal session
    // would lose its peer as well. The in-memory round trip pins the tag; this
    // pins that quinn, the framing and the codec agree on it too.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    // A Wayland node whose consent was withdrawn: it keeps its screens, and loses
    // everything that needed the portal.
    let revoked = ControlMsg::CapabilitiesChanged {
        capabilities: Capabilities::HAS_DISPLAYS | Capabilities::CAPABILITY_UPDATES,
    };
    client.send_control(&revoked).await.unwrap();
    assert_eq!(
        next_event(&mut server_events).await,
        SessionEvent::Control(revoked)
    );

    // And the grant arriving late, which is the other direction the same machine
    // moves in once the user answers the dialog.
    let granted = ControlMsg::CapabilitiesChanged {
        capabilities: Capabilities::HAS_DISPLAYS
            | Capabilities::CAPABILITY_UPDATES
            | Capabilities::CAPTURE_INPUT
            | Capabilities::INJECT_INPUT,
    };
    client.send_control(&granted).await.unwrap();
    assert_eq!(
        next_event(&mut server_events).await,
        SessionEvent::Control(granted)
    );
}

#[tokio::test]
async fn both_planes_are_usable_on_the_same_connection() {
    // Regression guard for the stream layout: if reliable input shared the
    // control stream, or the input stream tag were mismatched, one of these two
    // would never arrive.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    let reliable = InputFrame::new(
        2,
        MonitorId(1),
        InputEvent::Pointer(PointerEvent::Button {
            button: MouseButton::Left,
            pressed: false,
        }),
    );
    client.send_input(&reliable).await.unwrap();
    client
        .send_control(&ControlMsg::LayoutRequest)
        .await
        .unwrap();

    let mut saw_input = false;
    let mut saw_control = false;
    while !(saw_input && saw_control) {
        match next_event(&mut server_events).await {
            SessionEvent::Input(frame) => {
                assert_eq!(frame, reliable);
                saw_input = true;
            }
            SessionEvent::Control(msg) => {
                assert_eq!(msg, ControlMsg::LayoutRequest);
                saw_control = true;
            }
        }
    }
}

#[tokio::test]
async fn a_burst_of_reliable_frames_keeps_its_order() {
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    // Reliable frames only: the lossy plane makes no ordering promise, so
    // asserting on it would be asserting on luck.
    for n in 0..64u64 {
        client
            .send_input(&InputFrame::new(
                n,
                MonitorId(0),
                InputEvent::Pointer(PointerEvent::Scroll {
                    dx: 0.0,
                    dy: n as f64,
                    unit: ScrollUnit::Lines,
                }),
            ))
            .await
            .unwrap();
    }
    for n in 0..64u64 {
        assert_eq!(expect_input(next_event(&mut server_events).await).seq, n);
    }
}

#[tokio::test]
async fn sequence_numbers_are_assigned_monotonically_by_the_session() {
    // The receiver's SequenceGate discards stale motion by sequence number, so
    // non-monotonic numbering would silently drop live input.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    for _ in 0..8 {
        client
            .send_event(
                MonitorId(0),
                InputEvent::Pointer(PointerEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                }),
            )
            .await
            .unwrap();
    }
    let mut seqs = Vec::new();
    for _ in 0..8 {
        seqs.push(expect_input(next_event(&mut server_events).await).seq);
    }
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "sequence numbers not increasing: {seqs:?}"
    );
}

#[tokio::test]
async fn an_oversized_reliable_payload_still_arrives() {
    // MoveBy is Reliable, so it never rides a datagram; this also exercises the
    // path a payload takes when it would not fit the datagram limit.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    let frame = InputFrame::new(
        1,
        MonitorId(0),
        InputEvent::Pointer(PointerEvent::MoveBy { dx: -3.5, dy: 9.0 }),
    );
    client.send_input(&frame).await.unwrap();
    assert_eq!(expect_input(next_event(&mut server_events).await), frame);
}

#[tokio::test]
async fn both_sides_derive_the_same_pairing_proof_over_a_real_connection() {
    // The nonces come from two independent CSPRNG draws on two endpoints. If the
    // role ordering of the two nonces were wrong, this is where it would show.
    let client_id = Identity::generate().unwrap();
    let server_id = Identity::generate().unwrap();
    let empty = TrustStore::new();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &empty, &server_id, &empty, true).await;
    let (client, _ce) = client.expect("client handshake");
    let (server, _se) = server.expect("server handshake");

    assert!(!client.established().peer_was_paired);
    assert!(!server.established().peer_was_paired);

    let pin = Pin::parse("482913").unwrap();
    let client_side = PairingSession::new(client.established(), pin.clone());
    let server_side = PairingSession::new(server.established(), pin);
    assert!(server_side.accepts(&client_side.confirm()).unwrap());
    assert!(client_side.accepts(&server_side.confirm()).unwrap());

    // And a different PIN on one machine must not pair.
    let wrong = PairingSession::new(server.established(), Pin::parse("482914").unwrap());
    assert!(!wrong.accepts(&client_side.confirm()).unwrap());
}

#[tokio::test]
async fn a_large_clipboard_payload_round_trips_on_its_own_stream() {
    // Twenty megabytes: the size the clipboard stream exists for, and far more than
    // one QUIC flow-control window, so this only passes if the writer and the
    // reader agree about which stream the bytes are on.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    let msg = clipboard_payload(20 * 1024 * 1024);
    client.send_clipboard(&msg).await.unwrap();
    match next_event(&mut server_events).await {
        SessionEvent::Control(got) => assert_eq!(got, msg),
        other => panic!("expected the clipboard payload, got {other:?}"),
    }
}

#[tokio::test]
async fn the_clipboard_stream_answers_in_both_directions() {
    // The responder writes its half of the same bidirectional stream. If only the
    // initiator's direction were wired, a peer could send the clipboard but never
    // serve a request for it.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (_client, mut client_events) = client.expect("client handshake");
    let (server, _se) = server.expect("server handshake");

    let msg = ControlMsg::ClipboardStale { serial: 77 };
    server.send_clipboard(&msg).await.unwrap();
    match next_event(&mut client_events).await {
        SessionEvent::Control(got) => assert_eq!(got, msg),
        other => panic!("expected the stale reply, got {other:?}"),
    }
}

#[tokio::test]
async fn a_clipboard_transfer_does_not_hold_up_the_cursor() {
    // The whole reason the clipboard has a stream of its own. Sharing the control
    // stream would park this `TakeControl` behind twenty megabytes of image — and a
    // handoff that arrives after the transfer is a cursor frozen for its duration
    // on a link that is otherwise perfectly healthy.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    let bulk = clipboard_payload(20 * 1024 * 1024);
    let sender = client.clone();
    let writing = tokio::spawn(async move { sender.send_clipboard(&bulk).await });
    // Let the transfer get as far as its first blocked write before the handoff is
    // offered; without this the race is untested rather than won.
    tokio::task::yield_now().await;

    let handoff = ControlMsg::TakeControl {
        target: MonitorId(0),
        entry: NormPos::new(0.0, 0.5),
        via: wx_proto::Edge::Left,
    };
    client.send_control(&handoff).await.unwrap();

    match next_event(&mut server_events).await {
        SessionEvent::Control(got) => assert_eq!(
            got, handoff,
            "the clipboard transfer overtook the handoff, so they share a stream"
        ),
        other => panic!("expected the handoff first, got {other:?}"),
    }
    writing.await.unwrap().unwrap();
}

/// A `ClipboardData` message of a given payload size.
fn clipboard_payload(bytes: usize) -> ControlMsg {
    ControlMsg::ClipboardData {
        format: wx_proto::ClipboardFormat::Png,
        serial: 9,
        compression: wx_proto::Compression::None,
        // Not all one byte: a run of zeroes would be compressed away by any
        // transport that decided to try, and this has to measure the real thing.
        data: (0..bytes).map(|i| (i % 251) as u8).collect(),
    }
}

#[tokio::test]
async fn the_pair_confirm_message_survives_the_control_stream() {
    let client_id = Identity::generate().unwrap();
    let server_id = Identity::generate().unwrap();
    let empty = TrustStore::new();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &empty, &server_id, &empty, true).await;
    let (client, _ce) = client.expect("client handshake");
    let (server, mut server_events) = server.expect("server handshake");

    let pin = Pin::parse("135792").unwrap();
    let client_side = PairingSession::new(client.established(), pin.clone());
    let server_side = PairingSession::new(server.established(), pin);
    client.send_control(&client_side.confirm()).await.unwrap();

    match next_event(&mut server_events).await {
        SessionEvent::Control(msg) => assert!(server_side.accepts(&msg).unwrap()),
        other => panic!("expected a PairConfirm, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unpaired_peer_is_refused_by_the_real_transport() {
    let client_id = Identity::generate().unwrap();
    let server_id = Identity::generate().unwrap();
    let empty = TrustStore::new();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &empty, &server_id, &empty, false).await;

    match server {
        Err(TransportError::Refused(reason)) => assert_eq!(reason, RejectReason::NotPaired),
        other => panic!("server should have refused: {:?}", other.map(|_| ())),
    }
    // The dialling side must learn *why*, not just see a closed connection: that
    // is what lets the UI offer to pair instead of retrying forever.
    match client {
        Err(TransportError::RefusedByPeer(reason)) => {
            assert_eq!(reason, RejectReason::NotPaired)
        }
        other => panic!("client should have been refused: {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn a_blocked_peer_is_refused_even_with_a_pairing_dialog_open() {
    let client_id = Identity::generate().unwrap();
    let server_id = Identity::generate().unwrap();
    let mut server_trust = TrustStore::new();
    server_trust.block(client_id.node_id(), "unwanted");
    let mut client_trust = TrustStore::new();
    client_trust.trust(server_id.node_id(), "server");

    let (_endpoints, _client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, true).await;
    match server {
        Err(TransportError::Refused(reason)) => assert_eq!(reason, RejectReason::Blocked),
        other => panic!("server should have refused: {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn closing_a_session_ends_the_peer_event_stream() {
    // The peer must be able to tell a finished session from a stalled one, or it
    // never releases the modifiers the departing controller left held.
    let (client_id, client_trust, server_id, server_trust) = paired();
    let (_endpoints, client, server) =
        connect_pair(&client_id, &client_trust, &server_id, &server_trust, false).await;
    let (client, _ce) = client.expect("client handshake");
    let (_server, mut server_events) = server.expect("server handshake");

    client.close("done");
    let noticed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match server_events.next().await {
                None | Some(Err(_)) => return true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert_eq!(noticed, Ok(true), "server never noticed the disconnect");
}

#[tokio::test]
async fn a_connection_with_a_foreign_alpn_is_rejected() {
    // Another QUIC service on this port must fail in the TLS handshake rather
    // than turning into a puzzling decode error later.
    let server_id = Identity::generate().unwrap();
    let server_trust = TrustStore::new();
    let server = Endpoint::bind(loopback()).unwrap();
    let addr = server.local_addr().unwrap();
    let setup = SessionSetup {
        identity: &server_id,
        trust: &server_trust,
        local_info: node_info(server_id.node_id(), "server"),
        pairing_mode: false,
    };

    let mut foreign = quinn::Endpoint::client(loopback()).unwrap();
    let mut crypto = quinn::rustls::ClientConfig::builder_with_provider(Arc::new(
        quinn::rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&quinn::rustls::version::TLS13])
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AcceptAnything))
    .with_no_client_auth();
    crypto.alpn_protocols = vec![b"something-else/1".to_vec()];
    foreign.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap(),
    )));

    let (accepted, dialled) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(10), server.accept(&setup)),
        async {
            foreign
                .connect(addr, "winxtend.invalid")
                .unwrap()
                .await
                .map(|_| ())
        },
    );
    assert!(dialled.is_err(), "a foreign ALPN must not connect");
    assert!(
        matches!(accepted, Ok(Some(Err(_))) | Err(_)),
        "server must not hand up a session for a foreign ALPN"
    );
}

/// Certificate verifier for the foreign-ALPN client above. ALPN is negotiated
/// before certificates are exchanged, so this never actually runs; rustls simply
/// requires a verifier to build a client config.
#[derive(Debug)]
struct AcceptAnything;

impl quinn::rustls::client::danger::ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &quinn::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[quinn::rustls::pki_types::CertificateDer<'_>],
        _server_name: &quinn::rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: quinn::rustls::pki_types::UnixTime,
    ) -> Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error> {
        Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        quinn::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
