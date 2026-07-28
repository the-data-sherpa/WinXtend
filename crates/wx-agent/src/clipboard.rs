//! The clipboard sync state machine.
//!
//! Deliberately free of the platform, the network and the engine's `&mut self`,
//! like [`crate::autolayout`]: what is easy to get wrong here — telling this
//! machine's own write-back from a user copying something, answering a superseded
//! request with the wrong bytes, asking a peer twice for the same serial — is a
//! function of a serial, a format list and a fingerprint, and is tested without a
//! desktop, a peer or a runtime. [`crate::engine`] owns everything that touches
//! the OS clipboard or a session.
//!
//! # Offer, then request
//!
//! The wire protocol announces *what* the clipboard holds and waits to be asked
//! for it ([`wx_proto::ControlMsg::ClipboardOffer`]), rather than pushing bytes at
//! every machine in the mesh. What the agent does with that is one decision worth
//! stating plainly: a peer that can use an offered format **asks for it at once**,
//! because nothing in this system can see a paste happening. So the offer/request
//! split does not defer the transfer; what it buys is that a machine which cannot
//! use the format, or has the clipboard switched off for that peer, costs the
//! sender nothing but the offer.
//!
//! # The write-back echo
//!
//! Writing a received payload to the local clipboard moves the local change
//! serial, and a moved serial is exactly what a user copying something looks like.
//! Untracked, two machines offer the same content to each other forever.
//!
//! Suppressing it by serial alone does not work across platforms. The Wayland
//! backend moves the serial when [`wx_platform::traits::ClipboardAccess::write`]
//! sets the selection *and* again when the portal echoes the change back as
//! `SelectionOwnerChanged` — deliberately, and documented as harmless there
//! because "the counter promises to differ, not to count". Windows moves it once.
//! An agent that ignored exactly one serial would ping-pong on Wayland and an
//! agent that ignored two would go deaf on Windows.
//!
//! So the guard is on the *content*, not the count: after a write, remember which
//! format was written and a [`fingerprint`] of the bytes. Any later change that
//! still holds those bytes is this machine's own write, however many times the
//! serial moved. The first change that holds anything else clears the guard, and
//! is offered normally. This needs no platform to say whether it owns the
//! selection, which the trait has no way to report and two of the backends have no
//! way to know.
//!
//! It has one deliberate false positive: copying, by hand, content identical to
//! what a peer just sent. That offer is suppressed — and it is precisely the offer
//! that would have told the peer something it already has.

use std::collections::HashMap;

use wx_proto::codec::MAX_CLIPBOARD_BYTES;
use wx_proto::{Capabilities, ClipboardFormat, Compression, NodeId};

/// Formats this agent will synchronise, richest first.
///
/// The same order both platform backends report their own formats in, minus
/// [`ClipboardFormat::FileList`]. A file list is a set of absolute paths, and
/// writing one machine's paths onto another machine's clipboard produces a paste
/// that names files which are not there. Moving the files is `FILE_TRANSFER`,
/// which nothing in this build implements — so a file list is not synced, and this
/// list is the only place that decision is made.
pub const SYNCED: [ClipboardFormat; 3] = [
    ClipboardFormat::Png,
    ClipboardFormat::Html,
    ClipboardFormat::Utf8Text,
];

/// The capability a machine must advertise before a format may cross to it.
///
/// `None` for a format this build never synchronises, so the two lists cannot
/// drift apart: a format with no capability has no way to be sent.
///
/// HTML answers to `CLIPBOARD_TEXT` because that is the only bit the protocol has
/// for it, and because a backend that can carry text but silently could not carry
/// HTML would be a capability nobody could advertise the absence of.
pub fn capability_for(format: ClipboardFormat) -> Option<Capabilities> {
    match format {
        ClipboardFormat::Utf8Text | ClipboardFormat::Html => Some(Capabilities::CLIPBOARD_TEXT),
        ClipboardFormat::Png => Some(Capabilities::CLIPBOARD_IMAGE),
        ClipboardFormat::FileList => None,
    }
}

