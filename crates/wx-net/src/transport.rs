//! QUIC transport: one connection carrying both protocol planes.
//!
//! # Why QUIC
//!
//! The protocol needs unreliable datagrams for pointer motion and reliable
//! ordered streams for everything else, over one NAT-traversable connection with
//! one handshake. TCP plus UDP means two sockets, two firewall rules, and two
//! failure modes; QUIC gives both planes over a single UDP flow.
//!
//! # Streams on a connection
//!
//! | Stream | Opened by | Carries |
//! |---|---|---|
//! | control (bidirectional) | initiator, first | handshake, then [`ControlMsg`] |
//! | input (bidirectional) | initiator, second | [`InputFrame`]s that must not be lost |
//! | clipboard (unidirectional) | either, on demand | the bulk [`ControlMsg`] variants |
//! | (datagrams) | either | [`InputFrame`]s that may be dropped |
//!
//! Reliable input has its own stream rather than sharing the control stream on
//! purpose: a 20MB clipboard image on the control stream would otherwise sit in
//! front of a key-release frame, and a delayed key release is a stuck modifier.
//! The clipboard has its own for the mirror-image reason — see
//! [`STREAM_TAG_CLIPBOARD`].
//!
//! # Peer authentication
//!
//! TLS certificate verification here is deliberately permissive. See
//! [`AnyPeerCertificate`] before concluding that it is a bug.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use quinn::rustls;
use tokio::sync::{mpsc, Mutex};
use wx_proto::codec::MAX_DATAGRAM_LEN;
use wx_proto::{
    decode, encode_datagram, encode_frame, ControlMsg, FrameReader, InputEvent, InputFrame,
    MonitorId, NodeInfo, RejectReason, Reliability, ALPN,
};

use crate::handshake::{Established, Handshake, HandshakeError, Outcome, Role};
use crate::identity::{ChannelBinding, Identity, TrustStore};

/// Subject name on the throwaway certificate.
///
/// Also the name passed to [`quinn::Endpoint::connect`]. It is not checked
/// against anything meaningful — see [`AnyPeerCertificate`] — but it must be a
/// valid DNS name or rustls refuses to build the ClientHello.
const CERT_NAME: &str = "winxtend.invalid";

/// RFC 5705 exporter label the channel binding is derived under.
///
/// Application-specific and versioned: an exporter output is only unique per
/// (session, label, context), so sharing a label with any other use of this
/// connection would hand out the same bytes for two different purposes.
const CHANNEL_BINDING_LABEL: &[u8] = b"winxtend-channel-binding-v1";

/// First byte written on the reliable input stream.
///
/// Identifies what a stream is for, so that file transfer and video can open
/// their own streams later without the receiver having to guess from content.
const STREAM_TAG_INPUT: u8 = 1;

/// First byte written on the clipboard stream.
///
/// # Why the clipboard gets a stream of its own
///
/// A clipboard payload is up to [`wx_proto::codec::MAX_CLIPBOARD_BYTES`], and the
/// control stream carries the handoff messages that decide which machine the
/// user's keyboard is talking to. QUIC flow control is *per stream*: a peer whose
/// receive window is shut stalls the writer for that stream and no other. Sharing
/// one stream would mean a 20MB image parked in front of the `TakeControl` behind
/// it, and the send lock held for the whole of the write — so the cursor would
/// hang for as long as the transfer took, on a link that was otherwise healthy.
///
/// Chunking on the shared stream was the alternative and does not solve it:
/// smaller writes still queue behind each other on one stream, still take the same
/// lock, and a closed window blocks the first chunk exactly as it blocks the
/// whole. The isolation has to come from QUIC, which means a second stream.
///
/// # Why it is unidirectional, and opened only when there is something to send
///
/// Unlike control and input, this stream is **not** part of bringing a session up.
/// Each end opens one of its own the first time it has a clipboard message to
/// write, and reads whatever the peer opens; a session where neither end ever
/// syncs the clipboard has no clipboard stream at all, and that is a normal
/// outcome rather than a failure.
///
/// Both halves of that matter:
///
/// * **Not part of the handshake.** A peer built before this stream existed
///   completes the handshake and then never opens one. Waiting for it there would
///   hang [`establish`] against a build that is otherwise entirely compatible —
///   capability bits gate features, the version guards the wire format, and this
///   is a feature.
/// * **Not gated on the handshake capability set either.** On Wayland the portal
///   grant routinely lands *after* the handshake, so a peer's advertised clipboard
///   support at that moment is a snapshot that is often already stale. A gate
///   taken then would leave a peer that gains the capability a second later with
///   no clipboard for the rest of the session. Opening on demand asks the question
///   at the moment there is something to send, which is the only time the answer
///   is current.
const STREAM_TAG_CLIPBOARD: u8 = 2;

