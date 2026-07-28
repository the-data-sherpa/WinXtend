//! Control-plane messages, carried on a reliable ordered stream.
//!
//! Everything that is not per-event input lives here: the handshake, topology
//! changes, control handoff, clipboard, file transfer, and video negotiation.

use serde::{Deserialize, Serialize};

use crate::caps::{Capabilities, DisplayServer, Platform};
use crate::geom::{Edge, Monitor, NormPos, Rect};
use crate::ids::{GlobalMonitorId, MonitorId, NodeId, Signature};

/// Everything a peer needs to know about a node to place it in the mesh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: NodeId,
    /// Human-readable label shown in the UI. Defaults to the hostname, but is
    /// user-editable: hostnames are frequently unhelpful.
    pub name: String,
    pub platform: Platform,
    pub display_server: DisplayServer,
    pub capabilities: Capabilities,
    /// Displays attached right now, in the node's own coordinate space.
    pub monitors: Vec<Monitor>,
    /// Agent build version, for diagnosing mixed-version meshes.
    pub agent_version: String,
}

/// Where one monitor sits in the shared global desktop.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub monitor: GlobalMonitorId,
    /// Bounds in the global virtual desktop.
    ///
    /// Need not match the monitor's native resolution: the layout editor may
    /// place a 4K panel as a 1080p-sized rectangle so that cursor speed feels
    /// consistent across mismatched displays.
    pub global_bounds: Rect,
    /// Cursor speed multiplier while on this monitor. `1.0` is unscaled.
    pub cursor_scale: f32,
}

/// The full mesh layout.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Layout {
    pub placements: Vec<Placement>,
    /// Bumped on every change. Lets a reconnecting node tell whether its cached
    /// layout is current without re-fetching it.
    pub revision: u64,
}

/// Why a handshake was refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RejectReason {
    ProtocolTooOld {
        min_supported: u16,
    },
    /// No longer sent by this build: a peer ahead of us negotiates down instead
    /// of being refused — see `admit` in `crates/wx-net/src/handshake.rs` and
    /// [`crate::check_version`].
    ///
    /// Kept anyway, and not to be removed: it is a postcard variant index, so
    /// deleting it renumbers every variant after it, and peers on builds that
    /// predate the change still send it to us.
    ProtocolTooNew {
        ours: u16,
    },
    /// Peer key is not in the trust store; a pairing flow must run first.
    NotPaired,
    /// Peer key was explicitly blocked by the user.
    Blocked,
    /// Correct key, but the pairing PIN did not verify.
    BadPairingCode,
    Busy,
    Other(String),
}

/// Clipboard content types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardFormat {
    Utf8Text,
    /// HTML with formatting, for rich paste between editors.
    Html,
    Png,
    /// Newline-separated file paths — the handle used to start a file transfer.
    FileList,
}

/// Compression applied to a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compression {
    None,
    Zstd,
}

/// Video codec for screen streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Vp8,
    Vp9,
    Av1,
}

/// Parameters for a screen stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VideoConfig {
    pub codec: VideoCodec,
    pub max_fps: u16,
    /// Target bitrate in kbit/s.
    pub bitrate_kbps: u32,
    /// Cap on the streamed long edge in pixels; the source downscales to fit.
    pub max_dimension: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            // H.264 is the only codec with hardware decode essentially
            // everywhere, including inside a WebView2/WKWebView surface.
            codec: VideoCodec::H264,
            max_fps: 60,
            bitrate_kbps: 8_000,
            max_dimension: 2560,
        }
    }
}