/// Whether a machine advertising `caps` can be sent this format.
pub fn supported_by(caps: Capabilities, format: ClipboardFormat) -> bool {
    capability_for(format).is_some_and(|cap| caps.contains(cap))
}

/// How much of a payload [`fingerprint`] actually reads, from each end.
///
/// Bounded rather than whole because this runs on the engine loop, which is also
/// the loop carrying the user's keystrokes: hashing 32MiB there would be a visible
/// stall on every clipboard change, and the question being asked does not need a
/// cryptographic answer.
const FINGERPRINT_SAMPLE: usize = 64 * 1024;

/// A cheap identity for a clipboard payload.
///
/// Covers the length and up to [`FINGERPRINT_SAMPLE`] bytes from each end, so it
/// runs in constant time whatever the payload size. It answers one question — "is
/// the clipboard still holding the bytes this agent just wrote?" — where the
/// alternative is content a person copied. It is not a defence against a peer
/// constructing a collision, and does not need to be: the worst a collision can do
/// is suppress one offer of content that peer demonstrably already has.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn eat(hash: u64, chunk: &[u8]) -> u64 {
        chunk
            .iter()
            .fold(hash, |h, byte| (h ^ u64::from(*byte)).wrapping_mul(PRIME))
    }

    let head = bytes.len().min(FINGERPRINT_SAMPLE);
    let tail = bytes.len().saturating_sub(FINGERPRINT_SAMPLE).max(head);
    let mut hash = eat(OFFSET, &(bytes.len() as u64).to_le_bytes());
    hash = eat(hash, &bytes[..head]);
    eat(hash, &bytes[tail..])
}

/// Compression level.
///
/// zstd's own default. The payload is on its way across a LAN behind a user who
/// has just pressed Ctrl-C, so the useful trade is "as fast as possible while
/// still worth doing", not the last few percent.
const ZSTD_LEVEL: i32 = 3;

/// Formats that are already compressed, and are only made larger by trying again.
///
/// A PNG is deflate-compressed by definition; running zstd over one spends CPU on
/// both machines to move the same number of bytes. Kept as a predicate on the
/// format rather than as "compress and keep it if it helped", because that
/// alternative pays the CPU before finding out.
fn is_already_compressed(format: ClipboardFormat) -> bool {
    matches!(format, ClipboardFormat::Png)
}

/// Prepare a payload for the wire.
///
/// Never returns something larger than it was given: text that does not compress
/// — short strings, or a base64 blob somebody pasted — is sent as it is rather
/// than with zstd's framing added to it.
pub fn compress(format: ClipboardFormat, data: &[u8]) -> (Compression, Vec<u8>) {
    if is_already_compressed(format) {
        return (Compression::None, data.to_vec());
    }
    match zstd::bulk::compress(data, ZSTD_LEVEL) {
        Ok(packed) if packed.len() < data.len() => (Compression::Zstd, packed),
        Ok(_) => (Compression::None, data.to_vec()),
        Err(e) => {
            // Not fatal: the uncompressed payload is still correct, and refusing
            // the transfer over a failed optimisation would be the wrong trade.
            tracing::debug!(error = %e, "compressing a clipboard payload failed; sending it as it is");
            (Compression::None, data.to_vec())
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PayloadError {
    #[error(
        "a clipboard payload of {len} bytes exceeds the {MAX_CLIPBOARD_BYTES} the protocol carries"
    )]
    TooLarge { len: usize },
    #[error("decompressing a clipboard payload failed: {0}")]
    Corrupt(std::io::Error),
}

/// Recover a payload that arrived from a peer.
///
/// The decompressed size is bounded by [`MAX_CLIPBOARD_BYTES`] and not by what the
/// sender claims: a few hundred kilobytes of zstd expands to gigabytes if it is
/// asked to, and a peer that has been paired is still not entitled to this
/// machine's memory.
pub fn decompress(compression: Compression, data: &[u8]) -> Result<Vec<u8>, PayloadError> {
    match compression {
        Compression::None => {
            if data.len() > MAX_CLIPBOARD_BYTES {
                return Err(PayloadError::TooLarge { len: data.len() });
            }
            Ok(data.to_vec())
        }
        Compression::Zstd => {
            zstd::bulk::decompress(data, MAX_CLIPBOARD_BYTES).map_err(PayloadError::Corrupt)
        }
    }
}

/// What a moved change serial turned out to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalChange {
    /// The serial has not moved. The overwhelmingly common answer.
    Unchanged,
    /// The first look at this machine's clipboard.
    ///
    /// Never offered. Whatever is on the clipboard when the agent starts was put
    /// there before it existed, and pushing it at every peer on the first poll
    /// would mean two machines restarting together each adopting the other's
    /// pre-existing selection.
    FirstSighting,
    /// This machine's own write-back, still on the clipboard. Absorb it silently.
    Echo,
    /// The clipboard changed to something none of [`SYNCED`] describes.
    NothingToOffer,
    /// Genuinely new local content, to be announced to peers.
    Offer {
        serial: u64,
        /// Formats in [`SYNCED`] order, so the receiver's first supported choice
        /// is also the richest.
        formats: Vec<ClipboardFormat>,
    },
}

