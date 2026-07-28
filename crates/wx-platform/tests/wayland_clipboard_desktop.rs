//! The Wayland clipboard, driven against a real desktop clipboard.
//!
//! [`ClipboardState`] is unit-tested against a scripted transport in its own
//! module; what this adds is the half those tests cannot answer — that the bytes
//! it produces are the bytes another application actually sees, and that the
//! bytes another application puts on the clipboard are the ones it hands upwards.
//! `wl-copy`/`wl-paste` are the other application, the same pair the backend was
//! validated with by hand on the alpha target.
//!
//! What stands in for the portal here is [`DesktopSelection`], which implements
//! [`SelectionTransport`] the same shape `clipboard_portal::Requests` does — a
//! read that returns the selection's bytes, and an offer that serves them out of
//! [`ClipboardState::staged_for`], which is exactly what `serve_transfer` does
//! when a `SelectionTransfer` arrives. The D-Bus calls themselves need a consent
//! dialog and so are still only reachable by hand; everything above them is here.
//!
//! Skips, rather than fails, with no Wayland session or no `wl-clipboard`
//! installed — the rule every desktop-touching test in this crate follows.
//!
//! One test, not several: they share the machine's one clipboard, and `cargo
//! test` runs tests in parallel.
#![cfg(target_os = "linux")]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wx_platform::linux_wayland::clipboard::{
    self, ClipboardState, SelectionTransport, MAX_CLIPBOARD_BYTES, MIME_HTML, MIME_PNG, MIME_TEXT,
    MIME_URI_LIST,
};
use wx_platform::{PlatformError, Result};
use wx_proto::ClipboardFormat;

/// How long to let `wl-copy` take the selection before giving up on it.
const SETTLE: Duration = Duration::from_secs(5);

/// The application at the other end of the clipboard.
///
/// Stands in for `org.freedesktop.portal.Clipboard`: `selection_read` is
/// `SelectionRead` plus draining the pipe, and `set_selection` is `SetSelection`
/// plus the `SelectionTransfer` that follows it — the bytes come out of
/// [`ClipboardState::staged_for`] in both.
struct DesktopSelection {
    state: Arc<ClipboardState>,
    /// So a read answered from our own staged bytes can be shown not to have
    /// reached the transport at all.
    reads: AtomicUsize,
    /// Every MIME list this backend has offered, in order.
    offers: Mutex<Vec<Vec<String>>>,
}

impl SelectionTransport for DesktopSelection {
    fn selection_read(&self, mime: &str) -> Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        wl_paste(mime)
    }

    fn set_selection(&self, mimes: &[&'static str]) -> Result<()> {
        self.offers
            .lock()
            .unwrap()
            .push(mimes.iter().map(|m| (*m).to_string()).collect());
        // Exactly what `serve_transfer` does: the offer carries no bytes, so they
        // are fetched back out of the staging area the write put them in.
        let payload = self
            .state
            .staged_for(mimes[0])
            .ok_or(PlatformError::ClipboardEmpty)?;
        wl_copy(mimes[0], &payload)
    }
}

#[test]
fn the_backend_moves_real_bytes_on_and_off_the_desktop_clipboard() {
    let Some(reason) = usable_desktop() else {
        return;
    };
    println!("driving the real clipboard on {reason}\n");

    let state = Arc::new(ClipboardState::new());
    let transport = DesktopSelection {
        state: Arc::clone(&state),
        reads: AtomicUsize::new(0),
        offers: Mutex::new(Vec::new()),
    };
    let restore = wl_paste(MIME_TEXT).ok();

    reading_what_another_application_copied(&state, &transport);
    writing_what_another_application_pastes(&state, &transport);
    the_file_list_is_spelled_the_way_the_desktop_spells_it(&state, &transport);
    a_large_image_round_trips_and_an_oversized_one_is_refused(&state, &transport);
    the_serial_moves_once_per_change(&state, &transport);
    our_own_selection_is_read_back_without_the_portal(&state, &transport);

    // Put the machine's clipboard back roughly as it was found.
    if let Some(previous) = restore {
        let _ = wl_copy(MIME_TEXT, &previous);
    }
    println!("\nall checks passed");
}