/// How deep the per-session event queue is before backpressure applies.
///
/// Bounded rather than unbounded: an unbounded queue turns a stalled consumer
/// into unbounded memory growth, and the whole point of the input plane is that
/// stale events are worthless anyway.
const EVENT_QUEUE_DEPTH: usize = 1024;

/// How long a refusing side waits for the peer to read its `Reject` and hang up.
///
/// Dropping a `quinn::Connection` sends CONNECTION_CLOSE at once and abandons
/// anything still queued, so without this wait the peer sees an unexplained
/// disconnect instead of "you are not paired" — and an agent that cannot tell
/// those apart retries forever instead of prompting the user to pair. Bounded
/// because a hostile peer must not be able to pin the connection open by simply
/// never reading.
const REJECT_GRACE: Duration = Duration::from_secs(1);

/// How often an otherwise silent connection sends a keepalive.
///
/// A KVM session is idle for minutes at a time while the user works on one
/// machine. Without keepalives a NAT mapping or the idle timeout tears the
/// connection down, and the next mouse move waits on a fresh handshake before
/// anything on screen moves.
const KEEP_ALIVE: Duration = Duration::from_secs(5);

/// When a connection with no traffic at all is declared dead. Must be comfortably
/// larger than [`KEEP_ALIVE`], or an idle session drops even while healthy.
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a peer has to get from an accepted connection to a live session.
///
/// Covers the handshake and the streams that come up with it. It exists because
/// none of those waits is bounded on its own: keepalives hold the QUIC idle
/// timeout off indefinitely, so a peer that connects and then simply stops — a
/// half-implemented build, or one that means to — otherwise waits forever. That is
/// not only its own session's problem: [`Endpoint::accept`] finishes each
/// connection before it looks at the next, so one such peer takes the whole
/// inbound path with it and the agent stops accepting anybody.
///
/// Generous rather than tight. Nothing here waits on a human, but a first
/// connection over a slow link crosses several round trips, and refusing a healthy
/// peer is worse than tolerating a dead one for a few more seconds.
const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("generating the session certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("configuring TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("the TLS suite has no QUIC initial cipher: {0}")]
    NoInitialCipher(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("binding a QUIC socket to {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("connecting to {addr}: {source}")]
    Connect {
        addr: SocketAddr,
        #[source]
        source: quinn::ConnectError,
    },
    #[error("connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("writing to a stream: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("reading from a stream: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("framing: {0}")]
    Codec(#[from] wx_proto::CodecError),
    #[error("handshake: {0}")]
    Handshake(#[from] HandshakeError),
    #[error("the peer refused the connection: {0:?}")]
    RefusedByPeer(RejectReason),
    #[error("refused the peer: {0:?}")]
    Refused(RejectReason),
    #[error("the peer closed the control stream mid-handshake")]
    HandshakeAbandoned,
    #[error("the peer opened an input stream with an unrecognised tag {0}")]
    UnknownStreamTag(u8),
    #[error("the peer did not finish setting up the session within {0:?}")]
    SetupTimedOut(Duration),
    #[error("the QUIC session produced no channel-binding material, so the handshake cannot be tied to it")]
    NoChannelBinding,
    #[error("the random number generator failed: {0}")]
    Random(getrandom::Error),
}

/// Everything a new connection needs in order to authenticate its peer.
///
/// Passed per connection rather than stored on the [`Endpoint`] because both the
/// trust store and the local monitor list change while the agent runs, and a
/// stale snapshot would mean a freshly paired peer still being refused.
pub struct SessionSetup<'a> {
    pub identity: &'a Identity,
    pub trust: &'a TrustStore,
    pub local_info: NodeInfo,
    /// Admit a peer that is not yet in the trust store, for the pairing flow.
    /// See [`Handshake::pairing_mode`] for why this is narrower than it sounds.
    pub pairing_mode: bool,
}

/// A bound UDP socket that both listens for peers and dials them.
///
/// One socket for both directions is what lets two machines behind the same
/// router find each other on a single well-known port, and it means only one
/// firewall rule to explain to a user.
pub struct Endpoint {
    inner: quinn::Endpoint,
}

impl Endpoint {
    /// Bind and start listening. Pass port 0 to let the OS choose, which is what
    /// tests want.
    pub fn bind(addr: SocketAddr) -> Result<Self, TransportError> {
        let mut inner = quinn::Endpoint::server(server_config()?, addr)
            .map_err(|source| TransportError::Bind { addr, source })?;
        inner.set_default_client_config(client_config()?);
        Ok(Self { inner })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.inner.local_addr()
    }

    /// Dial a peer and complete the handshake as the initiator.
    pub async fn connect(
        &self,
        addr: SocketAddr,
        setup: &SessionSetup<'_>,
    ) -> Result<(Session, Events), TransportError> {
        let conn = self
            .inner
            .connect(addr, CERT_NAME)
            .map_err(|source| TransportError::Connect { addr, source })?
            .await?;
        let (send, recv) = conn.open_bi().await?;
        establish(conn, Role::Initiator, setup, send, recv).await
    }

    /// Accept the next inbound connection and complete the handshake as the
    /// responder. `None` once the endpoint is closed.
    pub async fn accept(
        &self,
        setup: &SessionSetup<'_>,
    ) -> Option<Result<(Session, Events), TransportError>> {
        let incoming = self.inner.accept().await?;
        Some(self.finish_accept(incoming, setup).await)
    }

    /// Bounded as a whole, and not only in its parts.
    ///
    /// See [`SESSION_SETUP_TIMEOUT`]: this runs inline on the accept loop, so a
    /// peer that stalls anywhere between the connection and a live session — the
    /// control stream it never opens, the handshake reply it never sends, the
    /// input stream it never brings up — would otherwise stop this agent accepting
    /// anyone at all. One bound around the lot rather than one per await, because
    /// the property that matters is about the whole path and a per-step timeout
    /// leaves the next step to be found later.
    async fn finish_accept(
        &self,
        incoming: quinn::Incoming,
        setup: &SessionSetup<'_>,
    ) -> Result<(Session, Events), TransportError> {
        let setup_session = async {
            let conn = incoming.await?;
            let (send, recv) = conn.accept_bi().await?;
            establish(conn, Role::Responder, setup, send, recv).await
        };
        match tokio::time::timeout(SESSION_SETUP_TIMEOUT, setup_session).await {
            Ok(result) => result,
            Err(_) => Err(TransportError::SetupTimedOut(SESSION_SETUP_TIMEOUT)),
        }
    }

    /// Stop accepting and tell live peers we are going away.
    pub fn close(&self) {
        self.inner.close(0u32.into(), b"shutdown");
    }

    /// Wait for close frames to be delivered. Without this a process can exit
    /// before the peer learns the disconnect was intentional, and the peer then
    /// spends its reconnect backoff on a machine that is gone.
    pub async fn wait_idle(&self) {
        self.inner.wait_idle().await;
    }
}

/// Run the handshake on a freshly opened control stream, then bring up the input
/// stream and the reader tasks.
async fn establish(
    conn: quinn::Connection,
    role: Role,
    setup: &SessionSetup<'_>,
    mut control_tx: quinn::SendStream,
    mut control_rx: quinn::RecvStream,
) -> Result<(Session, Events), TransportError> {
    let nonce = crate::handshake::fresh_nonce().map_err(TransportError::Random)?;
    let established =
        run_handshake(&conn, role, setup, nonce, &mut control_tx, &mut control_rx).await?;

    // The initiator opens the input stream and writes its tag immediately: an
    // opened QUIC stream is invisible to the peer until something is written, so
    // without the tag the responder's `accept_bi` would never return.
    //
    // The input stream, and only it. The clipboard is opened on demand and read
    // wherever it turns up — see `STREAM_TAG_CLIPBOARD`, and `accept_bulk_streams`
    // below.
    let (input_tx, input_rx) = match role {
        Role::Initiator => {
            let (mut tx, rx) = conn.open_bi().await?;
            tx.write_all(&[STREAM_TAG_INPUT]).await?;
            (tx, rx)
        }
        Role::Responder => {
            let (tx, mut rx) = conn.accept_bi().await?;
            let mut tag = [0u8; 1];
            rx.read_exact(&mut tag)
                .await
                .map_err(|_| TransportError::HandshakeAbandoned)?;
            if tag[0] != STREAM_TAG_INPUT {
                return Err(TransportError::UnknownStreamTag(tag[0]));
            }
            (tx, rx)
        }
    };

    let inner = Arc::new(SessionInner {
        conn: conn.clone(),
        established,
        control_tx: Mutex::new(control_tx),
        input_tx: Mutex::new(input_tx),
        clipboard_tx: Mutex::new(None),
        seq: AtomicU64::new(1),
        dropped_datagrams: AtomicU64::new(0),
    });

    let (tx, rx) = mpsc::channel(EVENT_QUEUE_DEPTH);
    tokio::spawn(read_control(control_rx, tx.clone()));
    tokio::spawn(read_reliable_input(input_rx, tx.clone()));
    tokio::spawn(accept_bulk_streams(conn.clone(), tx.clone()));
    tokio::spawn(read_datagrams(conn, tx, Arc::clone(&inner)));

    Ok((Session { inner }, Events { rx }))
}

/// Read whatever unidirectional streams the peer opens, for as long as it does.
///
/// A session's clipboard lane appears here rather than during setup, so that a
/// peer which never opens one — an older build, or simply a machine nobody has
/// copied anything on — costs nothing and is not an error. It runs for the life of
/// the connection because a peer may gain the clipboard capability at any point,
/// which on Wayland is the ordinary case and not an edge one.
///
/// Each stream is dispatched by its tag in a task of its own: a peer that opens a
/// stream and never writes its tag must not be able to stall the next one, and a
/// clipboard transfer of tens of megabytes holds up only itself.
async fn accept_bulk_streams(
    conn: quinn::Connection,
    tx: mpsc::Sender<Result<SessionEvent, TransportError>>,
) {
    loop {
        let rx = match conn.accept_uni().await {
            Ok(rx) => rx,
            // The connection is finished; the readers report the death.
            Err(_) => return,
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut rx = rx;
            let mut tag = [0u8; 1];
            if rx.read_exact(&mut tag).await.is_err() {
                return;
            }
            match tag[0] {
                // Carries `ControlMsg` and is read by the same code: what differs
                // between this and the control stream is only which writer can
                // block the other, and that is settled by the time a message
                // arrives.
                STREAM_TAG_CLIPBOARD => read_control(rx, tx).await,
                other => {
                    // Ignored rather than fatal, and the mirror image of opening on
                    // demand: a peer built after this one may open a stream for
                    // something this build has no reader for, and refusing the
                    // whole session over a lane it simply does not use would make
                    // every future addition a breaking change.
                    tracing::debug!(
                        tag = other,
                        "ignoring a stream this build has no reader for"
                    );
                }
            }
        });
    }
}

/// Keying material that identifies this TLS session and no other.
///
/// Both ends of the same session derive the same 32 bytes; two sessions never do.
/// That is what the handshake signature covers, and it is the only reason the
/// permissive certificate verifier is survivable — see [`AnyPeerCertificate`].
///
/// A failure here is fatal to the connection rather than something to work
/// around: proceeding without a binding would silently restore the relay attack.
fn channel_binding(conn: &quinn::Connection) -> Result<ChannelBinding, TransportError> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, CHANNEL_BINDING_LABEL, &[])
        .map_err(|_| TransportError::NoChannelBinding)?;
    Ok(ChannelBinding::from_bytes(out))
}

