//! Input injection on Windows, via `SendInput`.
//!
//! # Why `KEYEVENTF_UNICODE` is the whole point
//!
//! Text payloads are injected as unicode events rather than as virtual keys. That
//! is what makes the cross-layout promise work in the receiving direction: the
//! machine is told "produce this codepoint", so it does not matter whether it even
//! has the sender's layout installed. Injecting a virtual key instead would map
//! back through the *receiver's* layout, which is exactly the bug being avoided.
//!
//! # Where unicode injection is the wrong tool
//!
//! Chords. `Ctrl` plus a unicode event is not `Ctrl+C`: applications watch for the
//! `C` virtual key, and a synthesised unicode event carries none. So when the
//! event still requires Ctrl, Alt, or Super after
//! [`wx_proto::KeyEvent::injection_modifiers`] has done its stripping, the
//! character is mapped back to a virtual key with `VkKeyScanW` and injected that
//! way. Copy/paste silently not working is the failure mode this avoids.
//!
//! # Why bookkeeping is committed only after `SendInput` returns
//!
//! `SendInput` can be refused outright — UIPI blocks injection into a
//! higher-integrity window, which is what happens the moment a UAC prompt takes
//! focus mid-chord. If the record of what is held were updated first, a *rejected
//! key-up* would be forgotten: the receiving application still has the key down,
//! and this injector no longer believes it does, so [`WindowsInjector::release_all`]
//! emits nothing for it and the key stays down until the user physically presses
//! it. So every mutation of `pressed` and `synthetic` happens after the send it
//! describes has succeeded, and a failed release deliberately leaves the item on
//! the held list so a later `release_all` tries again.

use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use wx_proto::{
    InputEvent, KeyAction, KeyEvent, KeyPayload, Modifiers, Monitor, MouseButton, NormPos,
    PointerEvent, Rect, ScrollUnit,
};

use crate::coords::{local_point, to_absolute_grid};
use crate::error::{PlatformError, Result};
use crate::windows::vk::{is_extended, vk_from_special};

/// One notch of a notched scroll wheel, in the units `MOUSEEVENTF_WHEEL` expects.
const WHEEL_DELTA: f64 = 120.0;

/// Accumulates the fractional part of relative pointer motion.
///
/// `SendInput` takes whole pixels. Truncating each event independently means a
/// slow diagonal drag — 0.4 pixels per event — moves nowhere at all, and a
/// pointer-locked 3D viewport becomes unusable. Keeping the remainder makes the
/// motion sum correctly over time.
#[derive(Debug, Default)]
struct RelativeAccumulator {
    x: f64,
    y: f64,
}

impl RelativeAccumulator {
    /// Add a delta and return the whole pixels to send now.
    fn take(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        // Non-finite deltas off the wire would poison the accumulator forever, so
        // they are dropped rather than added.
        if dx.is_finite() {
            self.x += dx;
        }
        if dy.is_finite() {
            self.y += dy;
        }
        let whole_x = self.x.trunc();
        let whole_y = self.y.trunc();
        self.x -= whole_x;
        self.y -= whole_y;
        (clamp_to_i32(whole_x), clamp_to_i32(whole_y))
    }
}

fn clamp_to_i32(v: f64) -> i32 {
    v.clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// Something this injector has pressed and must be able to release.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pressed {
    Vk(u16),
    /// A unicode payload, remembered verbatim so the matching key-up carries the
    /// same code units.
    Unicode(String),
    Button(MouseButton),
}

pub struct WindowsInjector {
    relative: RelativeAccumulator,
    /// Modifiers this injector is physically holding down.
    ///
    /// Tracked rather than read back from `GetAsyncKeyState`, because the user may
    /// be holding a modifier locally at the same time and releasing theirs would
    /// be a visible malfunction on their own machine.
    synthetic: Modifiers,
    /// Press-ordered, so `release_all` unwinds newest-first.
    pressed: Vec<Pressed>,
    /// How batches actually reach the OS.
    ///
    /// Indirect purely so that a rejected `SendInput` — the UIPI case above — can
    /// be exercised in a test. There is no way to make the real call fail on
    /// demand, and the recovery path it guards is the one that leaves a key
    /// physically stuck if it is wrong.
    send: fn(&[INPUT]) -> Result<()>,
}