/// How to answer a [`wx_proto::ControlMsg::ClipboardRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serve {
    /// Read this format from the clipboard and send it.
    Read(ClipboardFormat),
    /// Answer [`wx_proto::ControlMsg::ClipboardStale`]. Never current content: the
    /// requester asked about one specific selection, and anything else would be a
    /// paste of something it never saw offered.
    Stale,
}

/// What this machine last saw on its own clipboard.
#[derive(Debug, Clone)]
struct Snapshot {
    serial: u64,
    /// Only the [`SYNCED`] formats, in that order.
    formats: Vec<ClipboardFormat>,
}

/// The payload most recently written on a peer's behalf. See the module docs.
#[derive(Debug, Clone, Copy)]
struct WriteBack {
    /// The serial read back immediately after the write. A backend that moves the
    /// serial synchronously is recognised here without reading anything at all.
    serial: u64,
    format: ClipboardFormat,
    digest: u64,
}

/// Everything the agent remembers about clipboard sync.
#[derive(Debug, Default)]
pub struct ClipboardSync {
    seen: Option<Snapshot>,
    write_back: Option<WriteBack>,
    /// The request outstanding with each peer, so an answer can be matched to it.
    ///
    /// Keyed by peer because two machines can be offering at once, and an answer
    /// carries no request id — the pair (serial, format) is the only handle there
    /// is, and it is only unique per peer.
    asked: HashMap<NodeId, (u64, ClipboardFormat)>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// The serial of the content this machine last looked at.
    ///
    /// The engine re-checks it after reading the clipboard: a read is not atomic
    /// with the check that authorised it, and content that changed underneath must
    /// be answered [`Serve::Stale`] rather than sent.
    pub fn serial(&self) -> Option<u64> {
        self.seen.as_ref().map(|s| s.serial)
    }

    /// The write-back guard as it stands: the format written and the serial the
    /// write produced.
    ///
    /// Exposed because the read [`ClipboardSync::observe`] may need is a blocking
    /// one, and blocking is not allowed where `observe` is called from. The caller
    /// asks this first, does the read elsewhere, and hands the answer back as the
    /// `digest` closure — so the decision is still made here and only the I/O moved.
    pub fn armed(&self) -> Option<(ClipboardFormat, u64)> {
        self.write_back.map(|w| (w.format, w.serial))
    }

