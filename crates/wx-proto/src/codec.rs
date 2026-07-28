//! Framing and (de)serialisation.
//!
//! Two transport shapes, deliberately different:
//!
//! * **Datagrams** carry [`crate::input::InputFrame`] and are self-delimiting, so
//!   they need no length prefix at all.
//! * **Streams** carry [`crate::control::ControlMsg`] over an ordered byte
//!   stream, so each message gets a 4-byte little-endian length prefix.
//!
//! The wire format is postcard: varint-encoded and compact, which matters
//! because a pointer-motion frame is sent hundreds of times per second.

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Largest single stream frame accepted.
///
/// Exists to bound memory against a hostile or confused peer: without it, a
/// four-byte length prefix claiming 4GiB would have us allocate it. Sized to
/// admit a large clipboard image; bulk file transfer is chunked and never
/// approaches this.
pub const MAX_FRAME_LEN: u32 = 32 * 1024 * 1024;

/// Bytes of length prefix ahead of each stream frame.
pub const LEN_PREFIX: usize = 4;

/// What the message carrying a clipboard payload costs on top of the payload
/// itself.
///
/// [`MAX_FRAME_LEN`] bounds the *encoded* frame, and the bytes travel inside
/// [`crate::ControlMsg::ClipboardData`], which spells a variant tag, a format tag,
/// a serial varint, a compression tag and the payload's own length varint around
/// them — under twenty bytes as postcard writes them today. Rounded far up rather
/// than counted to the byte, so that a field added to that message later cannot
/// quietly put the boundary back.
const CLIPBOARD_FRAME_OVERHEAD: usize = 64;

/// The most any one clipboard payload may be.
///
/// [`MAX_FRAME_LEN`] is what this codec will carry, so anything larger is rejected
/// where it is first seen — with a message naming the size — rather than handed
/// onwards to tear a session down at the frame writer. Less
/// [`CLIPBOARD_FRAME_OVERHEAD`], because the codec weighs the encoded message and
/// not the payload: a limit set to the frame length exactly would pass every check
/// upstream and still be refused here, which is the outcome this constant exists
/// to prevent.
///
/// Lives beside the frame limit rather than in a platform backend or in the agent
/// because all three need the same number and two of them would otherwise be
/// guessing at it: the platform backends bound what they will lift off the OS
/// clipboard, and the agent bounds what it will accept from a peer.
pub const MAX_CLIPBOARD_BYTES: usize = MAX_FRAME_LEN as usize - CLIPBOARD_FRAME_OVERHEAD;

/// Practical upper bound for a datagram payload.
///
/// QUIC datagrams must fit the path MTU or they are dropped outright rather than
/// fragmented, and a dropped input frame is a dropped keystroke. Kept
/// conservative to survive VPN and tunnel overhead.
pub const MAX_DATAGRAM_LEN: usize = 1200;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("serialisation failed: {0}")]
    Serialize(postcard::Error),
    #[error("deserialisation failed: {0}")]
    Deserialize(postcard::Error),
    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    FrameTooLarge { len: u64, max: u32 },
}

/// Serialise a message with its length prefix, ready to write to a stream.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    let payload = postcard::to_allocvec(msg).map_err(CodecError::Serialize)?;
    if payload.len() as u64 > MAX_FRAME_LEN as u64 {
        return Err(CodecError::FrameTooLarge {
            len: payload.len() as u64,
            max: MAX_FRAME_LEN,
        });
    }
    let mut out = Vec::with_capacity(LEN_PREFIX + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Serialise a message for a datagram: no length prefix, since the datagram
/// boundary already delimits it.
pub fn encode_datagram<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    postcard::to_allocvec(msg).map_err(CodecError::Serialize)
}

/// Deserialise a bare payload — one datagram, or one frame body already stripped
/// of its prefix by [`FrameReader`].
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    postcard::from_bytes(bytes).map_err(CodecError::Deserialize)
}