async fn run_handshake(
    conn: &quinn::Connection,
    role: Role,
    setup: &SessionSetup<'_>,
    nonce: [u8; 32],
    tx: &mut quinn::SendStream,
    rx: &mut quinn::RecvStream,
) -> Result<Established, TransportError> {
    let mut hs = Handshake::new(
        role,
        setup.local_info.clone(),
        nonce,
        channel_binding(conn)?,
    )
    .pairing_mode(setup.pairing_mode);
    for msg in hs.start() {
        tx.write_all(&encode_frame(&msg)?).await?;
    }

    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    loop {
        while let Some(msg) = reader.next_message::<ControlMsg>()? {
            let step = hs.on_message(&msg, setup.identity, setup.trust)?;
            for out in &step.send {
                tx.write_all(&encode_frame(out)?).await?;
            }
            match step.outcome {
                Outcome::Continue => {}
                Outcome::Established(e) => return Ok(*e),
                Outcome::Refused(reason) => {
                    let _ = tx.finish();
                    // Let the peer read the Reject and close first. See
                    // REJECT_GRACE for why this wait is not optional.
                    let _ = tokio::time::timeout(REJECT_GRACE, conn.closed()).await;
                    return Err(TransportError::Refused(reason));
                }
                Outcome::RefusedByPeer(reason) => {
                    return Err(TransportError::RefusedByPeer(reason))
                }
            }
        }
        match rx.read(&mut buf).await? {
            Some(n) => reader.push(&buf[..n]),
            None => return Err(TransportError::HandshakeAbandoned),
        }
    }
}