    /// Decide what a change serial and format list mean.
    ///
    /// `digest` is called at most once, and only when telling a write-back from a
    /// real copy actually requires the bytes — so the steady state, where nothing
    /// has changed or no write is outstanding, reads no clipboard content at all.
    /// It returns `None` if the read failed, which is treated as "not ours".
    pub fn observe<F>(&mut self, serial: u64, formats: &[ClipboardFormat], digest: F) -> LocalChange
    where
        F: FnOnce(ClipboardFormat) -> Option<u64>,
    {
        if self.seen.as_ref().is_some_and(|s| s.serial == serial) {
            return LocalChange::Unchanged;
        }

        let syncable: Vec<ClipboardFormat> =
            SYNCED.into_iter().filter(|f| formats.contains(f)).collect();
        let first = self.seen.is_none();
        self.seen = Some(Snapshot {
            serial,
            formats: syncable.clone(),
        });
        if first {
            return LocalChange::FirstSighting;
        }

        if let Some(write) = self.write_back {
            if serial == write.serial {
                return LocalChange::Echo;
            }
            if syncable.contains(&write.format) && digest(write.format) == Some(write.digest) {
                return LocalChange::Echo;
            }
            // Something else is on the clipboard now, so the write-back can never
            // come back. Held until this moment rather than until a timer expires,
            // because a backend that never echoes and one that echoes late are
            // indistinguishable from here.
            self.write_back = None;
        }

        if syncable.is_empty() {
            return LocalChange::NothingToOffer;
        }
        LocalChange::Offer {
            serial,
            formats: syncable,
        }
    }

    /// Answer a peer's request for one serial.
    ///
    /// Stale unless the request names the content on the clipboard *now* and a
    /// format that was part of it. A serial that has moved on is the case the
    /// serial exists for; a format that was never in the offer is the same failure
    /// wearing a different hat, and both are refused the same way.
    pub fn serve(&self, serial: u64, format: ClipboardFormat) -> Serve {
        match &self.seen {
            Some(seen) if seen.serial == serial && seen.formats.contains(&format) => {
                Serve::Read(format)
            }
            _ => Serve::Stale,
        }
    }

    /// Which of the formats a peer offered to ask for.
    ///
    /// The richest that both machines advertise support for. `None` means there is
    /// nothing worth a request, which is the case the offer/request split exists to
    /// make free.
    pub fn choose(
        offered: &[ClipboardFormat],
        local: Capabilities,
        peer: Capabilities,
    ) -> Option<ClipboardFormat> {
        SYNCED
            .into_iter()
            .find(|f| offered.contains(f) && supported_by(local, *f) && supported_by(peer, *f))
    }

    /// Record a request about to be sent. `false` if it duplicates the one already
    /// outstanding with that peer, which a repeated offer would otherwise produce.
    pub fn ask(&mut self, node: NodeId, serial: u64, format: ClipboardFormat) -> bool {
        if self.asked.get(&node) == Some(&(serial, format)) {
            return false;
        }
        self.asked.insert(node, (serial, format));
        true
    }

    /// Whether an arriving payload answers the request made to that peer.
    ///
    /// Unsolicited content is refused. A paired peer is trusted to be told what
    /// this machine has copied, not to decide what this machine's clipboard holds:
    /// without this, one machine in the mesh could overwrite everybody's clipboard
    /// at will, and no offer or capability check would ever see it.
    pub fn answers(&self, node: NodeId, serial: u64, format: ClipboardFormat) -> bool {
        self.asked.get(&node) == Some(&(serial, format))
    }

    /// The request to that peer is over, answered or refused.
    pub fn settled(&mut self, node: NodeId) {
        self.asked.remove(&node);
    }

    /// Arm the write-back guard for bytes about to be written locally.
    ///
    /// Called before the write, so that no ordering of the write and the backend's
    /// own change notification can slip an offer out ahead of the guard.
    pub fn writing(&mut self, format: ClipboardFormat, digest: u64) {
        self.write_back = Some(WriteBack {
            serial: self.seen.as_ref().map_or(0, |s| s.serial),
            format,
            digest,
        });
    }