/// Another application copies; this backend reports and reads it.
fn reading_what_another_application_copied(state: &ClipboardState, transport: &DesktopSelection) {
    println!("== another application copies, this backend reads ==");

    let text = "café — naïve ✓ 日本語 \u{1F4CB}";
    external_copy(MIME_TEXT, text.as_bytes(), state);
    richest_is(state, ClipboardFormat::Utf8Text);
    let read = state.read(transport, ClipboardFormat::Utf8Text).unwrap();
    assert_eq!(read, text.as_bytes());
    println!("  text        {} bytes, byte-exact: {text}", read.len());

    let html = "<p>a <b>rich</b> paste — <i>naïve</i></p>";
    external_copy(MIME_HTML, html.as_bytes(), state);
    // `wl-copy` offers `text/plain` alongside whatever it was given, exactly as a
    // real editor does, so the answer here is the *order*: a peer offered both
    // must be handed the one that keeps the formatting.
    richest_is(state, ClipboardFormat::Html);
    let read = state.read(transport, ClipboardFormat::Html).unwrap();
    assert_eq!(read, html.as_bytes());
    println!("  html        {} bytes, byte-exact: {html}", read.len());

    let png = fixture_png();
    external_copy(MIME_PNG, &png, state);
    richest_is(state, ClipboardFormat::Png);
    let read = state.read(transport, ClipboardFormat::Png).unwrap();
    assert_eq!(read, png);
    println!("  png         {} bytes, byte-exact", read.len());
    save_evidence("read-from-desktop-clipboard.png", &read);

    // What a file manager puts on the clipboard: a comment line, percent
    // escapes, a URI on another host and a link that is not a file at all.
    let list = "# a uri-list from a file manager\r\n\
                file:///tmp/wx%20clip/caf%C3%A9%20%231.txt\r\n\
                file://localhost/tmp/wx%20clip/plain.txt\r\n\
                file://elsewhere/tmp/not-ours.txt\r\n\
                https://example.invalid/a-link\r\n";
    external_copy(MIME_URI_LIST, list.as_bytes(), state);
    richest_is(state, ClipboardFormat::FileList);
    let read = state.read(transport, ClipboardFormat::FileList).unwrap();
    let paths = String::from_utf8(read).unwrap();
    assert_eq!(
        paths, "/tmp/wx clip/café #1.txt\n/tmp/wx clip/plain.txt",
        "the remote host and the http link must be skipped, not turned into paths"
    );
    println!("  uri-list    -> {paths:?}");

    // A clipboard holding only a format this backend does not map is not a
    // failure — it is a clipboard with nothing to sync.
    external_copy("application/x-private-thing", b"not ours", state);
    assert!(state.available_formats().is_empty());
    assert!(matches!(
        state.read(transport, ClipboardFormat::Utf8Text),
        Err(PlatformError::ClipboardEmpty)
    ));
    println!("  a private format -> no formats, and an empty clipboard rather than an error");
}