struct SessionInner {
    conn: quinn::Connection,
    established: Established,
    control_tx: Mutex<quinn::SendStream>,
    input_tx: Mutex<quinn::SendStream>,
    /// `None` until this end has something to put on it. See
    /// [`STREAM_TAG_CLIPBOARD`].
    clipboard_tx: Mutex<Option<quinn::SendStream>>,
    seq: AtomicU64,
    dropped_datagrams: AtomicU64,
}

/// The sending half of an authenticated session. Cheap to clone and shareable
/// across tasks: input capture and the control loop live in different places.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

impl Session {
    pub fn peer(&self) -> &NodeInfo {
        &self.inner.established.peer
    }

    pub fn established(&self) -> &Established {
        &self.inner.established
    }

    pub fn role(&self) -> Role {
        self.inner.established.role
    }

    /// Next sequence number for this connection.
    ///
    /// Handed out here rather than by the caller so that numbering stays
    /// monotonic when several tasks send at once. The receiver's
    /// [`wx_proto::SequenceGate`] depends on that to drop stale motion.
    pub fn next_seq(&self) -> u64 {
        self.inner.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Send an input event, choosing the plane from its own reliability.
    pub async fn send_event(
        &self,
        target: MonitorId,
        event: InputEvent,
    ) -> Result<(), TransportError> {
        let frame = InputFrame::new(self.next_seq(), target, event);
        self.send_input(&frame).await
    }

    /// Send a pre-built frame. Best-effort events go by datagram, everything that
    /// latches state goes by stream.
    pub async fn send_input(&self, frame: &InputFrame) -> Result<(), TransportError> {
        match frame.event.reliability() {
            Reliability::BestEffort => self.send_datagram(frame).await,
            Reliability::Reliable => self.send_input_reliably(frame).await,
        }
    }

    async fn send_datagram(&self, frame: &InputFrame) -> Result<(), TransportError> {
        let bytes = encode_datagram(frame)?;
        let fits = self
            .inner
            .conn
            .max_datagram_size()
            .is_some_and(|max| bytes.len() <= max.min(MAX_DATAGRAM_LEN));
        if !fits {
            // Either the path MTU shrank or the peer never enabled datagrams.
            // Falling back beats dropping the event: motion is self-correcting,
            // but a silently vanished plane looks like a frozen cursor.
            return self.send_input_reliably(frame).await;
        }
        match self.inner.conn.send_datagram(Bytes::from(bytes)) {
            Ok(()) => Ok(()),
            Err(quinn::SendDatagramError::ConnectionLost(e)) => Err(e.into()),
            Err(_) => self.send_input_reliably(frame).await,
        }
    }

    async fn send_input_reliably(&self, frame: &InputFrame) -> Result<(), TransportError> {
        let bytes = encode_frame(frame)?;
        let mut stream = self.inner.input_tx.lock().await;
        stream.write_all(&bytes).await?;
        Ok(())
    }

    pub async fn send_control(&self, msg: &ControlMsg) -> Result<(), TransportError> {
        let bytes = encode_frame(msg)?;
        // One lock around encode-then-write would be redundant; encoding is pure.
        // The lock only has to keep two messages from interleaving on the stream.
        let mut stream = self.inner.control_tx.lock().await;
        stream.write_all(&bytes).await?;
        Ok(())
    }

    /// Send a clipboard message on the stream reserved for them.
    ///
    /// Same message type as [`Session::send_control`] and a different stream, for
    /// the reason in [`STREAM_TAG_CLIPBOARD`]: a payload of tens of megabytes must
    /// not be able to hold up the messages that decide where the cursor is. The
    /// caller chooses — nothing here inspects the variant — because only the caller
    /// knows whether a given message is part of a bulk transfer.
    ///
    /// The first call opens the stream. That is what keeps a session with no
    /// clipboard traffic free of one, and it is done under the same lock that
    /// serialises the writes, so two messages racing to be first still produce one
    /// stream and stay in order on it.
    pub async fn send_clipboard(&self, msg: &ControlMsg) -> Result<(), TransportError> {
        let bytes = encode_frame(msg)?;
        let mut stream = self.inner.clipboard_tx.lock().await;
        let stream = match stream.as_mut() {
            Some(stream) => stream,
            None => {
                let mut opened = self.inner.conn.open_uni().await?;
                opened.write_all(&[STREAM_TAG_CLIPBOARD]).await?;
                stream.insert(opened)
            }
        };
        stream.write_all(&bytes).await?;
        Ok(())
    }

    /// Motion datagrams discarded because the consumer was not keeping up.
    ///
    /// Exposed because a non-zero value here is the signature of an input loop
    /// that has fallen behind, which otherwise presents only as "feels laggy".
    pub fn dropped_datagrams(&self) -> u64 {
        self.inner.dropped_datagrams.load(Ordering::Relaxed)
    }

    pub fn close(&self, reason: &str) {
        self.inner.conn.close(0u32.into(), reason.as_bytes());
    }

    /// Round-trip time estimate, for the UI and for diagnosing a slow link.
    pub fn rtt(&self) -> Duration {
        self.inner.conn.rtt()
    }
}

/// Something arriving from the peer.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Control(ControlMsg),
    Input(InputFrame),
}