    /// The write went through; here is the serial it produced.
    ///
    /// Recording it lets a backend that moves the serial synchronously — Windows,
    /// and the first of the Wayland backend's two moves — be recognised without
    /// reading the clipboard back at all.
    pub fn wrote(&mut self, serial: u64) {
        if let Some(write) = self.write_back.as_mut() {
            write.serial = serial;
        }
    }

    /// The write failed, so nothing of ours is on the clipboard to suppress.
    pub fn write_failed(&mut self) {
        self.write_back = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> NodeId {
        NodeId([n; 32])
    }

    /// A digest source that must not be consulted.
    fn never(_: ClipboardFormat) -> Option<u64> {
        panic!("the clipboard was read when nothing needed reading");
    }

    const TEXT: ClipboardFormat = ClipboardFormat::Utf8Text;
    const HTML: ClipboardFormat = ClipboardFormat::Html;
    const PNG: ClipboardFormat = ClipboardFormat::Png;
    const FILES: ClipboardFormat = ClipboardFormat::FileList;

    const BOTH: Capabilities =
        Capabilities(Capabilities::CLIPBOARD_TEXT.0 | Capabilities::CLIPBOARD_IMAGE.0);

    /// A sync that has already seen one selection, which is the ordinary state.
    fn running() -> ClipboardSync {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.observe(1, &[TEXT], never),
            LocalChange::FirstSighting,
            "the first look must never be offered"
        );
        sync
    }

    #[test]
    fn a_serial_that_has_not_moved_costs_nothing() {
        let mut sync = running();
        assert_eq!(sync.observe(1, &[TEXT], never), LocalChange::Unchanged);
        // Every poll takes this path; reading the clipboard here would pull
        // megabytes across the process boundary to learn nothing.
        assert_eq!(sync.observe(1, &[TEXT, PNG], never), LocalChange::Unchanged);
    }

    #[test]
    fn a_copy_is_offered_richest_format_first() {
        let mut sync = running();
        // The platform's own order is not promised, so the offer imposes one.
        assert_eq!(
            sync.observe(2, &[TEXT, HTML, PNG], never),
            LocalChange::Offer {
                serial: 2,
                formats: vec![PNG, HTML, TEXT],
            }
        );
    }

    #[test]
    fn a_file_list_is_never_offered() {
        // Advertising it would promise a paste that names files the other machine
        // does not have. `FILE_TRANSFER` is the feature that would fix that, and
        // nothing in this build implements it.
        let mut sync = running();
        assert_eq!(
            sync.observe(2, &[FILES], never),
            LocalChange::NothingToOffer
        );
        assert_eq!(capability_for(FILES), None);
        assert!(!supported_by(BOTH, FILES));
    }

    #[test]
    fn a_clipboard_holding_nothing_we_speak_is_not_an_error() {
        // Some application's private format. A clipboard with nothing to sync is
        // an ordinary clipboard, not a failure.
        let mut sync = running();
        assert_eq!(sync.observe(2, &[], never), LocalChange::NothingToOffer);
    }

    #[test]
    fn a_write_back_that_moves_the_serial_at_once_is_absorbed_without_a_read() {
        // Windows, and the synchronous half of the Wayland backend's two moves.
        let mut sync = running();
        sync.writing(TEXT, fingerprint(b"from the peer"));
        sync.wrote(2);
        assert_eq!(sync.observe(2, &[TEXT], never), LocalChange::Echo);
    }

