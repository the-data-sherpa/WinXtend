//! Encoding, decoding, and codec negotiation.
//!
//! # What is actually implemented
//!
//! One encoder: **lossless passthrough**, which packs the BGRA rows and
//! optionally zstd-compresses them. That is enough for the whole pipeline —
//! capture, pace, encode, send, decode, display — to be real and testable end to
//! end, which is the point. It is not enough to be a shipping feature:
//! 1920x1080 at 30 fps is 249 MB/s raw, and zstd on desktop content typically
//! gets 2-4x, so call it 60-120 MB/s. On anything short of wired gigabit the
//! QUIC send buffer fills, the pipeline starts dropping (see
//! [`crate::pipeline`]), and the picture updates about once a second. Passthrough
//! is for a LAN, a small window, or a bug hunt.
//!
//! # Why no H.264 here
//!
//! A real encoder is a large, platform-specific, hardware-dependent thing, and a
//! half-working one is worse than an honest seam: it produces a stream that
//! decodes on the developer's machine and shows green blocks on everyone else's.
//! [`Encoder`] is the seam. A real backend behind it must:
//!
//! * Emit an **IDR keyframe as its first packet**, and another whenever
//!   [`Encoder::request_keyframe`] is called. A viewer that connects mid-stream
//!   has no reference frame; without an on-demand keyframe it shows garbage until
//!   the next scheduled one, which with a long GOP can be seconds.
//! * Set `keyframe` on [`EncodedPacket`] truthfully. The transport uses it to
//!   decide what it may drop: a dropped P-frame is a glitch, a dropped keyframe
//!   is a frozen picture until the next one.
//! * Honour `VideoConfig::bitrate_kbps` with an actual rate controller, and
//!   `VideoConfig::max_dimension` by scaling before encode. The passthrough
//!   encoder honours neither, and says so in
//!   [`Negotiated::Passthrough`]'s documentation.
//! * Handle `VideoReconfigure` without tearing the stream down, and emit a
//!   keyframe after any resolution change, since the decoder must be reset.
//! * Survive encoder-session loss. Hardware encoders (NVENC, AMF, Quick Sync,
//!   VideoToolbox) lose their session on GPU driver reset, display topology
//!   changes, and user switches. A backend that treats that as fatal works
//!   perfectly until the first driver update.
//! * Never block longer than a frame interval. The pipeline drops frames rather
//!   than queueing them, so a slow encode costs frame rate, but an encode that
//!   blocks for a second stalls capture pacing too.
//!
//! # Negotiation
//!
//! [`wx_proto::VideoCodec`] has no raw/lossless variant, and protocol enums are
//! append-only, so passthrough cannot currently be named on the control plane. It
//! is therefore an explicit local fallback ([`Fallback::Passthrough`]) that both
//! ends must be configured to allow, not something advertised. When a real codec
//! lands, `VideoCodec` gains an appended variant and passthrough stops being a
//! special case.

use core::fmt;
use std::time::Duration;

use wx_proto::{Compression, VideoCodec, VideoConfig};

use crate::capture::{FrameError, RawFrame, BYTES_PER_PIXEL, MAX_FRAME_PIXELS};

/// Magic prefix on every passthrough packet.
///
/// Present so a decoder handed a packet from a different stream, a truncated
/// datagram, or a future format fails immediately and loudly instead of
/// interpreting whatever bytes it got as geometry and allocating from them.
pub const MAGIC: [u8; 4] = *b"WXV1";

/// Size of the passthrough packet header.
pub const HEADER_LEN: usize = 24;

/// Payload encoding of a passthrough packet.
///
/// This is a wire value like everything in `wx-proto`: the numbers are fixed and
/// **append-only**. Renumbering would make an old viewer decode a new stream's
/// zstd payload as raw pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadFormat {
    /// Packed BGRA8, top-down, no row padding.
    RawBgra,
    /// The same bytes, zstd-compressed.
    ZstdBgra,
}

impl PayloadFormat {
    const RAW_BGRA: u8 = 0;
    const ZSTD_BGRA: u8 = 1;