/// This backend writes; another application pastes it.
fn writing_what_another_application_pastes(state: &ClipboardState, transport: &DesktopSelection) {
    println!("\n== this backend writes, another application pastes ==");

    let text = "from the peer: résumé ✓ 日本語";
    state
        .write(transport, ClipboardFormat::Utf8Text, text.as_bytes())
        .unwrap();
    let pasted = String::from_utf8(wl_paste(MIME_TEXT).unwrap()).unwrap();
    assert_eq!(pasted, text);
    println!("  wl-paste            -> {pasted}");

    // The aliases exist so XWayland and older toolkits can paste from the offer.
    let offered = transport.offers.lock().unwrap().last().unwrap().clone();
    assert_eq!(offered, vec![MIME_TEXT, "text/plain", "UTF8_STRING"]);
    println!("  offered as          -> {offered:?}");
    for alias in ["text/plain", "UTF8_STRING"] {
        assert_eq!(
            state.staged_for(alias).as_deref(),
            Some(text.as_bytes()),
            "every alias must serve the same bytes"
        );
    }
    println!("  every alias serves the same bytes");

    let html = "<h1>written by the backend</h1>";
    state
        .write(transport, ClipboardFormat::Html, html.as_bytes())
        .unwrap();
    let pasted = String::from_utf8(wl_paste(MIME_HTML).unwrap()).unwrap();
    assert_eq!(pasted, html);
    println!("  wl-paste text/html  -> {pasted}");

    let png = fixture_png();
    state.write(transport, ClipboardFormat::Png, &png).unwrap();
    let pasted = wl_paste(MIME_PNG).unwrap();
    assert_eq!(pasted, png, "the image another application pastes is ours");
    println!(
        "  wl-paste image/png  -> {} bytes, byte-exact",
        pasted.len()
    );
    save_evidence("written-to-desktop-clipboard.png", &pasted);
}

/// The one conversion with real work in it, checked against GLib.
fn the_file_list_is_spelled_the_way_the_desktop_spells_it(
    state: &ClipboardState,
    transport: &DesktopSelection,
) {
    println!("\n== a file list is spelled the way the desktop spells it ==");

    // Every character class a naive encoder gets wrong: a space, a `#`, a `%`,
    // a `+`, sub-delims, and non-ASCII.
    let paths = "/tmp/wx clip/a #1 100% done+more.txt\n/tmp/wx clip/rén&co (v2)@home.txt";
    state
        .write(transport, ClipboardFormat::FileList, paths.as_bytes())
        .unwrap();
    let on_clipboard = String::from_utf8(wl_paste(MIME_URI_LIST).unwrap()).unwrap();
    println!("  on the clipboard:");
    for line in on_clipboard.lines() {
        println!("    {line}");
    }
    assert!(
        on_clipboard.contains("\r\n"),
        "RFC 2483 asks for CRLF endings"
    );

    // The claim in the module docs, checked against the desktop's own library
    // rather than against this backend's idea of it.
    if let Some(glib) = glib_uris(paths) {
        let ours: Vec<&str> = on_clipboard
            .lines()
            .map(|l| l.trim_end_matches('\r'))
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(ours, glib, "must match g_filename_to_uri");
        println!("  identical to g_filename_to_uri for every path");
    } else {
        println!("  (GLib not available to cross-check against; skipped)");
    }

    // And back, so the peer on the other side gets the paths it was given.
    let back = state.read(transport, ClipboardFormat::FileList).unwrap();
    assert_eq!(String::from_utf8(back).unwrap(), paths);
    println!("  round-trips back to the same paths");
}

/// The size limit, from both directions.
fn a_large_image_round_trips_and_an_oversized_one_is_refused(
    state: &ClipboardState,
    transport: &DesktopSelection,
) {
    println!("\n== size ==");

    let big: Vec<u8> = (0..24 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let started = Instant::now();
    state
        .write(transport, ClipboardFormat::Png, &big)
        .unwrap_or_else(|e| panic!("writing {} bytes: {e}", big.len()));
    let wrote = started.elapsed();
    let started = Instant::now();
    let pasted = wl_paste(MIME_PNG).unwrap();
    let read = started.elapsed();
    assert_eq!(pasted, big, "24 MiB must survive byte-exact");
    println!(
        "  {} MiB offered and taken by the compositor in {}ms, pasted back byte-exact in {}ms",
        big.len() / 1024 / 1024,
        wrote.as_millis(),
        read.as_millis()
    );

    let too_big = vec![0u8; MAX_CLIPBOARD_BYTES + 1];
    let err = state
        .write(transport, ClipboardFormat::Png, &too_big)
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains(&too_big.len().to_string())
            && message.contains(&MAX_CLIPBOARD_BYTES.to_string()),
        "the refusal must name both sizes: {message}"
    );
    println!("  {} bytes refused: {message}", too_big.len());

    // The refusal must not have displaced the selection that is really there.
    assert_eq!(
        state.staged_for(MIME_PNG).map(|b| b.len()),
        Some(big.len()),
        "a refused write must leave the standing offer answerable"
    );
    println!("  the standing 24 MiB offer is still answerable after the refusal");
}