    #[test]
    fn a_second_serial_move_for_the_same_write_back_is_still_absorbed() {
        // The bug this whole guard exists for. The Wayland backend moves the serial
        // when `write` sets the selection and again when the portal echoes the
        // change back, and both are indistinguishable from a user copying. An agent
        // that absorbed only the first would offer the peer back the very content
        // it had just been sent, forever.
        let mut sync = running();
        let payload = b"from the peer";
        sync.writing(TEXT, fingerprint(payload));
        sync.wrote(2);
        assert_eq!(sync.observe(2, &[TEXT], never), LocalChange::Echo);
        assert_eq!(
            sync.observe(3, &[TEXT], |f| {
                assert_eq!(f, TEXT);
                Some(fingerprint(payload))
            }),
            LocalChange::Echo,
            "the portal's echo was offered back to the peer that sent it"
        );
        // And a third, and a tenth: nothing here counts.
        assert_eq!(
            sync.observe(4, &[TEXT], |_| Some(fingerprint(payload))),
            LocalChange::Echo
        );
    }

    #[test]
    fn copying_something_else_after_a_write_back_is_offered() {
        // The other half of the guard. Suppressing by time or by counting would
        // eventually swallow a real copy, and a clipboard that silently stops
        // syncing is worse than one that never started.
        let mut sync = running();
        sync.writing(TEXT, fingerprint(b"from the peer"));
        sync.wrote(2);
        assert_eq!(sync.observe(2, &[TEXT], never), LocalChange::Echo);
        assert_eq!(
            sync.observe(3, &[TEXT], |_| Some(fingerprint(b"what the user typed"))),
            LocalChange::Offer {
                serial: 3,
                formats: vec![TEXT],
            }
        );
        // The guard is spent, so the next change needs no read to be believed.
        assert_eq!(
            sync.observe(4, &[TEXT], never),
            LocalChange::Offer {
                serial: 4,
                formats: vec![TEXT],
            }
        );
    }

    #[test]
    fn a_write_back_replaced_by_a_different_format_needs_no_read() {
        // Text arrives from a peer, then the user copies an image. The format list
        // alone settles it, which is the cheap path and the common one.
        let mut sync = running();
        sync.writing(TEXT, fingerprint(b"from the peer"));
        sync.wrote(2);
        assert_eq!(sync.observe(2, &[TEXT], never), LocalChange::Echo);
        assert_eq!(
            sync.observe(3, &[PNG], never),
            LocalChange::Offer {
                serial: 3,
                formats: vec![PNG],
            }
        );
    }

    #[test]
    fn a_clipboard_that_cannot_be_read_back_is_treated_as_somebody_elses() {
        // Erring towards offering: a suppressed offer is content that never syncs
        // and nothing in any log to say why, while a spurious offer costs one
        // round trip.
        let mut sync = running();
        sync.writing(TEXT, fingerprint(b"from the peer"));
        sync.wrote(2);
        assert_eq!(sync.observe(2, &[TEXT], never), LocalChange::Echo);
        assert_eq!(
            sync.observe(3, &[TEXT], |_| None),
            LocalChange::Offer {
                serial: 3,
                formats: vec![TEXT],
            }
        );
    }

    #[test]
    fn a_failed_write_leaves_nothing_to_suppress() {
        let mut sync = running();
        sync.writing(TEXT, fingerprint(b"from the peer"));
        sync.write_failed();
        assert_eq!(
            sync.observe(2, &[TEXT], never),
            LocalChange::Offer {
                serial: 2,
                formats: vec![TEXT],
            }
        );
    }

    #[test]
    fn the_current_serial_is_served_and_a_superseded_one_is_not() {
        let mut sync = running();
        sync.observe(2, &[TEXT, PNG], never);
        assert_eq!(sync.serve(2, PNG), Serve::Read(PNG));
        assert_eq!(sync.serve(2, TEXT), Serve::Read(TEXT));

        // The user copies again between the offer and the request arriving. This is
        // the whole reason the serial is on the wire.
        sync.observe(3, &[TEXT], never);
        assert_eq!(sync.serve(2, PNG), Serve::Stale);
        assert_eq!(
            sync.serve(2, TEXT),
            Serve::Stale,
            "answering with the new text would paste content the peer never saw offered"
        );
        assert_eq!(sync.serve(3, TEXT), Serve::Read(TEXT));
    }