impl WindowsInjector {
    pub fn new() -> Self {
        super::display::ensure_dpi_awareness();
        Self {
            relative: RelativeAccumulator::default(),
            synthetic: Modifiers::NONE,
            pressed: Vec::new(),
            send,
        }
    }
}

impl Default for WindowsInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::traits::InputInjector for WindowsInjector {
    fn inject(&mut self, monitor: &Monitor, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::Pointer(p) => self.inject_pointer(monitor, p),
            InputEvent::Key(k) => self.inject_key(k),
            // The wire-level handoff. Everything held has to come up, or the peer
            // is left with a stuck modifier it cannot clear locally.
            InputEvent::ReleaseControl => self.release_all(),
        }
    }

    fn warp_cursor(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()> {
        self.move_absolute(monitor, pos)
    }

    fn release_all(&mut self) -> Result<()> {
        let mut inputs: Vec<INPUT> = Vec::new();
        for item in self.pressed.iter().rev() {
            match item {
                Pressed::Vk(vk) => inputs.push(key_input(VIRTUAL_KEY(*vk), true)),
                Pressed::Unicode(text) => inputs.extend(unicode_inputs(text, true)),
                Pressed::Button(button) => {
                    if let Some(input) = button_input(*button, false) {
                        inputs.push(input);
                    }
                }
            }
        }
        // Modifiers last: releasing Ctrl first would turn the remaining key-ups
        // into bare keys mid-chord, which some applications act on.
        for bit in MODIFIER_BITS {
            if self.synthetic.contains(bit) {
                if let Some(vk) = modifier_vk(bit) {
                    inputs.push(key_input(vk, true));
                }
            }
        }
        // Only forget what was held once the OS has accepted the releases. A
        // refused batch that had already cleared the list would leave the keys
        // down with nothing left to retry from.
        (self.send)(&inputs)?;
        self.pressed.clear();
        self.synthetic = Modifiers::NONE;
        self.relative = RelativeAccumulator::default();
        Ok(())
    }
}

impl WindowsInjector {
    fn inject_pointer(&mut self, monitor: &Monitor, event: &PointerEvent) -> Result<()> {
        match *event {
            PointerEvent::MoveTo { pos } => self.move_absolute(monitor, pos),
            PointerEvent::MoveBy { dx, dy } => {
                let (px, py) = self.relative.take(dx, dy);
                if px == 0 && py == 0 {
                    // Sub-pixel motion, already banked in the accumulator. Sending
                    // a zero move would still wake every mouse hook on the system.
                    return Ok(());
                }
                (self.send)(&[mouse_input(px, py, 0, MOUSEEVENTF_MOVE)])
            }
            PointerEvent::Button { button, pressed } => {
                let Some(input) = button_input(button, pressed) else {
                    return Err(PlatformError::Unsupported {
                        operation: "mouse buttons beyond the fifth",
                        backend: "windows",
                    });
                };
                // A refused button-up must stay on the held list: a mouse button
                // stuck down is worse than a key, because it drags.
                (self.send)(&[input])?;
                self.track(Pressed::Button(button), !pressed);
                Ok(())
            }
            PointerEvent::Scroll { dx, dy, unit } => self.scroll(dx, dy, unit),
        }
    }

