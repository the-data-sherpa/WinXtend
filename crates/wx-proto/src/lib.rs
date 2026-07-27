//! WinXtend wire protocol.
//!
//! Shared vocabulary for every WinXtend component: the agent, the UI, and the
//! optional relay. Deliberately free of I/O, async, and platform code so that it
//! compiles for every target and is exhaustively testable without a network.
//!
//! # Shape of the protocol
//!
//! Two planes with different delivery requirements:
//!
//! | Plane | Transport | Carries |
//! |---|---|---|
//! | Input | QUIC datagrams, unreliable | pointer motion, keys |
//! | Control | QUIC streams, reliable + ordered | handshake, layout, clipboard, files, video |
//!
//! Splitting them is the point. Pointer motion sent reliably means a single lost
//! packet holds back every newer position behind it, which is exactly the
//! "laggy cursor" failure of TCP-based KVM software. Motion is dropped and
//! superseded instead; anything that latches state ([`input::Reliability`])
//! still goes reliably.
//!
//! # Design notes worth knowing before changing anything
//!
//! * **Positions cross the wire normalized** to `0.0..1.0` within a monitor
//!   ([`geom::NormPos`]), never as pixels — a 4K display can then drive a 1080p
//!   one with no scaling arithmetic on either side.
//! * **Keys cross the wire as text**, resolved through the *sender's* layout
//!   ([`keyboard::KeyPayload::Text`]), so a Norwegian keyboard types `å` on a US
//!   machine. This is the one idea worth taking wholesale from Hydra.
//! * **Monitors live in one global coordinate space** ([`control::Placement`]),
//!   so adjacency is geometry rather than configuration. Split edges and
//!   L-shaped layouts need no special syntax.
//! * **Enum variants are append-only.** postcard encodes them by index;
//!   inserting a variant silently reinterprets older peers' messages.

pub mod caps;
pub mod codec;
pub mod control;
pub mod geom;
pub mod ids;
pub mod input;
pub mod keyboard;

pub use caps::{
    check_version, Capabilities, DisplayServer, Platform, VersionCheck, MIN_COMPATIBLE_VERSION,
    PROTOCOL_VERSION,
};
pub use codec::{decode, encode_datagram, encode_frame, CodecError, FrameReader};
pub use control::{
    ClipboardFormat, Compression, ControlMsg, Layout, NodeInfo, Placement, RejectReason,
    VideoCodec, VideoConfig,
};
pub use geom::{Edge, Monitor, NormPos, Point, Rect};
pub use ids::{GlobalMonitorId, IdParseError, MonitorId, NodeId, Signature};
pub use input::{
    InputEvent, InputFrame, MouseButton, PointerEvent, Reliability, ScrollUnit, SequenceGate,
};
pub use keyboard::{KeyAction, KeyEvent, KeyPayload, Modifiers, SpecialKey};

/// DNS-SD service type used for peer auto-discovery on the local network.
///
/// UDP because the transport is QUIC. Peers publish their [`NodeId`] and agent
/// version as TXT records, so the UI can list discovered machines — and tell
/// paired from unpaired ones — before any connection is attempted.
pub const DISCOVERY_SERVICE_TYPE: &str = "_winxtend._udp.local.";

/// Default port for the agent's QUIC listener.
pub const DEFAULT_PORT: u16 = 24800;

/// ALPN identifier for the QUIC handshake.
///
/// Versioned so a future incompatible protocol can share the port without ever
/// being mistaken for this one — the TLS handshake fails cleanly instead of
/// producing a confusing decode error later.
pub const ALPN: &[u8] = b"winxtend/1";
