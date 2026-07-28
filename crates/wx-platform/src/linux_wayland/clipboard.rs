//! Clipboard content, MIME mapping, and the selection this machine owns.
//!
//! Deliberately free of D-Bus and async, like [`super::session`] and
//! [`super::capture`]: the rules that are easy to get subtly wrong — which MIME
//! type a format is, how a `file://` URI becomes an absolute path, what may be
//! advertised, when the change serial moves — are pure functions of bytes and are
//! tested on every target. The portal half lives in [`super::clipboard_portal`],
//! behind `cfg(target_os = "linux")`.
//!
//! # Why the portal, and why there is no fallback
//!
//! Probed on the alpha target (Ubuntu 26.04 / GNOME Shell 50.1 / Mutter 50.1):
//!
//! | Candidate | Verdict |
//! |---|---|
//! | `zwlr_data_control_manager_v1` | absent — not in the registry, and not even a string in `libmutter-18` |
//! | `ext_data_control_manager_v1` | absent — Mutter 50 has not adopted the standardised successor either |
//! | `wl_data_device` | present and useless: it delivers no selection to a surfaceless client and discards `set_selection`, **both silently** |
//! | `org.freedesktop.portal.Clipboard` | works, with no focused surface |
//!
//! The `wl_data_device` row is the reason this is not written with a fallback
//! order. Neither direction produces a protocol error, so a backend built on it
//! looks healthy and never moves a byte. The staging `ext-data-control` XML is
//! installed under `/usr/share/wayland-protocols/` on that machine; that is the
//! shared definition package and says nothing about what the compositor
//! implements.
//!
//! # It rides the injection session, and it is a separate grant
//!
//! `RequestClipboard` is refused for anything that is not a `RemoteDesktop`
//! session, so this shares the one [`super::PortalSession`] injection already
//! holds — a second `CreateSession` would mean a second consent dialog for the
//! same machine. What it does *not* share is the grant: the dialog carries "Allow
//! Clipboard Access" as a toggle independent of remote interaction, so `Start`
//! answers `clipboard_enabled` separately from the devices and this backend
//! advertises exactly what came back. See [`super::session::CLIPBOARD_CAPABILITIES`].
//!
//! # Content moves over a pipe, not a buffer
//!
//! Every read is a `SelectionRead` returning a file descriptor, and every paste
//! out of a selection this machine owns is a `SelectionTransfer` signal asking for
//! one. So *offering* a selection is a promise to still be there when somebody
//! pastes: an offer with nothing behind it hangs the receiving application's
//! paste rather than failing it. [`ClipboardState`] holds the bytes behind every
//! offer this backend has made for exactly that reason, and stages them before
//! `SetSelection` is called rather than after.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use wx_proto::codec::MAX_FRAME_LEN;
use wx_proto::ClipboardFormat;

use crate::error::{PlatformError, Result};

/// The most any one clipboard payload may be.
///
/// [`MAX_FRAME_LEN`] is what the protocol codec will carry, so anything larger is
/// rejected here — with a message naming the size — rather than handed upwards to
/// tear a session down at the frame writer.
pub const MAX_CLIPBOARD_BYTES: usize = MAX_FRAME_LEN as usize;

/// The MIME type each format is read and written as.
///
/// First in the list is what this backend offers first and prefers to read.
pub const MIME_TEXT: &str = "text/plain;charset=utf-8";
pub const MIME_HTML: &str = "text/html";
pub const MIME_PNG: &str = "image/png";
pub const MIME_URI_LIST: &str = "text/uri-list";

/// Text has aliases, and they are not decoration.
///
/// The portal hands the MIME type straight through to the compositor's data
/// offer, so what is offered here is exactly what a pasting application gets to
/// choose from. Applications that came through XWayland ask for `UTF8_STRING`,
/// and plenty of older toolkits ask for bare `text/plain`; both carry UTF-8 in
/// practice on this desktop, and offering them costs one entry in a list. The
/// bytes served for every alias are the same bytes.
const TEXT_MIMES: &[&str] = &[MIME_TEXT, "text/plain", "UTF8_STRING"];
const HTML_MIMES: &[&str] = &[MIME_HTML];
const PNG_MIMES: &[&str] = &[MIME_PNG];
const URI_LIST_MIMES: &[&str] = &[MIME_URI_LIST];

/// Formats richest first.
///
/// The order [`ClipboardState::available_formats`] reports, and the same one the
/// Windows backend uses: a peer offered several formats should pick the one that
/// keeps the most, rather than flattening a table to plain text.
const PREFERENCE: [ClipboardFormat; 4] = [
    ClipboardFormat::FileList,
    ClipboardFormat::Png,
    ClipboardFormat::Html,
    ClipboardFormat::Utf8Text,
];

/// Every MIME type a format is offered as, preferred one first.
pub fn mimes_for(format: ClipboardFormat) -> &'static [&'static str] {
    match format {
        ClipboardFormat::Utf8Text => TEXT_MIMES,
        ClipboardFormat::Html => HTML_MIMES,
        ClipboardFormat::Png => PNG_MIMES,
        ClipboardFormat::FileList => URI_LIST_MIMES,
    }
}

/// Which format a MIME type from the compositor means, if any.
///
/// Parameters and case are normalised first: `text/plain; charset=UTF-8` and
/// `text/plain;charset=utf-8` are the same offer, and a compositor is free to
/// spell it either way.
pub fn format_of(mime: &str) -> Option<ClipboardFormat> {
    let canonical = canonical_mime(mime);
    PREFERENCE.into_iter().find(|format| {
        mimes_for(*format)
            .iter()
            .any(|m| canonical_mime(m) == canonical)
    })
}