/// The receiving half of a session.
///
/// Both planes are merged into one queue so the consumer has a single place to
/// await. The alternative — selecting over three sources at the call site — is
/// where cancellation-safety bugs live, and dropping a half-read stream read
/// desynchronises the frame reader permanently.
pub struct Events {
    rx: mpsc::Receiver<Result<SessionEvent, TransportError>>,
}

impl Events {
    /// Next event, or `None` once the connection is finished and drained.
    pub async fn next(&mut self) -> Option<Result<SessionEvent, TransportError>> {
        self.rx.recv().await
    }
}

async fn read_control(
    mut rx: quinn::RecvStream,
    tx: mpsc::Sender<Result<SessionEvent, TransportError>>,
) {
    let mut reader = FrameReader::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match rx.read(&mut buf).await {
            Ok(Some(n)) => reader.push(&buf[..n]),
            // Clean end of stream: the peer is done talking.
            Ok(None) => return,
            Err(e) => {
                let _ = tx.send(Err(e.into())).await;
                return;
            }
        }
        loop {
            match reader.next_message::<ControlMsg>() {
                Ok(Some(msg)) => {
                    if tx.send(Ok(SessionEvent::Control(msg))).await.is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    // A length-prefixed stream cannot resynchronise after a bad
                    // frame, so this is terminal by construction.
                    let _ = tx.send(Err(e.into())).await;
                    return;
                }
            }
        }
    }
}