    pub const fn to_u8(self) -> u8 {
        match self {
            PayloadFormat::RawBgra => Self::RAW_BGRA,
            PayloadFormat::ZstdBgra => Self::ZSTD_BGRA,
        }
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            Self::RAW_BGRA => Some(PayloadFormat::RawBgra),
            Self::ZSTD_BGRA => Some(PayloadFormat::ZstdBgra),
            _ => None,
        }
    }

    pub const fn compression(self) -> Compression {
        match self {
            PayloadFormat::RawBgra => Compression::None,
            PayloadFormat::ZstdBgra => Compression::Zstd,
        }
    }

    pub const fn for_compression(c: Compression) -> Self {
        match c {
            Compression::None => PayloadFormat::RawBgra,
            Compression::Zstd => PayloadFormat::ZstdBgra,
        }
    }
}

/// Self-describing header on a passthrough packet.
///
/// Self-describing because the control plane's [`VideoConfig`] describes what was
/// *requested*, not what a given frame actually is. A monitor resolution change
/// mid-stream means one packet's dimensions differ from the next, and a decoder
/// that trusted the negotiated size would tear the picture apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub format: PayloadFormat,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub timestamp: Duration,
}

impl PacketHeader {
    const FLAG_KEYFRAME: u8 = 1 << 0;

    pub fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC);
        out.push(self.format.to_u8());
        out.push(if self.keyframe {
            Self::FLAG_KEYFRAME
        } else {
            0
        });
        // Reserved: written as zero, ignored on read, so one more small field
        // can be added later without a format change.
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        // Microseconds: a u64 covers 584,000 years, and saturating means a
        // nonsense Duration cannot wrap the timeline back to zero.
        let micros = u64::try_from(self.timestamp.as_micros()).unwrap_or(u64::MAX);
        out.extend_from_slice(&micros.to_le_bytes());
    }

    /// Parse a header from the front of a packet.
    ///
    /// Validates geometry here rather than at pixel-copy time: everything after
    /// this point sizes buffers from these numbers.
    pub fn parse(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() < HEADER_LEN {
            return Err(DecodeError::Truncated {
                got: bytes.len(),
                need: HEADER_LEN,
            });
        }
        if bytes[..4] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let format =
            PayloadFormat::from_u8(bytes[4]).ok_or(DecodeError::UnknownFormat(bytes[4]))?;
        let keyframe = bytes[5] & Self::FLAG_KEYFRAME != 0;
        let width = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let height = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let micros = u64::from_le_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        ]);

        if width == 0 || height == 0 {
            return Err(DecodeError::Geometry(FrameError::Empty { width, height }));
        }
        if u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS {
            return Err(DecodeError::Geometry(FrameError::TooLarge {
                width,
                height,
                max: MAX_FRAME_PIXELS,
            }));
        }

        Ok(Self {
            format,
            keyframe,
            width,
            height,
            timestamp: Duration::from_micros(micros),
        })
    }

    /// Bytes the pixels occupy once decoded, padding-free.
    fn packed_len(&self) -> usize {
        self.width as usize * BYTES_PER_PIXEL * self.height as usize
    }
}

/// One encoded frame, ready for the wire.
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedPacket {
    pub width: u32,
    pub height: u32,
    /// Monotonic offset from the start of the capture session, carried through
    /// from [`RawFrame::timestamp`] rather than re-read after encoding, so
    /// encode latency does not distort the timeline.
    pub timestamp: Duration,
    /// Whether this packet decodes without reference to any earlier one.
    pub keyframe: bool,
    pub data: Vec<u8>,
}