/// One change, one bump — including our own writes, as on Windows.
fn the_serial_moves_once_per_change(state: &ClipboardState, transport: &DesktopSelection) {
    println!("\n== the change serial ==");

    let before = state.change_serial();
    assert_eq!(
        state.change_serial(),
        before,
        "an idle poll must not move it"
    );

    external_copy(MIME_TEXT, b"somebody else copied", state);
    let after_theirs = state.change_serial();
    assert!(after_theirs > before, "an external copy must move it");

    let idle = state.change_serial();
    assert_eq!(idle, after_theirs, "polling again must not move it");

    state
        .write(transport, ClipboardFormat::Utf8Text, b"we copied")
        .unwrap();
    let after_ours = state.change_serial();
    assert!(
        after_ours > after_theirs,
        "our own write must move it too, as GetClipboardSequenceNumber does"
    );
    println!(
        "  {before} -> {after_theirs} (their copy) -> {after_ours} (ours); idle polls hold still"
    );
}

/// `SelectionRead` refuses a selection the calling session owns, so this path
/// must never reach the portal.
fn our_own_selection_is_read_back_without_the_portal(
    state: &ClipboardState,
    transport: &DesktopSelection,
) {
    println!("\n== reading back our own selection ==");

    let text = "the peer sent this, and the agent re-reads it";
    state
        .write(transport, ClipboardFormat::Utf8Text, text.as_bytes())
        .unwrap();
    let before = transport.reads.load(Ordering::Relaxed);
    let read = state.read(transport, ClipboardFormat::Utf8Text).unwrap();
    assert_eq!(read, text.as_bytes());
    assert_eq!(
        transport.reads.load(Ordering::Relaxed),
        before,
        "SelectionRead fails for a selection the session owns; this must not call it"
    );
    println!("  answered from the staged bytes, with no SelectionRead at all");

    // Until somebody else copies, at which point it goes back to the portal.
    external_copy(MIME_TEXT, b"and now somebody else has it", state);
    let read = state.read(transport, ClipboardFormat::Utf8Text).unwrap();
    assert_eq!(read, b"and now somebody else has it");
    assert!(
        transport.reads.load(Ordering::Relaxed) > before,
        "once the selection is somebody else's the portal is the only answer"
    );
    println!("  and goes back to the portal once the selection is somebody else's");
}

// ---------------------------------------------------------------- the desktop

/// The format the backend would hand a peer, with what the compositor really
/// offered printed beside it.
fn richest_is(state: &ClipboardState, expected: ClipboardFormat) {
    let formats = state.available_formats();
    assert_eq!(
        formats.first(),
        Some(&expected),
        "offered {:?}",
        list_types().unwrap()
    );
}

/// Put a selection on the real clipboard as another application, then tell the
/// backend about it the way `SelectionOwnerChanged` would.
fn external_copy(mime: &str, bytes: &[u8], state: &ClipboardState) {
    wl_copy(mime, bytes).unwrap();
    // The MIME list comes off the compositor rather than being asserted, so what
    // the backend is told is what a pasting application would really see.
    state.selection_changed(list_types().unwrap(), Some(false));
}

/// Hand a selection to the compositor and wait until it is really there.
///
/// `wl-copy` forks: the process that owns the selection outlives the one spawned
/// here, so nothing of its stdio may be piped — a background owner holding the
/// read end of an inherited pipe would leave the wait below blocked until the
/// clipboard changed hands again.
fn wl_copy(mime: &str, bytes: &[u8]) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg(mime)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| PlatformError::Other(format!("spawning wl-copy: {e}")))?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(bytes)
        .map_err(|e| PlatformError::Other(format!("feeding wl-copy: {e}")))?;
    let status = child
        .wait()
        .map_err(|e| PlatformError::Other(format!("waiting for wl-copy: {e}")))?;
    if !status.success() {
        return Err(PlatformError::Other(format!(
            "wl-copy --type {mime} exited with {status}"
        )));
    }
    wait_for_offer(mime, bytes)
}