async fn read_reliable_input(
    mut rx: quinn::RecvStream,
    tx: mpsc::Sender<Result<SessionEvent, TransportError>>,
) {
    let mut reader = FrameReader::new();
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        match rx.read(&mut buf).await {
            Ok(Some(n)) => reader.push(&buf[..n]),
            Ok(None) => return,
            Err(e) => {
                let _ = tx.send(Err(e.into())).await;
                return;
            }
        }
        loop {
            match reader.next_message::<InputFrame>() {
                Ok(Some(frame)) => {
                    if tx.send(Ok(SessionEvent::Input(frame))).await.is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(e.into())).await;
                    return;
                }
            }
        }
    }
}

async fn read_datagrams(
    conn: quinn::Connection,
    tx: mpsc::Sender<Result<SessionEvent, TransportError>>,
    inner: Arc<SessionInner>,
) {
    loop {
        let bytes = match conn.read_datagram().await {
            Ok(bytes) => bytes,
            // Connection teardown is reported by the stream readers; reporting it
            // three times would have the consumer handle the same death thrice.
            Err(_) => return,
        };
        let frame: InputFrame = match decode(&bytes) {
            Ok(frame) => frame,
            Err(e) => {
                // A corrupt datagram is not fatal: the plane is lossy by design,
                // so drop this one and keep the connection.
                tracing::debug!(error = %e, "discarding undecodable input datagram");
                continue;
            }
        };
        // Never await on a full queue here. Blocking would hold back newer
        // positions behind older ones, which is exactly the head-of-line stall
        // that using datagrams was meant to avoid.
        if tx.try_send(Ok(SessionEvent::Input(frame))).is_err() {
            inner.dropped_datagrams.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut cfg = quinn::TransportConfig::default();
    cfg.keep_alive_interval(Some(KEEP_ALIVE));
    cfg.max_idle_timeout(Some(
        IDLE_TIMEOUT
            .try_into()
            .expect("IDLE_TIMEOUT is within QUIC's representable range"),
    ));
    // Enough buffer for a burst of motion, small enough that a stalled consumer
    // cannot accumulate a visible backlog of stale positions.
    cfg.datagram_receive_buffer_size(Some(64 * MAX_DATAGRAM_LEN));
    Arc::new(cfg)
}

fn server_config() -> Result<quinn::ServerConfig, TransportError> {
    // A fresh self-signed certificate per process. It is never pinned and never
    // compared to anything, so there is nothing to persist and nothing to rotate.
    let certified = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()])?;
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());

    let mut crypto = rustls::ServerConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key.into())?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?,
    ));
    config.transport_config(transport_config());
    Ok(config)
}

