//! Low-level input capture on Windows, via `WH_KEYBOARD_LL` and `WH_MOUSE_LL`.
//!
//! # Why a dedicated thread
//!
//! Low-level hooks are dispatched to the thread that installed them, and only
//! while that thread pumps messages. Installing them on the agent's main thread
//! would tie input latency to whatever else that thread is doing, so they get a
//! thread of their own that does nothing but `GetMessageW`.
//!
//! # Why the callback must be fast
//!
//! The hook chain is synchronous for the entire desktop: every keystroke on the
//! machine waits for this callback to return. Exceed `LowLevelHooksTimeout`
//! (300ms by default) and Windows silently unhooks us — capture stops with no
//! error anywhere, and the only symptom is that WinXtend quietly stops working
//! until it is restarted. The sink pushes into a queue and returns.
//!
//! # Why injected events are ignored
//!
//! Our own `SendInput` output comes back through these hooks. Without the
//! `LLKHF_INJECTED` check, injecting on machine A would capture the event again
//! and re-send it, and two machines pointing at each other would saturate the link
//! within milliseconds.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use wx_proto::{KeyAction, Modifiers, MouseButton, Point, ScrollUnit};

use crate::error::{PlatformError, Result};
use crate::keyres::{KeyResolver, RawKey};
use crate::traits::{CaptureSink, CapturedEvent};
use crate::windows::vk::special_from_vk;

/// One notch of the scroll wheel in the units the hook reports.
const WHEEL_DELTA: f64 = 120.0;

/// `ToUnicodeEx` flag that resolves a keystroke without disturbing the kernel's
/// dead-key state.
///
/// Essential, not an optimisation. Without it, asking the OS what a key means
/// consumes the user's own pending dead key, so composing an accented character
/// locally stops working the moment WinXtend is running. Composition is handled by
/// [`crate::keyres`] precisely so this flag can be set.
const TOUNICODE_NO_STATE_CHANGE: u32 = 0x4;

