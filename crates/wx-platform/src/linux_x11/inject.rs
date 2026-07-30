//! Input injection over XTEST.
//!
//! XTEST is the X server's own "pretend this came from the hardware" extension:
//! `XTestFakeInput` puts a key, button or motion event into the server at the
//! bottom of the input pipeline, so it reaches whatever has focus exactly as a
//! real one would. No permission is asked for and no dialog appears — which is
//! precisely the property a machine nobody sits at needs, and the reason this
//! backend exists at all.
//!
//! # What is held, and why it is tracked rather than read back
//!
//! A dropped release strands Ctrl or a mouse button down on the receiving desktop,
//! and the user cannot clear it without physically pressing that key on that
//! machine. So [`Injector`] records exactly what it pressed and
//! [`Injector::release_all`] unwinds precisely that. Never a blanket release: the
//! X server has one keyboard state shared with whoever is sitting at the machine,
//! and letting go of a key this injector never pressed would take theirs up
//! underneath them.
//!
//! Bookkeeping is committed only *after* the request that carries the event has
//! been flushed, for the same reason the Windows and Wayland backends do it: a
//! release that never reached the server must stay on the held list so a later
//! `release_all` tries again.
//!
//! # Three records drive the same physical keys
//!
//! A modifier can be wanted at once by a bare `SpecialKey::ShiftLeft` the sender
//! forwarded, by the chord an event asks for, and by the keymap level a character
//! needs — and each has to leave the key alone while another still wants it down.
//! That is the same problem the Wayland injector in
//! `crates/wx-platform/src/linux_wayland/inject.rs` solves, and the model
//! here is deliberately the same one, because every wrong answer is silent: press a
//! key that is already down and the server sees a repeat, release one this injector
//! never pressed and the user's own modifier goes up under them, trust a stale
//! claim and the chord loses its modifier without a word.
//!
//! # The two X11 specifics
//!
//! **Everything is a keycode.** Unlike Windows there is no "type this codepoint"
//! request, so a character has to be found on the receiving desktop's own layout —
//! [`super::keymap`] — and a character that is not on it is refused rather than
//! guessed at.
//!
//! **Scrolling is buttons.** X11 core has no scroll axis: a notch up is a click of
//! button 4 and a notch down a click of button 5, with 6 and 7 for horizontal. A
//! trackpad's pixel deltas therefore have to be banked into whole detents rather
//! than truncated, or a slow flick scrolls nowhere at all.

use std::sync::Arc;

use wx_proto::{
    InputEvent, KeyAction, KeyEvent, KeyPayload, Modifiers, Monitor, MouseButton, NormPos,
    PointerEvent, ScrollUnit,
};

use super::keymap::{keysym_from_special, Keymap, LevelMods};
use super::BACKEND;
use crate::coords::local_point;
use crate::error::{PlatformError, Result};
use crate::linux_wayland::keys::{
    char_keysyms, dead_keysym, evdev_from_special, XKB_KEYCODE_OFFSET,
};

/// X11 core button numbers. The wheel occupies four of them, which is why
/// scrolling is expressed as clicks below.
const BUTTON_LEFT: u8 = 1;
const BUTTON_MIDDLE: u8 = 2;
const BUTTON_RIGHT: u8 = 3;
const BUTTON_SCROLL_UP: u8 = 4;
const BUTTON_SCROLL_DOWN: u8 = 5;
const BUTTON_SCROLL_LEFT: u8 = 6;
const BUTTON_SCROLL_RIGHT: u8 = 7;
/// The thumb buttons, and what browsers read as Back and Forward.
const BUTTON_BACK: u8 = 8;
const BUTTON_FORWARD: u8 = 9;
/// Where a mouse's remaining buttons land. X11 numbers them on past this with no
/// fixed meaning, which is exactly what [`MouseButton::Extra`] is.
const FIRST_EXTRA_BUTTON: u8 = 10;

/// Logical pixels of smooth scrolling that make one wheel detent.
///
/// X11 has no smooth scroll to pass a trackpad delta through to, so the wire's
/// pixel unit has to be converted into whole clicks. Ten is the `wl_pointer`
/// convention the sending side's compositor emits one wheel notch as, so a flick
/// on a Wayland machine travels a comparable distance on an X11 one. The remainder
/// is banked rather than truncated — see [`ScrollAccumulator`].
const SCROLL_PIXELS_PER_DETENT: f64 = 10.0;

/// Fallback keycodes for the modifiers, used only when the keymap names none.
///
/// The evdev code plus eight, which is where every Linux X server puts them. A
/// fallback rather than the answer, because a layout is free to move a modifier —
/// `ctrl:nocaps` puts Control on the Caps Lock key — and pressing the usual
/// position would then toggle the user's capitals instead of holding Control.
const FALLBACK_CTRL: u8 = 37;
const FALLBACK_ALT: u8 = 64;
const FALLBACK_SUPER: u8 = 133;
const FALLBACK_SHIFT: u8 = 50;
const FALLBACK_ALT_GR: u8 = 108;

/// X11 keysyms for the modifiers this injector holds.
const XK_CONTROL_L: u32 = 0xffe3;
const XK_ALT_L: u32 = 0xffe9;
const XK_SUPER_L: u32 = 0xffeb;
const XK_SHIFT_L: u32 = 0xffe1;

/// One request this backend puts on the wire.
///
/// Named rather than sent straight through so that a test can assert on exactly
/// what would have reached the server. Injection is the one thing in this crate
/// that cannot be exercised against the developer's own session — it would type
/// into whatever window they have focused — so the seam has to be here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Request {
    /// Pointer motion. `relative` picks between a position on the screen and a
    /// delta from wherever the pointer is.
    Motion {
        x: i16,
        y: i16,
        relative: bool,
    },
    Button {
        button: u8,
        press: bool,
    },
    Key {
        keycode: u8,
        press: bool,
    },
}

/// Where this backend's requests go, and what it may ask the server.
///
/// `Sync` as well as `Send` because the injector holds it behind an [`Arc`] and
/// the same connection answers display enumeration from the engine loop.
pub(super) trait Sink: Send + Sync {
    /// Put one request on the wire and flush it.
    ///
    /// Flushed per event rather than batched: input latency is the product, and a
    /// buffered keystroke that arrives with the next one is worse than an extra
    /// write.
    fn send(&self, request: Request) -> Result<()>;

    /// The receiving desktop's layout as it stands now.
    ///
    /// Re-asked rather than cached on the injector, because the user can change
    /// their layout mid-session and every answer this module gives depends on it.
    fn keymap(&self) -> Result<Arc<Keymap>>;

    /// Whether the desktop's Caps Lock is on right now.
    ///
    /// Read from the server rather than tracked, because it is *the desktop's*
    /// state: this injector did not set it and must not clear it, and something
    /// else may have changed it since the last keystroke.
    fn caps_locked(&self) -> Result<bool>;
}

/// The sink for a backend that has no X server to talk to.
///
/// Its own type rather than an `Option` on the injector, so that every refusal
/// names the real cause — "no display named in DISPLAY", a connection that was
/// closed — instead of a generic "unsupported" that sends whoever reads the log
/// looking in the wrong place.
pub(super) struct NoServer {
    reason: String,
}

impl NoServer {
    pub(super) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    fn refuse<T>(&self) -> Result<T> {
        Err(PlatformError::Other(self.reason.clone()))
    }
}

impl Sink for NoServer {
    fn send(&self, _request: Request) -> Result<()> {
        self.refuse()
    }

    fn keymap(&self) -> Result<Arc<Keymap>> {
        self.refuse()
    }

    fn caps_locked(&self) -> Result<bool> {
        self.refuse()
    }
}

/// Accumulates the fractional part of relative pointer motion.
///
/// XTEST takes whole pixels. Truncating each event independently means a slow
/// diagonal drag — 0.4 pixels per event — moves nowhere at all, and a
/// pointer-locked 3D viewport becomes unusable. Keeping the remainder makes the
/// motion sum correctly over time.
#[derive(Debug, Default)]
struct RelativeAccumulator {
    x: f64,
    y: f64,
}

impl RelativeAccumulator {
    fn take(&mut self, dx: f64, dy: f64) -> (i16, i16) {
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
        (clamp_i16(whole_x), clamp_i16(whole_y))
    }
}

/// Accumulates scrolling that has not yet amounted to a whole detent.
///
/// The same rule as [`RelativeAccumulator`] and for the same reason: X11 can only
/// express whole clicks, so a trackpad delta smaller than one would otherwise
/// vanish and slow scrolling would do nothing at all.
#[derive(Debug, Default)]
struct ScrollAccumulator {
    x: f64,
    y: f64,
}