impl fmt::Debug for EncodedPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedPacket")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("timestamp", &self.timestamp)
            .field("keyframe", &self.keyframe)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// What a stream is actually carrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFormat {
    /// Whole frames, losslessly, no inter-frame prediction. Not representable as
    /// a [`VideoCodec`].
    Passthrough(Compression),
    /// A real codec's bitstream.
    Codec(VideoCodec),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("frame geometry: {0}")]
    Geometry(#[from] FrameError),
    /// Asked for a compression this build does not contain. A feature-gated
    /// dependency must fail loudly, not silently send uncompressed data the peer
    /// will try to inflate.
    #[error("{0:?} compression is not compiled into this build")]
    CompressionUnavailable(Compression),
    #[error("no encoder implementation for {0:?}")]
    NoImplementation(VideoCodec),
    #[error("encoder failed: {detail}")]
    Backend { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("packet of {got} bytes is shorter than the {need} bytes needed")]
    Truncated { got: usize, need: usize },
    #[error("packet does not start with the expected magic")]
    BadMagic,
    #[error("unknown payload format {0}")]
    UnknownFormat(u8),
    #[error("payload is {got} bytes but the geometry needs {expected}")]
    PayloadSize { got: usize, expected: usize },
    #[error("frame geometry: {0}")]
    Geometry(#[from] FrameError),
    #[error("{0:?} decompression is not compiled into this build")]
    CompressionUnavailable(Compression),
    #[error("decompression failed: {detail}")]
    Decompress { detail: String },
}

/// Turns raw frames into packets.
pub trait Encoder: Send {
    fn format(&self) -> StreamFormat;

    /// Encode one frame.
    ///
    /// `Ok(None)` is legal: a real codec may buffer a frame internally and emit
    /// nothing for it (B-frame reordering, lookahead). Callers must not treat it
    /// as an error or as end of stream.
    fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedPacket>, EncodeError>;

    /// Ask for the next packet to be independently decodable.
    ///
    /// Called when a viewer joins or reports corruption. The default is a no-op
    /// because every passthrough packet is already a keyframe; a predictive codec
    /// must override it.
    fn request_keyframe(&mut self) {}

    /// Apply a renegotiated configuration in place.
    ///
    /// Default accepts anything, because passthrough has no rate control and no
    /// scaler to reconfigure. A real codec must reset its rate controller and, if
    /// the resolution changed, emit a keyframe — a decoder cannot follow a
    /// resolution change mid-GOP.
    fn reconfigure(&mut self, _config: &VideoConfig) -> Result<(), EncodeError> {
        Ok(())
    }
}

/// Turns packets back into frames.
pub trait Decoder: Send {
    fn format(&self) -> StreamFormat;

    /// `Ok(None)` for a packet that yields no displayable frame yet.
    fn decode(&mut self, packet: &[u8]) -> Result<Option<RawFrame>, DecodeError>;
}

/// Lossless encoder: packs rows, optionally compresses, prepends a header.
#[derive(Debug, Clone)]
pub struct PassthroughEncoder {
    compression: Compression,
    level: i32,
}

impl PassthroughEncoder {
    /// zstd level 1, not the library default of 3.
    ///
    /// At 30 fps the budget per frame is 33ms for capture, compress, and send
    /// combined. Level 1 gets most of the ratio for a fraction of the time;
    /// higher levels lower the frame rate to save bandwidth that a LAN has
    /// anyway.
    pub const DEFAULT_LEVEL: i32 = 1;

    pub fn new(compression: Compression) -> Result<Self, EncodeError> {
        match compression {
            Compression::None => {}
            Compression::Zstd => {
                if !zstd_available() {
                    return Err(EncodeError::CompressionUnavailable(compression));
                }
            }
        }
        Ok(Self {
            compression,
            level: Self::DEFAULT_LEVEL,
        })
    }

    pub fn raw() -> Self {
        Self {
            compression: Compression::None,
            level: Self::DEFAULT_LEVEL,
        }
    }

    pub fn with_level(mut self, level: i32) -> Self {
        self.level = level;
        self
    }
}

impl Encoder for PassthroughEncoder {
    fn format(&self) -> StreamFormat {
        StreamFormat::Passthrough(self.compression)
    }

    fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedPacket>, EncodeError> {
        let packed = frame.to_packed_bytes();
        let payload = match self.compression {
            Compression::None => packed,
            Compression::Zstd => compress_zstd(&packed, self.level)?,
        };

        let header = PacketHeader {
            format: PayloadFormat::for_compression(self.compression),
            // Every packet stands alone: there is no reference frame to lose, so
            // a dropped packet costs exactly one frame and nothing after it.
            keyframe: true,
            width: frame.width(),
            height: frame.height(),
            timestamp: frame.timestamp(),
        };

        let mut data = Vec::with_capacity(HEADER_LEN + payload.len());
        header.write_to(&mut data);
        data.extend_from_slice(&payload);

        Ok(Some(EncodedPacket {
            width: frame.width(),
            height: frame.height(),
            timestamp: frame.timestamp(),
            keyframe: true,
            data,
        }))
    }
}

/// Reads packets produced by [`PassthroughEncoder`].
#[derive(Debug, Clone, Default)]
pub struct PassthroughDecoder;

impl PassthroughDecoder {
    pub fn new() -> Self {
        Self
    }

    /// Decode one packet into a frame.
    ///
    /// Every allocation is sized from the header's geometry, which
    /// [`PacketHeader::parse`] has already bounded, so a malicious packet cannot
    /// name a size it does not then have to supply.
    pub fn decode_packet(&self, packet: &[u8]) -> Result<RawFrame, DecodeError> {
        let header = PacketHeader::parse(packet)?;
        let payload = &packet[HEADER_LEN..];
        let expected = header.packed_len();

        let pixels = match header.format {
            PayloadFormat::RawBgra => {
                if payload.len() != expected {
                    return Err(DecodeError::PayloadSize {
                        got: payload.len(),
                        expected,
                    });
                }
                payload.to_vec()
            }
            PayloadFormat::ZstdBgra => {
                let out = decompress_zstd(payload, expected)?;
                if out.len() != expected {
                    // A compression bomb, or a corrupted stream. Either way the
                    // geometry and the payload disagree and the frame is a lie.
                    return Err(DecodeError::PayloadSize {
                        got: out.len(),
                        expected,
                    });
                }
                out
            }
        };

        Ok(RawFrame::packed(
            header.width,
            header.height,
            pixels,
            header.timestamp,
        )?)
    }
}

impl Decoder for PassthroughDecoder {
    fn format(&self) -> StreamFormat {
        // The packet header, not the negotiated config, decides per packet; this
        // is the nominal format only.
        StreamFormat::Passthrough(Compression::None)
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Option<RawFrame>, DecodeError> {
        self.decode_packet(packet).map(Some)
    }
}

/// Whether this build can do zstd at all.
pub const fn zstd_available() -> bool {
    cfg!(feature = "zstd")
}

#[cfg(feature = "zstd")]
fn compress_zstd(data: &[u8], level: i32) -> Result<Vec<u8>, EncodeError> {
    zstd::bulk::compress(data, level).map_err(|e| EncodeError::Backend {
        detail: format!("zstd compress: {e}"),
    })
}

#[cfg(not(feature = "zstd"))]
fn compress_zstd(_data: &[u8], _level: i32) -> Result<Vec<u8>, EncodeError> {
    Err(EncodeError::CompressionUnavailable(Compression::Zstd))
}

#[cfg(feature = "zstd")]
fn decompress_zstd(data: &[u8], capacity: usize) -> Result<Vec<u8>, DecodeError> {
    // `capacity` is a hard ceiling, not a hint: it comes from validated geometry,
    // so a payload that claims to inflate to more than the frame needs fails here
    // instead of exhausting memory.
    zstd::bulk::decompress(data, capacity).map_err(|e| DecodeError::Decompress {
        detail: format!("{e}"),
    })
}

#[cfg(not(feature = "zstd"))]
fn decompress_zstd(_data: &[u8], _capacity: usize) -> Result<Vec<u8>, DecodeError> {
    Err(DecodeError::CompressionUnavailable(Compression::Zstd))
}

/// Preference order when several codecs are mutually supported.
///
/// H.264 first because it is the only codec with hardware decode essentially
/// everywhere, including inside the WebView the viewer runs in — matching
/// [`VideoConfig::default`]. AV1 next for efficiency where both ends are modern,
/// then VP9, then VP8 as the floor.
pub const CODEC_PREFERENCE: [VideoCodec; 4] = [
    VideoCodec::H264,
    VideoCodec::Av1,
    VideoCodec::Vp9,
    VideoCodec::Vp8,
];

/// Codecs this build can encode.
///
/// Empty on purpose: passthrough is not a [`VideoCodec`]. When a real encoder
/// lands it is added here, and negotiation starts succeeding without any other
/// change.
pub fn supported_codecs() -> &'static [VideoCodec] {
    &[]
}

/// Pick the codec two nodes should use.
///
/// Order-independent by construction: the answer comes from
/// [`CODEC_PREFERENCE`], never from the order either side happened to list its
/// codecs in. If it depended on argument order, the two ends could each pick a
/// different "best" codec from the same pair of lists and the stream would never
/// start — a failure that only shows up between mismatched builds.
pub fn preferred_codec(source: &[VideoCodec], sink: &[VideoCodec]) -> Option<VideoCodec> {
    CODEC_PREFERENCE
        .iter()
        .copied()
        .find(|c| source.contains(c) && sink.contains(c))
}

/// What to do when the requested codec cannot be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Send lossless frames instead. Both ends must be configured for it, since
    /// it cannot be advertised on the control plane.
    Passthrough(Compression),
    /// Answer `ControlMsg::VideoUnavailable` and stream nothing.
    Refuse,
}