    #[test]
    fn a_format_that_was_never_offered_is_stale_not_served() {
        let mut sync = running();
        sync.observe(2, &[TEXT], never);
        assert_eq!(sync.serve(2, PNG), Serve::Stale);
        assert_eq!(sync.serve(2, FILES), Serve::Stale);
    }

    #[test]
    fn a_request_before_anything_has_been_seen_is_stale() {
        let sync = ClipboardSync::new();
        assert_eq!(sync.serve(1, TEXT), Serve::Stale);
    }

    #[test]
    fn the_richest_format_both_machines_support_is_the_one_asked_for() {
        let text_only = Capabilities::CLIPBOARD_TEXT;
        assert_eq!(
            ClipboardSync::choose(&[PNG, HTML, TEXT], BOTH, BOTH),
            Some(PNG)
        );
        // A peer that only claims text must never be asked for the image, however
        // richly it was offered.
        assert_eq!(
            ClipboardSync::choose(&[PNG, HTML, TEXT], text_only, BOTH),
            Some(HTML)
        );
        assert_eq!(
            ClipboardSync::choose(&[PNG, HTML, TEXT], BOTH, text_only),
            Some(HTML)
        );
        assert_eq!(ClipboardSync::choose(&[PNG], text_only, BOTH), None);
        assert_eq!(ClipboardSync::choose(&[FILES], BOTH, BOTH), None);
        assert_eq!(
            ClipboardSync::choose(&[TEXT], Capabilities::NONE, BOTH),
            None,
            "a machine that advertises nothing accepts nothing"
        );
    }

    #[test]
    fn a_repeated_offer_is_only_asked_about_once() {
        // A peer that re-offers the same serial — because a session was replaced,
        // or simply because it re-advertised — must not produce a second transfer
        // of the same twenty megabytes.
        let mut sync = running();
        assert!(sync.ask(node(2), 7, PNG));
        assert!(!sync.ask(node(2), 7, PNG));
        // A different peer is a different transfer.
        assert!(sync.ask(node(3), 7, PNG));
        // And a new serial from the same peer is new content.
        assert!(sync.ask(node(2), 8, PNG));
    }

    #[test]
    fn only_the_payload_that_was_asked_for_is_accepted() {
        let mut sync = running();
        sync.ask(node(2), 7, PNG);
        assert!(sync.answers(node(2), 7, PNG));
        // Unsolicited, wrong serial, wrong format, wrong peer: all refused. A
        // paired machine may tell this one what it has copied; it may not decide
        // what this one's clipboard holds.
        assert!(!sync.answers(node(2), 8, PNG));
        assert!(!sync.answers(node(2), 7, TEXT));
        assert!(!sync.answers(node(3), 7, PNG));
        let fresh = ClipboardSync::new();
        assert!(!fresh.answers(node(2), 7, PNG));
    }

    #[test]
    fn a_settled_request_stops_accepting_answers() {
        // Otherwise a peer could resend the same payload at any later moment and
        // still have it written.
        let mut sync = running();
        sync.ask(node(2), 7, PNG);
        sync.settled(node(2));
        assert!(!sync.answers(node(2), 7, PNG));
    }

    #[test]
    fn every_clipboard_capability_this_build_advertises_is_actually_synced() {
        // The house rule, in the same shape as the `FILE_TRANSFER` test in the
        // Windows backend: a capability nobody implements is a promise a peer waits
        // on forever. Both clipboard bits are advertised by the Windows and Wayland
        // backends, so both have to be reachable from `SYNCED`.
        for cap in [Capabilities::CLIPBOARD_TEXT, Capabilities::CLIPBOARD_IMAGE] {
            assert!(
                SYNCED.iter().any(|f| capability_for(*f) == Some(cap)),
                "{} is advertised but no synced format uses it",
                cap.describe()
            );
        }
        // And the converse: nothing in `SYNCED` may be missing a capability, or it
        // would be sent to machines that never claimed they could take it.
        for format in SYNCED {
            assert!(capability_for(format).is_some(), "{format:?}");
        }
    }