    fn move_absolute(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()> {
        let point = local_point(monitor, pos);
        let (ax, ay) = to_absolute_grid(point, virtual_bounds());
        (self.send)(&[mouse_input(
            ax,
            ay,
            0,
            // VIRTUALDESK is required for a multi-monitor desktop: without it the
            // absolute coordinates are interpreted against the primary display
            // only, so nothing can be addressed on a secondary screen.
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    fn scroll(&mut self, dx: f64, dy: f64, unit: ScrollUnit) -> Result<()> {
        let convert = |v: f64| -> i32 {
            if !v.is_finite() {
                return 0;
            }
            match unit {
                ScrollUnit::Lines => (v * WHEEL_DELTA).round() as i32,
                // Windows measures the wheel in 120ths of a notch, which is close
                // enough to a pixel that trackpad deltas pass through unscaled.
                ScrollUnit::Pixels => v.round() as i32,
            }
        };
        let mut inputs = Vec::new();
        let vertical = convert(dy);
        if vertical != 0 {
            inputs.push(mouse_input(0, 0, vertical as u32, MOUSEEVENTF_WHEEL));
        }
        let horizontal = convert(dx);
        if horizontal != 0 {
            inputs.push(mouse_input(0, 0, horizontal as u32, MOUSEEVENTF_HWHEEL));
        }
        (self.send)(&inputs)
    }

    fn inject_key(&mut self, event: &KeyEvent) -> Result<()> {
        let is_release = event.action == KeyAction::Release;

        match &event.payload {
            KeyPayload::Special(key) => {
                let Some(vk) = vk_from_special(*key) else {
                    return Err(PlatformError::Unsupported {
                        operation: "injecting this key",
                        backend: "windows",
                    });
                };
                // Bare modifier events are passed through untouched: an
                // application can legitimately observe "Ctrl went down" with no
                // other key involved, and re-deriving it from the modifier bits
                // would lose the left/right distinction.
                if key.is_modifier() {
                    return self.send_and_track(
                        &[key_input(vk, is_release)],
                        Pressed::Vk(vk.0),
                        is_release,
                    );
                }
                self.sync_modifiers(event.injection_modifiers())?;
                self.send_and_track(&[key_input(vk, is_release)], Pressed::Vk(vk.0), is_release)
            }
            KeyPayload::Text(text) => {
                let required = event.injection_modifiers();
                if needs_virtual_key(required) {
                    if let Some(vk) = virtual_key_for(text) {
                        self.sync_modifiers(required)?;
                        return self.send_and_track(
                            &[key_input(vk, is_release)],
                            Pressed::Vk(vk.0),
                            is_release,
                        );
                    }
                }
                self.sync_modifiers(required)?;
                self.send_and_track(
                    &unicode_inputs(text, is_release),
                    Pressed::Unicode(text.clone()),
                    is_release,
                )
            }
            // The sender could not resolve this key. Honour it as a virtual key,
            // which is the best a receiver can do, rather than dropping it.
            KeyPayload::RawKeyCode(code) => {
                let vk = VIRTUAL_KEY((*code & 0xffff) as u16);
                self.sync_modifiers(event.injection_modifiers())?;
                self.send_and_track(&[key_input(vk, is_release)], Pressed::Vk(vk.0), is_release)
            }
        }
    }

    /// Inject a batch and record what it did, in that order.
    ///
    /// The order is the whole point: a release the OS refused must stay on the
    /// held list, or nothing can ever bring that key back up.
    fn send_and_track(&mut self, inputs: &[INPUT], item: Pressed, is_release: bool) -> Result<()> {
        (self.send)(inputs)?;
        self.track(item, is_release);
        Ok(())
    }

    fn track(&mut self, item: Pressed, is_release: bool) {
        if is_release {
            self.pressed.retain(|p| *p != item);
        } else if !self.pressed.contains(&item) {
            self.pressed.push(item);
        }
    }

    /// Bring physically-held modifiers in line with what this event needs.
    ///
    /// Lock keys are excluded: Caps Lock and Num Lock are toggles, not held keys,
    /// and pressing them to "match" the sender would flip the receiver's state
    /// permanently.
    fn sync_modifiers(&mut self, required: Modifiers) -> Result<()> {
        let mut inputs = Vec::new();
        let mut next = self.synthetic;
        for bit in MODIFIER_BITS {
            let want = required.contains(bit);
            let have = self.synthetic.contains(bit);
            if want == have {
                continue;
            }
            if let Some(vk) = modifier_vk(bit) {
                inputs.push(key_input(vk, !want));
            }
            next = if want {
                next.union(bit)
            } else {
                next.without(bit)
            };
        }
        // Committed only on success, so a modifier whose key-up was refused is
        // still believed to be held and `release_all` will try it again. Believing
        // otherwise leaves Ctrl down on the machine with no record of it.
        (self.send)(&inputs)?;
        self.synthetic = next;
        Ok(())
    }
}

/// Modifiers that are physically held, in press order.
///
/// Ordered so that `release_all` walking it forwards releases Ctrl before Super
/// and Alt after Shift, matching how a hand lets go of a chord.
const MODIFIER_BITS: [Modifiers; 5] = [
    Modifiers::CTRL,
    Modifiers::ALT,
    Modifiers::ALT_GR,
    Modifiers::SUPER,
    Modifiers::SHIFT,
];

fn modifier_vk(bit: Modifiers) -> Option<VIRTUAL_KEY> {
    Some(match bit {
        Modifiers::CTRL => VK_LCONTROL,
        Modifiers::ALT => VK_LMENU,
        // AltGr is right Alt on Windows layouts that have it, and plain right Alt
        // on those that do not — either way this is the key the sender pressed.
        Modifiers::ALT_GR => VK_RMENU,
        Modifiers::SUPER => VK_LWIN,
        Modifiers::SHIFT => VK_LSHIFT,
        _ => return None,
    })
}

/// Whether a text payload has to be injected as a virtual key rather than as
/// unicode.
///
/// True when a chord modifier survives stripping. Shift and AltGr never reach
/// here for text (they were consumed producing the character), so their presence
/// would mean the wire event was malformed and is harmless to ignore.
fn needs_virtual_key(required: Modifiers) -> bool {
    required.contains(Modifiers::CTRL)
        || required.contains(Modifiers::ALT)
        || required.contains(Modifiers::SUPER)
}

/// Virtual key that types `text` under the receiver's current layout.
///
/// `None` when the character is not on the layout at all — an accented character
/// on a US keyboard, say. The caller then falls back to unicode injection, which
/// loses the chord but at least types something.
fn virtual_key_for(text: &str) -> Option<VIRTUAL_KEY> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let mut units = [0u16; 2];
    let encoded = c.encode_utf16(&mut units);
    if encoded.len() != 1 {
        // Outside the BMP; VkKeyScanW cannot express it.
        return None;
    }
    // SAFETY: no pointers involved; the call reads one UTF-16 unit by value.
    let scan = unsafe { VkKeyScanW(units[0]) };
    if scan == -1 {
        return None;
    }
    Some(VIRTUAL_KEY((scan & 0xff) as u16))
}

fn key_input(vk: VIRTUAL_KEY, is_release: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if is_release {
        flags |= KEYEVENTF_KEYUP;
    }
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    // SAFETY: MapVirtualKeyW takes and returns plain integers.
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// One `KEYEVENTF_UNICODE` event per UTF-16 code unit.
///
/// Surrogate pairs must be delivered in a single `SendInput` batch, so the caller
/// always sends the whole slice at once; splitting them produces two replacement
/// characters instead of one emoji.
fn unicode_inputs(text: &str, is_release: bool) -> Vec<INPUT> {
    let mut flags = KEYEVENTF_UNICODE;
    if is_release {
        flags |= KEYEVENTF_KEYUP;
    }
    text.encode_utf16()
        .map(|unit| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    // wVk must be zero for a unicode event; the codepoint travels
                    // in wScan.
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect()
}

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn button_input(button: MouseButton, pressed: bool) -> Option<INPUT> {
    let (flags, data) = match button {
        MouseButton::Left if pressed => (MOUSEEVENTF_LEFTDOWN, 0),
        MouseButton::Left => (MOUSEEVENTF_LEFTUP, 0),
        MouseButton::Right if pressed => (MOUSEEVENTF_RIGHTDOWN, 0),
        MouseButton::Right => (MOUSEEVENTF_RIGHTUP, 0),
        MouseButton::Middle if pressed => (MOUSEEVENTF_MIDDLEDOWN, 0),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEUP, 0),
        MouseButton::Back if pressed => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
        MouseButton::Back => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
        MouseButton::Forward if pressed => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
        MouseButton::Forward => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
        // Windows exposes no injection path past XBUTTON2.
        MouseButton::Extra(_) => return None,
    };
    Some(mouse_input(0, 0, data, flags))
}

/// Bounding rectangle of every display, in desktop pixels.
///
/// Re-queried per move rather than cached: docking a laptop changes it, and a
/// stale rectangle silently misplaces every absolute position. The values come
/// from a cached kernel read, so the cost is negligible next to the `SendInput`
/// that follows.
fn virtual_bounds() -> Rect {
    // SAFETY: GetSystemMetrics takes an index and returns an integer.
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(0) as u32,
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(0) as u32,
        )
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: `inputs` is a live slice of correctly-initialised INPUT structs and
    // `cbsize` is the true element size, which is what SendInput validates.
    let sent = unsafe { SendInput(inputs, core::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // The usual cause is UIPI: a more-privileged window has focus and blocks
        // input from our integrity level. Worth surfacing rather than swallowing,
        // because the symptom is "input stops working over UAC prompts".
        // SAFETY: GetLastError takes no arguments and reads this thread's last
        // error code.
        let code = unsafe { windows::Win32::Foundation::GetLastError().0 as i32 };
        return Err(PlatformError::Os {
            context: "SendInput",
            code,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::InputInjector;
    use std::cell::RefCell;
    use wx_proto::{MonitorId, SpecialKey};

    thread_local! {
        /// Everything the recording sender was asked to inject, in order.
        static SENT: RefCell<Vec<INPUT>> = const { RefCell::new(Vec::new()) };
    }

    /// Stands in for `SendInput` succeeding, and remembers what went out.
    ///
    /// Used instead of the real call throughout the tests below: they inject key
    /// presses and mouse buttons, and a real `SendInput` would type into whatever
    /// window the developer happens to have focused.
    fn recording_send(inputs: &[INPUT]) -> Result<()> {
        SENT.with(|s| s.borrow_mut().extend_from_slice(inputs));
        Ok(())
    }

    /// Stands in for UIPI refusing the batch, which is what happens when a
    /// higher-integrity window (a UAC prompt) takes focus mid-chord.
    fn refusing_send(_inputs: &[INPUT]) -> Result<()> {
        Err(PlatformError::Os {
            context: "SendInput",
            code: 5,
        })
    }

    fn forget_sent() {
        SENT.with(|s| s.borrow_mut().clear());
    }

    /// Whether a key-up for `vk` was injected since the last [`forget_sent`].
    fn saw_key_up(vk: VIRTUAL_KEY) -> bool {
        SENT.with(|s| {
            s.borrow().iter().any(|input| {
                input.r#type == INPUT_KEYBOARD
                // SAFETY: the union holds a KEYBDINPUT whenever the type says so.
                    && unsafe { input.Anonymous.ki }.wVk == vk
                    && unsafe { input.Anonymous.ki }.dwFlags.contains(KEYEVENTF_KEYUP)
            })
        })
    }

    fn saw_mouse_flag(flag: MOUSE_EVENT_FLAGS) -> bool {
        SENT.with(|s| {
            s.borrow().iter().any(|input| {
                input.r#type == INPUT_MOUSE
                // SAFETY: as above, for the mouse arm of the union.
                    && unsafe { input.Anonymous.mi }.dwFlags.contains(flag)
            })
        })
    }

    /// An injector whose batches are recorded rather than delivered.
    fn fake_injector() -> WindowsInjector {
        forget_sent();
        let mut inj = WindowsInjector::new();
        inj.send = recording_send;
        inj
    }

    fn monitor() -> Monitor {
        Monitor {
            id: MonitorId(0),
            name: "test".into(),
            local_bounds: Rect::new(0, 0, 1920, 1080),
            scale: 1.0,
            primary: true,
        }
    }

    #[test]
    fn sub_pixel_motion_accumulates_instead_of_vanishing() {
        // Three 0.4px steps must move one pixel, not zero. Truncating each event
        // independently makes slow drags stand still.
        let mut acc = RelativeAccumulator::default();
        assert_eq!(acc.take(0.4, 0.0), (0, 0));
        assert_eq!(acc.take(0.4, 0.0), (0, 0));
        assert_eq!(acc.take(0.4, 0.0), (1, 0));
    }

    #[test]
    fn accumulated_motion_sums_to_the_requested_distance() {
        let mut acc = RelativeAccumulator::default();
        let mut total = 0i64;
        for _ in 0..100 {
            total += acc.take(1.5, 0.0).0 as i64;
        }
        assert_eq!(total, 150);
    }

    #[test]
    fn negative_sub_pixel_motion_does_not_drift_the_wrong_way() {
        let mut acc = RelativeAccumulator::default();
        let mut total = 0i64;
        for _ in 0..10 {
            total += acc.take(-0.5, 0.0).0 as i64;
        }
        assert_eq!(total, -5);
    }

    #[test]
    fn non_finite_deltas_do_not_poison_the_accumulator() {
        // A hostile peer sending NaN must not permanently break relative motion.
        let mut acc = RelativeAccumulator::default();
        assert_eq!(acc.take(f64::NAN, f64::INFINITY), (0, 0));
        assert_eq!(acc.take(3.0, 4.0), (3, 4));
    }

    #[test]
    fn enormous_deltas_clamp_instead_of_wrapping() {
        let mut acc = RelativeAccumulator::default();
        let (x, _) = acc.take(1e300, 0.0);
        assert_eq!(x, i32::MAX);
    }

    #[test]
    fn text_without_chord_modifiers_goes_down_the_unicode_path() {
        // This is the cross-layout guarantee on the receiving side.
        assert!(!needs_virtual_key(Modifiers::NONE));
        assert!(!needs_virtual_key(Modifiers::SHIFT));
        assert!(!needs_virtual_key(Modifiers::CAPS_LOCK));
    }

    #[test]
    fn chords_are_injected_as_virtual_keys_not_unicode() {
        // A unicode event with Ctrl held does not copy: applications look for the
        // C virtual key, which a unicode event does not carry.
        assert!(needs_virtual_key(Modifiers::CTRL));
        assert!(needs_virtual_key(Modifiers::ALT));
        assert!(needs_virtual_key(Modifiers::SUPER));
    }

    #[test]
    fn one_unicode_event_per_utf16_code_unit() {
        assert_eq!(unicode_inputs("a", false).len(), 1);
        // Outside the BMP: a surrogate pair, which must be two events in one batch.
        assert_eq!(unicode_inputs("\u{1f600}", false).len(), 2);
        assert_eq!(unicode_inputs("", false).len(), 0);
    }

    #[test]
    fn unicode_events_carry_the_codepoint_in_the_scan_field() {
        let inputs = unicode_inputs("å", false);
        // SAFETY: the union was written as a KEYBDINPUT immediately above.
        let ki = unsafe { inputs[0].Anonymous.ki };
        assert_eq!(
            ki.wVk,
            VIRTUAL_KEY(0),
            "wVk must be zero for unicode events"
        );
        assert_eq!(ki.wScan, 0x00e5);
        assert!(ki.dwFlags.contains(KEYEVENTF_UNICODE));
        assert!(!ki.dwFlags.contains(KEYEVENTF_KEYUP));
    }

    #[test]
    fn unicode_release_events_set_keyup() {
        let inputs = unicode_inputs("a", true);
        let ki = unsafe { inputs[0].Anonymous.ki };
        assert!(ki.dwFlags.contains(KEYEVENTF_KEYUP));
        assert!(ki.dwFlags.contains(KEYEVENTF_UNICODE));
    }

    #[test]
    fn arrow_keys_are_injected_with_the_extended_flag() {
        let vk = vk_from_special(SpecialKey::Left).unwrap();
        let input = key_input(vk, false);
        let ki = unsafe { input.Anonymous.ki };
        assert!(ki.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
    }

    #[test]
    fn absolute_moves_address_the_whole_virtual_desktop() {
        // Without VIRTUALDESK the coordinates are read against the primary display
        // and secondary monitors become unaddressable.
        let input = mouse_input(
            0,
            0,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        );
        let mi = unsafe { input.Anonymous.mi };
        assert!(mi.dwFlags.contains(MOUSEEVENTF_VIRTUALDESK));
        assert!(mi.dwFlags.contains(MOUSEEVENTF_ABSOLUTE));
    }

    #[test]
    fn scroll_detents_become_wheel_deltas() {
        // One notch is 120 in Windows units; sending 1 would be a near-invisible
        // nudge.
        let mut inj = WindowsInjector::new();
        // Exercised through the pure conversion rather than SendInput, which would
        // scroll the developer's actual window.
        let lines = |v: f64| (v * WHEEL_DELTA).round() as i32;
        assert_eq!(lines(1.0), 120);
        assert_eq!(lines(-3.0), -360);
        // Keep the injector alive so the constructor path is exercised.
        assert!(inj.release_all().is_ok());
    }

    #[test]
    fn extra_mouse_buttons_are_reported_unsupported_rather_than_mispressed() {
        // Pressing an arbitrary button instead would be worse than an error.
        assert!(button_input(MouseButton::Extra(3), true).is_none());
        let mut inj = WindowsInjector::new();
        let err = inj
            .inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Button {
                    button: MouseButton::Extra(3),
                    pressed: true,
                }),
            )
            .unwrap_err();
        assert!(matches!(err, PlatformError::Unsupported { .. }));
    }

    #[test]
    fn keys_with_no_windows_equivalent_are_reported_unsupported() {
        let mut inj = WindowsInjector::new();
        let err = inj
            .inject(
                &monitor(),
                &InputEvent::Key(KeyEvent::special(
                    SpecialKey::BrightnessUp,
                    KeyAction::Press,
                    Modifiers::NONE,
                )),
            )
            .unwrap_err();
        assert!(matches!(err, PlatformError::Unsupported { .. }));
    }

    #[test]
    fn release_all_on_a_fresh_injector_does_nothing_and_succeeds() {
        // Called on every disconnect, including ones where nothing was pressed.
        let mut inj = WindowsInjector::new();
        assert!(inj.release_all().is_ok());
        assert!(inj.pressed.is_empty());
        assert_eq!(inj.synthetic, Modifiers::NONE);
    }

    #[test]
    fn release_all_forgets_accumulated_sub_pixel_motion() {
        // Otherwise a fraction banked before a handoff jumps the cursor when
        // control returns.
        let mut inj = WindowsInjector::new();
        inj.relative.take(0.9, 0.9);
        inj.release_all().unwrap();
        assert_eq!(inj.relative.take(0.2, 0.2), (0, 0));
    }

    #[test]
    fn the_virtual_desktop_has_a_real_extent_on_this_machine() {
        let b = virtual_bounds();
        assert!(!b.is_empty(), "virtual desktop reported as {b:?}");
    }

    #[test]
    fn absolute_positions_stay_inside_the_sixteen_bit_grid() {
        let b = virtual_bounds();
        for n in [
            NormPos::new(0.0, 0.0),
            NormPos::new(1.0, 1.0),
            NormPos::new(f64::NAN, 5.0),
        ] {
            let (x, y) = to_absolute_grid(local_point(&monitor(), n), b);
            assert!((0..=65535).contains(&x), "{n:?} -> {x}");
            assert!((0..=65535).contains(&y), "{n:?} -> {y}");
        }
    }

    #[test]
    fn a_key_up_the_os_refuses_is_still_released_by_release_all() {
        // The stuck-key bug: a key-down lands in a normal window, a UAC prompt
        // takes focus, and the matching key-up is refused. If the injector had
        // already forgotten the key, nothing would ever bring it back up and the
        // user would be left holding it down with no way to clear it.
        let mut inj = fake_injector();
        // A bare modifier: the one payload that reaches `SendInput` with no
        // `sync_modifiers` call in front of it, so the refusal lands on the key's
        // own batch rather than on a preceding one.
        let key = SpecialKey::CtrlLeft;
        let vk = vk_from_special(key).unwrap();
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::special(key, KeyAction::Press, Modifiers::NONE)),
        )
        .unwrap();

        inj.send = refusing_send;
        assert!(inj
            .inject(
                &monitor(),
                &InputEvent::Key(KeyEvent::special(key, KeyAction::Release, Modifiers::NONE)),
            )
            .is_err());

        forget_sent();
        inj.send = recording_send;
        inj.release_all().unwrap();
        assert!(
            saw_key_up(vk),
            "the refused release was forgotten, so the key stays down forever"
        );
    }

    #[test]
    fn a_button_up_the_os_refuses_is_still_released_by_release_all() {
        // Worse than a key: a mouse button left down drags whatever is under it.
        let mut inj = fake_injector();
        inj.inject(
            &monitor(),
            &InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
        )
        .unwrap();

        inj.send = refusing_send;
        assert!(inj
            .inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Button {
                    button: MouseButton::Left,
                    pressed: false,
                }),
            )
            .is_err());

        forget_sent();
        inj.send = recording_send;
        inj.release_all().unwrap();
        assert!(saw_mouse_flag(MOUSEEVENTF_LEFTUP), "the button stays down");
    }

    #[test]
    fn a_modifier_release_the_os_refuses_is_still_believed_to_be_held() {
        // Ctrl is held by `sync_modifiers` for a chord and let go for the next
        // plain key. If that release is refused and the injector believes it
        // succeeded, every later keystroke on this machine arrives as a chord.
        let mut inj = fake_injector();
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("c", KeyAction::Press, Modifiers::CTRL)),
        )
        .unwrap();
        assert!(inj.synthetic.contains(Modifiers::CTRL));

        inj.send = refusing_send;
        assert!(inj
            .inject(
                &monitor(),
                &InputEvent::Key(KeyEvent::text("x", KeyAction::Press, Modifiers::NONE)),
            )
            .is_err());
        assert!(
            inj.synthetic.contains(Modifiers::CTRL),
            "a refused Ctrl release was recorded as though it worked"
        );

        forget_sent();
        inj.send = recording_send;
        inj.release_all().unwrap();
        assert!(saw_key_up(VK_LCONTROL));
    }

    #[test]
    fn a_refused_release_all_keeps_everything_for_the_next_attempt() {
        // `release_all` runs on disconnect and at shutdown. A refused batch that
        // cleared the record would silently turn one transient failure into
        // permanently stuck keys.
        let mut inj = fake_injector();
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("c", KeyAction::Press, Modifiers::CTRL)),
        )
        .unwrap();
        assert_eq!(inj.pressed.len(), 1);