thread_local! {
    /// Hook state, owned by the hook thread.
    ///
    /// A thread-local rather than a global: hook procedures are bare C callbacks
    /// with no user-data parameter, and the callbacks only ever run on the thread
    /// that installed the hooks. That makes a thread-local both sufficient and
    /// free of locking on the input hot path — and a mutex here would risk the
    /// hook timeout described above.
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

struct HookState {
    sink: CaptureSink,
    resolver: KeyResolver,
    suppress: Arc<AtomicBool>,
    /// Modifier state as tracked from the hook stream itself.
    ///
    /// Not read from `GetKeyboardState`, which is per-thread and reflects the
    /// *hook thread's* queue rather than the physical keyboard, so it is always
    /// empty here.
    modifiers: Modifiers,
    /// Virtual keys currently down, to tell a real press from auto-repeat. The
    /// low-level hook reports both as `WM_KEYDOWN` with no repeat flag.
    down: HashSet<u16>,
    /// Last absolute cursor position, for deriving deltas.
    last_position: Option<Point>,
}

pub struct WindowsCapture {
    thread: Option<JoinHandle<()>>,
    /// Thread id of the hook thread, so `stop` can post it a quit message.
    thread_id: Arc<AtomicU32>,
    suppress: Arc<AtomicBool>,
}

impl WindowsCapture {
    pub fn new() -> Self {
        super::display::ensure_dpi_awareness();
        Self {
            thread: None,
            thread_id: Arc::new(AtomicU32::new(0)),
            suppress: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for WindowsCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::traits::InputCapture for WindowsCapture {
    fn start(&mut self, sink: CaptureSink) -> Result<()> {
        if self.thread.is_some() {
            return Err(PlatformError::AlreadyCapturing);
        }

        let suppress = Arc::clone(&self.suppress);
        let thread_id = Arc::clone(&self.thread_id);
        // Hook installation happens on the new thread, so its success has to come
        // back over a channel: reporting "capture started" when the hooks failed
        // would leave the agent believing it owns input that it does not.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let thread = std::thread::Builder::new()
            .name("wx-capture".into())
            .spawn(move || hook_thread(sink, suppress, thread_id, ready_tx))
            .map_err(|e| PlatformError::Other(format!("cannot spawn capture thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                tracing::debug!("low-level keyboard and mouse hooks installed");
                self.thread = Some(thread);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            // The thread died before reporting, which can only be a panic during
            // setup.
            Err(_) => {
                let _ = thread.join();
                Err(PlatformError::Other(
                    "capture thread exited during startup".into(),
                ))
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Err(PlatformError::NotCapturing);
        };
        let tid = self.thread_id.swap(0, Ordering::SeqCst);
        if tid != 0 {
            // SAFETY: posting a message to a thread id is safe regardless of
            // whether the thread still exists; the call fails harmlessly if not.
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        // Suppression must not survive a stop, or a restart would begin swallowing
        // the user's local input with no remote peer to send it to.
        self.suppress.store(false, Ordering::SeqCst);
        let joined = thread
            .join()
            .map_err(|_| PlatformError::Other("capture thread panicked".into()));
        tracing::debug!("low-level hooks removed");
        joined
    }

    fn is_capturing(&self) -> bool {
        self.thread.is_some()
    }

    fn set_suppress_local(&mut self, suppress: bool) -> Result<()> {
        self.suppress.store(suppress, Ordering::SeqCst);
        Ok(())
    }

    fn suppresses_local(&self) -> bool {
        self.suppress.load(Ordering::SeqCst)
    }
}

impl Drop for WindowsCapture {
    fn drop(&mut self) {
        // Leaving hooks installed after the owner is gone would keep swallowing
        // the user's input with nothing left to deliver it to.
        if self.thread.is_some() {
            let _ = crate::traits::InputCapture::stop(self);
        }
    }
}

fn hook_thread(
    sink: CaptureSink,
    suppress: Arc<AtomicBool>,
    thread_id: Arc<AtomicU32>,
    ready: mpsc::Sender<Result<()>>,
) {
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(HookState {
            sink,
            resolver: KeyResolver::new(),
            suppress,
            modifiers: Modifiers::NONE,
            down: HashSet::new(),
            last_position: None,
        });
    });

    // SAFETY: both hooks are installed for this thread only (`dwthreadid` 0 with a
    // null module means "global, dispatched to this thread"), and both are removed
    // before the thread exits, below. The procedures are `extern "system"` with the
    // signature `HOOKPROC` requires.
    let hooks = unsafe {
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0);
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0);
        match (keyboard, mouse) {
            (Ok(k), Ok(m)) => Ok((k, m)),
            (k, m) => {
                // Partial success has to be undone, or a stale hook keeps
                // delivering to a thread that is about to exit.
                if let Ok(k) = k {
                    let _ = UnhookWindowsHookEx(k);
                }
                if let Ok(m) = m {
                    let _ = UnhookWindowsHookEx(m);
                }
                Err(PlatformError::Os {
                    context: "SetWindowsHookEx",
                    code: windows::Win32::Foundation::GetLastError().0 as i32,
                })
            }
        }
    };

    let (keyboard, mouse) = match hooks {
        Ok(pair) => pair,
        Err(e) => {
            STATE.with(|slot| *slot.borrow_mut() = None);
            let _ = ready.send(Err(e));
            return;
        }
    };

    // SAFETY: returns this thread's id, no arguments.
    thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let _ = ready.send(Ok(()));

    // Low-level hooks are only dispatched while this thread pumps messages.
    // GetMessageW returns 0 on WM_QUIT, which is how `stop` ends the loop.
    let mut msg = MSG::default();
    // SAFETY: `msg` is a live local; the loop ends when GetMessageW reports
    // WM_QUIT (0) or an error (-1, also falsy here).
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {}

    // SAFETY: both handles came from SetWindowsHookExW on this thread and are
    // unhooked exactly once.
    unsafe {
        let _ = UnhookWindowsHookEx(keyboard);
        let _ = UnhookWindowsHookEx(mouse);
    }
    STATE.with(|slot| *slot.borrow_mut() = None);
}

/// SAFETY: installed as a `WH_KEYBOARD_LL` procedure, so `lparam` is a valid
/// `KBDLLHOOKSTRUCT` pointer whenever `code` is `HC_ACTION`, and this runs on the
/// thread that owns `STATE`.
unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let info = *(lparam.0 as *const KBDLLHOOKSTRUCT);

    // Our own SendInput output. Re-capturing it would feed a loop between two
    // machines that each inject what the other captured.
    if info.flags.contains(LLKHF_INJECTED) {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let message = wparam.0 as u32;
    let is_down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let is_up = message == WM_KEYUP || message == WM_SYSKEYUP;
    if !is_down && !is_up {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let suppress = STATE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(state) = borrow.as_mut() else {
            return false;
        };
        state.handle_key(info.vkCode as u16, info.scanCode, is_down);
        state.suppress.load(Ordering::Relaxed)
    });

    if suppress {
        // Non-zero stops the chain, so local applications never see the key. This
        // is what stops keystrokes landing on two machines at once.
        return LRESULT(1);
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// SAFETY: installed as a `WH_MOUSE_LL` procedure, so `lparam` is a valid
/// `MSLLHOOKSTRUCT` pointer whenever `code` is `HC_ACTION`.
unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let info = *(lparam.0 as *const MSLLHOOKSTRUCT);
    if info.flags & LLMHF_INJECTED != 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let message = wparam.0 as u32;
    let suppress = STATE.with(|slot| {
        let mut borrow = slot.borrow_mut();
        let Some(state) = borrow.as_mut() else {
            return false;
        };
        state.handle_mouse(message, &info);
        state.suppress.load(Ordering::Relaxed)
    });

    if suppress {
        return LRESULT(1);
    }
    CallNextHookEx(None, code, wparam, lparam)
}

impl HookState {
    fn handle_key(&mut self, vk: u16, scan: u32, is_down: bool) {
        self.update_modifiers(vk, is_down);

        let action = if !is_down {
            self.down.remove(&vk);
            KeyAction::Release
        } else if self.down.insert(vk) {
            KeyAction::Press
        } else {
            // Already down: the OS is auto-repeating. Kept distinct so the
            // receiver can drop repeats and generate its own.
            KeyAction::Repeat
        };

        let raw = self.raw_key(vk, scan, action);
        let events = self.resolver.resolve(&raw);
        for event in events {
            (self.sink)(CapturedEvent::Key(event));
        }
    }

    /// Track modifier state from the hook stream.
    ///
    /// Right Alt becomes `ALT_GR` rather than `ALT`. On layouts that have AltGr it
    /// genuinely is the layout-shift key, and on layouts that do not the injector
    /// maps `ALT_GR` back to right Alt, so plain `Alt+Tab` from the right-hand key
    /// still works. Reporting both bits would be worse: `ALT` is not stripped from
    /// text payloads, so the receiver would hold Alt while typing and turn every
    /// letter into a menu accelerator.
    fn update_modifiers(&mut self, vk: u16, is_down: bool) {
        let bit = match VIRTUAL_KEY(vk) {
            VK_LSHIFT | VK_RSHIFT | VK_SHIFT => Modifiers::SHIFT,
            VK_LCONTROL | VK_RCONTROL | VK_CONTROL => Modifiers::CTRL,
            VK_LMENU => Modifiers::ALT,
            VK_RMENU => Modifiers::ALT_GR,
            VK_LWIN | VK_RWIN => Modifiers::SUPER,
            // Lock keys latch on press rather than following the key, so they are
            // read from the OS instead of tracked here.
            VK_CAPITAL | VK_NUMLOCK => {
                if is_down {
                    self.refresh_lock_state();
                }
                return;
            }
            _ => return,
        };
        self.modifiers = if is_down {
            self.modifiers.union(bit)
        } else {
            self.modifiers.without(bit)
        };
    }

    fn refresh_lock_state(&mut self) {
        // SAFETY: GetKeyState takes and returns integers. The low bit is the toggle
        // state, which is process-wide rather than per-queue, so it is meaningful
        // even on the hook thread.
        let caps = unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 != 0;
        let num = unsafe { GetKeyState(VK_NUMLOCK.0 as i32) } & 1 != 0;
        self.modifiers = self
            .modifiers
            .without(Modifiers::CAPS_LOCK)
            .without(Modifiers::NUM_LOCK);
        if caps {
            self.modifiers = self.modifiers.union(Modifiers::CAPS_LOCK);
        }
        if num {
            self.modifiers = self.modifiers.union(Modifiers::NUM_LOCK);
        }
    }

    fn raw_key(&self, vk: u16, scan: u32, action: KeyAction) -> RawKey {
        if let Some(special) = special_from_vk(vk) {
            return RawKey::special(vk as u32, special, action, self.modifiers);
        }
        match self.translate(vk, scan) {
            Translation::Text(text) => RawKey::text(vk as u32, text, action, self.modifiers),
            Translation::Dead(accent) => RawKey::dead(vk as u32, accent, action, self.modifiers),
            Translation::None => RawKey::unmapped(vk as u32, action, self.modifiers),
        }
    }

    /// Ask the *focused application's* keyboard layout what this key produces.
    ///
    /// The layout is read from the foreground window's thread, not from ours: a
    /// user with two layouts installed switches per-window, and using the hook
    /// thread's layout would resolve keys through whichever layout happened to be
    /// active when the agent started.
    fn translate(&self, vk: u16, scan: u32) -> Translation {
        let mut key_state = [0u8; 256];

        // Shift and AltGr are applied because they are part of producing the
        // character. Ctrl, Alt, and Super are deliberately left clear: with Ctrl
        // set the layout yields a C0 control code, which no receiver can inject,
        // and the chord is reconstructed from the modifier bits instead.
        if self.modifiers.contains(Modifiers::SHIFT) {
            key_state[VK_SHIFT.0 as usize] = 0x80;
        }
        if self.modifiers.contains(Modifiers::CAPS_LOCK) {
            key_state[VK_CAPITAL.0 as usize] = 0x01;
        }
        if self.modifiers.contains(Modifiers::ALT_GR) {
            // Windows layouts implement AltGr as Ctrl+Alt, so both have to be set
            // for the third-level characters to appear at all.
            key_state[VK_CONTROL.0 as usize] = 0x80;
            key_state[VK_MENU.0 as usize] = 0x80;
        }

        // SAFETY: GetForegroundWindow may return a null window, which
        // GetWindowThreadProcessId then reports as thread 0, and GetKeyboardLayout
        // treats thread 0 as "the calling thread" — a usable fallback rather than
        // an error.
        let hkl = unsafe {
            let hwnd = GetForegroundWindow();
            let tid = GetWindowThreadProcessId(hwnd, None);
            GetKeyboardLayout(tid)
        };

        let mut buffer = [0u16; 8];
        // SAFETY: all three buffers are live locals with lengths the wrapper passes
        // through, and TOUNICODE_NO_STATE_CHANGE keeps the call from mutating the
        // kernel's dead-key state.
        let written = unsafe {
            ToUnicodeEx(
                vk as u32,
                scan,
                &key_state,
                &mut buffer,
                TOUNICODE_NO_STATE_CHANGE,
                Some(hkl),
            )
        };

        if written < 0 {
            // Dead key: the buffer holds the accent, which composes with the next
            // keystroke in `keyres`.
            return match char::from_u32(buffer[0] as u32) {
                Some(accent) => Translation::Dead(accent),
                None => Translation::None,
            };
        }
        if written == 0 {
            return Translation::None;
        }
        let len = (written as usize).min(buffer.len());
        Translation::Text(String::from_utf16_lossy(&buffer[..len]))
    }

    fn handle_mouse(&mut self, message: u32, info: &MSLLHOOKSTRUCT) {
        let position = Point::new(info.pt.x as f64, info.pt.y as f64);

        match message {
            WM_MOUSEMOVE => {
                let (dx, dy) = match self.last_position {
                    Some(previous) => (position.x - previous.x, position.y - previous.y),
                    // First event after capture starts: no delta is knowable, and
                    // inventing one from the screen origin would fling the cursor.
                    None => (0.0, 0.0),
                };
                self.last_position = Some(position);
                if dx == 0.0 && dy == 0.0 {
                    return;
                }
                (self.sink)(CapturedEvent::PointerMotion { dx, dy, position });
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                // The delta is a signed 16-bit value in the high word, in 120ths of
                // a notch. High-resolution wheels and trackpads send fractions.
                let raw = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
                let amount = raw as f64 / WHEEL_DELTA;
                let (dx, dy) = if message == WM_MOUSEWHEEL {
                    (0.0, amount)
                } else {
                    (amount, 0.0)
                };
                (self.sink)(CapturedEvent::Scroll {
                    dx,
                    dy,
                    unit: ScrollUnit::Lines,
                });
            }
            _ => {
                if let Some((button, pressed)) = mouse_button(message, info.mouseData) {
                    (self.sink)(CapturedEvent::Button { button, pressed });
                }
            }
        }
    }
}

enum Translation {
    Text(String),
    Dead(char),
    None,
}

/// Decode a button message into the protocol's button and transition.
///
/// Pure so the mapping can be tested without a mouse.
fn mouse_button(message: u32, mouse_data: u32) -> Option<(MouseButton, bool)> {
    // The X-button number is in the high word, the same place the wheel delta
    // lives for wheel messages.
    let x_button = ((mouse_data >> 16) & 0xffff) as u16;
    Some(match message {
        WM_LBUTTONDOWN => (MouseButton::Left, true),
        WM_LBUTTONUP => (MouseButton::Left, false),
        WM_RBUTTONDOWN => (MouseButton::Right, true),
        WM_RBUTTONUP => (MouseButton::Right, false),
        WM_MBUTTONDOWN => (MouseButton::Middle, true),
        WM_MBUTTONUP => (MouseButton::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let pressed = message == WM_XBUTTONDOWN;
            match x_button {
                XBUTTON1 => (MouseButton::Back, pressed),
                XBUTTON2 => (MouseButton::Forward, pressed),
                // Buttons past the fifth: forward them so a gaming mouse's extra
                // buttons are not silently dropped by the sender.
                other => (MouseButton::Extra(other.saturating_sub(3) as u8), pressed),
            }
        }
        _ => return None,
    })
}

/// A `SpecialKey` a bare modifier press should resolve to, for tests that need a
/// known-good virtual key without touching the OS.
#[cfg(test)]
fn modifier_special(vk: VIRTUAL_KEY) -> Option<wx_proto::SpecialKey> {
    special_from_vk(vk.0).filter(|k| k.is_modifier())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::InputCapture;
    use std::sync::Mutex;
    use wx_proto::SpecialKey;

    #[test]
    fn mouse_messages_map_to_the_right_button_and_transition() {
        assert_eq!(
            mouse_button(WM_LBUTTONDOWN, 0),
            Some((MouseButton::Left, true))
        );
        assert_eq!(
            mouse_button(WM_LBUTTONUP, 0),
            Some((MouseButton::Left, false))
        );
        assert_eq!(
            mouse_button(WM_RBUTTONDOWN, 0),
            Some((MouseButton::Right, true))
        );
        assert_eq!(
            mouse_button(WM_MBUTTONUP, 0),
            Some((MouseButton::Middle, false))
        );
    }

    #[test]
    fn side_buttons_are_distinguished_by_the_high_word() {
        assert_eq!(
            mouse_button(WM_XBUTTONDOWN, (XBUTTON1 as u32) << 16),
            Some((MouseButton::Back, true))
        );
        assert_eq!(
            mouse_button(WM_XBUTTONUP, (XBUTTON2 as u32) << 16),
            Some((MouseButton::Forward, false))
        );
    }

    #[test]
    fn buttons_past_the_fifth_are_forwarded_rather_than_dropped() {
        // A gaming mouse's extra buttons are worth something to whoever bound them.
        assert_eq!(
            mouse_button(WM_XBUTTONDOWN, 5u32 << 16),
            Some((MouseButton::Extra(2), true))
        );
    }

    #[test]
    fn non_button_messages_produce_no_button_event() {
        assert_eq!(mouse_button(WM_MOUSEMOVE, 0), None);
        assert_eq!(mouse_button(WM_MOUSEWHEEL, 120 << 16), None);
    }

    /// Extract a wheel delta from `mouseData` the way `handle_mouse` does.
    fn wheel_notches(mouse_data: u32) -> f64 {
        (((mouse_data >> 16) & 0xffff) as u16 as i16) as f64 / WHEEL_DELTA
    }

    #[test]
    fn wheel_deltas_are_signed_so_scrolling_down_does_not_read_as_up() {
        // The high word is a signed short. Reading it unsigned turns one notch down
        // into +546 notches up, which throws the page to the top.
        let down = ((-120i16) as u16 as u32) << 16;
        assert_eq!(wheel_notches(down), -1.0);
        let up = (120i16 as u16 as u32) << 16;
        assert_eq!(wheel_notches(up), 1.0);
    }

    #[test]
    fn high_resolution_wheels_report_fractional_notches() {
        // Precision trackpads and free-spin wheels send deltas finer than 120, and
        // rounding them to whole notches makes smooth scrolling jerky.
        let partial = (30i16 as u16 as u32) << 16;
        assert_eq!(wheel_notches(partial), 0.25);
    }

    #[test]
    fn modifier_keys_resolve_to_sided_specials() {
        assert_eq!(modifier_special(VK_LCONTROL), Some(SpecialKey::CtrlLeft));
        assert_eq!(modifier_special(VK_RSHIFT), Some(SpecialKey::ShiftRight));
        assert_eq!(modifier_special(VK_F5), None);
    }

    #[test]
    fn a_fresh_capture_is_not_running_and_not_suppressing() {
        let capture = WindowsCapture::new();
        assert!(!capture.is_capturing());
        assert!(!capture.suppresses_local());
    }

    #[test]
    fn stopping_a_capture_that_never_started_is_an_error_not_a_panic() {
        let mut capture = WindowsCapture::new();
        assert!(matches!(capture.stop(), Err(PlatformError::NotCapturing)));
    }

    #[test]
    fn suppression_can_be_toggled_before_capture_starts() {
        // The agent flips this on every edge crossing and must not have to care
        // whether the hooks happen to be installed yet.
        let mut capture = WindowsCapture::new();
        capture.set_suppress_local(true).unwrap();
        assert!(capture.suppresses_local());
        capture.set_suppress_local(false).unwrap();
        assert!(!capture.suppresses_local());
    }

    /// Installs real hooks. Kept to a single start/stop cycle with suppression off,
    /// so it cannot swallow the developer's input even if it fails midway.
    #[test]
    fn hooks_install_and_tear_down_cleanly() {
        let seen: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_store = Arc::clone(&seen);

        let mut capture = WindowsCapture::new();
        capture
            .start(Box::new(move |event| {
                sink_store.lock().unwrap().push(event);
            }))
            .expect("low-level hooks must install for an interactive session");
        assert!(capture.is_capturing());

        // Starting twice would double every keystroke.
        let second = capture.start(Box::new(|_| {}));
        assert!(matches!(second, Err(PlatformError::AlreadyCapturing)));

        capture.stop().unwrap();
        assert!(!capture.is_capturing());
        // Nothing asserted about the events themselves: a test run has no
        // guaranteed input. What matters is that the thread joined.
    }

    #[test]
    fn stopping_clears_suppression_so_a_restart_cannot_swallow_input() {
        let mut capture = WindowsCapture::new();
        capture.start(Box::new(|_| {})).unwrap();
        capture.set_suppress_local(true).unwrap();
        capture.stop().unwrap();
        assert!(!capture.suppresses_local());
    }
}