impl ScrollAccumulator {
    /// Add a scroll in detents and return the whole clicks to send now.
    fn take(&mut self, dx: f64, dy: f64) -> (i32, i32) {
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
        (clamp_i32(whole_x), clamp_i32(whole_y))
    }
}

fn clamp_i16(v: f64) -> i16 {
    v.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

fn clamp_i32(v: f64) -> i32 {
    v.clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

/// Something this injector pressed and is therefore responsible for releasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Key(u8),
    Button(u8),
}

/// One modifier key a keystroke asks to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModifierStep {
    keycode: u8,
    release: bool,
    /// Whether the key itself has to move, or only the bookkeeping.
    ///
    /// False on a press whose key is already down for someone else's reason, and
    /// on a release of a key this injector never pressed or that another record
    /// still wants: a record that stops wanting a key forgets it, but must not
    /// pull it out from under the others.
    physical: bool,
}

/// The keycodes this layout puts the held modifiers on.
///
/// Resolved once per keystroke from the keymap rather than assumed, and carried as
/// a value so the planning below stays pure and testable with no server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModifierKeys {
    ctrl: u8,
    alt: u8,
    super_: u8,
    shift: u8,
    /// `None` on a layout with no level-3 key at all. Pressing right Alt on the
    /// chance it works turns the keystroke into an Alt chord and opens a menu, so
    /// the character is refused instead.
    level3: Option<u8>,
}

impl ModifierKeys {
    fn resolve(keymap: &Keymap) -> Self {
        Self {
            ctrl: keymap.keycode(XK_CONTROL_L).unwrap_or(FALLBACK_CTRL),
            alt: keymap.keycode(XK_ALT_L).unwrap_or(FALLBACK_ALT),
            super_: keymap.keycode(XK_SUPER_L).unwrap_or(FALLBACK_SUPER),
            shift: keymap
                .shift_keycode()
                .or_else(|| keymap.keycode(XK_SHIFT_L))
                .unwrap_or(FALLBACK_SHIFT),
            level3: keymap.level3_keycode(),
        }
    }

    /// The level-3 key, or the usual position when the layout named none.
    ///
    /// Only for releasing and for planning that must not press it: a *press* goes
    /// through [`ModifierKeys::level3`] so a layout without one refuses rather than
    /// opening a menu.
    fn level3_or_usual(&self) -> u8 {
        self.level3.unwrap_or(FALLBACK_ALT_GR)
    }
}

/// The receiving half of the input path on X11.
pub(super) struct Injector {
    sink: Arc<dyn Sink>,
    /// Press-ordered, so `release_all` unwinds newest first.
    held: Vec<Held>,
    /// Modifiers this injector is holding because the sender asked for them.
    chord: Modifiers,
    /// Shift and the level-3 key held to reach a keymap level. Kept apart from
    /// `chord` because they are consumed by layout resolution rather than requested
    /// by the sender — but they drive the same two physical keys, so neither set
    /// moves one without asking the other.
    level: LevelMods,
    /// Modifier keycodes this injector actually pressed and has not let go of.
    ///
    /// `chord` and `level` record what each keystroke *wants*, which is not the
    /// same thing: a modifier the sender is already holding is wanted by both and
    /// pressed by neither. Only this list says who will release the key.
    pressed: Vec<u8>,
    relative: RelativeAccumulator,
    scroll: ScrollAccumulator,
}

impl Injector {
    pub(super) fn new(sink: Arc<dyn Sink>) -> Self {
        Self {
            sink,
            held: Vec::new(),
            chord: Modifiers::NONE,
            level: LevelMods::default(),
            pressed: Vec::new(),
            relative: RelativeAccumulator::default(),
            scroll: ScrollAccumulator::default(),
        }
    }