/// A control-plane message.
///
/// Variants are only ever **appended** — postcard identifies them by index, so
/// inserting one renumbers the rest and silently corrupts older peers' decoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Opens a connection. Always the first message sent.
    Hello {
        /// Sender's [`crate::PROTOCOL_VERSION`]: the *top* of the range it
        /// speaks, not a demand for an exact match. The receiver settles on a
        /// version both ends implement — [`crate::check_version`].
        protocol: u16,
        info: NodeInfo,
        /// Random per-connection value, signed in [`ControlMsg::AuthProof`] to
        /// prove key ownership and to make each handshake unreplayable.
        nonce: [u8; 32],
    },
    /// Accepts a [`ControlMsg::Hello`].
    Welcome {
        protocol: u16,
        info: NodeInfo,
        nonce: [u8; 32],
    },
    /// Signature over the peer's nonce, proving possession of the private key
    /// matching the advertised [`NodeId`].
    AuthProof {
        signature: Signature,
    },
    Reject {
        reason: RejectReason,
    },

    /// Asks an unpaired peer to begin pairing. The user confirms a short code
    /// shown on both machines.
    PairRequest {
        info: NodeInfo,
    },
    /// Proof of knowing the pairing code, binding it to this connection's nonce
    /// so a captured value cannot be reused later.
    PairConfirm {
        code_proof: [u8; 32],
    },
    PairResult {
        accepted: bool,
    },

    /// Displays were added, removed, or rearranged locally.
    MonitorsChanged {
        monitors: Vec<Monitor>,
    },
    /// Authoritative layout push. Any node may edit the layout; the highest
    /// revision wins.
    LayoutUpdate {
        layout: Layout,
    },
    /// Ask a peer for its current layout after reconnecting.
    LayoutRequest,

    /// Cursor is entering this peer. Sent before any input frames.
    TakeControl {
        target: MonitorId,
        /// Where the cursor enters, so it appears at the seam rather than
        /// jumping to the middle of the screen.
        entry: NormPos,
        /// Edge it came in through, used to nudge the cursor clear of the seam
        /// and avoid immediately re-triggering a crossing back.
        via: Edge,
    },
    /// Cursor has left. Peer restores its local pointer and releases held keys.
    YieldControl,

    /// New clipboard content is available, described but not yet sent.
    ///
    /// Offer-then-request rather than pushing eagerly: a 40MB screenshot copied
    /// on one machine should not cross the network until something actually
    /// pastes it.
    ClipboardOffer {
        formats: Vec<ClipboardFormat>,
        /// Increments per clipboard change; identifies which content a later
        /// request refers to.
        serial: u64,
    },
    ClipboardRequest {
        format: ClipboardFormat,
        serial: u64,
    },
    ClipboardData {
        format: ClipboardFormat,
        serial: u64,
        compression: Compression,
        data: Vec<u8>,
    },
    /// The requested serial has been superseded; the content is gone.
    ClipboardStale {
        serial: u64,
    },

    /// Begin a file transfer. Bytes follow on a dedicated stream.
    FileTransferOffer {
        transfer_id: u64,
        /// Entry count and total size, for a progress bar before bytes flow.
        file_count: u32,
        total_bytes: u64,
        /// Root name for the transfer, for display only — never used as a path.
        root_name: String,
    },
    FileTransferAccept {
        transfer_id: u64,
    },
    FileTransferDecline {
        transfer_id: u64,
    },
    FileTransferProgress {
        transfer_id: u64,
        bytes_done: u64,
    },
    FileTransferDone {
        transfer_id: u64,
        error: Option<String>,
    },

    /// Ask a peer to start streaming one of its monitors.
    VideoStart {
        monitor: MonitorId,
        config: VideoConfig,
    },
    /// Renegotiate an active stream, e.g. after the viewer window resizes.
    VideoReconfigure {
        monitor: MonitorId,
        config: VideoConfig,
    },
    VideoStop {
        monitor: MonitorId,
    },
    /// Source could not honour a [`ControlMsg::VideoStart`].
    VideoUnavailable {
        monitor: MonitorId,
        reason: String,
    },

    /// Controlling node's screensaver engaged; lock this session too.
    LockSession,

    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },

    /// Clean shutdown. Lets the peer distinguish an intentional disconnect from
    /// a network failure, and so decide whether to retry.
    Goodbye {
        reason: String,
    },

    /// What this node can do has changed since it said hello.
    ///
    /// [`ControlMsg::Hello`] carries the answer that was true when the connection
    /// opened, and on most platforms that answer never changes. On Wayland it does:
    /// input capture and injection depend on an `xdg-desktop-portal` session the
    /// compositor may revoke at any moment, and a peer that never heard about it
    /// would keep routing the cursor onto a machine that has stopped being able to
    /// accept it.
    ///
    /// Appended to the end of the enum rather than grouped with the other
    /// node-description messages, because the wire format is positional: inserting a
    /// variant anywhere else would renumber every one after it and break peers on an
    /// older build.
    ///
    /// Appending is only half of it. A build that predates this variant cannot
    /// decode it either, and a control stream that fails to decode is torn down —
    /// so this must only ever be sent to a peer whose advertised capabilities
    /// contain [`Capabilities::CAPABILITY_UPDATES`].
    CapabilitiesChanged {
        capabilities: Capabilities,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::PROTOCOL_VERSION;
    use crate::ids::MonitorId;

    fn node_info() -> NodeInfo {
        NodeInfo {
            id: NodeId([1u8; 32]),
            name: "owen-desktop".into(),
            platform: Platform::Windows,
            display_server: DisplayServer::Windows,
            capabilities: Capabilities::CAPTURE_INPUT
                | Capabilities::INJECT_INPUT
                | Capabilities::HAS_DISPLAYS,
            monitors: vec![Monitor {
                id: MonitorId(0),
                name: "DP-1".into(),
                local_bounds: Rect::new(0, 0, 3840, 2160),
                scale: 1.5,
                primary: true,
            }],
            agent_version: "0.1.0".into(),
        }
    }

    fn round_trip(msg: &ControlMsg) {
        let bytes = postcard::to_allocvec(msg).unwrap();
        let back: ControlMsg = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(&back, msg);
    }

    #[test]
    fn handshake_messages_round_trip() {
        round_trip(&ControlMsg::Hello {
            protocol: PROTOCOL_VERSION,
            info: node_info(),
            nonce: [9u8; 32],
        });
        round_trip(&ControlMsg::Welcome {
            protocol: PROTOCOL_VERSION,
            info: node_info(),
            nonce: [3u8; 32],
        });
        round_trip(&ControlMsg::AuthProof {
            signature: Signature([7u8; 64]),
        });
        round_trip(&ControlMsg::Reject {
            reason: RejectReason::NotPaired,
        });
    }

    #[test]
    fn every_reject_reason_round_trips() {
        for reason in [
            RejectReason::ProtocolTooOld { min_supported: 1 },
            RejectReason::ProtocolTooNew { ours: 1 },
            RejectReason::NotPaired,
            RejectReason::Blocked,
            RejectReason::BadPairingCode,
            RejectReason::Busy,
            RejectReason::Other("disk full".into()),
        ] {
            round_trip(&ControlMsg::Reject { reason });
        }
    }

    #[test]
    fn control_handoff_round_trips() {
        round_trip(&ControlMsg::TakeControl {
            target: MonitorId(2),
            entry: NormPos::new(0.0, 0.42),
            via: Edge::Left,
        });
        round_trip(&ControlMsg::YieldControl);
    }

    #[test]
    fn clipboard_flow_round_trips() {
        round_trip(&ControlMsg::ClipboardOffer {
            formats: vec![ClipboardFormat::Utf8Text, ClipboardFormat::Png],
            serial: 12,
        });
        round_trip(&ControlMsg::ClipboardRequest {
            format: ClipboardFormat::Png,
            serial: 12,
        });
        round_trip(&ControlMsg::ClipboardData {
            format: ClipboardFormat::Png,
            serial: 12,
            compression: Compression::Zstd,
            data: vec![0xde, 0xad, 0xbe, 0xef],
        });
        round_trip(&ControlMsg::ClipboardStale { serial: 11 });
    }

    #[test]
    fn layout_round_trips_with_multiple_placements() {
        let layout = Layout {
            revision: 7,
            placements: vec![
                Placement {
                    monitor: GlobalMonitorId::new(NodeId([1u8; 32]), MonitorId(0)),
                    global_bounds: Rect::new(0, 0, 1920, 1080),
                    cursor_scale: 1.0,
                },
                Placement {
                    monitor: GlobalMonitorId::new(NodeId([2u8; 32]), MonitorId(0)),
                    global_bounds: Rect::new(1920, -200, 2560, 1440),
                    cursor_scale: 1.4,
                },
            ],
        };
        round_trip(&ControlMsg::LayoutUpdate { layout });
    }

    #[test]
    fn video_negotiation_round_trips() {
        round_trip(&ControlMsg::VideoStart {
            monitor: MonitorId(0),
            config: VideoConfig::default(),
        });
        round_trip(&ControlMsg::VideoUnavailable {
            monitor: MonitorId(0),
            reason: "no capture permission".into(),
        });
    }

    #[test]
    fn file_transfer_flow_round_trips() {
        round_trip(&ControlMsg::FileTransferOffer {
            transfer_id: 1,
            file_count: 12,
            total_bytes: 4_294_967_296,
            root_name: "photos".into(),
        });
        round_trip(&ControlMsg::FileTransferDone {
            transfer_id: 1,
            error: None,
        });
        round_trip(&ControlMsg::FileTransferDone {
            transfer_id: 1,
            error: Some("permission denied".into()),
        });
    }

    #[test]
    fn a_capability_change_round_trips() {
        round_trip(&ControlMsg::CapabilitiesChanged {
            capabilities: Capabilities::CAPTURE_INPUT | Capabilities::INJECT_INPUT,
        });
        // A Wayland node whose portal session was revoked: it keeps its displays and
        // loses everything that needed permission.
        round_trip(&ControlMsg::CapabilitiesChanged {
            capabilities: Capabilities::HAS_DISPLAYS,
        });
    }

    #[test]
    fn appending_a_variant_did_not_renumber_the_existing_ones() {
        // The wire format is positional, so a variant added anywhere but the end
        // silently changes what every later one decodes as. This pins the tail.
        let goodbye = postcard::to_allocvec(&ControlMsg::Goodbye {
            reason: "bye".into(),
        })
        .unwrap();
        let changed = postcard::to_allocvec(&ControlMsg::CapabilitiesChanged {
            capabilities: Capabilities::NONE,
        })
        .unwrap();
        assert_eq!(
            changed[0],
            goodbye[0] + 1,
            "CapabilitiesChanged must sit immediately after Goodbye"
        );
    }

    #[test]
    fn default_video_config_prefers_universally_decodable_codec() {
        // The Tauri webview must be able to decode this without a bundled codec.
        assert_eq!(VideoConfig::default().codec, VideoCodec::H264);
    }

    #[test]
    fn total_bytes_exceeds_u32_range() {
        // Guards against a u32 regression: single files well over 4GiB are
        // ordinary for disk images and video.
        round_trip(&ControlMsg::FileTransferOffer {
            transfer_id: 9,
            file_count: 1,
            total_bytes: u64::MAX,
            root_name: "big.iso".into(),
        });
    }
}