    #[test]
    fn text_round_trips_through_compression() {
        let text = "the quick brown fox ".repeat(200);
        let (compression, packed) = compress(TEXT, text.as_bytes());
        assert_eq!(compression, Compression::Zstd);
        assert!(packed.len() < text.len(), "text that repeats must compress");
        assert_eq!(decompress(compression, &packed).unwrap(), text.as_bytes());
    }

    #[test]
    fn an_image_is_not_recompressed() {
        // A PNG is already deflate-compressed. Running zstd over one spends CPU on
        // both machines to move the same bytes.
        let png = vec![0x89u8, 0x50, 0x4e, 0x47];
        let (compression, packed) = compress(PNG, &png);
        assert_eq!(compression, Compression::None);
        assert_eq!(packed, png);
    }

    #[test]
    fn a_payload_that_does_not_compress_is_sent_as_it_is() {
        // Two cases, and both are ordinary. A short copy is smaller than zstd's
        // framing on its own, and high-entropy text — a key, a base64 blob — has
        // nothing left to remove. A transfer that grows by being compressed is the
        // one case where the whole exercise is a loss.
        let (compression, packed) = compress(TEXT, b"hi");
        assert_eq!(compression, Compression::None);
        assert_eq!(packed, b"hi");

        let mut state = 0x2545_f491_4f6c_dd1du64;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 33) as u8
            })
            .collect();
        let (compression, packed) = compress(TEXT, &noise);
        assert_eq!(compression, Compression::None);
        assert_eq!(packed, noise);
    }

    #[test]
    fn a_zip_bomb_is_refused_rather_than_allocated() {
        // A paired peer is still not entitled to this machine's memory: a few
        // hundred kilobytes of zstd expands to gigabytes if it is asked to.
        let huge = vec![0u8; MAX_CLIPBOARD_BYTES + 1];
        let packed = zstd::bulk::compress(&huge, ZSTD_LEVEL).unwrap();
        assert!(
            packed.len() < 1024 * 1024,
            "the bomb has to be small to be a bomb"
        );
        assert!(matches!(
            decompress(Compression::Zstd, &packed),
            Err(PayloadError::Corrupt(_))
        ));
    }

    #[test]
    fn an_oversized_uncompressed_payload_is_refused_by_size() {
        let huge = vec![0u8; MAX_CLIPBOARD_BYTES + 1];
        assert!(matches!(
            decompress(Compression::None, &huge),
            Err(PayloadError::TooLarge { .. })
        ));
        // And the limit itself is inclusive, or a payload the sender's own check
        // passed would be refused here.
        assert!(decompress(Compression::None, &huge[..MAX_CLIPBOARD_BYTES]).is_ok());
    }

    #[test]
    fn a_corrupt_payload_is_an_error_not_a_panic() {
        assert!(matches!(
            decompress(Compression::Zstd, b"not zstd at all"),
            Err(PayloadError::Corrupt(_))
        ));
    }

    #[test]
    fn the_fingerprint_separates_payloads_and_is_stable() {
        assert_eq!(fingerprint(b"same"), fingerprint(b"same"));
        assert_ne!(fingerprint(b"same"), fingerprint(b"different"));
        // Length is part of it, so a prefix is not the same payload.
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abcd"));
        assert_ne!(fingerprint(b""), fingerprint(b"\0"));

        // Beyond the sample it is the tail that distinguishes, which is what makes
        // it constant time on a 32MiB image.
        let mut a = vec![7u8; FINGERPRINT_SAMPLE * 4];
        let mut b = a.clone();
        *a.last_mut().unwrap() = 1;
        *b.last_mut().unwrap() = 2;
        assert_ne!(fingerprint(&a), fingerprint(&b));
        // And the head, for a payload whose end is identical.
        a[0] = 9;
        b[0] = 9;
        b[1] = 10;
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }
}