fn wl_paste(mime: &str) -> Result<Vec<u8>> {
    let out = Command::new("wl-paste")
        .arg("--no-newline")
        .arg("--type")
        .arg(mime)
        .output()
        .map_err(|e| PlatformError::Other(format!("spawning wl-paste: {e}")))?;
    if !out.status.success() {
        return Err(PlatformError::Other(format!(
            "wl-paste --type {mime} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

fn list_types() -> Result<Vec<String>> {
    let out = Command::new("wl-paste")
        .arg("--list-types")
        .output()
        .map_err(|e| PlatformError::Other(format!("spawning wl-paste: {e}")))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// `wl-copy` forks before it owns the selection, so the next paste can race it.
///
/// The wait is on the *content*, not on the MIME type being offered: the
/// selection that was already on the machine's clipboard usually offers
/// `text/plain` too, so a type-only check would pass instantly and every
/// assertion after it would be made against somebody else's copy.
fn wait_for_offer(mime: &str, expected: &[u8]) -> Result<()> {
    let deadline = Instant::now() + SETTLE;
    loop {
        if wl_paste(mime).is_ok_and(|got| got == expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Other(format!(
                "the compositor never took the {mime} selection"
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What `g_filename_to_uri` makes of the same paths, or `None` where GLib's
/// Python bindings are not installed.
fn glib_uris(paths: &str) -> Option<Vec<String>> {
    let out = Command::new("python3")
        .arg("-c")
        .arg(
            "import sys, gi\n\
             gi.require_version('GLib', '2.0')\n\
             from gi.repository import GLib\n\
             for line in sys.stdin.read().splitlines():\n\
             \x20   if line:\n\
             \x20       print(GLib.filename_to_uri(line, None))\n",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()
        .and_then(|mut child| {
            child.stdin.take()?.write_all(paths.as_bytes()).ok()?;
            child.wait_with_output().ok()
        })?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// A real PNG, from `WX_CLIPBOARD_PNG` when one is supplied, and otherwise the
/// smallest valid one — so the test needs no image library to run.
fn fixture_png() -> Vec<u8> {
    if let Ok(path) = std::env::var("WX_CLIPBOARD_PNG") {
        if let Ok(bytes) = std::fs::read(&path) {
            return bytes;
        }
    }
    const ONE_PIXEL: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8,
        0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    ONE_PIXEL.to_vec()
}

/// Drop a byte-exact copy of something that crossed the clipboard where a
/// reviewer can look at it, when a directory has been named for it.
fn save_evidence(name: &str, bytes: &[u8]) {
    let Ok(dir) = std::env::var("WX_CLIPBOARD_EVIDENCE_DIR") else {
        return;
    };
    let path = PathBuf::from(dir).join(name);
    if std::fs::write(&path, bytes).is_ok() {
        println!("  saved {}", path.display());
    }
}

/// `Some(description)` when this machine can run the test, `None` after printing
/// why it is being skipped.
fn usable_desktop() -> Option<String> {
    let Ok(display) = std::env::var("WAYLAND_DISPLAY") else {
        println!("skipped: no WAYLAND_DISPLAY, so there is no clipboard to drive");
        return None;
    };
    for tool in ["wl-copy", "wl-paste"] {
        if Command::new(tool)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            println!("skipped: {tool} is not installed (wl-clipboard)");
            return None;
        }
    }
    // Touching the clipboard at all must not be assumed to work: a compositor
    // that refuses is a skip, not a failure.
    if list_types().is_err() {
        println!("skipped: the compositor would not answer wl-paste --list-types");
        return None;
    }
    let _ = clipboard::mimes_for(ClipboardFormat::Utf8Text);
    Some(format!("WAYLAND_DISPLAY={display}"))
}