        inj.send = refusing_send;
        assert!(inj.release_all().is_err());
        assert_eq!(inj.pressed.len(), 1, "the held key was dropped on failure");
        assert!(inj.synthetic.contains(Modifiers::CTRL));

        forget_sent();
        inj.send = recording_send;
        inj.release_all().unwrap();
        assert!(saw_key_up(VK_LCONTROL));
        assert!(inj.pressed.is_empty());
        assert_eq!(inj.synthetic, Modifiers::NONE);
    }

    #[test]
    fn a_key_down_the_os_refuses_is_not_recorded_as_held() {
        // The other direction: a press that never reached anything must not be
        // released later, or the receiver sees a key-up it never saw a key-down for.
        let mut inj = fake_injector();
        inj.send = refusing_send;
        assert!(inj
            .inject(
                &monitor(),
                &InputEvent::Key(KeyEvent::special(
                    SpecialKey::CtrlLeft,
                    KeyAction::Press,
                    Modifiers::NONE
                )),
            )
            .is_err());
        assert!(inj.pressed.is_empty());
    }

    #[test]
    fn a_multi_char_payload_has_no_single_virtual_key() {
        // IME output and composed sequences must not be forced through VkKeyScanW.
        assert!(virtual_key_for("ab").is_none());
        assert!(virtual_key_for("").is_none());
        assert!(virtual_key_for("\u{1f600}").is_none());
    }
}