    pub(super) fn inject(&mut self, monitor: &Monitor, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::Pointer(p) => self.inject_pointer(monitor, p),
            InputEvent::Key(k) => self.inject_key(k),
            // The wire-level handoff: everything held has to come up, or the peer
            // is left with a stuck modifier it cannot clear locally.
            InputEvent::ReleaseControl => self.release_all(),
        }
    }

    pub(super) fn warp_cursor(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()> {
        self.move_absolute(monitor, pos)
    }

    /// Let go of everything this injector is holding, newest first.
    ///
    /// The one method that must work on an error path, because that is the only
    /// path it is ever called from: a disconnect, a revoked session, a shutdown.
    /// Two rules follow. Nothing is forgotten until the release that describes it
    /// has actually been flushed, so a failure halfway leaves the rest on the list
    /// for the next attempt rather than pretending the desktop is clear. And the
    /// modifier keycodes are resolved from the fallbacks when the keymap cannot be
    /// read, because a server that has stopped answering questions is exactly when
    /// something is still held down.
    pub(super) fn release_all(&mut self) -> Result<()> {
        if self.held.is_empty()
            && self.pressed.is_empty()
            && self.chord.is_empty()
            && self.level == LevelMods::default()
        {
            self.relative = RelativeAccumulator::default();
            self.scroll = ScrollAccumulator::default();
            return Ok(());
        }

        for held in self.held.clone().iter().rev() {
            match *held {
                Held::Key(keycode) => self.sink.send(Request::Key {
                    keycode,
                    press: false,
                })?,
                Held::Button(button) => self.sink.send(Request::Button {
                    button,
                    press: false,
                })?,
            }
            self.held.retain(|h| h != held);
        }

        // Modifiers last: letting Ctrl go first would turn the remaining releases
        // into bare keys mid-chord, which some applications act on.
        for keycode in self.modifier_release_order() {
            self.sink.send(Request::Key {
                keycode,
                press: false,
            })?;
            self.pressed.retain(|c| *c != keycode);
        }

        // Only once every release is on the wire.
        self.chord = Modifiers::NONE;
        self.level = LevelMods::default();
        self.relative = RelativeAccumulator::default();
        self.scroll = ScrollAccumulator::default();
        Ok(())
    }

    /// Every modifier keycode this injector pressed, in release order.
    ///
    /// Read off `pressed` rather than off the two records, which name the same key
    /// more than once and name keys the sender pressed itself. Those are already
    /// up: the held list was unwound first, and there is only ever one key down to
    /// let go of.
    ///
    /// Ordered by hand rather than by press order so that Ctrl goes last whichever
    /// way round the chord was assembled.
    fn modifier_release_order(&self) -> Vec<u8> {
        // The keymap is asked for, not required: this runs on the path where the
        // connection may already be failing, and the usual positions are a better
        // answer than declining to release anything.
        let keys = match self.sink.keymap() {
            Ok(keymap) => ModifierKeys::resolve(&keymap),
            Err(_) => ModifierKeys {
                ctrl: FALLBACK_CTRL,
                alt: FALLBACK_ALT,
                super_: FALLBACK_SUPER,
                shift: FALLBACK_SHIFT,
                level3: Some(FALLBACK_ALT_GR),
            },
        };

        let mut codes = Vec::new();
        for code in [
            keys.level3_or_usual(),
            keys.shift,
            keys.super_,
            keys.alt,
            keys.ctrl,
        ] {
            if self.pressed.contains(&code) && !codes.contains(&code) {
                codes.push(code);
            }
        }
        // Anything else on the list — a modifier the layout has since moved, so the
        // keycode resolved differently from the one that was pressed — still has to
        // come up, or it stays down forever.
        for code in &self.pressed {
            if !codes.contains(code) {
                codes.push(*code);
            }
        }
        codes
    }

    fn inject_pointer(&mut self, monitor: &Monitor, event: &PointerEvent) -> Result<()> {
        match *event {
            PointerEvent::MoveTo { pos } => self.move_absolute(monitor, pos),
            PointerEvent::MoveBy { dx, dy } => {
                let (px, py) = self.relative.take(dx, dy);
                if px == 0 && py == 0 {
                    // Sub-pixel motion, already banked. Sending a zero move would
                    // still wake every pointer listener on the desktop.
                    return Ok(());
                }
                self.sink.send(Request::Motion {
                    x: px,
                    y: py,
                    relative: true,
                })
            }
            PointerEvent::Button { button, pressed } => self.inject_button(button, pressed),
            PointerEvent::Scroll { dx, dy, unit } => self.inject_scroll(dx, dy, unit),
        }
    }

    fn move_absolute(&mut self, monitor: &Monitor, pos: NormPos) -> Result<()> {
        let point = local_point(monitor, pos);
        self.sink.send(Request::Motion {
            // X11 screen coordinates are 16-bit, and `local_point` has already
            // clamped into the monitor, so this only bites on a rectangle the
            // server could not have produced.
            x: clamp_i16(point.x),
            y: clamp_i16(point.y),
            relative: false,
        })
    }

    fn inject_button(&mut self, button: MouseButton, pressed: bool) -> Result<()> {
        let code = x11_button(button).ok_or(PlatformError::Unsupported {
            operation: "this mouse button",
            backend: BACKEND,
        })?;
        self.sink.send(Request::Button {
            button: code,
            press: pressed,
        })?;
        // Recorded only once the server has it. A button-up that never left this
        // process must stay on the list, because a mouse button left down drags
        // whatever is under it.
        self.track(Held::Button(code), !pressed);
        Ok(())
    }

    /// Scroll by clicking the wheel buttons, which is all X11 core offers.
    fn inject_scroll(&mut self, dx: f64, dy: f64, unit: ScrollUnit) -> Result<()> {
        let detents = |v: f64| match unit {
            ScrollUnit::Lines => v,
            ScrollUnit::Pixels => v / SCROLL_PIXELS_PER_DETENT,
        };
        // The wire follows the Windows sense — positive is away from the user — and
        // so does X11's button 4, which is scroll *up*. So the vertical sign passes
        // through unchanged, unlike on the Wayland backend where it has to be
        // flipped.
        let (x, y) = self.scroll.take(detents(dx), detents(dy));
        if x == 0 && y == 0 {
            return Ok(());
        }

        // Vertical first, because a diagonal flick is overwhelmingly a vertical one
        // with a little sideways in it, and the application sees the dominant axis
        // first.
        self.click_wheel(y, BUTTON_SCROLL_UP, BUTTON_SCROLL_DOWN)?;
        self.click_wheel(x, BUTTON_SCROLL_RIGHT, BUTTON_SCROLL_LEFT)
    }

    /// One press-and-release of a wheel button per detent.
    ///
    /// Both halves, because an application counts the *press* but a button left
    /// down is a button held: X11 makes no distinction between the wheel and a
    /// finger, so a missing release strands button 4 down and every later click
    /// arrives as a drag.
    fn click_wheel(&mut self, detents: i32, positive: u8, negative: u8) -> Result<()> {
        let button = if detents >= 0 { positive } else { negative };
        // `unsigned_abs` and not a negation: `ScrollAccumulator::take` clamps into
        // `i32`, so `i32::MIN` is a value a peer can actually produce, and it has no
        // positive counterpart to negate to. Negating it panics in a debug build and
        // wraps back to `i32::MIN` in a release one — where the bound below would
        // then pass it through as an empty range and drop the scroll entirely, at
        // exactly the input the bound exists to catch.
        let count = detents.unsigned_abs();
        // A peer sending an absurd delta would otherwise hold this loop — and the
        // engine thread it runs on — for as long as it liked.
        for _ in 0..count.min(MAX_SCROLL_CLICKS) {
            self.sink.send(Request::Button {
                button,
                press: true,
            })?;
            self.sink.send(Request::Button {
                button,
                press: false,
            })?;
        }
        Ok(())
    }

    fn inject_key(&mut self, event: &KeyEvent) -> Result<()> {
        let release = event.action == KeyAction::Release;
        match &event.payload {
            KeyPayload::Special(key) => {
                let keycode = self.special_keycode(*key)?;
                // Bare modifier events pass through untouched: an application can
                // legitimately observe "Ctrl went down" with no other key involved,
                // and re-deriving it from the modifier bits would lose the
                // left/right distinction the sender took care to keep.
                if key.is_modifier() {
                    return self.forward_modifier(keycode, release);
                }
                self.sync_chord(event.injection_modifiers())?;
                // A Shift or level-3 key still held to reach some earlier
                // character's level is not part of this key, and leaving it down
                // turns Enter into AltGr+Enter.
                self.sync_level(LevelMods::default())?;
                self.send_key(keycode, release)
            }
            KeyPayload::Text(text) => self.inject_text(text, event, release),
            // The sender could not resolve this key through its own layout, so what
            // arrives is a keycode from a platform whose numbering may not be ours.
            // Honouring it is still better than dropping the keystroke — the same
            // call the Windows and Wayland backends make — but it is worth a line
            // in the log when a key turns out to do something unexpected.
            KeyPayload::RawKeyCode(code) => {
                let Ok(keycode) = u8::try_from(*code) else {
                    tracing::debug!(
                        code,
                        "an unresolved raw keycode is outside the range X11 can address; dropping it"
                    );
                    return Err(PlatformError::Unsupported {
                        operation: "injecting a keycode X11 cannot address",
                        backend: BACKEND,
                    });
                };
                tracing::debug!(
                    code,
                    "injecting an unresolved raw keycode; the sender's numbering may not be this server's"
                );
                self.sync_chord(event.injection_modifiers())?;
                self.sync_level(LevelMods::default())?;
                self.send_key(keycode, release)
            }
        }
    }

    /// The keycode a key with no textual meaning sits on.
    ///
    /// The keymap first, because the keysym is what the key *means*: `ctrl:nocaps`
    /// moves Control onto the Caps Lock key, and pressing the usual position would
    /// toggle the user's capitals instead of holding Control. The evdev offset is
    /// the fallback for the keys a minimal keymap leaves out — a server with no
    /// media keys mapped still has the hardware position, and pressing it is a
    /// better answer than refusing.
    fn special_keycode(&self, key: wx_proto::SpecialKey) -> Result<u8> {
        if let Ok(keymap) = self.sink.keymap() {
            if let Some(keycode) = keymap.keycode(keysym_from_special(key)) {
                return Ok(keycode);
            }
        }
        evdev_from_special(key)
            .and_then(|code| u8::try_from(code + XKB_KEYCODE_OFFSET).ok())
            .ok_or(PlatformError::Unsupported {
                operation: "injecting this key",
                backend: BACKEND,
            })
    }

    /// Type text by finding it on the receiving desktop's own layout.
    ///
    /// This is where the cross-layout promise is kept. Its limit here is the one
    /// [`super::keymap`] states: a character the layout cannot produce is named in
    /// the log and refused, rather than typed as whatever shares its position.
    fn inject_text(&mut self, text: &str, event: &KeyEvent, release: bool) -> Result<()> {
        let mut chars = text.chars();
        let (Some(first), rest) = (chars.next(), chars.as_str()) else {
            // An empty payload is not a keystroke. Nothing to press, nothing to
            // fail about.
            return Ok(());
        };

        if rest.is_empty() {
            let strokes = self.resolve(first)?;
            self.sync_chord(event.injection_modifiers())?;
            // One key means the press and release can be paired the way the sender
            // sent them, so a held letter repeats on the receiver as it would
            // locally.
            if let [only] = strokes[..] {
                self.sync_level(only.mods)?;
                self.send_key(only.keycode, release)?;
                // The level modifiers belong to this keystroke and nothing after
                // it. Held past the key-up they reach the next event and the peer
                // sees a modifier it never sent.
                if release {
                    self.sync_level(LevelMods::default())?;
                }
                return Ok(());
            }
            // A dead-key sequence has no single key to hold: the accent has to be
            // pressed and let go before the base letter means anything. So the
            // whole thing is typed on the press and the release has nothing left to
            // do.
            if release {
                return Ok(());
            }
            return self.tap(&strokes);
        }

        // Several codepoints in one event: IME output, an emoji with a modifier, or
        // a composed sequence the sender flushed as one payload. Same reasoning as
        // above — nothing here is a single held key.
        if release {
            return Ok(());
        }
        self.sync_chord(event.injection_modifiers())?;
        for c in text.chars() {
            let strokes = self.resolve(c)?;
            self.tap(&strokes)?;
        }
        Ok(())
    }

    /// Press and release each key in turn, holding each one's level only for it.
    fn tap(&mut self, strokes: &[Resolved]) -> Result<()> {
        for stroke in strokes {
            self.sync_level(stroke.mods)?;
            self.send_key(stroke.keycode, false)?;
            self.send_key(stroke.keycode, true)?;
        }
        self.sync_level(LevelMods::default())
    }

    /// The keys that produce `c` on the loaded layout, in the order to press them.
    ///
    /// Usually one. Two when the layout has no single key for the character but
    /// does have the dead accent that composes it — `é` on a Norwegian keyboard is
    /// the dead acute followed by `e`, and typing it that way is what somebody
    /// sitting at that machine would do.
    fn resolve(&self, c: char) -> Result<Vec<Resolved>> {
        let keymap = self.sink.keymap()?;

        if let Some(stroke) = keymap.stroke_for_any(char_keysyms(c)) {
            return Ok(vec![self.reachable(&keymap, stroke)?]);
        }

        // Nothing direct. If the sender composed this character, the receiver's
        // layout very likely composes it the same way.
        if let Some((base, mark)) = crate::keyres::decompose(c) {
            let dead = dead_keysym(mark).and_then(|k| keymap.stroke(k));
            let base = keymap.stroke_for_any(char_keysyms(base));
            if let (Some(dead), Some(base)) = (dead, base) {
                tracing::debug!(
                    character = %c,
                    "composing through the layout's dead key; there is no single key for it"
                );
                return Ok(vec![
                    self.reachable(&keymap, dead)?,
                    self.reachable(&keymap, base)?,
                ]);
            }
        }

        // Named rather than swallowed, because this is the one failure the whole
        // text-over-the-wire design exists to avoid and the user needs to be able to
        // attribute it. Pressing something else instead would type a character
        // nobody asked for.
        //
        // X11 is the one platform where this is fixable — a scratch keycode can be
        // remapped to any keysym — and that is the piece deliberately left for
        // later, because a remap not restored on every exit path leaves the
        // keyboard corrupted until the user logs out.
        tracing::warn!(
            codepoint = format!("U+{:04X}", c as u32),
            character = %c,
            "the receiving desktop's keyboard layout cannot produce this character, so it was not injected"
        );
        Err(PlatformError::Unsupported {
            operation: "injecting a character the loaded keyboard layout cannot produce",
            backend: BACKEND,
        })
    }

    /// Turn a keymap lookup into something this backend can actually press.
    ///
    /// Two adjustments, both about state that belongs to the desktop rather than to
    /// us. Caps Lock is a *locked* modifier this injector did not set and must not
    /// clear, so the level is worked around it: pressing `a` with Caps Lock on
    /// produces `A` unless the shift is inverted. And a level-3 character on a
    /// layout that defines no such key is refused rather than attempted, because
    /// pressing right Alt on the chance it works turns the keystroke into an Alt
    /// chord and opens a menu.
    fn reachable(&self, keymap: &Keymap, stroke: super::keymap::Stroke) -> Result<Resolved> {
        let mut mods = stroke.mods;
        if stroke.alphabetic && keymap.caps_is_lock() && self.sink.caps_locked().unwrap_or(false) {
            mods.shift = !mods.shift;
        }
        if mods.level3 && keymap.level3_keycode().is_none() {
            return Err(PlatformError::Unsupported {
                operation: "injecting a character whose layout level has no modifier key",
                backend: BACKEND,
            });
        }
        Ok(Resolved {
            keycode: stroke.keycode,
            mods,
        })
    }

    /// Bring the chord modifiers in line with what this event asks for.
    ///
    /// Lock keys are deliberately absent: Caps Lock and Num Lock are toggles, not
    /// held keys, and pressing them to "match" the sender would flip the receiver's
    /// state permanently.
    fn sync_chord(&mut self, required: Modifiers) -> Result<()> {
        let wanted = required
            .without(Modifiers::CAPS_LOCK)
            .without(Modifiers::NUM_LOCK);
        // Answered without touching the server, so a keystroke that needs no
        // modifier does not depend on the keymap being readable. Deliberately not
        // "the record already says what this event wants": a bit the record borrowed
        // rather than pressed goes stale the moment the sender lets the key go, and
        // skipping on the record alone would inject the next Ctrl+C as a bare `c`.
        if wanted.is_empty() && self.chord.is_empty() {
            return Ok(());
        }
        let keys = self.modifier_keys()?;
        for (bit, step) in self.chord_plan(wanted, keys) {
            self.apply(step)?;
            self.chord = if step.release {
                self.chord.without(bit)
            } else {
                self.chord.union(bit)
            };
        }
        Ok(())
    }

    /// Hold exactly the Shift and level-3 key a keymap level needs.
    fn sync_level(&mut self, required: LevelMods) -> Result<()> {
        if required == LevelMods::default() && self.level == LevelMods::default() {
            return Ok(());
        }
        let keys = self.modifier_keys()?;
        let [shift, third] = self.level_plan(required, keys);
        if let Some(step) = shift {
            self.apply(step)?;
            self.level.shift = required.shift;
        }
        if let Some(step) = third {
            self.apply(step)?;
            self.level.level3 = required.level3;
        }
        Ok(())
    }

    fn modifier_keys(&self) -> Result<ModifierKeys> {
        let keymap = self.sink.keymap()?;
        Ok(ModifierKeys::resolve(&keymap))
    }

    /// Send one step, if it is one the key has to feel, and remember the result.
    ///
    /// `pressed` is updated only after the request that carried it, for the same
    /// reason the held list is: a release the server never received leaves this
    /// injector still responsible for the key.
    fn apply(&mut self, step: ModifierStep) -> Result<()> {
        if !step.physical {
            return Ok(());
        }
        self.sink.send(Request::Key {
            keycode: step.keycode,
            press: !step.release,
        })?;
        if step.release {
            self.pressed.retain(|c| *c != step.keycode);
        } else if !self.pressed.contains(&step.keycode) {
            self.pressed.push(step.keycode);
        }
        Ok(())
    }

    /// The chord modifiers `required` changes, and whether each one's key has to
    /// move.
    ///
    /// Pure, because the difficulty here is not the sending: three independent
    /// records drive the same physical Shift key — a bare `SpecialKey::ShiftLeft`
    /// the sender forwarded, which is injected as an ordinary key and lands on the
    /// held list; this chord set; and the keymap level — and each has to leave the
    /// key alone while another still wants it down.
    ///
    /// Wanting a bit the record already claims is therefore not always a no-op. The
    /// claim may have been borrowed from a key the sender has since released, in
    /// which case the chord is re-asserted rather than assumed.
    fn chord_plan(
        &self,
        required: Modifiers,
        keys: ModifierKeys,
    ) -> Vec<(Modifiers, ModifierStep)> {
        let mut steps = Vec::new();
        for (bit, code) in [
            (Modifiers::CTRL, keys.ctrl),
            (Modifiers::ALT, keys.alt),
            (Modifiers::SUPER, keys.super_),
            (Modifiers::SHIFT, keys.shift),
            (Modifiers::ALT_GR, keys.level3_or_usual()),
        ] {
            let wanted_elsewhere = self.sender_holds(code)
                || (bit == Modifiers::SHIFT && self.level.shift)
                || (bit == Modifiers::ALT_GR && self.level.level3);
            let step = self.step(
                code,
                required.contains(bit),
                self.chord.contains(bit),
                wanted_elsewhere,
            );
            if let Some(step) = step {
                steps.push((bit, step));
            }
        }
        steps
    }

    /// The two level modifiers `required` changes, Shift first.
    ///
    /// The mirror of [`Injector::chord_plan`]: the level path must not release a
    /// Shift the sender is holding or one this injector is holding for a chord, and
    /// it must not press a second time a key either of them already has down.
    fn level_plan(&self, required: LevelMods, keys: ModifierKeys) -> [Option<ModifierStep>; 2] {
        let shift = self.step(
            keys.shift,
            required.shift,
            self.level.shift,
            self.sender_holds(keys.shift) || self.chord.contains(Modifiers::SHIFT),
        );
        let level3 = keys.level3_or_usual();
        let third = self.step(
            level3,
            required.level3,
            self.level.level3,
            self.sender_holds(level3) || self.chord.contains(Modifiers::ALT_GR),
        );
        [shift, third]
    }

    /// What one record's change means for one modifier key.
    ///
    /// `have` is what the record claims, `wanted_elsewhere` what the other records
    /// claim, and `pressed` who actually has the key down — three separate facts,
    /// and every wrong answer here is a silent one.
    fn step(
        &self,
        code: u8,
        want: bool,
        have: bool,
        wanted_elsewhere: bool,
    ) -> Option<ModifierStep> {
        let down = self.key_down(code);
        if want {
            (!have || !down).then_some(ModifierStep {
                keycode: code,
                release: false,
                physical: !down,
            })
        } else {
            have.then_some(ModifierStep {
                keycode: code,
                release: true,
                physical: self.pressed.contains(&code) && !wanted_elsewhere,
            })
        }
    }

    /// Whether this modifier key is believed to be down at all.
    fn key_down(&self, code: u8) -> bool {
        self.pressed.contains(&code) || self.sender_holds(code)
    }

    /// Whether the sender is holding this key itself.
    ///
    /// A forwarded bare modifier press is injected as an ordinary key, so it is
    /// invisible to `chord` and `level` — and it is the user's own Shift, which
    /// neither of them may release.
    fn sender_holds(&self, code: u8) -> bool {
        self.held.contains(&Held::Key(code))
    }

    /// A bare modifier key the sender forwarded, which is theirs from here on.
    ///
    /// The sender's own key is the authority on it: their key-up takes it up
    /// whoever pressed it, and their key-down needs nothing sent if this injector is
    /// already holding it for a chord. Either way this injector stops being the one
    /// responsible for releasing it, so what the two records still claim about that
    /// key becomes a borrowed claim like any other.
    fn forward_modifier(&mut self, code: u8, release: bool) -> Result<()> {
        if !release && self.key_down(code) {
            self.pressed.retain(|c| *c != code);
            self.track(Held::Key(code), false);
            return Ok(());
        }
        self.send_key(code, release)?;
        self.pressed.retain(|c| *c != code);
        Ok(())
    }

    fn send_key(&mut self, keycode: u8, release: bool) -> Result<()> {
        self.sink.send(Request::Key {
            keycode,
            press: !release,
        })?;
        self.track(Held::Key(keycode), release);
        Ok(())
    }

    fn track(&mut self, item: Held, release: bool) {
        if release {
            self.held.retain(|h| *h != item);
        } else if !self.held.contains(&item) {
            self.held.push(item);
        }
    }
}