/// Lowercase, with the whitespace around parameters removed.
///
/// `UTF8_STRING` survives this because it is matched after the same treatment on
/// both sides.
fn canonical_mime(mime: &str) -> String {
    mime.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The portal-side half: everything that needs a live session.
///
/// A trait rather than a concrete type so that every rule in this file is
/// testable with no desktop. The one implementation is `Requests` in
/// [`super::clipboard_portal`], which hops onto the portal thread and blocks
/// until it answers.
pub trait SelectionTransport: Send + Sync {
    /// Read the current selection as `mime`, blocking the caller.
    fn selection_read(&self, mime: &str) -> Result<Vec<u8>>;

    /// Offer a new selection carrying exactly `mimes`.
    ///
    /// The bytes are not passed: they are staged on [`ClipboardState`] first,
    /// because the portal may ask for them the instant this returns.
    fn set_selection(&self, mimes: &[&'static str]) -> Result<()>;
}

/// The error for a live session whose clipboard toggle was refused.
///
/// Its own function because it is the one clipboard failure that is neither the
/// session being unavailable nor a bad payload: the user granted remote
/// interaction and withheld the clipboard, which is a decision they can revisit
/// and so must be reported as a permission rather than as a fault.
pub fn not_granted() -> PlatformError {
    PlatformError::PermissionDenied(
        "the desktop portal session was granted without clipboard access; \
         re-run the agent and enable \"Allow Clipboard Access\" in the dialog"
            .into(),
    )
}

/// Everything the backend knows about the clipboard right now.
///
/// Shared between the portal thread, which writes what the compositor reports,
/// and [`super::WaylandClipboard`], which reads it on whichever thread the agent
/// calls the trait on.
#[derive(Default)]
pub struct ClipboardState {
    /// Bumped on every selection change the portal reports.
    ///
    /// Never derived from the content: the point of
    /// [`crate::traits::ClipboardAccess::change_serial`] is that noticing "nothing
    /// happened" must not cost a megabyte of image data crossing the process
    /// boundary, and the portal has a real event to count.
    serial: AtomicU64,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// `Some` only while a session with clipboard access is live.
    transport: Option<Arc<dyn SelectionTransport>>,
    /// MIME types the current selection offers, as the portal last reported them.
    offered: Vec<String>,
    /// Whether the selection now on the clipboard is one this session put there.
    ours: bool,
    /// The bytes behind every offer this backend has made, by MIME type.
    ///
    /// Held for as long as the offer stands, because a paste in another
    /// application arrives as a `SelectionTransfer` asking for them — possibly
    /// minutes later, and possibly several times.
    staged: Vec<(&'static str, Arc<[u8]>)>,
}

impl ClipboardState {
    pub fn new() -> Self {
        Self::default()
    }

    /// A clipboard-enabled session came up.
    pub fn attach(&self, transport: Arc<dyn SelectionTransport>) {
        self.lock().transport = Some(transport);
    }

    /// The session went away.
    ///
    /// The staged bytes go with it: the compositor drops our offer when the
    /// session closes, so keeping them would only let a later read answer from a
    /// selection nobody can still paste.
    pub fn detach(&self) {
        let mut inner = self.lock();
        inner.transport = None;
        inner.offered.clear();
        inner.staged.clear();
        inner.ours = false;
    }

    /// The transport, if a clipboard-enabled session is live.
    pub fn transport(&self) -> Option<Arc<dyn SelectionTransport>> {
        self.lock().transport.clone()
    }

    /// A new selection is on the clipboard.
    ///
    /// The serial moves for our own writes too, exactly as
    /// `GetClipboardSequenceNumber` does on Windows: the contents did change, and
    /// a backend that hid its own writes would make the two platforms disagree
    /// about what the counter means. Whether the change was ours is recorded
    /// rather than acted on — see [`ClipboardState::selection_is_ours`].
    pub fn selection_changed(&self, mimes: Vec<String>, session_is_owner: Option<bool>) {
        {
            let mut inner = self.lock();
            inner.ours = session_is_owner.unwrap_or(false);
            // Somebody else's selection replaces ours, and the bytes behind our
            // offer can never be asked for again.
            if !inner.ours {
                inner.staged.clear();
            }
            inner.offered = mimes;
        }
        self.bump();
    }

    /// Whether the selection now on the clipboard was put there by this session.
    ///
    /// Reported by the portal alongside the change. Nothing in this backend acts
    /// on it — the trait has no place to say it — but it is what the agent-side
    /// clipboard loop (issue #9) needs to avoid sending a peer back the very
    /// content it just sent us.
    pub fn selection_is_ours(&self) -> bool {
        self.lock().ours
    }

    pub fn change_serial(&self) -> u64 {
        self.serial.load(Ordering::Relaxed)
    }

    /// The bytes behind an offer this backend made, for a `SelectionTransfer`.
    pub fn staged_for(&self, mime: &str) -> Option<Arc<[u8]>> {
        let canonical = canonical_mime(mime);
        self.lock()
            .staged
            .iter()
            .find(|(m, _)| canonical_mime(m) == canonical)
            .map(|(_, bytes)| Arc::clone(bytes))
    }

    /// What the current selection can be read as.
    ///
    /// Empty rather than an error when the clipboard holds nothing this backend
    /// understands: a clipboard carrying only some application's private format
    /// is not a failure, it is a clipboard with nothing to sync.
    pub fn available_formats(&self) -> Vec<ClipboardFormat> {
        let offered = self.lock().offered.clone();
        PREFERENCE
            .into_iter()
            .filter(|format| offered.iter().any(|mime| format_of(mime) == Some(*format)))
            .collect()
    }

    /// Read the selection in one format.
    ///
    /// A selection this session owns is answered from the staged bytes rather
    /// than from the portal, and that is not an optimisation. Measured on the
    /// alpha target: `SelectionRead` for a selection the calling session owns
    /// fails with `org.freedesktop.portal.Error.Failed: Internal error`. The
    /// portal will not read a selection back to whoever put it there — reasonably
    /// enough, since answering would mean asking this same process for the bytes
    /// it already holds. Without this branch every read after a write of our own
    /// would fail, which is the ordinary case for an agent that notices the serial
    /// move and re-reads.
    pub fn read(
        &self,
        transport: &dyn SelectionTransport,
        format: ClipboardFormat,
    ) -> Result<Vec<u8>> {
        let mime = self
            .pick_read_mime(format)
            .ok_or(PlatformError::ClipboardEmpty)?;
        if let Some(staged) = self.staged_if_ours(&mime) {
            return to_wire(format, &staged);
        }
        let raw = transport.selection_read(&mime)?;
        to_wire(format, &raw)
    }

    /// The bytes behind our own offer, when the selection is still ours.
    ///
    /// Both halves matter: `ours` alone would answer from bytes another
    /// application has since replaced, and a staged payload alone would answer
    /// after somebody else had taken the selection over.
    fn staged_if_ours(&self, mime: &str) -> Option<Arc<[u8]>> {
        let inner = self.lock();
        if !inner.ours {
            return None;
        }
        let canonical = canonical_mime(mime);
        inner
            .staged
            .iter()
            .find(|(m, _)| canonical_mime(m) == canonical)
            .map(|(_, bytes)| Arc::clone(bytes))
    }

    /// Put one format on the clipboard, replacing whatever was there.
    ///
    /// One format at a time, and replacing rather than adding, because that is
    /// what `SetSelection` does and what the Windows backend does: a second write
    /// is a new selection, not a second flavour of the old one.
    pub fn write(
        &self,
        transport: &dyn SelectionTransport,
        format: ClipboardFormat,
        data: &[u8],
    ) -> Result<()> {
        let payload: Arc<[u8]> = Arc::from(from_wire(format, data)?);
        let mimes = mimes_for(format);

        // Staged *before* the offer. The portal is free to signal a transfer the
        // moment `SetSelection` returns, and an offer whose bytes are not there
        // yet is the paste that hangs in the receiving application rather than
        // failing.
        self.stage(mimes, payload);
        match transport.set_selection(mimes) {
            Ok(()) => {
                // Recorded here rather than waiting for the portal's echo. The
                // echo does arrive, and quickly, but a read between the two would
                // go to the portal for a selection this session owns — which is
                // the one thing `SelectionRead` refuses. See [`ClipboardState::read`].
                self.own(mimes);
                // The portal echoes the change back as a `SelectionOwnerChanged`,
                // which is what normally moves the serial. Moving it here as well
                // means a compositor that does not echo still reports the change,
                // and a doubled bump is harmless: the counter promises to differ,
                // not to count.
                self.bump();
                Ok(())
            }
            Err(e) => {
                self.lock().staged.clear();
                Err(e)
            }
        }
    }

    /// Which offered MIME type to ask for, honouring the aliases.
    fn pick_read_mime(&self, format: ClipboardFormat) -> Option<String> {
        let offered = self.lock().offered.clone();
        // In our own preference order rather than the compositor's, so that a
        // selection offering both `text/plain;charset=utf-8` and bare
        // `text/plain` is read as the one that states its encoding.
        for candidate in mimes_for(format) {
            if let Some(found) = offered
                .iter()
                .find(|mime| canonical_mime(mime) == canonical_mime(candidate))
            {
                return Some(found.clone());
            }
        }
        None
    }

    fn stage(&self, mimes: &[&'static str], payload: Arc<[u8]>) {
        let mut inner = self.lock();
        inner.staged = mimes
            .iter()
            .map(|mime| (*mime, Arc::clone(&payload)))
            .collect();
    }

    /// Record that the offer went through, without waiting for the portal's echo.
    fn own(&self, mimes: &[&'static str]) {
        let mut inner = self.lock();
        inner.ours = true;
        inner.offered = mimes.iter().map(|m| (*m).to_string()).collect();
    }

    fn bump(&self) {
        self.serial.fetch_add(1, Ordering::Relaxed);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Poisoning means the portal thread panicked mid-update. Refusing to read
        // would turn that into a panic in the agent's clipboard loop, which is
        // strictly worse than answering from state that may be one update stale.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Convert what the compositor handed over into what the wire format promises.
pub fn to_wire(format: ClipboardFormat, raw: &[u8]) -> Result<Vec<u8>> {
    check_size(raw.len(), "the clipboard")?;
    match format {
        ClipboardFormat::Utf8Text => Ok(valid_utf8(raw, "clipboard text")?.into()),
        ClipboardFormat::Html => Ok(valid_utf8(raw, "clipboard html")?.into()),
        ClipboardFormat::Png => Ok(raw.to_vec()),
        ClipboardFormat::FileList => {
            let list = valid_utf8(raw, "clipboard uri list")?;
            let paths = paths_from_uri_list(list)?;
            let joined = paths.join("\n");
            check_size(joined.len(), "the clipboard's file list")?;
            Ok(joined.into_bytes())
        }
    }
}

/// Convert what the wire format promises into what the compositor is offered.
pub fn from_wire(format: ClipboardFormat, data: &[u8]) -> Result<Vec<u8>> {
    check_size(data.len(), "the payload")?;
    match format {
        ClipboardFormat::Utf8Text => Ok(valid_utf8(data, "clipboard text")?.into()),
        ClipboardFormat::Html => Ok(valid_utf8(data, "clipboard html")?.into()),
        ClipboardFormat::Png => Ok(data.to_vec()),
        ClipboardFormat::FileList => {
            let text = valid_utf8(data, "clipboard file list")?;
            let list = uri_list_from_paths(text)?;
            check_size(list.len(), "the payload's uri list")?;
            Ok(list.into_bytes())
        }
    }
}

fn check_size(len: usize, what: &str) -> Result<()> {
    if len > MAX_CLIPBOARD_BYTES {
        return Err(PlatformError::Other(format!(
            "{what} holds {len} bytes; the protocol carries at most {MAX_CLIPBOARD_BYTES}"
        )));
    }
    Ok(())
}

fn valid_utf8<'a>(bytes: &'a [u8], what: &str) -> Result<&'a str> {
    std::str::from_utf8(bytes).map_err(|e| PlatformError::Malformed(format!("{what}: {e}")))
}

/// Newline-separated absolute paths → a `text/uri-list`.
///
/// The one conversion in this backend with real work in it. The wire format is
/// paths; `text/uri-list` is `file://` URIs with percent-encoding, CRLF line
/// endings (RFC 2483) and comment lines. Nothing above
/// [`crate::traits::ClipboardAccess`] knows about any of that.
pub fn uri_list_from_paths(paths: &str) -> Result<String> {
    let mut out = String::new();
    let mut any = false;
    for line in paths.lines() {
        let path = line.trim_end_matches('\r');
        if path.is_empty() {
            continue;
        }
        if !path.starts_with('/') {
            return Err(PlatformError::Malformed(format!(
                "clipboard file list holds a relative path: {path}"
            )));
        }
        out.push_str("file://");
        percent_encode_into(path.as_bytes(), &mut out);
        // CRLF, and a trailing one, because that is what RFC 2483 asks for and
        // what GTK and Firefox put on the clipboard — a receiver that splits on
        // it and drops the empty tail is the common case, and one that does not
        // would mis-read the desktop's own file manager too.
        out.push_str("\r\n");
        any = true;
    }
    if !any {
        return Err(PlatformError::Malformed(
            "clipboard file list holds no paths".into(),
        ));
    }
    Ok(out)
}

/// A `text/uri-list` → newline-separated absolute paths.
pub fn paths_from_uri_list(list: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for line in list.lines() {
        let entry = line.trim_end_matches('\r').trim();
        // RFC 2483: a line beginning with `#` is a comment, not a URI.
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        match file_uri_to_path(entry) {
            Ok(path) => out.push(path),
            // A uri-list is allowed to hold things that are not local files —
            // dragging a link out of a browser produces one. Those are not file
            // lists, and refusing the whole selection over them would lose the
            // real paths sitting beside them.
            Err(reason) => {
                tracing::debug!(uri = %entry, reason = %reason, "skipping a clipboard URI that is not a local file");
            }
        }
    }
    if out.is_empty() {
        // Every entry was a comment, a remote URI, or something else with no
        // local path behind it. There is no file list here to hand upwards.
        return Err(PlatformError::ClipboardEmpty);
    }
    Ok(out)
}

/// One `file://` URI → one absolute path.
///
/// `Err` carries why it was skipped, for the log; it is never surfaced to the
/// caller, because a uri-list may legitimately mix local files with other
/// schemes.
fn file_uri_to_path(uri: &str) -> std::result::Result<String, String> {
    let rest = strip_scheme(uri).ok_or_else(|| "not a file:// URI".to_string())?;
    // `file://host/path`. An empty authority and `localhost` both mean this
    // machine; anything else names a path that is not on it.
    let path = match rest.strip_prefix("//") {
        Some(after) => {
            let (authority, path) = match after.find('/') {
                Some(slash) => after.split_at(slash),
                // `file://somehost` with no path at all.
                None => return Err("no path after the authority".into()),
            };
            if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
                return Err(format!("names another host: {authority}"));
            }
            path
        }
        // `file:/path` — legal, and what some libraries emit.
        None => rest,
    };
    // Fragments and queries have no meaning for a local file, and a `#` in a
    // real filename would have been percent-encoded.
    let path = path.split(['?', '#']).next().unwrap_or(path);

    let decoded = percent_decode(path)?;
    let decoded =
        String::from_utf8(decoded).map_err(|e| format!("the path is not valid UTF-8: {e}"))?;
    if !decoded.starts_with('/') {
        return Err(format!("is not an absolute path: {decoded}"));
    }
    // The wire format separates paths with newlines, so a path containing one
    // would silently become two paths on the peer.
    if decoded.contains('\n') {
        return Err("the path contains a newline".into());
    }
    Ok(decoded)
}

fn strip_scheme(uri: &str) -> Option<&str> {
    let (scheme, rest) = uri.split_once(':')?;
    scheme.eq_ignore_ascii_case("file").then_some(rest)
}

/// Bytes that may appear unescaped in the path of a `file://` URI.
///
/// Unreserved (RFC 3986 §2.3) plus the sub-delimiters and `:@` that `pchar`
/// admits, plus `/` itself. The same set GLib's `g_filename_to_uri` leaves alone,
/// so a path this backend puts on the clipboard is spelled the way the desktop's
/// own file manager spells it.
fn unescaped(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&byte)
}

fn percent_encode_into(bytes: &[u8], out: &mut String) {
    for &byte in bytes {
        if unescaped(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0xf));
        }
    }
}

fn hex_digit(nibble: u8) -> char {
    // Uppercase, as RFC 3986 §2.1 prefers for the ones a producer emits.
    char::from(b"0123456789ABCDEF"[nibble as usize])
}

fn percent_decode(text: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let hex = bytes
            .get(i + 1..i + 3)
            .ok_or_else(|| "a percent escape is cut short".to_string())?;
        let high = hex_value(hex[0])?;
        let low = hex_value(hex[1])?;
        out.push(high << 4 | low);
        i += 3;
    }
    Ok(out)
}

fn hex_value(byte: u8) -> std::result::Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!(
            "a percent escape holds {:?}, which is not a hex digit",
            byte as char
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport that records what it was asked and answers from a script.
    #[derive(Default)]
    struct FakeTransport {
        selection: Mutex<Vec<u8>>,
        offered: Mutex<Vec<&'static str>>,
        asked: Mutex<Vec<String>>,
        fail: bool,
    }

    impl SelectionTransport for FakeTransport {
        fn selection_read(&self, mime: &str) -> Result<Vec<u8>> {
            self.asked.lock().unwrap().push(mime.to_string());
            Ok(self.selection.lock().unwrap().clone())
        }

        fn set_selection(&self, mimes: &[&'static str]) -> Result<()> {
            if self.fail {
                return Err(PlatformError::Other("the portal said no".into()));
            }
            *self.offered.lock().unwrap() = mimes.to_vec();
            Ok(())
        }
    }

    fn state_offering(mimes: &[&str]) -> ClipboardState {
        let state = ClipboardState::new();
        state.selection_changed(mimes.iter().map(|m| m.to_string()).collect(), Some(false));
        state
    }

    #[test]
    fn every_format_maps_to_the_mime_type_the_issue_fixed() {
        assert_eq!(mimes_for(ClipboardFormat::Utf8Text)[0], MIME_TEXT);
        assert_eq!(mimes_for(ClipboardFormat::Html)[0], MIME_HTML);
        assert_eq!(mimes_for(ClipboardFormat::Png)[0], MIME_PNG);
        assert_eq!(mimes_for(ClipboardFormat::FileList)[0], MIME_URI_LIST);
        for format in PREFERENCE {
            for mime in mimes_for(format) {
                assert_eq!(format_of(mime), Some(format), "{mime}");
            }
        }
    }

    #[test]
    fn a_mime_type_spelled_differently_is_still_the_same_offer() {
        // Compositors are free to write the parameter with a space, or the
        // charset in capitals; both are the same media type and a backend that
        // missed one would report a clipboard with text on it as empty.
        for spelling in [
            "text/plain; charset=utf-8",
            "text/plain;charset=UTF-8",
            "TEXT/PLAIN;CHARSET=UTF-8",
        ] {
            assert_eq!(
                format_of(spelling),
                Some(ClipboardFormat::Utf8Text),
                "{spelling}"
            );
        }
        assert_eq!(format_of("application/x-libreoffice-internal"), None);
    }

    #[test]
    fn formats_are_reported_richest_first_and_only_when_offered() {
        let state = state_offering(&["text/plain;charset=utf-8", "text/html", "image/png"]);
        assert_eq!(
            state.available_formats(),
            vec![
                ClipboardFormat::Png,
                ClipboardFormat::Html,
                ClipboardFormat::Utf8Text
            ],
            "a peer offered several formats must be able to pick the richest"
        );
    }

    #[test]
    fn a_clipboard_holding_nothing_we_understand_is_empty_rather_than_broken() {
        // Somebody copied a spreadsheet cell range in a private format. That is a
        // clipboard with nothing to sync, not a backend failure — reporting an
        // error here would have the agent stop advertising the clipboard.
        let state = state_offering(&["application/x-qt-windows-mime"]);
        assert!(state.available_formats().is_empty());
    }

    #[test]
    fn the_change_serial_moves_on_a_change_and_holds_still_otherwise() {
        let state = ClipboardState::new();
        let before = state.change_serial();
        assert_eq!(state.change_serial(), before, "reading it must not move it");
        state.selection_changed(vec![MIME_TEXT.into()], Some(false));
        let after = state.change_serial();
        assert_ne!(after, before);
        assert_eq!(state.change_serial(), after);
        state.selection_changed(vec![MIME_PNG.into()], Some(false));
        assert_ne!(state.change_serial(), after);
    }

    #[test]
    fn our_own_write_moves_the_serial_too() {
        // `GetClipboardSequenceNumber` on Windows moves for the writer as well,
        // and the counter is a cross-platform contract: it promises the contents
        // changed, not that somebody else changed them. Which side changed it is
        // `selection_is_ours`.
        let state = ClipboardState::new();
        let transport = FakeTransport::default();
        let before = state.change_serial();
        state
            .write(&transport, ClipboardFormat::Utf8Text, b"hello")
            .unwrap();
        assert_ne!(state.change_serial(), before);
    }

    #[test]
    fn the_portal_saying_the_selection_is_ours_is_recorded() {
        let state = ClipboardState::new();
        assert!(!state.selection_is_ours());
        state.selection_changed(vec![MIME_TEXT.into()], Some(true));
        assert!(state.selection_is_ours());
        state.selection_changed(vec![MIME_TEXT.into()], Some(false));
        assert!(!state.selection_is_ours());
        // A portal that omits the field is not claiming we own it.
        state.selection_changed(vec![MIME_TEXT.into()], None);
        assert!(!state.selection_is_ours());
    }

    #[test]
    fn text_round_trips_including_non_ascii() {
        let state = state_offering(&[MIME_TEXT]);
        let transport = FakeTransport::default();
        let text = "naïve — 日本語 — 🎉";
        *transport.selection.lock().unwrap() = text.as_bytes().to_vec();
        let read = state.read(&transport, ClipboardFormat::Utf8Text).unwrap();
        assert_eq!(read, text.as_bytes());
    }

    #[test]
    fn reading_back_our_own_selection_never_asks_the_portal() {
        // Measured on the alpha target: `SelectionRead` for a selection the
        // calling session owns fails with "Internal error". The agent notices its
        // own write move the serial and re-reads, so without this every such read
        // would fail — and it would fail against a clipboard that is perfectly
        // fine.
        let state = ClipboardState::new();
        let transport = FakeTransport::default();
        let text = "naïve — 日本語 — 🎉";
        state
            .write(&transport, ClipboardFormat::Utf8Text, text.as_bytes())
            .unwrap();
        assert_eq!(
            state.read(&transport, ClipboardFormat::Utf8Text).unwrap(),
            text.as_bytes()
        );
        assert!(
            transport.asked.lock().unwrap().is_empty(),
            "the portal refuses to read a selection back to whoever set it"
        );
        // The formats follow from the offer, without waiting for the echo either.
        assert_eq!(state.available_formats(), vec![ClipboardFormat::Utf8Text]);
    }

    #[test]
    fn once_somebody_else_copies_reads_go_back_to_the_portal() {
        // The other half of the same rule: answering from staged bytes after the
        // selection has moved on would hand a peer content that is no longer on
        // the clipboard.
        let state = ClipboardState::new();
        let transport = FakeTransport::default();
        state
            .write(&transport, ClipboardFormat::Utf8Text, b"ours")
            .unwrap();
        state.selection_changed(vec![MIME_TEXT.into()], Some(false));
        *transport.selection.lock().unwrap() = b"theirs".to_vec();
        assert_eq!(
            state.read(&transport, ClipboardFormat::Utf8Text).unwrap(),
            b"theirs"
        );
    }

    #[test]
    fn a_png_is_handed_over_byte_for_byte() {
        let state = state_offering(&[MIME_PNG]);
        let transport = FakeTransport::default();
        // A PNG signature followed by bytes that are not valid UTF-8, which is
        // the point: nothing on the image path may try to read it as text.
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe];
        state.write(&transport, ClipboardFormat::Png, &png).unwrap();
        *transport.selection.lock().unwrap() = state.staged_for(MIME_PNG).unwrap().to_vec();
        assert_eq!(state.read(&transport, ClipboardFormat::Png).unwrap(), png);
    }

    #[test]
    fn html_round_trips_unwrapped() {
        // Unlike Windows, which has to wrap and unwrap the CF_HTML header, the
        // portal carries `text/html` as-is. Anything added here would arrive in a
        // peer's editor as visible markup.
        let state = state_offering(&[MIME_HTML]);
        let transport = FakeTransport::default();
        let html = "<p>a <b>bold</b> claim</p>";
        state
            .write(&transport, ClipboardFormat::Html, html.as_bytes())
            .unwrap();
        let staged = state.staged_for(MIME_HTML).unwrap();
        assert_eq!(&*staged, html.as_bytes(), "no wrapper is added");
        *transport.selection.lock().unwrap() = staged.to_vec();
        assert_eq!(
            state.read(&transport, ClipboardFormat::Html).unwrap(),
            html.as_bytes()
        );
    }

    #[test]
    fn a_file_list_becomes_uris_and_comes_back_as_paths() {
        let paths = "/home/ada/notes.txt\n/home/ada/holiday photo.png";
        let list = uri_list_from_paths(paths).unwrap();
        assert_eq!(
            list,
            "file:///home/ada/notes.txt\r\nfile:///home/ada/holiday%20photo.png\r\n"
        );
        assert_eq!(
            paths_from_uri_list(&list).unwrap(),
            vec!["/home/ada/notes.txt", "/home/ada/holiday photo.png"]
        );
    }

    #[test]
    fn a_file_list_round_trips_through_the_wire_conversions() {
        let paths = "/tmp/a b\n/tmp/ünïcode\n/tmp/100%.txt\n/tmp/hash#tag";
        let offered = from_wire(ClipboardFormat::FileList, paths.as_bytes()).unwrap();
        let back = to_wire(ClipboardFormat::FileList, &offered).unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), paths);
    }

    #[test]
    fn the_characters_a_naive_encoder_gets_wrong_survive() {
        // `#` would otherwise be read as a fragment, `%` as the start of an
        // escape, and a space would split the URI. Each of these is a real
        // filename and each fails silently if it is not escaped.
        let list = uri_list_from_paths("/tmp/a#b\n/tmp/100%\n/tmp/a b\n/tmp/q?x").unwrap();
        assert!(list.contains("file:///tmp/a%23b"), "{list}");
        assert!(list.contains("file:///tmp/100%25"), "{list}");
        assert!(list.contains("file:///tmp/a%20b"), "{list}");
        assert!(list.contains("file:///tmp/q%3Fx"), "{list}");
        assert_eq!(
            paths_from_uri_list(&list).unwrap(),
            vec!["/tmp/a#b", "/tmp/100%", "/tmp/a b", "/tmp/q?x"]
        );
    }

    #[test]
    fn the_characters_a_naive_encoder_over_escapes_are_left_alone() {
        // GLib leaves these unescaped, so escaping them here would mean a path
        // this backend puts on the clipboard is spelled differently from the same
        // path put there by the file manager — a difference that shows up as a
        // mismatch in whatever compares them.
        let list = uri_list_from_paths("/tmp/it's (a+b),v=1:2@x").unwrap();
        assert_eq!(list, "file:///tmp/it's%20(a+b),v=1:2@x\r\n");
    }

    #[test]
    fn a_lowercase_escape_from_another_producer_decodes_the_same() {
        assert_eq!(
            paths_from_uri_list("file:///tmp/a%2fb%20c").unwrap(),
            vec!["/tmp/a/b c"]
        );
    }

    #[test]
    fn comments_blank_lines_and_bare_lf_are_all_accepted_on_the_way_in() {
        // RFC 2483 asks for CRLF and comment lines; producers vary. Losing a real
        // path because the producer used bare LF would look like a dropped file.
        let list = "# some comment\r\nfile:///tmp/a\n\r\nfile:///tmp/b\r\n";
        assert_eq!(paths_from_uri_list(list).unwrap(), vec!["/tmp/a", "/tmp/b"]);
    }

    #[test]
    fn localhost_and_an_empty_authority_are_both_this_machine() {
        assert_eq!(
            paths_from_uri_list("file:///tmp/a\r\nfile://localhost/tmp/b\r\nfile:/tmp/c").unwrap(),
            vec!["/tmp/a", "/tmp/b", "/tmp/c"]
        );
    }

    #[test]
    fn a_uri_on_another_host_is_skipped_rather_than_turned_into_a_local_path() {
        // `file://fileserver/share/x` is not `/share/x` on this machine, and
        // handing that path to a peer would have it open — or overwrite — the
        // wrong file.
        let list = "file://fileserver/share/x\r\nfile:///tmp/real\r\n";
        assert_eq!(paths_from_uri_list(list).unwrap(), vec!["/tmp/real"]);
    }

    #[test]
    fn a_link_dragged_out_of_a_browser_is_not_a_file_list() {
        // Browsers put `text/uri-list` on the clipboard for links too. There are
        // no paths in one, and inventing them would be worse than saying so.
        let err = paths_from_uri_list("https://example.com/thing\r\n").unwrap_err();
        assert!(matches!(err, PlatformError::ClipboardEmpty), "{err:?}");
    }

    #[test]
    fn a_path_carrying_a_newline_is_refused_rather_than_split_in_two() {
        // The wire format separates paths with newlines, so this would silently
        // become two paths — one of them nonexistent — on the peer.
        let err = paths_from_uri_list("file:///tmp/two%0Alines\r\n").unwrap_err();
        assert!(matches!(err, PlatformError::ClipboardEmpty), "{err:?}");
    }

    #[test]
    fn a_relative_path_is_refused_because_the_peer_has_no_way_to_resolve_it() {
        let err = uri_list_from_paths("notes.txt").unwrap_err();
        assert!(matches!(err, PlatformError::Malformed(_)), "{err:?}");
        let err = from_wire(ClipboardFormat::FileList, b"").unwrap_err();
        assert!(matches!(err, PlatformError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn a_truncated_percent_escape_does_not_produce_half_a_path() {
        let err = paths_from_uri_list("file:///tmp/a%2\r\n").unwrap_err();
        assert!(matches!(err, PlatformError::ClipboardEmpty), "{err:?}");
        let err = paths_from_uri_list("file:///tmp/a%zz\r\n").unwrap_err();
        assert!(matches!(err, PlatformError::ClipboardEmpty), "{err:?}");
    }

    #[test]
    fn content_over_the_frame_limit_is_refused_here_with_the_size_in_the_message() {
        // `MAX_FRAME_LEN` is what the codec will carry. Letting this through would
        // fail at the frame writer instead, and take the peer session down with
        // it rather than one paste.
        let big = vec![b'x'; MAX_CLIPBOARD_BYTES + 1];
        let err = from_wire(ClipboardFormat::Png, &big).unwrap_err();
        assert!(
            err.to_string().contains(&MAX_CLIPBOARD_BYTES.to_string()),
            "{err}"
        );
        let err = to_wire(ClipboardFormat::Png, &big).unwrap_err();
        assert!(matches!(err, PlatformError::Other(_)), "{err:?}");
        // And exactly the limit is fine, so the message is about the limit rather
        // than about being close to it.
        assert!(from_wire(ClipboardFormat::Png, &big[..MAX_CLIPBOARD_BYTES]).is_ok());
    }

    #[test]
    fn a_size_refusal_is_not_something_the_agent_will_retry_forever() {
        let big = vec![b'x'; MAX_CLIPBOARD_BYTES + 1];
        let err = from_wire(ClipboardFormat::Png, &big).unwrap_err();
        assert!(!err.is_transient());
    }

    #[test]
    fn text_that_is_not_utf8_is_reported_as_malformed_rather_than_mangled() {
        let err = to_wire(ClipboardFormat::Utf8Text, &[0xff, 0xfe]).unwrap_err();
        assert!(matches!(err, PlatformError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn reading_a_format_the_clipboard_does_not_hold_is_empty_not_a_failure() {
        let state = state_offering(&[MIME_TEXT]);
        let transport = FakeTransport::default();
        let err = state.read(&transport, ClipboardFormat::Png).unwrap_err();
        assert!(matches!(err, PlatformError::ClipboardEmpty), "{err:?}");
        assert!(
            transport.asked.lock().unwrap().is_empty(),
            "nothing should have been asked of the portal"
        );
    }

    #[test]
    fn the_charset_bearing_spelling_is_preferred_when_both_are_offered() {
        let state = state_offering(&["text/plain", "text/plain;charset=utf-8"]);
        let transport = FakeTransport::default();
        state.read(&transport, ClipboardFormat::Utf8Text).unwrap();
        assert_eq!(
            transport.asked.lock().unwrap()[0],
            "text/plain;charset=utf-8"
        );
    }

    #[test]
    fn an_application_offering_only_bare_text_plain_can_still_be_read() {
        let state = state_offering(&["text/plain"]);
        assert_eq!(state.available_formats(), vec![ClipboardFormat::Utf8Text]);
        let transport = FakeTransport::default();
        *transport.selection.lock().unwrap() = b"plain".to_vec();
        assert_eq!(
            state.read(&transport, ClipboardFormat::Utf8Text).unwrap(),
            b"plain"
        );
        assert_eq!(transport.asked.lock().unwrap()[0], "text/plain");
    }

    #[test]
    fn the_bytes_behind_an_offer_are_there_before_the_offer_is_made() {
        // The gotcha this whole design is shaped around: a transfer request can
        // arrive the instant `SetSelection` returns, and an offer with nothing
        // behind it hangs the pasting application rather than failing it.
        struct AsksDuringSet(Arc<ClipboardState>);
        impl SelectionTransport for AsksDuringSet {
            fn selection_read(&self, _: &str) -> Result<Vec<u8>> {
                unreachable!()
            }
            fn set_selection(&self, _: &[&'static str]) -> Result<()> {
                assert!(
                    self.0.staged_for(MIME_TEXT).is_some(),
                    "the portal could ask for these bytes right now"
                );
                Ok(())
            }
        }
        let state = Arc::new(ClipboardState::new());
        let transport = AsksDuringSet(Arc::clone(&state));
        state
            .write(&transport, ClipboardFormat::Utf8Text, b"hi")
            .unwrap();
    }

    #[test]
    fn every_alias_of_an_offered_format_serves_the_same_bytes() {
        let state = ClipboardState::new();
        let transport = FakeTransport::default();
        state
            .write(&transport, ClipboardFormat::Utf8Text, b"shared")
            .unwrap();
        for mime in TEXT_MIMES {
            assert_eq!(
                &*state.staged_for(mime).expect(mime),
                b"shared",
                "an offered {mime} that answers nothing hangs a paste"
            );
        }
        assert_eq!(*transport.offered.lock().unwrap(), TEXT_MIMES.to_vec());
    }

    #[test]
    fn an_offer_the_portal_refused_leaves_nothing_staged() {
        // Otherwise the backend would hold bytes for a selection it does not own,
        // and answer a transfer request meant for whoever really does.
        let state = ClipboardState::new();
        let transport = FakeTransport {
            fail: true,
            ..Default::default()
        };
        assert!(state
            .write(&transport, ClipboardFormat::Utf8Text, b"nope")
            .is_err());
        assert!(state.staged_for(MIME_TEXT).is_none());
    }

    #[test]
    fn somebody_elses_copy_drops_the_bytes_behind_our_offer() {
        // Once another application owns the selection, no transfer request for
        // ours can ever arrive; holding megabytes of image for it would be a leak
        // that lasts as long as the agent runs.
        let state = ClipboardState::new();
        let transport = FakeTransport::default();
        state
            .write(&transport, ClipboardFormat::Png, &[0x89, b'P'])
            .unwrap();
        assert!(state.staged_for(MIME_PNG).is_some());
        state.selection_changed(vec![MIME_TEXT.into()], Some(false));
        assert!(state.staged_for(MIME_PNG).is_none());
    }

    #[test]
    fn our_own_copy_keeps_the_bytes_behind_the_offer() {
        // The portal echoes our own `SetSelection` back as a change with
        // `session_is_owner` set. Dropping the staged bytes on that echo would
        // discard exactly the content the next paste is going to ask for.
        let state = ClipboardState::new();
        let transport = FakeTransport::default();
        state
            .write(&transport, ClipboardFormat::Utf8Text, b"still here")
            .unwrap();
        state.selection_changed(vec![MIME_TEXT.into()], Some(true));
        assert_eq!(&*state.staged_for(MIME_TEXT).unwrap(), b"still here");
    }

    #[test]
    fn losing_the_session_takes_the_clipboard_state_with_it() {
        let state = ClipboardState::new();
        state.attach(Arc::new(FakeTransport::default()));
        let transport = FakeTransport::default();
        state
            .write(&transport, ClipboardFormat::Utf8Text, b"gone soon")
            .unwrap();
        state.selection_changed(vec![MIME_TEXT.into()], Some(true));

        state.detach();
        assert!(state.transport().is_none());
        assert!(state.available_formats().is_empty());
        assert!(state.staged_for(MIME_TEXT).is_none());
        assert!(!state.selection_is_ours());
    }

    #[test]
    fn a_refused_clipboard_reads_as_a_permission_the_user_can_still_grant() {
        // Distinct from the session being unavailable: remote interaction was
        // granted and the clipboard toggle was not, which is something the user
        // can go back and change.
        assert!(matches!(not_granted(), PlatformError::PermissionDenied(_)));
    }
}
