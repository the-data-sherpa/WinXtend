//! The Windows `CF_HTML` clipboard format.
//!
//! Windows does not put bare HTML on the clipboard. `CF_HTML` is the HTML wrapped
//! in a header of byte offsets that tell the pasting application which part of the
//! document is the actual selection. Get the offsets wrong and applications paste
//! nothing, or paste the header text itself as visible content — a failure that
//! looks like "rich paste is broken between these two machines" and is invisible
//! from the sending side.
//!
//! Kept as pure byte manipulation so the offsets can be tested without a clipboard.

/// Fixed width for every offset field, so the header can be built once and then
/// patched in place.
///
/// Writing the offsets after the fact is the only way to get them right: each one
/// is a byte offset into the finished document, and the document's length depends
/// on the digits used to write them.
const OFFSET_DIGITS: usize = 10;

const PREFIX: &str = "<html><body>\r\n<!--StartFragment-->";
const SUFFIX: &str = "<!--EndFragment-->\r\n</body></html>";

/// Wrap an HTML fragment in a valid `CF_HTML` document.
pub fn wrap_cf_html(fragment: &str) -> Vec<u8> {
    // Built with zero-filled offsets first, then patched, because the offsets are
    // measured against the finished bytes.
    let header = format!(
        "Version:0.9\r\nStartHTML:{0:0width$}\r\nEndHTML:{0:0width$}\r\nStartFragment:{0:0width$}\r\nEndFragment:{0:0width$}\r\n",
        0,
        width = OFFSET_DIGITS
    );

    let mut doc = header;
    let start_html = doc.len();
    doc.push_str(PREFIX);
    let start_fragment = doc.len();
    doc.push_str(fragment);
    let end_fragment = doc.len();
    doc.push_str(SUFFIX);
    let end_html = doc.len();

    let mut bytes = doc.into_bytes();
    for (field, value) in [
        ("StartHTML:", start_html),
        ("EndHTML:", end_html),
        ("StartFragment:", start_fragment),
        ("EndFragment:", end_fragment),
    ] {
        patch_offset(&mut bytes, field, value);
    }

    // CF_HTML is a NUL-terminated byte string; some readers rely on it.
    bytes.push(0);
    bytes
}

fn patch_offset(bytes: &mut [u8], field: &str, value: usize) {
    let Some(at) = find(bytes, field.as_bytes()) else {
        return;
    };
    let digits = format!("{value:0width$}", width = OFFSET_DIGITS).into_bytes();
    let start = at + field.len();
    if start + digits.len() <= bytes.len() {
        bytes[start..start + digits.len()].copy_from_slice(&digits);
    }
}

/// Recover the fragment from a `CF_HTML` document.
///
/// Tries the declared offsets first, then the fragment comments, then gives back
/// the whole payload. Layered because the header is written by whichever
/// application copied the content, and plenty of them get the offsets wrong —
/// refusing to paste anything would be a worse outcome than pasting slightly too
/// much.
pub fn strip_cf_html(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);

    if let (Some(start), Some(end)) = (
        parse_offset(&text, "StartFragment:"),
        parse_offset(&text, "EndFragment:"),
    ) {
        // Offsets are byte offsets and may be nonsense; validated before slicing so
        // a hostile clipboard cannot panic us.
        if start <= end && end <= bytes.len() {
            if let Some(slice) = text.get(start..end) {
                return slice.to_string();
            }
        }
    }

    if let (Some(start), Some(end)) = (
        find(bytes, b"<!--StartFragment-->"),
        find(bytes, b"<!--EndFragment-->"),
    ) {
        let start = start + b"<!--StartFragment-->".len();
        if start <= end {
            if let Some(slice) = text.get(start..end) {
                return slice.to_string();
            }
        }
    }

    // No recognisable header at all: treat the payload as plain HTML. Peers that
    // are not Windows send exactly that.
    text.trim_end_matches('\0').to_string()
}