/// The agreed stream format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiated {
    Codec(VideoCodec),
    /// Lossless frames. Note what this silently does *not* honour:
    /// `bitrate_kbps` (there is no rate control) and `max_dimension` (there is
    /// no scaler). A viewer that asked for 2 Mbit/s at 720p gets whatever the
    /// full-resolution frame costs, and the pipeline's frame dropping is the only
    /// thing keeping the link alive.
    Passthrough(Compression),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NegotiateError {
    #[error("no encoder for {requested:?} and fallback is disallowed")]
    NoCommonCodec { requested: VideoCodec },
    /// Rejected because a zero frame rate becomes a division by zero in every
    /// pacing calculation downstream.
    #[error("max_fps must be at least 1")]
    ZeroFps,
    #[error("max_dimension must be at least 1")]
    ZeroDimension,
    #[error("{0:?} compression is not compiled into this build")]
    CompressionUnavailable(Compression),
}

/// Reject configurations that would break arithmetic downstream.
pub fn validate_config(config: &VideoConfig) -> Result<(), NegotiateError> {
    if config.max_fps == 0 {
        return Err(NegotiateError::ZeroFps);
    }
    if config.max_dimension == 0 {
        return Err(NegotiateError::ZeroDimension);
    }
    Ok(())
}

/// Decide how to answer a `ControlMsg::VideoStart`.
///
/// The sink has already chosen a codec by the time it asks, so this is the
/// source's side of the question: can I produce that, and if not, what then.
pub fn negotiate(
    config: &VideoConfig,
    source_supports: &[VideoCodec],
    fallback: Fallback,
) -> Result<Negotiated, NegotiateError> {
    validate_config(config)?;

    if source_supports.contains(&config.codec) {
        return Ok(Negotiated::Codec(config.codec));
    }

    match fallback {
        Fallback::Refuse => Err(NegotiateError::NoCommonCodec {
            requested: config.codec,
        }),
        Fallback::Passthrough(compression) => {
            if compression == Compression::Zstd && !zstd_available() {
                return Err(NegotiateError::CompressionUnavailable(compression));
            }
            Ok(Negotiated::Passthrough(compression))
        }
    }
}

