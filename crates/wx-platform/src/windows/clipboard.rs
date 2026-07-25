//! Clipboard access on Windows.
//!
//! The Win32 clipboard is a single global lock that any process can hold, and it
//! must be opened, used, and closed without anything slow in between. Everything
//! here goes through [`ClipboardLock`], which retries briefly on a busy clipboard
//! and closes on drop even if the body panics — a leaked open clipboard freezes
//! copy and paste for the entire session, which is far worse than a failed paste.

use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use wx_proto::ClipboardFormat;

use crate::error::{PlatformError, Result};
use crate::windows::cfhtml::{strip_cf_html, wrap_cf_html};

/// `CF_UNICODETEXT`. Always UTF-16 on Windows; `CF_TEXT` would go through the ANSI
/// codepage and mangle anything outside it.
const CF_UNICODETEXT: u32 = 13;
/// `CF_HDROP`, the shell's file-drop format.
const CF_HDROP: u32 = 15;

/// How many times to retry a busy clipboard before giving up.
///
/// Applications hold the clipboard across a paint or a slow COM call, so a single
/// failure means nothing. A bounded retry keeps a genuinely wedged owner from
/// turning into an unbounded stall on the caller's thread.
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

pub struct WindowsClipboard {
    /// Registered format id for `CF_HTML`.
    html: u32,
    /// Registered format id for `PNG`, which every browser and image editor on
    /// Windows publishes alongside `CF_DIB`.
    png: u32,
}

impl WindowsClipboard {
    pub fn new() -> Self {
        Self {
            html: register("HTML Format"),
            png: register("PNG"),
        }
    }

    fn format_id(&self, format: ClipboardFormat) -> u32 {
        match format {
            ClipboardFormat::Utf8Text => CF_UNICODETEXT,
            ClipboardFormat::Html => self.html,
            ClipboardFormat::Png => self.png,
            ClipboardFormat::FileList => CF_HDROP,
        }
    }
}

impl Default for WindowsClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::traits::ClipboardAccess for WindowsClipboard {
    fn available_formats(&self) -> Result<Vec<ClipboardFormat>> {
        let _lock = ClipboardLock::acquire()?;
        let mut out = Vec::new();
        // Ordered most-specific first, so a peer offered several formats picks the
        // richest one rather than flattening a table to plain text.
        for format in [
            ClipboardFormat::FileList,
            ClipboardFormat::Png,
            ClipboardFormat::Html,
            ClipboardFormat::Utf8Text,
        ] {
            let id = self.format_id(format);
            // SAFETY: takes a format id by value; the clipboard is open.
            if unsafe { IsClipboardFormatAvailable(id) }.is_ok() {
                out.push(format);
            }
        }
        Ok(out)
    }

    fn read(&self, format: ClipboardFormat) -> Result<Vec<u8>> {
        let _lock = ClipboardLock::acquire()?;
        let id = self.format_id(format);
        let raw = read_global(id)?;

        Ok(match format {
            ClipboardFormat::Utf8Text => utf16_bytes_to_string(&raw).into_bytes(),
            ClipboardFormat::Html => strip_cf_html(&raw).into_bytes(),
            ClipboardFormat::Png => raw,
            ClipboardFormat::FileList => read_file_list(&raw)?.join("\n").into_bytes(),
        })
    }

    fn write(&self, format: ClipboardFormat, data: &[u8]) -> Result<()> {
        let payload = match format {
            ClipboardFormat::Utf8Text => {
                let text = std::str::from_utf8(data)
                    .map_err(|e| PlatformError::Malformed(format!("clipboard text: {e}")))?;
                string_to_utf16_bytes(text)
            }
            ClipboardFormat::Html => {
                let text = std::str::from_utf8(data)
                    .map_err(|e| PlatformError::Malformed(format!("clipboard html: {e}")))?;
                wrap_cf_html(text)
            }
            ClipboardFormat::Png => data.to_vec(),
            ClipboardFormat::FileList => {
                let text = std::str::from_utf8(data)
                    .map_err(|e| PlatformError::Malformed(format!("clipboard file list: {e}")))?;
                build_hdrop(text.lines())
            }
        };

        let lock = ClipboardLock::acquire()?;
        lock.replace(self.format_id(format), &payload)
    }