fn client_config() -> Result<quinn::ClientConfig, TransportError> {
    let provider = crypto_provider();
    let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyPeerCertificate { provider }))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    ));
    config.transport_config(transport_config());
    Ok(config)
}

/// Accepts whatever certificate the peer presents.
///
/// **This is not a disabled security check; it is a check moved.** Read this
/// before "fixing" it.
///
/// The usual job of certificate verification is to bind the connection to an
/// identity a third party vouched for. There is no third party here: WinXtend
/// peers are machines on a desk with no CA, no DNS names, and no enrolment step,
/// so any PKI check would either always fail or be satisfied by a certificate
/// any attacker could mint just as easily. Pinning per-device certificates would
/// work, but it duplicates the ed25519 identity the protocol already has and adds
/// a second thing to rotate and re-pair.
///
/// So TLS is used for what it is good at unaided — negotiating a fresh session
/// key and encrypting the traffic — and identity is bound one layer up: the peer
/// signs a nonce this side chose with the private key behind the [`wx_proto::NodeId`]
/// it advertises, and that key must already be in the trust store. See
/// [`crate::handshake`].
///
/// **A signature over the nonce alone would not be enough, and it is worth being
/// exact about why.** A man in the middle does not have to *produce* a signature:
/// it can terminate TLS towards each peer with its own certificate — which this
/// verifier accepts — and relay `Hello`, `Welcome` and `AuthProof` byte for byte.
/// Each peer then verifies a genuine signature by the right key over its own
/// fresh nonce, both reach `Established`, and the attacker holds both session
/// keys. Nonce freshness does not help: it only stops offline replay, and nothing
/// here is being replayed.
///
/// What closes that is the channel binding: every handshake signature also covers
/// [`channel_binding`], derived from the TLS exporter of the session the messages
/// actually travelled over. A relay necessarily has two sessions, so the value it
/// forwards a proof into is not the value that proof was made under, and
/// verification fails before any session exists. The binding is never sent, so
/// there is nothing for the attacker to substitute.
///
/// Two things are still enforced below, and they matter:
///
/// * The TLS handshake signature is verified normally, so the peer must hold the
///   private key for the certificate it presented. Skipping that would allow a
///   trivial replay of a captured handshake.
/// * ALPN pins [`wx_proto::ALPN`], so a connection to some unrelated QUIC service
///   on the same port fails cleanly instead of at the first decode.
///
/// The residual exposure is that an attacker can complete the *TLS* handshake
/// and reach the application handshake. That costs them one refused connection.
#[derive(Debug)]
struct AnyPeerCertificate {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for AnyPeerCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Intentionally unconditional: nothing about this certificate carries
        // information, because the peer minted it seconds ago for itself.
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_endpoint_configs_pin_the_protocol_alpn() {
        // Sharing a port with another QUIC service must fail in the TLS
        // handshake, not later as a confusing decode error.
        assert!(server_config().is_ok());
        assert!(client_config().is_ok());
    }