/// Build the encoder for a negotiated format.
pub fn encoder_for(negotiated: Negotiated) -> Result<Box<dyn Encoder>, EncodeError> {
    match negotiated {
        Negotiated::Passthrough(compression) => Ok(Box::new(PassthroughEncoder::new(compression)?)),
        // Unreachable via `negotiate` while `supported_codecs()` is empty, but
        // stated rather than panicked so adding a codec to that list without an
        // implementation is a clean error instead of a crash.
        Negotiated::Codec(codec) => Err(EncodeError::NoImplementation(codec)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_frame(width: u32, height: u32, stride: usize) -> RawFrame {
        let mut buf = vec![0xAAu8; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize * BYTES_PER_PIXEL {
                buf[y * stride + x] = (x as u32 * 31 + y as u32 * 17) as u8;
            }
        }
        RawFrame::new(width, height, stride, buf, Duration::from_micros(4321)).unwrap()
    }

    #[test]
    fn raw_round_trip_reproduces_the_frame_exactly() {
        let frame = gradient_frame(17, 9, 17 * 4);
        let packet = PassthroughEncoder::raw().encode(&frame).unwrap().unwrap();
        let back = PassthroughDecoder::new()
            .decode_packet(&packet.data)
            .unwrap();
        assert_eq!(back.width(), frame.width());
        assert_eq!(back.height(), frame.height());
        assert_eq!(back.to_packed_bytes(), frame.to_packed_bytes());
        assert_eq!(back.timestamp(), frame.timestamp());
    }

    #[test]
    fn round_trip_drops_stride_padding_and_the_picture_still_lines_up() {
        // The failure this guards against is padding surviving into the payload:
        // the receiver then reads each row four bytes late and the image shears.
        let frame = gradient_frame(13, 7, 128);
        let packet = PassthroughEncoder::raw().encode(&frame).unwrap().unwrap();
        assert_eq!(packet.data.len(), HEADER_LEN + 13 * 4 * 7);

        let back = PassthroughDecoder::new()
            .decode_packet(&packet.data)
            .unwrap();
        assert!(back.is_packed());
        for y in 0..7 {
            assert_eq!(back.row(y).unwrap(), frame.row(y).unwrap(), "row {y}");
        }
    }

    #[test]
    fn single_pixel_frame_round_trips() {
        let frame = RawFrame::packed(1, 1, vec![1, 2, 3, 4], Duration::ZERO).unwrap();
        let packet = PassthroughEncoder::raw().encode(&frame).unwrap().unwrap();
        let back = PassthroughDecoder::new()
            .decode_packet(&packet.data)
            .unwrap();
        assert_eq!(back.as_bytes(), &[1, 2, 3, 4]);
    }

    #[test]
    fn every_passthrough_packet_is_independently_decodable() {
        // Nothing downstream may assume a reference frame exists, and the
        // transport uses this flag to decide what it can drop.
        let frame = gradient_frame(4, 4, 16);
        let mut enc = PassthroughEncoder::raw();
        for _ in 0..3 {
            let packet = enc.encode(&frame).unwrap().unwrap();
            assert!(packet.keyframe);
            assert!(PacketHeader::parse(&packet.data).unwrap().keyframe);
        }
    }

    #[test]
    fn header_round_trips_including_extreme_values() {
        let header = PacketHeader {
            format: PayloadFormat::ZstdBgra,
            keyframe: false,
            width: 7680,
            height: 4320,
            timestamp: Duration::from_micros(u64::MAX),
        };
        let mut bytes = Vec::new();
        header.write_to(&mut bytes);
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(PacketHeader::parse(&bytes).unwrap(), header);
    }

    #[test]
    fn header_reserved_bytes_are_ignored_on_read() {
        // Forward compatibility: a newer sender using the reserved field must not
        // make this build reject the packet outright.
        let header = PacketHeader {
            format: PayloadFormat::RawBgra,
            keyframe: true,
            width: 2,
            height: 2,
            timestamp: Duration::ZERO,
        };
        let mut bytes = Vec::new();
        header.write_to(&mut bytes);
        bytes[6] = 0xFF;
        bytes[7] = 0xFF;
        assert_eq!(PacketHeader::parse(&bytes).unwrap(), header);
    }

    #[test]
    fn truncated_packet_is_rejected_at_every_length() {
        let frame = gradient_frame(5, 5, 20);
        let packet = PassthroughEncoder::raw().encode(&frame).unwrap().unwrap();
        let dec = PassthroughDecoder::new();
        for len in 0..packet.data.len() {
            let err = dec.decode_packet(&packet.data[..len]).unwrap_err();
            assert!(
                matches!(
                    err,
                    DecodeError::Truncated { .. } | DecodeError::PayloadSize { .. }
                ),
                "length {len} gave {err:?}"
            );
        }
        assert!(dec.decode_packet(&packet.data).is_ok());
    }

    #[test]
    fn packet_with_trailing_garbage_is_rejected() {
        // A length mismatch means the sender and receiver disagree about the
        // frame; guessing which is right would show a corrupt picture.
        let frame = gradient_frame(3, 3, 12);
        let mut packet = PassthroughEncoder::raw().encode(&frame).unwrap().unwrap();
        packet.data.push(0);
        let err = PassthroughDecoder::new()
            .decode_packet(&packet.data)
            .unwrap_err();
        assert!(matches!(err, DecodeError::PayloadSize { .. }), "{err:?}");
    }

    #[test]
    fn foreign_bytes_are_rejected_by_magic() {
        let junk = vec![0u8; 4096];
        let err = PassthroughDecoder::new().decode_packet(&junk).unwrap_err();
        assert!(matches!(err, DecodeError::BadMagic), "{err:?}");
    }

    #[test]
    fn unknown_payload_format_is_rejected_rather_than_guessed() {
        let mut bytes = Vec::new();
        PacketHeader {
            format: PayloadFormat::RawBgra,
            keyframe: true,
            width: 1,
            height: 1,
            timestamp: Duration::ZERO,
        }
        .write_to(&mut bytes);
        bytes[4] = 200;
        bytes.extend_from_slice(&[0u8; 4]);
        let err = PassthroughDecoder::new().decode_packet(&bytes).unwrap_err();
        assert!(matches!(err, DecodeError::UnknownFormat(200)), "{err:?}");
    }

    #[test]
    fn hostile_header_geometry_is_refused_before_allocating() {
        // 4 billion by 4 billion pixels, with four bytes of payload. Sizing an
        // allocation from this is how a decoder becomes a denial of service.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(PayloadFormat::RawBgra.to_u8());
        bytes.push(1);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        let err = PassthroughDecoder::new().decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Geometry(FrameError::TooLarge { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn zero_dimension_header_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(PayloadFormat::RawBgra.to_u8());
        bytes.push(1);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&1920u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        let err = PassthroughDecoder::new().decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(err, DecodeError::Geometry(FrameError::Empty { .. })),
            "{err:?}"
        );
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_round_trip_is_lossless() {
        let frame = gradient_frame(64, 32, 64 * 4 + 16);
        let mut enc = PassthroughEncoder::new(Compression::Zstd).unwrap();
        let packet = enc.encode(&frame).unwrap().unwrap();
        assert_eq!(
            PacketHeader::parse(&packet.data).unwrap().format,
            PayloadFormat::ZstdBgra
        );
        let back = PassthroughDecoder::new()
            .decode_packet(&packet.data)
            .unwrap();
        assert_eq!(back.to_packed_bytes(), frame.to_packed_bytes());
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_beats_raw_on_flat_content() {
        // A blank desktop is the case that makes passthrough usable at all; if
        // compression did not help here it would not be worth having.
        let frame = RawFrame::packed(256, 256, vec![0u8; 256 * 256 * 4], Duration::ZERO).unwrap();
        let compressed = PassthroughEncoder::new(Compression::Zstd)
            .unwrap()
            .encode(&frame)
            .unwrap()
            .unwrap();
        let raw = PassthroughEncoder::raw().encode(&frame).unwrap().unwrap();
        assert!(
            compressed.data.len() * 10 < raw.data.len(),
            "compressed {} vs raw {}",
            compressed.data.len(),
            raw.data.len()
        );
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn corrupt_compressed_payload_errors_instead_of_panicking() {
        let frame = gradient_frame(8, 8, 32);
        let mut packet = PassthroughEncoder::new(Compression::Zstd)
            .unwrap()
            .encode(&frame)
            .unwrap()
            .unwrap();
        for b in packet.data[HEADER_LEN..].iter_mut() {
            *b ^= 0xFF;
        }
        let err = PassthroughDecoder::new()
            .decode_packet(&packet.data)
            .unwrap_err();
        assert!(
            matches!(
                err,
                DecodeError::Decompress { .. } | DecodeError::PayloadSize { .. }
            ),
            "{err:?}"
        );
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn compression_bomb_cannot_exceed_the_declared_frame_size() {
        // Header says 4x4 (64 bytes); payload inflates to a megabyte. The decoder
        // must stop at the declared size rather than trust the payload.
        let bomb = zstd::bulk::compress(&vec![0u8; 1024 * 1024], 1).unwrap();
        let mut bytes = Vec::new();
        PacketHeader {
            format: PayloadFormat::ZstdBgra,
            keyframe: true,
            width: 4,
            height: 4,
            timestamp: Duration::ZERO,
        }
        .write_to(&mut bytes);
        bytes.extend_from_slice(&bomb);
        let err = PassthroughDecoder::new().decode_packet(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                DecodeError::Decompress { .. } | DecodeError::PayloadSize { .. }
            ),
            "{err:?}"
        );
    }

    #[cfg(not(feature = "zstd"))]
    #[test]
    fn asking_for_zstd_without_the_feature_fails_loudly() {
        let err = PassthroughEncoder::new(Compression::Zstd).unwrap_err();
        assert!(
            matches!(err, EncodeError::CompressionUnavailable(Compression::Zstd)),
            "{err:?}"
        );
    }

    #[test]
    fn negotiation_is_independent_of_the_order_codecs_are_listed() {
        let source = [VideoCodec::Vp8, VideoCodec::H264, VideoCodec::Av1];
        let sink = [VideoCodec::Av1, VideoCodec::H264, VideoCodec::Vp8];
        let reversed_source = [VideoCodec::Av1, VideoCodec::H264, VideoCodec::Vp8];
        let reversed_sink = [VideoCodec::Vp8, VideoCodec::H264, VideoCodec::Av1];
        assert_eq!(preferred_codec(&source, &sink), Some(VideoCodec::H264));
        assert_eq!(
            preferred_codec(&reversed_source, &reversed_sink),
            Some(VideoCodec::H264)
        );
        // Symmetric too: both ends running this function get the same answer.
        assert_eq!(
            preferred_codec(&sink, &source),
            preferred_codec(&source, &sink)
        );
    }

    #[test]
    fn negotiation_picks_the_best_shared_codec_not_merely_a_shared_one() {
        assert_eq!(
            preferred_codec(
                &[VideoCodec::Vp8, VideoCodec::Vp9, VideoCodec::Av1],
                &[VideoCodec::Vp8, VideoCodec::Av1]
            ),
            Some(VideoCodec::Av1)
        );
    }

    #[test]
    fn no_shared_codec_yields_no_choice() {
        assert_eq!(
            preferred_codec(&[VideoCodec::Vp8], &[VideoCodec::Av1]),
            None
        );
        assert_eq!(preferred_codec(&[], &[VideoCodec::H264]), None);
        assert_eq!(preferred_codec(&[], &[]), None);
    }

    #[test]
    fn this_build_falls_back_to_passthrough_for_the_default_config() {
        // Honest statement of where the crate stands: no real codec exists yet,
        // so the default H.264 request can only be served losslessly.
        let got = negotiate(
            &VideoConfig::default(),
            supported_codecs(),
            Fallback::Passthrough(Compression::None),
        )
        .unwrap();
        assert_eq!(got, Negotiated::Passthrough(Compression::None));
    }

    #[test]
    fn refusing_fallback_reports_the_codec_that_was_asked_for() {
        let err = negotiate(
            &VideoConfig::default(),
            supported_codecs(),
            Fallback::Refuse,
        )
        .unwrap_err();
        assert_eq!(
            err,
            NegotiateError::NoCommonCodec {
                requested: VideoCodec::H264
            }
        );
    }

    #[test]
    fn a_supported_codec_is_used_in_preference_to_fallback() {
        let config = VideoConfig {
            codec: VideoCodec::Vp9,
            ..VideoConfig::default()
        };
        let got = negotiate(
            &config,
            &[VideoCodec::Vp9],
            Fallback::Passthrough(Compression::None),
        )
        .unwrap();
        assert_eq!(got, Negotiated::Codec(VideoCodec::Vp9));
    }

    #[test]
    fn zero_fps_is_rejected_before_it_becomes_a_division_by_zero() {
        let config = VideoConfig {
            max_fps: 0,
            ..VideoConfig::default()
        };
        assert_eq!(
            negotiate(&config, supported_codecs(), Fallback::Refuse).unwrap_err(),
            NegotiateError::ZeroFps
        );
    }

    #[test]
    fn zero_max_dimension_is_rejected() {
        let config = VideoConfig {
            max_dimension: 0,
            ..VideoConfig::default()
        };
        assert_eq!(
            negotiate(&config, supported_codecs(), Fallback::Refuse).unwrap_err(),
            NegotiateError::ZeroDimension
        );
    }

    #[test]
    fn every_codec_appears_exactly_once_in_the_preference_order() {
        // A codec missing from the list can never be negotiated, and a duplicate
        // hides the entry after it. Both are silent.
        for codec in [
            VideoCodec::H264,
            VideoCodec::Vp8,
            VideoCodec::Vp9,
            VideoCodec::Av1,
        ] {
            let hits = CODEC_PREFERENCE.iter().filter(|c| **c == codec).count();
            assert_eq!(hits, 1, "{codec:?} appears {hits} times");
        }
    }

    #[test]
    fn encoder_for_a_codec_without_an_implementation_errors_rather_than_panics() {
        let err = encoder_for(Negotiated::Codec(VideoCodec::H264))
            .err()
            .expect("no H.264 encoder exists yet");
        assert!(matches!(err, EncodeError::NoImplementation(_)), "{err:?}");
    }

    #[test]
    fn built_encoder_reports_the_format_it_was_negotiated_for() {
        let enc = encoder_for(Negotiated::Passthrough(Compression::None)).unwrap();
        assert_eq!(enc.format(), StreamFormat::Passthrough(Compression::None));
    }
}