    fn change_serial(&self) -> Result<u64> {
        // SAFETY: no arguments, and deliberately does not need the clipboard open —
        // which is the point, since polling must not contend for the global lock.
        Ok(unsafe { GetClipboardSequenceNumber() } as u64)
    }
}

fn register(name: &str) -> u32 {
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    wide.push(0);
    // SAFETY: `wide` is NUL-terminated and outlives the call. Registering the same
    // name twice returns the same id, so this is safe to call repeatedly.
    unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) }
}

/// Serializes clipboard access within this process.
///
/// `OpenClipboard` only excludes *other* processes: the clipboard is associated
/// with the calling task, so a second thread in this process opens it successfully
/// while the first still holds it. Two threads then interleave `EmptyClipboard`
/// with another's `GlobalLock`, and the reader is left holding a pointer into a
/// block the writer has freed. That is not a theoretical race — it reproduces as a
/// `STATUS_HEAP_CORRUPTION` crash of the whole process within a handful of
/// concurrent reads and writes.
static PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds the clipboard open for as long as it is alive.
struct ClipboardLock {
    /// Dropped after `CloseClipboard`, so no other thread can open the clipboard
    /// until this one is finished with it.
    _process: std::sync::MutexGuard<'static, ()>,
}

impl ClipboardLock {
    fn acquire() -> Result<Self> {
        // Poisoning is recovered from rather than propagated: a panic while holding
        // the clipboard must not disable copy and paste for the rest of the process
        // lifetime, and the OS-level state was restored by `Drop` regardless.
        let process = PROCESS_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        for attempt in 0..OPEN_ATTEMPTS {
            // SAFETY: passing no owner window is documented and means the clipboard
            // is associated with the current task.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self { _process: process });
            }
            if attempt + 1 < OPEN_ATTEMPTS {
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
        }
        Err(PlatformError::ClipboardBusy)
    }

    /// Empty the clipboard and publish one format.
    ///
    /// `EmptyClipboard` is mandatory before `SetClipboardData`: it transfers
    /// ownership to us. Skipping it means the write silently does nothing.
    fn replace(&self, format: u32, payload: &[u8]) -> Result<()> {
        // SAFETY: the clipboard is open for the lifetime of `self`.
        unsafe { EmptyClipboard() }.map_err(|e| PlatformError::Os {
            context: "EmptyClipboard",
            code: e.code().0,
        })?;

        let handle = alloc_global(payload)?;

        // SAFETY: `handle` is a GMEM_MOVEABLE block sized to `payload`, and is
        // unlocked. On success the clipboard owns it and must not be freed here.
        match unsafe { SetClipboardData(format, Some(HANDLE(handle.0))) } {
            Ok(_) => Ok(()),
            Err(e) => {
                // Ownership did not transfer, so the block is still ours to free.
                // SAFETY: nothing else holds `handle`.
                unsafe {
                    let _ = GlobalFree(Some(handle));
                }
                Err(PlatformError::Os {
                    context: "SetClipboardData",
                    code: e.code().0,
                })
            }
        }
    }
}

impl Drop for ClipboardLock {
    fn drop(&mut self) {
        // SAFETY: matches the OpenClipboard in `acquire`. Runs even on panic, which
        // is the reason this is a guard type: a leaked open clipboard wedges copy
        // and paste for every application until the process exits.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Copy a clipboard payload out of the OS's global memory.
fn read_global(format: u32) -> Result<Vec<u8>> {
    // SAFETY: the caller holds the clipboard open.
    let handle = unsafe { GetClipboardData(format) }.map_err(|_| PlatformError::ClipboardEmpty)?;
    let global = HGLOBAL(handle.0);

    // SAFETY: `global` came from GetClipboardData, so it is a valid movable block
    // owned by the clipboard. It is unlocked again below before returning.
    let ptr = unsafe { GlobalLock(global) };
    if ptr.is_null() {
        return Err(PlatformError::Os {
            context: "GlobalLock",
            code: 0,
        });
    }
    // SAFETY: `ptr` is locked and `GlobalSize` reports the block's true length, so
    // the slice is in bounds. The data is copied before unlocking, because the
    // clipboard may hand the block to another process afterwards.
    let bytes = unsafe {
        let len = GlobalSize(global);
        let slice = core::slice::from_raw_parts(ptr as *const u8, len);
        let owned = slice.to_vec();
        let _ = GlobalUnlock(global);
        owned
    };
    Ok(bytes)
}

fn alloc_global(payload: &[u8]) -> Result<HGLOBAL> {
    // A zero-length allocation is legal but produces a block nothing can lock, so
    // one byte is the floor.
    let len = payload.len().max(1);
    // SAFETY: GMEM_MOVEABLE is what SetClipboardData requires.
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, len) }.map_err(|e| PlatformError::Os {
        context: "GlobalAlloc",
        code: e.code().0,
    })?;