/// Reassembles length-prefixed frames from a stream delivered in arbitrary
/// chunks.
///
/// A stream read boundary has nothing to do with a message boundary: one read
/// can return half a frame, or three frames and a fragment. This buffers until
/// each frame is whole.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    /// How much of `buf` has been consumed. Frames are handed out by copying and
    /// advancing this, then compacting in bulk, rather than shifting the whole
    /// buffer on every frame.
    consumed: usize,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly read bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.compact_if_worthwhile();
        self.buf.extend_from_slice(data);
    }

    /// Take the next complete frame body, if one has fully arrived.
    ///
    /// Returns `Ok(None)` when more bytes are needed. An `Err` is unrecoverable:
    /// the stream is desynchronised and the connection must be dropped, because
    /// there is no way to resynchronise a length-prefixed stream after a bad
    /// prefix.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, CodecError> {
        let available = &self.buf[self.consumed..];
        if available.len() < LEN_PREFIX {
            return Ok(None);
        }

        let len = u32::from_le_bytes([available[0], available[1], available[2], available[3]]);
        if len > MAX_FRAME_LEN {
            return Err(CodecError::FrameTooLarge {
                len: len as u64,
                max: MAX_FRAME_LEN,
            });
        }

        let total = LEN_PREFIX + len as usize;
        if available.len() < total {
            return Ok(None);
        }

        let frame = available[LEN_PREFIX..total].to_vec();
        self.consumed += total;
        Ok(Some(frame))
    }

    /// Decode the next frame directly into a message type.
    pub fn next_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, CodecError> {
        match self.next_frame()? {
            Some(bytes) => decode(&bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Bytes buffered but not yet formed into a complete frame.
    pub fn pending_len(&self) -> usize {
        self.buf.len() - self.consumed
    }

    /// Reclaim space taken by already-consumed frames.
    ///
    /// Deferred until the consumed prefix is worth moving, so a stream of small
    /// control messages does not memmove the tail on every single one.
    fn compact_if_worthwhile(&mut self) {
        const THRESHOLD: usize = 8 * 1024;
        if self.consumed == 0 {
            return;
        }
        if self.consumed >= self.buf.len() {
            // Fully drained: the common steady state.
            self.buf.clear();
            self.consumed = 0;
        } else if self.consumed >= THRESHOLD {
            self.buf.drain(..self.consumed);
            self.consumed = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlMsg;
    use crate::geom::NormPos;
    use crate::ids::MonitorId;
    use crate::input::{InputEvent, InputFrame, PointerEvent};

    fn ping(n: u64) -> ControlMsg {
        ControlMsg::Ping { nonce: n }
    }

    #[test]
    fn single_frame_round_trips() {
        let mut reader = FrameReader::new();
        reader.push(&encode_frame(&ping(1)).unwrap());
        assert_eq!(reader.next_message::<ControlMsg>().unwrap(), Some(ping(1)));
        assert_eq!(reader.next_message::<ControlMsg>().unwrap(), None);
    }

    #[test]
    fn several_frames_in_one_read() {
        let mut bytes = Vec::new();
        for n in 0..5 {
            bytes.extend_from_slice(&encode_frame(&ping(n)).unwrap());
        }

        let mut reader = FrameReader::new();
        reader.push(&bytes);
        for n in 0..5 {
            assert_eq!(reader.next_message::<ControlMsg>().unwrap(), Some(ping(n)));
        }
        assert_eq!(reader.next_message::<ControlMsg>().unwrap(), None);
    }

    #[test]
    fn frame_split_across_every_possible_boundary() {
        // The bug this guards against only shows up at specific split points —
        // most often mid-length-prefix — so try them all rather than one.
        let encoded = encode_frame(&ping(0xdead_beef)).unwrap();
        for split in 0..=encoded.len() {
            let mut reader = FrameReader::new();
            reader.push(&encoded[..split]);
            let early = reader.next_message::<ControlMsg>().unwrap();
            if split < encoded.len() {
                assert_eq!(early, None, "completed early at split {split}");
            }
            reader.push(&encoded[split..]);
            let got = if early.is_some() {
                early
            } else {
                reader.next_message::<ControlMsg>().unwrap()
            };
            assert_eq!(got, Some(ping(0xdead_beef)), "failed at split {split}");
        }
    }

    #[test]
    fn one_byte_at_a_time() {
        let encoded = encode_frame(&ping(77)).unwrap();
        let mut reader = FrameReader::new();
        for (i, b) in encoded.iter().enumerate() {
            reader.push(&[*b]);
            let got = reader.next_message::<ControlMsg>().unwrap();
            if i + 1 == encoded.len() {
                assert_eq!(got, Some(ping(77)));
            } else {
                assert_eq!(got, None, "completed early after {} bytes", i + 1);
            }
        }
    }

    #[test]
    fn oversized_length_prefix_is_rejected_without_allocating() {
        // A hostile peer claiming a 4GiB frame must not cause a 4GiB allocation.
        let mut reader = FrameReader::new();
        reader.push(&u32::MAX.to_le_bytes());
        let err = reader.next_frame().unwrap_err();
        assert!(matches!(err, CodecError::FrameTooLarge { .. }), "{err:?}");
    }

    #[test]
    fn frame_at_exactly_the_limit_is_allowed() {
        let mut reader = FrameReader::new();
        reader.push(&MAX_FRAME_LEN.to_le_bytes());
        // Body has not arrived, but the prefix itself must not be an error.
        assert_eq!(reader.next_frame().unwrap(), None);
    }

    #[test]
    fn empty_payload_frame_is_valid() {
        let mut reader = FrameReader::new();
        reader.push(&0u32.to_le_bytes());
        assert_eq!(reader.next_frame().unwrap(), Some(Vec::new()));
    }

    #[test]
    fn pending_len_tracks_incomplete_data() {
        let mut reader = FrameReader::new();
        assert_eq!(reader.pending_len(), 0);
        reader.push(&[1, 2]);
        assert_eq!(reader.pending_len(), 2);

        let encoded = encode_frame(&ping(1)).unwrap();
        let mut reader = FrameReader::new();
        reader.push(&encoded);
        reader.next_frame().unwrap();
        assert_eq!(reader.pending_len(), 0);
    }

    #[test]
    fn buffer_does_not_grow_without_bound_across_many_frames() {
        // Steady-state operation must reclaim consumed bytes, or a long-lived
        // connection leaks.
        let mut reader = FrameReader::new();
        for n in 0..10_000u64 {
            reader.push(&encode_frame(&ping(n)).unwrap());
            assert_eq!(reader.next_message::<ControlMsg>().unwrap(), Some(ping(n)));
        }
        assert_eq!(reader.pending_len(), 0);
        assert!(
            reader.buf.capacity() < 4096,
            "buffer grew to {} bytes",
            reader.buf.capacity()
        );
    }

    #[test]
    fn garbage_payload_errors_without_panicking() {
        let mut reader = FrameReader::new();
        reader.push(&3u32.to_le_bytes());
        reader.push(&[0xff, 0xff, 0xff]);
        assert!(reader.next_message::<ControlMsg>().is_err());
    }

    #[test]
    fn input_frames_fit_a_datagram() {
        let frame = InputFrame::new(
            u64::MAX,
            MonitorId(u32::MAX),
            InputEvent::Pointer(PointerEvent::MoveTo {
                pos: NormPos::new(0.123_456_789, 0.987_654_321),
            }),
        );
        let bytes = encode_datagram(&frame).unwrap();
        assert!(
            bytes.len() <= MAX_DATAGRAM_LEN,
            "datagram is {} bytes",
            bytes.len()
        );
        assert_eq!(decode::<InputFrame>(&bytes).unwrap(), frame);
    }

    #[test]
    fn the_clipboard_limit_leaves_room_for_the_message_that_carries_it() {
        // The payload limit is checked where the bytes are first seen and the frame
        // limit at the writer, so a payload limit set to the frame length exactly
        // would pass every upstream check and still be refused here.
        assert!(MAX_CLIPBOARD_BYTES + CLIPBOARD_FRAME_OVERHEAD <= MAX_FRAME_LEN as usize);
        let framed = encode_frame(&ControlMsg::ClipboardData {
            format: crate::control::ClipboardFormat::Png,
            serial: u64::MAX,
            compression: crate::control::Compression::None,
            data: Vec::new(),
        })
        .unwrap();
        assert!(
            framed.len() - LEN_PREFIX <= CLIPBOARD_FRAME_OVERHEAD,
            "the reserved allowance must cover what the message itself costs"
        );
    }
}