/// The largest number of wheel clicks one scroll event may become.
///
/// A liveness bound rather than a policy: injection runs inline on the agent's
/// engine loop, so a peer sending a delta of ten million must not be able to hold
/// that thread while a hundred thousand round trips go out.
const MAX_SCROLL_CLICKS: u32 = 64;

/// A character's place on the loaded layout, with the desktop's own state already
/// worked around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resolved {
    keycode: u8,
    mods: LevelMods,
}

/// The X11 button number for a wire mouse button.
///
/// Not the evdev code: X11 numbers its buttons from one and spends four of them on
/// the wheel, so the two are unrelated and sharing a table would click something
/// else. Back and Forward are 8 and 9, which is what every browser watches for.
fn x11_button(button: MouseButton) -> Option<u8> {
    Some(match button {
        MouseButton::Left => BUTTON_LEFT,
        MouseButton::Middle => BUTTON_MIDDLE,
        MouseButton::Right => BUTTON_RIGHT,
        MouseButton::Back => BUTTON_BACK,
        MouseButton::Forward => BUTTON_FORWARD,
        // X11 numbers the rest with no fixed meaning, which is what `Extra` is.
        // Past 255 there is no button number to send, so it is refused rather than
        // wrapped onto something that would click.
        MouseButton::Extra(n) => FIRST_EXTRA_BUTTON.checked_add(n)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wx_proto::{MonitorId, Rect, SpecialKey};

    use super::super::keymap::{RawKeymap, MODIFIER_COUNT};

    /// A sink that records what would have reached the server.
    ///
    /// Used instead of a real connection throughout: these tests press keys and
    /// mouse buttons, and a real XTEST call would type into whatever window the
    /// developer happens to have focused.
    struct Recording {
        sent: Mutex<Vec<Request>>,
        keymap: Arc<Keymap>,
        caps: bool,
        /// Set to make every send fail, which is what a server that has gone away
        /// does mid-chord.
        refusing: Mutex<bool>,
    }

    impl Recording {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                keymap: Arc::new(us_layout()),
                caps: false,
                refusing: Mutex::new(false),
            })
        }

        fn with_caps_lock() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                keymap: Arc::new(us_layout()),
                caps: true,
                refusing: Mutex::new(false),
            })
        }

        fn refuse(&self, refusing: bool) {
            *self.refusing.lock().unwrap() = refusing;
        }

        fn take(&self) -> Vec<Request> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }

        fn saw(&self, request: Request) -> bool {
            self.sent.lock().unwrap().contains(&request)
        }
    }

    impl Sink for Recording {
        fn send(&self, request: Request) -> Result<()> {
            if *self.refusing.lock().unwrap() {
                return Err(PlatformError::Os {
                    context: "xtest_fake_input",
                    code: 32,
                });
            }
            self.sent.lock().unwrap().push(request);
            Ok(())
        }

        fn keymap(&self) -> Result<Arc<Keymap>> {
            Ok(Arc::clone(&self.keymap))
        }

        fn caps_locked(&self) -> Result<bool> {
            Ok(self.caps)
        }
    }

    /// Keycodes as a live X server reports them for a US layout, cut down to the
    /// keys these tests press.
    fn us_layout() -> Keymap {
        let rows: &[(u8, &[u32])] = &[
            (37, &[XK_CONTROL_L, 0, XK_CONTROL_L, 0]),
            (38, &[b'a' as u32, b'A' as u32, 0, 0]),
            (39, &[b's' as u32, b'S' as u32, 0, 0]),
            (50, &[XK_SHIFT_L, 0, XK_SHIFT_L, 0]),
            (54, &[b'c' as u32, b'C' as u32, 0, 0]),
            (64, &[XK_ALT_L, 0, XK_ALT_L, 0]),
            (66, &[0xffe5, 0, 0xffe5, 0]),
            (92, &[0xfe03, 0, 0xfe03, 0]),
            (94, &[0x3c, 0x3e, 0x3c, 0x3e, 0x7c, 0xa6, 0x7c]),
            (133, &[XK_SUPER_L, 0, XK_SUPER_L, 0]),
        ];
        let min = rows[0].0;
        let max = rows[rows.len() - 1].0;
        let per = 7usize;
        let mut keysyms = vec![0u32; usize::from(max - min + 1) * per];
        for (keycode, row) in rows {
            let base = usize::from(keycode - min) * per;
            keysyms[base..base + row.len()].copy_from_slice(row);
        }
        let mut modifiers: [Vec<u8>; MODIFIER_COUNT] = Default::default();
        modifiers[0] = vec![50];
        modifiers[1] = vec![66];
        modifiers[2] = vec![37];
        modifiers[3] = vec![64];
        modifiers[6] = vec![133];
        modifiers[7] = vec![92];
        Keymap::from_raw(&RawKeymap {
            min_keycode: min,
            keysyms_per_keycode: per as u8,
            keysyms,
            modifiers,
        })
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

    fn injector(sink: &Arc<Recording>) -> Injector {
        Injector::new(Arc::clone(sink) as Arc<dyn Sink>)
    }

    fn offline() -> Injector {
        Injector::new(Arc::new(NoServer::new("no X server for the test")))
    }

    #[test]
    fn every_event_is_refused_while_there_is_no_server() {
        // The whole point of returning a real error: a peer driving a machine whose
        // connection never came up must be told, not quietly ignored.
        let mut inj = offline();
        for event in [
            InputEvent::Pointer(PointerEvent::MoveTo {
                pos: NormPos::new(0.5, 0.5),
            }),
            InputEvent::Pointer(PointerEvent::MoveBy { dx: 3.0, dy: 4.0 }),
            InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
            InputEvent::Pointer(PointerEvent::Scroll {
                dx: 0.0,
                dy: 1.0,
                unit: ScrollUnit::Lines,
            }),
            InputEvent::Key(KeyEvent::text("a", KeyAction::Press, Modifiers::NONE)),
            InputEvent::Key(KeyEvent::special(
                SpecialKey::Enter,
                KeyAction::Press,
                Modifiers::NONE,
            )),
        ] {
            assert!(
                inj.inject(&monitor(), &event).is_err(),
                "{event:?} reported success with no server"
            );
        }
        assert!(inj.warp_cursor(&monitor(), NormPos::new(0.5, 0.5)).is_err());
    }

    #[test]
    fn the_refusal_names_the_real_reason_rather_than_a_generic_one() {
        // "no display named in DISPLAY" and "the connection was closed" send whoever
        // reads the log to different places.
        let mut inj = Injector::new(Arc::new(NoServer::new("DISPLAY names no server")));
        let err = inj
            .inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::MoveBy { dx: 1.0, dy: 0.0 }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("DISPLAY names no server"), "{err}");
    }

    #[test]
    fn an_absolute_move_lands_on_the_screen_pixel_the_wire_addressed() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.warp_cursor(&monitor(), NormPos::new(0.5, 0.5)).unwrap();
        assert_eq!(
            sink.take(),
            vec![Request::Motion {
                x: 960,
                y: 540,
                relative: false
            }]
        );
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
    fn non_finite_deltas_do_not_poison_the_accumulator() {
        let mut acc = RelativeAccumulator::default();
        assert_eq!(acc.take(f64::NAN, f64::INFINITY), (0, 0));
        assert_eq!(acc.take(3.0, 4.0), (3, 4));
    }

    #[test]
    fn a_relative_move_that_rounds_to_nothing_never_reaches_the_server() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Pointer(PointerEvent::MoveBy { dx: 0.2, dy: 0.2 }),
        )
        .unwrap();
        assert!(sink.take().is_empty());
    }

    #[test]
    fn a_notch_of_scrolling_is_a_click_of_the_wheel_button() {
        // X11 has no scroll axis, and an application counts the press. A missing
        // release would strand button 4 down and turn every later click into a drag.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Pointer(PointerEvent::Scroll {
                dx: 0.0,
                dy: 1.0,
                unit: ScrollUnit::Lines,
            }),
        )
        .unwrap();
        assert_eq!(
            sink.take(),
            vec![
                Request::Button {
                    button: BUTTON_SCROLL_UP,
                    press: true
                },
                Request::Button {
                    button: BUTTON_SCROLL_UP,
                    press: false
                },
            ]
        );
    }

    #[test]
    fn scrolling_towards_the_user_clicks_the_other_wheel_button() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Pointer(PointerEvent::Scroll {
                dx: -1.0,
                dy: -2.0,
                unit: ScrollUnit::Lines,
            }),
        )
        .unwrap();
        let sent = sink.take();
        assert_eq!(
            sent.iter()
                .filter(|r| **r
                    == Request::Button {
                        button: BUTTON_SCROLL_DOWN,
                        press: true
                    })
                .count(),
            2
        );
        assert!(sent.contains(&Request::Button {
            button: BUTTON_SCROLL_LEFT,
            press: true
        }));
    }

    #[test]
    fn a_trackpad_flick_smaller_than_a_detent_is_banked_rather_than_lost() {
        // The pixel unit has to be converted into whole clicks, so slow scrolling
        // would otherwise do nothing at all.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        for _ in 0..4 {
            inj.inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Scroll {
                    dx: 0.0,
                    dy: 3.0,
                    unit: ScrollUnit::Pixels,
                }),
            )
            .unwrap();
        }
        // 12 logical pixels is one whole detent with two banked.
        assert_eq!(
            sink.take()
                .iter()
                .filter(|r| matches!(r, Request::Button { press: true, .. }))
                .count(),
            1
        );
    }

    #[test]
    fn an_absurd_scroll_delta_cannot_hold_the_engine_loop() {
        // Injection runs inline on the thread that also forwards input and answers
        // heartbeats, so a hostile peer must not be able to buy a hundred thousand
        // round trips with one datagram.
        //
        // Both directions, and at a magnitude that saturates the accumulator's clamp
        // rather than merely a large one: downward lands on `i32::MIN`, the value
        // with no positive counterpart, and a plain negation there would panic in a
        // debug build and silently drop the whole scroll in a release one.
        for (dy, button) in [
            (1e9, BUTTON_SCROLL_UP),
            (-1e9, BUTTON_SCROLL_DOWN),
            (-3e9, BUTTON_SCROLL_DOWN),
        ] {
            let sink = Recording::new();
            let mut inj = injector(&sink);
            inj.inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Scroll {
                    dx: 0.0,
                    dy,
                    unit: ScrollUnit::Lines,
                }),
            )
            .unwrap();
            let sent = sink.take();
            assert_eq!(sent.len(), MAX_SCROLL_CLICKS as usize * 2, "dy {dy}");
            assert!(
                sent.iter()
                    .all(|r| matches!(r, Request::Button { button: b, .. } if *b == button)),
                "dy {dy} scrolled the wrong way: {sent:?}"
            );
        }
    }

    #[test]
    fn a_scroll_that_rounds_to_nothing_is_not_sent() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        for (dx, dy) in [(0.0, 0.0), (f64::NAN, f64::NAN)] {
            inj.inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Scroll {
                    dx,
                    dy,
                    unit: ScrollUnit::Pixels,
                }),
            )
            .unwrap();
        }
        assert!(sink.take().is_empty());
    }

    #[test]
    fn the_named_mouse_buttons_are_the_ones_x11_applications_watch_for() {
        assert_eq!(x11_button(MouseButton::Left), Some(1));
        assert_eq!(x11_button(MouseButton::Middle), Some(2));
        assert_eq!(x11_button(MouseButton::Right), Some(3));
        // Not the evdev numbering, and not the wheel: 8 and 9 are what a browser
        // reads as Back and Forward.
        assert_eq!(x11_button(MouseButton::Back), Some(8));
        assert_eq!(x11_button(MouseButton::Forward), Some(9));
        assert_eq!(x11_button(MouseButton::Extra(0)), Some(10));
    }

    #[test]
    fn a_button_past_the_end_of_the_range_is_refused_rather_than_wrapped() {
        assert_eq!(x11_button(MouseButton::Extra(245)), Some(255));
        assert_eq!(x11_button(MouseButton::Extra(246)), None);

        let sink = Recording::new();
        let mut inj = injector(&sink);
        let err = inj
            .inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Button {
                    button: MouseButton::Extra(250),
                    pressed: true,
                }),
            )
            .unwrap_err();
        assert!(matches!(err, PlatformError::Unsupported { .. }));
    }

    #[test]
    fn a_letter_is_typed_on_the_key_the_layout_puts_it_on() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("a", KeyAction::Press, Modifiers::NONE)),
        )
        .unwrap();
        assert_eq!(
            sink.take(),
            vec![Request::Key {
                keycode: 38,
                press: true
            }]
        );
    }

    #[test]
    fn a_capital_holds_shift_for_exactly_that_keystroke() {
        // The stuck-Shift defect: held past the key-up it reaches the next event,
        // and a following Home arrives as Shift+Home.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("A", KeyAction::Press, Modifiers::SHIFT)),
        )
        .unwrap();
        assert_eq!(
            sink.take(),
            vec![
                Request::Key {
                    keycode: 50,
                    press: true
                },
                Request::Key {
                    keycode: 38,
                    press: true
                },
            ]
        );

        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("A", KeyAction::Release, Modifiers::SHIFT)),
        )
        .unwrap();
        assert_eq!(
            sink.take(),
            vec![
                Request::Key {
                    keycode: 38,
                    press: false
                },
                Request::Key {
                    keycode: 50,
                    press: false
                },
            ]
        );
        assert!(inj.pressed.is_empty(), "Shift was left down");
    }

    #[test]
    fn a_level_three_character_holds_the_key_the_layout_names() {
        // Not right Alt by position: `lv3:lsgt_switch` puts the level-3 modifier
        // beside left Shift, and pressing right Alt there would open a menu.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("|", KeyAction::Press, Modifiers::NONE)),
        )
        .unwrap();
        let sent = sink.take();
        assert_eq!(
            sent[0],
            Request::Key {
                keycode: 92,
                press: true
            }
        );
        assert_eq!(
            sent[1],
            Request::Key {
                keycode: 94,
                press: true
            }
        );
    }

    #[test]
    fn caps_lock_inverts_a_letter_and_leaves_punctuation_alone() {
        // Caps Lock is the desktop's own locked modifier: this injector did not set
        // it and must not clear it, so the level is worked around it instead.
        let sink = Recording::with_caps_lock();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("a", KeyAction::Press, Modifiers::NONE)),
        )
        .unwrap();
        assert!(
            sink.saw(Request::Key {
                keycode: 50,
                press: true
            }),
            "`a` with Caps Lock on needs Shift to come out lowercase"
        );

        let sink = Recording::with_caps_lock();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("<", KeyAction::Press, Modifiers::NONE)),
        )
        .unwrap();
        assert!(
            !sink.saw(Request::Key {
                keycode: 50,
                press: true
            }),
            "Caps Lock must not shift a key the layout did not mark alphabetic"
        );
    }

    #[test]
    fn a_chord_presses_the_modifier_and_the_key_the_character_sits_on() {
        // Ctrl+C. `injection_modifiers` keeps the Ctrl, and the character is still
        // resolved through the layout.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("c", KeyAction::Press, Modifiers::CTRL)),
        )
        .unwrap();
        assert_eq!(
            sink.take(),
            vec![
                Request::Key {
                    keycode: 37,
                    press: true
                },
                Request::Key {
                    keycode: 54,
                    press: true
                },
            ]
        );
    }

    #[test]
    fn a_special_key_resolves_through_the_keymap_before_the_hardware_position() {
        // `ctrl:nocaps` is the case: the keysym is what the key means, and pressing
        // the usual position would toggle the user's capitals instead.
        let mut modifiers: [Vec<u8>; MODIFIER_COUNT] = Default::default();
        modifiers[2] = vec![66];
        let remapped = Keymap::from_raw(&RawKeymap {
            min_keycode: 66,
            keysyms_per_keycode: 4,
            keysyms: vec![XK_CONTROL_L, 0, XK_CONTROL_L, 0],
            modifiers,
        });
        let sink = Arc::new(Recording {
            sent: Mutex::new(Vec::new()),
            keymap: Arc::new(remapped),
            caps: false,
            refusing: Mutex::new(false),
        });
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::special(
                SpecialKey::CtrlLeft,
                KeyAction::Press,
                Modifiers::NONE,
            )),
        )
        .unwrap();
        assert_eq!(
            sink.take(),
            vec![Request::Key {
                keycode: 66,
                press: true
            }]
        );
    }

    #[test]
    fn a_key_the_keymap_does_not_carry_falls_back_to_its_hardware_position() {
        // A minimal keymap with no media keys still has the hardware, and pressing
        // it is a better answer than refusing.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::special(
                SpecialKey::F5,
                KeyAction::Press,
                Modifiers::NONE,
            )),
        )
        .unwrap();
        // KEY_F5 is evdev 63, which is keycode 71 on every Linux X server.
        assert_eq!(
            sink.take(),
            vec![Request::Key {
                keycode: 71,
                press: true
            }]
        );
    }

    #[test]
    fn a_character_the_layout_cannot_produce_is_refused_rather_than_mistyped() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        let err = inj
            .inject(
                &monitor(),
                &InputEvent::Key(KeyEvent::text("€", KeyAction::Press, Modifiers::NONE)),
            )
            .unwrap_err();
        assert!(matches!(err, PlatformError::Unsupported { .. }), "{err}");
        assert!(sink.take().is_empty(), "something was pressed anyway");
    }

    #[test]
    fn an_empty_text_payload_is_not_a_keystroke() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        assert!(inj
            .inject(
                &monitor(),
                &InputEvent::Key(KeyEvent::text("", KeyAction::Press, Modifiers::NONE)),
            )
            .is_ok());
        assert!(sink.take().is_empty());
    }

    #[test]
    fn release_all_on_a_fresh_injector_does_nothing_and_succeeds() {
        // Called on every disconnect, including the ones where nothing was ever
        // pressed and no connection was ever made.
        let mut inj = offline();
        assert!(inj.release_all().is_ok());
        assert!(inj.held.is_empty());
    }

    #[test]
    fn release_all_lets_go_of_every_key_and_button_it_pressed() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
        )
        .unwrap();
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("c", KeyAction::Press, Modifiers::CTRL)),
        )
        .unwrap();
        sink.take();

        inj.release_all().unwrap();
        let sent = sink.take();
        assert!(sent.contains(&Request::Button {
            button: BUTTON_LEFT,
            press: false
        }));
        assert!(sent.contains(&Request::Key {
            keycode: 54,
            press: false
        }));
        assert!(sent.contains(&Request::Key {
            keycode: 37,
            press: false
        }));
        // Ctrl last, or the remaining key-ups arrive as bare keys mid-chord.
        let ctrl = sent
            .iter()
            .position(|r| {
                *r == Request::Key {
                    keycode: 37,
                    press: false,
                }
            })
            .unwrap();
        let letter = sent
            .iter()
            .position(|r| {
                *r == Request::Key {
                    keycode: 54,
                    press: false,
                }
            })
            .unwrap();
        assert!(
            letter < ctrl,
            "Ctrl was released before the key it modified"
        );
        assert!(inj.held.is_empty());
        assert!(inj.pressed.is_empty());
        assert_eq!(inj.chord, Modifiers::NONE);
    }

    #[test]
    fn a_refused_release_all_keeps_everything_for_the_next_attempt() {
        // The acceptance criterion this backend is written against: a backend that
        // leaves a modifier stuck is worse than one that does nothing. A refused
        // batch that cleared the record would turn one transient failure into
        // permanently stuck keys.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Key(KeyEvent::text("c", KeyAction::Press, Modifiers::CTRL)),
        )
        .unwrap();
        sink.take();

        sink.refuse(true);
        assert!(inj.release_all().is_err());
        assert_eq!(inj.held.len(), 1, "the held key was dropped on failure");
        assert!(inj.chord.contains(Modifiers::CTRL));

        sink.refuse(false);
        inj.release_all().unwrap();
        let sent = sink.take();
        assert!(sent.contains(&Request::Key {
            keycode: 54,
            press: false
        }));
        assert!(sent.contains(&Request::Key {
            keycode: 37,
            press: false
        }));
        assert!(inj.held.is_empty());
        assert!(inj.pressed.is_empty());
    }

    #[test]
    fn a_release_all_that_failed_halfway_does_not_re_release_what_it_already_sent() {
        // The other half of the same rule. A second key-up for a key that is already
        // up is a report the receiving desktop has no way to make sense of.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        for button in [MouseButton::Left, MouseButton::Right] {
            inj.inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Button {
                    button,
                    pressed: true,
                }),
            )
            .unwrap();
        }
        sink.take();
        assert_eq!(inj.held.len(), 2);

        // The first release goes out, then the connection dies.
        struct Flaky {
            sent: Mutex<Vec<Request>>,
            budget: Mutex<usize>,
            keymap: Arc<Keymap>,
        }
        impl Sink for Flaky {
            fn send(&self, request: Request) -> Result<()> {
                let mut budget = self.budget.lock().unwrap();
                if *budget == 0 {
                    return Err(PlatformError::Os {
                        context: "xtest_fake_input",
                        code: 32,
                    });
                }
                *budget -= 1;
                self.sent.lock().unwrap().push(request);
                Ok(())
            }
            fn keymap(&self) -> Result<Arc<Keymap>> {
                Ok(Arc::clone(&self.keymap))
            }
            fn caps_locked(&self) -> Result<bool> {
                Ok(false)
            }
        }
        let flaky = Arc::new(Flaky {
            sent: Mutex::new(Vec::new()),
            budget: Mutex::new(1),
            keymap: Arc::new(us_layout()),
        });
        inj.sink = Arc::clone(&flaky) as Arc<dyn Sink>;

        assert!(inj.release_all().is_err());
        assert_eq!(
            inj.held.len(),
            1,
            "the release that did go out was not forgotten"
        );
        assert_eq!(
            flaky.sent.lock().unwrap().as_slice(),
            &[Request::Button {
                button: BUTTON_RIGHT,
                press: false
            }],
            "the newest press is released first"
        );
    }

    #[test]
    fn release_all_still_lets_go_when_the_keymap_can_no_longer_be_read() {
        // A server that has stopped answering questions is exactly when something
        // is still held down. Declining to release because the layout is unreadable
        // would leave Ctrl down with nothing left to clear it.
        struct SendOnly {
            sent: Mutex<Vec<Request>>,
        }
        impl Sink for SendOnly {
            fn send(&self, request: Request) -> Result<()> {
                self.sent.lock().unwrap().push(request);
                Ok(())
            }
            fn keymap(&self) -> Result<Arc<Keymap>> {
                Err(PlatformError::Other("the connection was closed".into()))
            }
            fn caps_locked(&self) -> Result<bool> {
                Ok(false)
            }
        }
        let sink = Arc::new(SendOnly {
            sent: Mutex::new(Vec::new()),
        });
        let mut inj = Injector::new(Arc::clone(&sink) as Arc<dyn Sink>);
        inj.pressed.push(FALLBACK_CTRL);
        inj.chord = Modifiers::CTRL;

        inj.release_all().unwrap();
        assert_eq!(
            sink.sent.lock().unwrap().as_slice(),
            &[Request::Key {
                keycode: FALLBACK_CTRL,
                press: false
            }]
        );
        assert!(inj.pressed.is_empty());
    }

    #[test]
    fn a_modifier_the_layout_has_since_moved_is_still_released() {
        // The keymap changed under a held key, so the keycode resolves differently
        // from the one that was pressed. Releasing only what resolves now would
        // leave the original down forever.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.pressed.push(200);
        inj.chord = Modifiers::CTRL;
        inj.release_all().unwrap();
        assert!(sink.saw(Request::Key {
            keycode: 200,
            press: false
        }));
        assert!(inj.pressed.is_empty());
    }

    #[test]
    fn nothing_is_recorded_as_held_when_the_press_could_not_be_sent() {
        // A press that never reached the server must not produce a release later,
        // or the desktop sees a key-up it never saw a key-down for.
        let sink = Recording::new();
        sink.refuse(true);
        let mut inj = injector(&sink);
        assert!(inj
            .inject(
                &monitor(),
                &InputEvent::Pointer(PointerEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                }),
            )
            .is_err());
        assert!(inj.held.is_empty());
    }

    #[test]
    fn release_control_clears_the_record() {
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.inject(
            &monitor(),
            &InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
        )
        .unwrap();
        inj.inject(&monitor(), &InputEvent::ReleaseControl).unwrap();
        assert!(inj.held.is_empty());
    }

    #[test]
    fn release_all_forgets_accumulated_sub_pixel_motion_and_scrolling() {
        // A fraction banked before a handoff would otherwise jump the cursor when
        // control returns.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.relative.take(0.9, 0.9);
        inj.scroll.take(0.9, 0.9);
        inj.inject(
            &monitor(),
            &InputEvent::Pointer(PointerEvent::Button {
                button: MouseButton::Left,
                pressed: true,
            }),
        )
        .unwrap();
        inj.release_all().unwrap();
        assert_eq!(inj.relative.take(0.2, 0.2), (0, 0));
        assert_eq!(inj.scroll.take(0.2, 0.2), (0, 0));
    }

    #[test]
    fn the_held_list_does_not_grow_on_auto_repeat() {
        // A held key repeating would otherwise queue one release per repeat.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.track(Held::Key(38), false);
        inj.track(Held::Key(38), false);
        assert_eq!(inj.held.len(), 1);
        inj.track(Held::Key(38), true);
        assert!(inj.held.is_empty());
    }

    /// The modifier keycodes a US layout resolves to, for the planning tests.
    fn keys() -> ModifierKeys {
        ModifierKeys::resolve(&us_layout())
    }

    #[test]
    fn the_modifier_keys_come_from_the_layout_rather_than_from_fixed_positions() {
        let k = keys();
        assert_eq!(k.ctrl, 37);
        assert_eq!(k.alt, 64);
        assert_eq!(k.super_, 133);
        assert_eq!(k.shift, 50);
        assert_eq!(k.level3, Some(92));
    }

    #[test]
    fn a_layout_with_no_level_three_key_still_has_one_to_release() {
        // A press must refuse rather than guess — pressing right Alt on the chance
        // it works opens a menu — but a release has to name a key whatever the
        // layout says, or a level-3 modifier pressed before the layout changed
        // stays down.
        let empty = Keymap::default();
        let k = ModifierKeys::resolve(&empty);
        assert_eq!(k.level3, None);
        assert_eq!(k.level3_or_usual(), FALLBACK_ALT_GR);
    }

    #[test]
    fn a_special_key_that_needs_shift_actually_holds_it() {
        // Shift+Tab has no text form, so `injection_modifiers` keeps the Shift.
        // Ignoring the bit sends a bare Tab and the reverse-tabbing silently does
        // not happen.
        let inj = offline();
        let event = KeyEvent::special(SpecialKey::Tab, KeyAction::Press, Modifiers::SHIFT);
        assert_eq!(
            inj.chord_plan(event.injection_modifiers(), keys()),
            vec![(
                Modifiers::SHIFT,
                ModifierStep {
                    keycode: 50,
                    release: false,
                    physical: true,
                }
            )]
        );
    }

    #[test]
    fn a_shifted_character_is_not_shifted_twice() {
        // The sender already spent Shift producing the `A`, so
        // `injection_modifiers` strips it and the chord path must ask for nothing.
        // The keymap level presses Shift once, on its own.
        let mut inj = offline();
        let event = KeyEvent::text("A", KeyAction::Press, Modifiers::SHIFT);
        assert!(inj
            .chord_plan(event.injection_modifiers(), keys())
            .is_empty());

        let [shift, third] = inj.level_plan(
            LevelMods {
                shift: true,
                level3: false,
            },
            keys(),
        );
        assert_eq!(
            shift,
            Some(ModifierStep {
                keycode: 50,
                release: false,
                physical: true,
            })
        );
        assert_eq!(third, None);

        inj.level.shift = true;
        inj.pressed.push(50);
        let [shift, _] = inj.level_plan(LevelMods::default(), keys());
        assert_eq!(
            shift,
            Some(ModifierStep {
                keycode: 50,
                release: true,
                physical: true,
            })
        );
    }

    #[test]
    fn a_shift_the_sender_is_holding_is_never_released_by_a_keystroke() {
        // A forwarded `SpecialKey::ShiftLeft` is injected as an ordinary key, so it
        // is on the held list and invisible to both modifier sets. Letting the level
        // path release it after a `!` loses the Shift from the Shift+Home that
        // follows.
        let mut inj = offline();
        inj.held.push(Held::Key(50));
        inj.level.shift = true;

        let [shift, _] = inj.level_plan(LevelMods::default(), keys());
        let step = shift.expect("the level record still has to let go of its Shift");
        assert!(step.release);
        assert!(!step.physical, "the sender's own Shift key must stay down");

        inj.level.shift = false;
        let [shift, _] = inj.level_plan(
            LevelMods {
                shift: true,
                level3: false,
            },
            keys(),
        );
        assert!(!shift.unwrap().physical);
    }

    #[test]
    fn the_chord_and_the_level_do_not_fight_over_the_same_key() {
        // Both drive the Shift key. Whichever stops wanting it while the other still
        // does must forget it without touching the key.
        let mut inj = offline();
        inj.chord = Modifiers::SHIFT;
        inj.level.shift = true;
        inj.pressed.push(50);

        let [shift, _] = inj.level_plan(LevelMods::default(), keys());
        assert!(!shift.unwrap().physical);

        assert_eq!(
            inj.chord_plan(Modifiers::NONE, keys()),
            vec![(
                Modifiers::SHIFT,
                ModifierStep {
                    keycode: 50,
                    release: true,
                    physical: false,
                }
            )]
        );
    }

    #[test]
    fn a_press_that_was_elided_is_not_released_afterwards() {
        // The sender was already holding Ctrl, so `Text("c", CTRL)` records the
        // chord without pressing anything. When the sender lets Ctrl go, the key is
        // up — and a key-up for a key that is already up is a report the receiving
        // desktop has no way to make sense of.
        let mut inj = offline();
        inj.held.push(Held::Key(37));
        assert_eq!(
            inj.chord_plan(Modifiers::CTRL, keys()),
            vec![(
                Modifiers::CTRL,
                ModifierStep {
                    keycode: 37,
                    release: false,
                    physical: false,
                }
            )]
        );
        inj.chord = Modifiers::CTRL;

        inj.held.clear();
        assert_eq!(
            inj.chord_plan(Modifiers::NONE, keys()),
            vec![(
                Modifiers::CTRL,
                ModifierStep {
                    keycode: 37,
                    release: true,
                    physical: false,
                }
            )]
        );
    }

    #[test]
    fn a_chord_whose_modifier_has_gone_up_underneath_it_is_re_asserted() {
        // The record still says CTRL because it borrowed the sender's own key, but
        // that key is up. A `Text("c", CTRL)` arriving now — capture that started
        // mid-chord, so no bare Ctrl press preceded it — must inject Ctrl+c, not a
        // bare `c`.
        let mut inj = offline();
        inj.chord = Modifiers::CTRL;

        let event = KeyEvent::text("c", KeyAction::Press, Modifiers::CTRL);
        assert_eq!(
            inj.chord_plan(event.injection_modifiers(), keys()),
            vec![(
                Modifiers::CTRL,
                ModifierStep {
                    keycode: 37,
                    release: false,
                    physical: true,
                }
            )]
        );

        inj.pressed.push(37);
        assert!(inj
            .chord_plan(event.injection_modifiers(), keys())
            .is_empty());
    }

    #[test]
    fn a_modifier_the_sender_forwards_is_not_pressed_twice() {
        // This injector pressed Shift for a Shift+Tab chord; the sender then
        // forwards its own ShiftLeft. The key is already down, and from here the
        // sender's key-up is what will take it up.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.chord = Modifiers::SHIFT;
        inj.pressed.push(50);

        inj.forward_modifier(50, false).unwrap();
        assert!(inj.pressed.is_empty(), "the sender owns the key now");
        assert!(inj.sender_holds(50));
        assert!(sink.take().is_empty(), "the key was pressed a second time");

        inj.held.clear();
        assert_eq!(
            inj.chord_plan(Modifiers::SHIFT, keys()),
            vec![(
                Modifiers::SHIFT,
                ModifierStep {
                    keycode: 50,
                    release: false,
                    physical: true,
                }
            )]
        );
    }

    #[test]
    fn release_all_lets_a_shared_modifier_key_go_exactly_once() {
        // Three records can name the Shift key at the same time, and the held list
        // is unwound first.
        let sink = Recording::new();
        let mut inj = injector(&sink);
        inj.held.push(Held::Key(50));
        inj.chord = Modifiers::SHIFT | Modifiers::CTRL;
        inj.level.shift = true;
        inj.pressed.push(37);
        assert_eq!(inj.modifier_release_order(), vec![37]);

        inj.held.clear();
        inj.pressed.push(50);
        assert_eq!(inj.modifier_release_order(), vec![50, 37]);
    }
}