    // SAFETY: `handle` is a fresh block of exactly `len` bytes, so writing
    // `payload` into it is in bounds. Unlocked immediately afterwards, as
    // SetClipboardData requires an unlocked block.
    unsafe {
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            let _ = GlobalFree(Some(handle));
            return Err(PlatformError::Os {
                context: "GlobalLock",
                code: 0,
            });
        }
        core::ptr::copy_nonoverlapping(payload.as_ptr(), ptr as *mut u8, payload.len());
        let _ = GlobalUnlock(handle);
    }
    Ok(handle)
}

/// Read the paths out of a `CF_HDROP` payload.
fn read_file_list(raw: &[u8]) -> Result<Vec<String>> {
    if raw.len() < core::mem::size_of::<DropFilesHeader>() {
        return Err(PlatformError::Malformed(
            "CF_HDROP payload too short".into(),
        ));
    }
    let drop = HDROP(raw.as_ptr() as *mut core::ffi::c_void);

    // SAFETY: `drop` points at a CF_HDROP block that the clipboard produced and
    // that outlives this function. Index `u32::MAX` is the documented way to ask
    // for the count rather than a path.
    let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };

    let mut paths = Vec::with_capacity(count as usize);
    for i in 0..count {
        // SAFETY: same block; the first call sizes the buffer and the second fills
        // it, which is the documented two-call pattern.
        let needed = unsafe { DragQueryFileW(drop, i, None) };
        let mut buf = vec![0u16; needed as usize + 1];
        let written = unsafe { DragQueryFileW(drop, i, Some(&mut buf)) };
        if written == 0 {
            continue;
        }
        paths.push(String::from_utf16_lossy(&buf[..written as usize]));
    }
    Ok(paths)
}

/// Layout of the `DROPFILES` structure that prefixes a `CF_HDROP` payload.
///
/// Declared locally rather than imported so the byte layout used by
/// [`build_hdrop`] is visible next to the code that writes it — a mismatch here
/// produces a file list the shell reads as garbage paths.
#[repr(C)]
struct DropFilesHeader {
    /// Byte offset from the start of the structure to the first path.
    p_files: u32,
    pt_x: i32,
    pt_y: i32,
    /// Whether the drop happened in a window's non-client area. Always false here.
    f_nc: u32,
    /// Non-zero means the path list is UTF-16.
    f_wide: u32,
}