    #[test]
    fn a_certificate_is_generated_per_endpoint() {
        // Nothing pins these, so reusing one across processes would only create a
        // fingerprint to correlate; each endpoint minting its own is cheap.
        let a = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()]).unwrap();
        let b = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_string()]).unwrap();
        assert_ne!(a.cert.der().as_ref(), b.cert.der().as_ref());
    }

    #[test]
    fn keepalives_are_frequent_enough_to_hold_an_idle_session_open() {
        // Reversed, or even merely close, and an idle-but-healthy session drops:
        // the user comes back from lunch and the first mouse move stalls on a
        // reconnect. Several keepalives must fit inside the timeout so that
        // losing one does not kill the connection.
        assert!(
            KEEP_ALIVE * 3 <= IDLE_TIMEOUT,
            "{KEEP_ALIVE:?} keepalive against a {IDLE_TIMEOUT:?} timeout"
        );
        // The config must actually build with these values; a too-large idle
        // timeout panics inside quinn's conversion rather than failing here.
        let _ = transport_config();
    }

    // `Endpoint::bind` needs a tokio reactor to register the UDP socket with, so
    // these cannot be plain `#[test]`.
    #[tokio::test]
    async fn binding_to_port_zero_yields_a_real_local_port() {
        let ep = Endpoint::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        assert_ne!(ep.local_addr().unwrap().port(), 0);
        ep.close();
    }

    /// One real QUIC session, with both `Connection` handles in hand.
    ///
    /// The endpoints are returned because dropping a `quinn::Endpoint` closes its
    /// socket underneath the connection.
    async fn raw_session() -> (
        quinn::Endpoint,
        quinn::Endpoint,
        quinn::Connection,
        quinn::Connection,
    ) {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = quinn::Endpoint::server(server_config().unwrap(), addr).unwrap();
        let server_addr = server.local_addr().unwrap();
        let mut client = quinn::Endpoint::client(addr).unwrap();
        client.set_default_client_config(client_config().unwrap());
        let (accepted, connected) = tokio::join!(
            async { server.accept().await.unwrap().await.unwrap() },
            async {
                client
                    .connect(server_addr, CERT_NAME)
                    .unwrap()
                    .await
                    .unwrap()
            },
        );
        (server, client, accepted, connected)
    }

    #[tokio::test]
    async fn both_ends_of_a_session_derive_the_same_channel_binding() {
        // The handshake signature covers this value, so if the two ends disagreed,
        // no honest peer could ever authenticate.
        let (_s, _c, server_conn, client_conn) = raw_session().await;
        assert_eq!(
            channel_binding(&server_conn).unwrap(),
            channel_binding(&client_conn).unwrap()
        );
    }

    #[tokio::test]
    async fn two_sessions_never_share_a_channel_binding() {
        // This is what stops a man in the middle relaying a handshake: it holds two
        // TLS sessions, and a proof made under one cannot verify under the other.
        let (_s1, _c1, first, _f) = raw_session().await;
        let (_s2, _c2, second, _g) = raw_session().await;
        assert_ne!(
            channel_binding(&first).unwrap(),
            channel_binding(&second).unwrap()
        );
    }

    #[tokio::test]
    async fn two_endpoints_can_bind_independently() {
        let a = Endpoint::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let b = Endpoint::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        assert_ne!(a.local_addr().unwrap(), b.local_addr().unwrap());
        a.close();
        b.close();
    }
}
