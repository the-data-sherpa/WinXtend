//! Optional screen streaming.
//!
//! Input sharing alone cannot drive a machine with no monitor attached. This
//! crate closes that gap: a node advertising
//! [`Capabilities::VIDEO_SOURCE`](wx_proto::Capabilities::VIDEO_SOURCE) can
//! stream a screen to the UI, which is what makes a headless mini-PC usable
//! rather than merely reachable.
//!
//! Strictly optional — a mesh of machines that each have their own monitor never
//! starts a stream, and pays nothing for this crate being present.
//!
//! # Honest state of this crate
//!
//! Deliberately modest, because a half-working encoder is worse than a clean
//! seam: it produces a stream that decodes on the author's machine and shows
//! green blocks everywhere else, and nobody can tell whether the bug is in the
//! capture, the encoder, or the transport.
//!
//! | Piece | State |
//! |---|---|
//! | Capture, Windows | Real, via GDI `BitBlt`. Good for 15-30 fps on 4K. |
//! | Capture, macOS/Linux | Stubs that fail cleanly, with the requirements documented. |
//! | Encode | Lossless passthrough only (raw BGRA, optionally zstd). |
//! | Real codecs | Not implemented. [`Encoder`] is the seam; [`encode`] says what a backend must do. |
//! | Pipeline | Real, and the frame-dropping policy is the part that matters. |
//!
//! Passthrough is a LAN-only path — see [`encode`] for the bandwidth arithmetic.
//! It exists so the whole chain is end-to-end functional and testable now, and so
//! that adding a real encoder later is one trait implementation rather than a
//! redesign.
//!
//! # Shape
//!
//! ```text
//!   ScreenCapture ──frames──> FrameSlot ──> Encoder ──packets──> PacketSink
//!   (paced by FramePacer)     (one frame,             (a QUIC stream, or a test)
//!                              newest wins)
//! ```
//!
//! The single-frame slot is the load-bearing design decision. A 4K BGRA frame is
//! 33MB, so any queue at all turns a slow link into unbounded memory growth. A
//! backlogged stream here loses frame rate instead, which is what a viewer
//! expects and what keeps the agent alive.

pub mod capture;
pub mod encode;
pub mod pipeline;

#[cfg(target_os = "windows")]
mod capture_windows;

#[cfg(target_os = "macos")]
mod capture_macos;

#[cfg(target_os = "linux")]
mod capture_linux;

pub use capture::{
    open as open_capture, CaptureConfig, CaptureError, CaptureTarget, FrameError, RawFrame,
    ScreenCapture, StaticCapture, BYTES_PER_PIXEL, MAX_FPS, MAX_FRAME_PIXELS,
};
pub use encode::{
    encoder_for, negotiate, preferred_codec, supported_codecs, validate_config, DecodeError,
    Decoder, EncodeError, EncodedPacket, Encoder, Fallback, NegotiateError, Negotiated,
    PacketHeader, PassthroughDecoder, PassthroughEncoder, PayloadFormat, StreamFormat,
    CODEC_PREFERENCE, HEADER_LEN,
};
pub use pipeline::{FramePacer, FrameSlot, PacketSink, PipelineStats, VideoPipeline};

/// Capabilities this build can honestly advertise.
///
/// A node must not claim [`VIDEO_SOURCE`](wx_proto::Capabilities::VIDEO_SOURCE)
/// on a platform where capture is a stub: the peer would offer a "view screen"
/// button that always fails, which is worse than not offering it. So this is
/// computed from what is actually compiled in rather than hardcoded.
///
/// `VIDEO_SINK` is unconditional — decoding passthrough packets needs no platform
/// support at all.
pub fn video_capabilities() -> wx_proto::Capabilities {
    let sink = wx_proto::Capabilities::VIDEO_SINK;
    if cfg!(target_os = "windows") {
        sink.union(wx_proto::Capabilities::VIDEO_SOURCE)
    } else {
        sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wx_proto::{Capabilities, Compression, VideoConfig};

    #[test]
    fn capture_capability_is_only_claimed_where_a_backend_exists() {
        let caps = video_capabilities();
        assert!(caps.contains(Capabilities::VIDEO_SINK));
        assert_eq!(
            caps.contains(Capabilities::VIDEO_SOURCE),
            cfg!(target_os = "windows")
        );
    }

    #[test]
    fn a_negotiated_stream_encodes_and_decodes_through_the_public_api() {
        // The seam test: everything a caller needs is re-exported, and the
        // negotiate -> encode -> decode path holds together.
        let agreed = negotiate(
            &VideoConfig::default(),
            supported_codecs(),
            Fallback::Passthrough(Compression::None),
        )
        .unwrap();
        let mut encoder = encoder_for(agreed).unwrap();

        let frame = RawFrame::packed(4, 2, (0..32).collect(), Duration::from_millis(9)).unwrap();
        let packet = encoder.encode(&frame).unwrap().unwrap();
        let decoded = PassthroughDecoder::new()
            .decode_packet(&packet.data)
            .unwrap();
        assert_eq!(decoded.to_packed_bytes(), frame.to_packed_bytes());
        assert_eq!(decoded.timestamp(), Duration::from_millis(9));
    }
}