/// Build a `CF_HDROP` payload: the header, then a double-NUL-terminated UTF-16
/// list of paths.
///
/// Pure byte assembly so the layout can be verified without the shell.
pub fn build_hdrop<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<u8> {
    let header_len = core::mem::size_of::<DropFilesHeader>() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&header_len.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // fWide: the list below is UTF-16. Omitting this makes the shell read the
    // UTF-16 bytes as ANSI and see only the first character of each path.
    out.extend_from_slice(&1u32.to_le_bytes());

    let mut any = false;
    for path in paths {
        if path.is_empty() {
            continue;
        }
        any = true;
        for unit in path.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    // The list is terminated by an extra NUL. An empty list is just the double NUL.
    if !any {
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Decode a NUL-terminated UTF-16 clipboard payload.
///
/// Odd trailing bytes are dropped rather than trusted: a truncated payload would
/// otherwise read one byte past the block.
fn utf16_bytes_to_string(raw: &[u8]) -> String {
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn string_to_utf16_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2 + 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Enumerate the raw format ids currently on the clipboard, for diagnostics.
///
/// Not part of the trait: it exists because "why did rich paste not work" is
/// answered by knowing which formats the source application actually published.
pub fn debug_formats() -> Result<Vec<u32>> {
    let _lock = ClipboardLock::acquire()?;
    let mut out = Vec::new();
    let mut current = 0u32;
    loop {
        // SAFETY: the clipboard is open; zero starts the enumeration and zero is
        // returned when it ends.
        current = unsafe { EnumClipboardFormats(current) };
        if current == 0 {
            break;
        }
        out.push(current);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::ClipboardAccess;

    #[test]
    fn utf16_round_trips_through_the_clipboard_encoding() {
        for text in ["", "hello", "åäö", "漢字", "emoji 👍🏽"] {
            let encoded = string_to_utf16_bytes(text);
            assert_eq!(utf16_bytes_to_string(&encoded), text, "{text:?}");
        }
    }

    #[test]
    fn utf16_decoding_stops_at_the_terminator() {
        // Windows reports the block's allocated size, not the string length, so
        // trailing slack is normal and must not become trailing NULs in the text.
        let mut bytes = string_to_utf16_bytes("hi");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(utf16_bytes_to_string(&bytes), "hi");
    }

    #[test]
    fn a_truncated_utf16_payload_does_not_panic() {
        assert_eq!(utf16_bytes_to_string(&[0x41]), "");
        assert_eq!(utf16_bytes_to_string(&[0x41, 0x00, 0x42]), "A");
    }

    #[test]
    fn unpaired_surrogates_are_replaced_rather_than_rejected() {
        // A hostile or buggy peer can put a lone surrogate on the clipboard.
        let bytes = [0x00, 0xd8, 0x00, 0x00];
        let text = utf16_bytes_to_string(&bytes);
        assert!(text.chars().all(|c| c == '\u{fffd}'));
    }

    #[test]
    fn hdrop_header_declares_the_offset_to_the_path_list() {
        let payload = build_hdrop(["C:\\a.txt"].into_iter());
        let offset = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;
        assert_eq!(offset, 20, "DROPFILES is 20 bytes on every Windows ABI");
        assert!(payload.len() > offset);
    }

    #[test]
    fn hdrop_marks_the_path_list_as_wide() {
        // Without fWide the shell reads UTF-16 as ANSI and every path becomes one
        // character long.
        let payload = build_hdrop(["C:\\a.txt"].into_iter());
        assert_eq!(u32::from_le_bytes(payload[16..20].try_into().unwrap()), 1);
    }

    #[test]
    fn hdrop_paths_are_utf16_and_double_nul_terminated() {
        let payload = build_hdrop(["C:\\å.txt", "C:\\b.txt"].into_iter());
        let units: Vec<u16> = payload[20..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let joined: Vec<String> = units
            .split(|u| *u == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
        assert_eq!(joined, vec!["C:\\å.txt", "C:\\b.txt"]);
        assert_eq!(&units[units.len() - 2..], &[0, 0]);
    }

    #[test]
    fn an_empty_hdrop_is_still_well_formed() {
        // A peer can offer a file list and then send nothing; the shell must see a
        // valid empty list rather than a truncated block.
        let payload = build_hdrop(std::iter::empty());
        assert_eq!(payload.len(), 24);
        assert_eq!(&payload[20..], &[0, 0, 0, 0]);
    }

    #[test]
    fn blank_lines_do_not_become_empty_paths() {
        let payload = build_hdrop("C:\\a.txt\n\nC:\\b.txt".lines());
        let units: Vec<u16> = payload[20..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let paths: Vec<String> = units
            .split(|u| *u == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn a_short_hdrop_payload_is_rejected_not_dereferenced() {
        // Straight off the wire: reading a HDROP from fewer than 20 bytes would
        // walk off the end of the allocation.
        assert!(read_file_list(&[0u8; 4]).is_err());
    }

    #[test]
    fn registering_the_same_format_twice_yields_the_same_id() {
        assert_eq!(register("HTML Format"), register("HTML Format"));
        assert_ne!(register("HTML Format"), register("PNG"));
    }

    /// Serializes the tests that mutate the one real clipboard.
    ///
    /// The product's own [`PROCESS_LOCK`] makes each individual call atomic, but a
    /// test that writes and then reads needs the pair to be atomic too, or another
    /// test's write lands in between.
    static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn the_change_serial_needs_no_clipboard_lock_so_polling_cannot_contend() {
        // Read while the clipboard is held open: a serial query that needed the
        // lock would block here, and the clipboard poller runs constantly.
        let cb = WindowsClipboard::new();
        let _guard = exclusive();
        let held = ClipboardLock::acquire().unwrap();
        assert!(cb.change_serial().is_ok());
        drop(held);
    }

    /// Touches the real clipboard. Restores the previous text so a test run does
    /// not silently eat what the developer had copied.
    #[test]
    fn text_written_to_the_clipboard_reads_back_unchanged() {
        let cb = WindowsClipboard::new();
        let _guard = exclusive();
        let previous = cb.read(ClipboardFormat::Utf8Text).ok();

        let sample = "WinXtend clipboard test åäö 漢字";
        cb.write(ClipboardFormat::Utf8Text, sample.as_bytes())
            .unwrap();
        let back = cb.read(ClipboardFormat::Utf8Text).unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), sample);

        if let Some(previous) = previous {
            let _ = cb.write(ClipboardFormat::Utf8Text, &previous);
        }
    }

    #[test]
    fn writing_text_makes_text_an_available_format() {
        let cb = WindowsClipboard::new();
        let _guard = exclusive();
        let previous = cb.read(ClipboardFormat::Utf8Text).ok();

        cb.write(ClipboardFormat::Utf8Text, b"available").unwrap();
        assert!(cb
            .available_formats()
            .unwrap()
            .contains(&ClipboardFormat::Utf8Text));

        if let Some(previous) = previous {
            let _ = cb.write(ClipboardFormat::Utf8Text, &previous);
        }
    }

    #[test]
    fn html_written_to_the_clipboard_reads_back_as_the_same_fragment() {
        // Rich paste crossing machines depends on the CF_HTML header surviving a
        // real round trip through the OS, not just through `wrap`/`strip`.
        let cb = WindowsClipboard::new();
        let _guard = exclusive();
        let previous = cb.read(ClipboardFormat::Utf8Text).ok();

        let fragment = "<b>bold</b> and <i>å</i>";
        cb.write(ClipboardFormat::Html, fragment.as_bytes())
            .unwrap();
        let back = cb.read(ClipboardFormat::Html).unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), fragment);

        if let Some(previous) = previous {
            let _ = cb.write(ClipboardFormat::Utf8Text, &previous);
        }
    }

    #[test]
    fn the_change_serial_moves_when_the_clipboard_changes() {
        let cb = WindowsClipboard::new();
        let _guard = exclusive();
        let previous = cb.read(ClipboardFormat::Utf8Text).ok();

        let before = cb.change_serial().unwrap();
        cb.write(ClipboardFormat::Utf8Text, b"serial probe")
            .unwrap();
        let after = cb.change_serial().unwrap();
        assert_ne!(before, after);

        if let Some(previous) = previous {
            let _ = cb.write(ClipboardFormat::Utf8Text, &previous);
        }
    }

    #[test]
    fn concurrent_readers_and_writers_do_not_corrupt_the_heap() {
        // Regression test for a real crash: without the process-wide lock, one
        // thread's EmptyClipboard frees the block another is reading through
        // GlobalLock, and the process dies with STATUS_HEAP_CORRUPTION.
        let _guard = exclusive();
        let previous = WindowsClipboard::new().read(ClipboardFormat::Utf8Text).ok();

        let threads: Vec<_> = (0..4)
            .map(|i| {
                std::thread::spawn(move || {
                    let cb = WindowsClipboard::new();
                    for n in 0..25 {
                        let text = format!("thread {i} value {n}");
                        let _ = cb.write(ClipboardFormat::Utf8Text, text.as_bytes());
                        let _ = cb.read(ClipboardFormat::Utf8Text);
                        let _ = cb.available_formats();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        if let Some(previous) = previous {
            let _ = WindowsClipboard::new().write(ClipboardFormat::Utf8Text, &previous);
        }
    }

    #[test]
    fn invalid_utf8_is_rejected_before_it_reaches_the_os() {
        let cb = WindowsClipboard::new();
        let err = cb
            .write(ClipboardFormat::Utf8Text, &[0xff, 0xfe])
            .unwrap_err();
        assert!(matches!(err, PlatformError::Malformed(_)));
    }
}