fn parse_offset(text: &str, field: &str) -> Option<usize> {
    let at = text.find(field)? + field.len();
    let rest = &text[at..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_offsets_point_at_the_fragment_itself() {
        // The single thing that has to be right: an application slices at these
        // offsets and pastes whatever it finds.
        let fragment = "<b>hello</b>";
        let doc = wrap_cf_html(fragment);
        let text = String::from_utf8(doc.clone()).unwrap();
        let start = parse_offset(&text, "StartFragment:").unwrap();
        let end = parse_offset(&text, "EndFragment:").unwrap();
        assert_eq!(&text[start..end], fragment);
    }

    #[test]
    fn declared_html_offsets_bracket_the_whole_document() {
        let doc = wrap_cf_html("x");
        let text = String::from_utf8(doc.clone()).unwrap();
        let start = parse_offset(&text, "StartHTML:").unwrap();
        let end = parse_offset(&text, "EndHTML:").unwrap();
        assert!(text[start..end].starts_with("<html>"));
        assert!(text[start..end].ends_with("</html>"));
    }

    #[test]
    fn wrapping_then_stripping_round_trips() {
        for fragment in ["", "plain", "<p>with <i>markup</i></p>", "unicode: åβ漢"] {
            let doc = wrap_cf_html(fragment);
            assert_eq!(strip_cf_html(&doc), fragment, "fragment {fragment:?}");
        }
    }

    #[test]
    fn offsets_stay_correct_for_multibyte_content() {
        // Offsets are bytes, not characters. Counting characters would misplace
        // every fragment containing a non-ASCII byte.
        let fragment = "漢字テスト";
        let doc = wrap_cf_html(fragment);
        assert_eq!(strip_cf_html(&doc), fragment);
    }

    #[test]
    fn the_document_is_nul_terminated() {
        assert_eq!(wrap_cf_html("x").last(), Some(&0));
    }

    #[test]
    fn a_payload_with_no_header_is_treated_as_plain_html() {
        // Non-Windows peers send bare HTML; refusing it would break rich paste in
        // one direction only.
        assert_eq!(strip_cf_html(b"<b>bare</b>"), "<b>bare</b>");
    }

    #[test]
    fn bogus_offsets_fall_back_to_the_fragment_comments() {
        // Real applications do write wrong offsets. Pasting the comment-delimited
        // fragment is better than pasting nothing.
        let doc = "Version:0.9\r\nStartFragment:0000009999\r\nEndFragment:0000000001\r\n\
                   <html><body><!--StartFragment--><b>x</b><!--EndFragment--></body></html>";
        assert_eq!(strip_cf_html(doc.as_bytes()), "<b>x</b>");
    }

    #[test]
    fn out_of_range_offsets_do_not_panic() {
        let doc = "StartFragment:9999999999\r\nEndFragment:9999999999\r\nbody";
        let _ = strip_cf_html(doc.as_bytes());
    }

    #[test]
    fn inverted_offsets_do_not_panic() {
        let doc = "StartFragment:0000000050\r\nEndFragment:0000000010\r\nbody";
        let _ = strip_cf_html(doc.as_bytes());
    }

    #[test]
    fn a_truncated_header_does_not_panic() {
        for bad in [
            &b""[..],
            &b"Version:"[..],
            &b"StartFragment:"[..],
            &b"StartFragment:abc"[..],
            &[0xff, 0xfe, 0x00][..],
        ] {
            let _ = strip_cf_html(bad);
        }
    }

    #[test]
    fn a_fragment_containing_the_marker_text_does_not_confuse_the_offsets() {
        // Offsets are authoritative; the comment search is only a fallback, so the
        // literal marker inside the content must survive.
        let fragment = "code: &lt;!--StartFragment--&gt;";
        let doc = wrap_cf_html(fragment);
        assert_eq!(strip_cf_html(&doc), fragment);
    }
}
