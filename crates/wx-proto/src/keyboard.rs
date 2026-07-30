//! Layout-independent keyboard events.
//!
//! The scheme worth borrowing from Hydra: resolve keystrokes to **text** on the
//! sending machine, using the sender's own keyboard layout, and transmit the
//! resulting characters rather than scancodes or virtual key codes.
//!
//! Sending scancodes is what makes every Synergy-lineage tool demand identical
//! layouts on every machine. A Norwegian keyboard's `å` sits where a US keyboard
//! has `[`; forward the scancode and the receiver types `[`. Forward the
//! character `å` and the receiver's job is merely "produce this codepoint",
//! which every OS can do regardless of installed layout — via a synthesised
//! unicode event on Windows, `CGEventKeyboardSetUnicodeString` on macOS, or the
//! receiver's own keymap on Linux. The Linux backends resolve against that keymap
//! and refuse a character it cannot produce rather than mistyping it, which is a
//! real limit of this scheme and is documented where it bites, in
//! `wx-platform`'s `linux_wayland::keymap` and `linux_x11::keymap`.
//!
//! Keys with no textual meaning (arrows, F-keys, modifiers) can't go through
//! that path, so they travel as the semantic [`SpecialKey`] enum instead.

use serde::{Deserialize, Serialize};

/// What a key event does to key state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyAction {
    Press,
    Release,
    /// OS-generated auto-repeat while held. Kept distinct from `Press` so the
    /// receiver can drop repeats it would rather generate locally.
    Repeat,
}

/// Modifier keys held at the time of the event.
///
/// A bitflag set rather than individual bools: it is one byte on the wire, and
/// modifier state is queried as a set far more often than one flag at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1 << 0);
    pub const CTRL: Self = Self(1 << 1);
    /// Alt on Windows/Linux, Option on macOS.
    pub const ALT: Self = Self(1 << 2);
    /// Windows key, or Command on macOS.
    pub const SUPER: Self = Self(1 << 3);
    /// AltGr, distinct from a plain right Alt: on many European layouts it is a
    /// layout-shift key, and conflating the two breaks `@`, `€`, and `{`.
    pub const ALT_GR: Self = Self(1 << 4);
    pub const CAPS_LOCK: Self = Self(1 << 5);
    pub const NUM_LOCK: Self = Self(1 << 6);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::ops::BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// Keys that produce no text and so cannot be sent as characters.
///
/// Deliberately semantic rather than positional. New variants must only ever be
/// appended: postcard encodes enum variants by index, so inserting one in the
/// middle silently reinterprets every older peer's messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecialKey {
    Escape,
    Backspace,
    Tab,
    Enter,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    // Bare modifier presses. Needed because an application can observe "Ctrl is
    // down" without any other key ever arriving.
    ShiftLeft,
    ShiftRight,
    CtrlLeft,
    CtrlRight,
    AltLeft,
    AltRight,
    SuperLeft,
    SuperRight,
    CapsLock,
    NumLock,
    ScrollLock,
    PrintScreen,
    Pause,
    Menu,
    // Media and system keys, forwarded to whichever machine has the cursor.
    VolumeUp,
    VolumeDown,
    VolumeMute,
    MediaPlayPause,
    MediaNext,
    MediaPrev,
    MediaStop,
    BrightnessUp,
    BrightnessDown,
}

impl SpecialKey {
    /// Whether this key is itself a modifier.
    ///
    /// Used to suppress the redundant text path: a bare Shift press must not be
    /// mistaken for an attempt to type something.
    pub fn is_modifier(self) -> bool {
        matches!(
            self,
            SpecialKey::ShiftLeft
                | SpecialKey::ShiftRight
                | SpecialKey::CtrlLeft
                | SpecialKey::CtrlRight
                | SpecialKey::AltLeft
                | SpecialKey::AltRight
                | SpecialKey::SuperLeft
                | SpecialKey::SuperRight
        )
    }

    /// The modifier bit this key contributes, if any.
    pub fn modifier_bit(self) -> Option<Modifiers> {
        Some(match self {
            SpecialKey::ShiftLeft | SpecialKey::ShiftRight => Modifiers::SHIFT,
            SpecialKey::CtrlLeft | SpecialKey::CtrlRight => Modifiers::CTRL,
            SpecialKey::AltLeft => Modifiers::ALT,
            SpecialKey::AltRight => Modifiers::ALT,
            SpecialKey::SuperLeft | SpecialKey::SuperRight => Modifiers::SUPER,
            SpecialKey::CapsLock => Modifiers::CAPS_LOCK,
            SpecialKey::NumLock => Modifiers::NUM_LOCK,
            _ => return None,
        })
    }
}

/// What a key event carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyPayload {
    /// Text the sender resolved through its own layout.
    ///
    /// Usually one grapheme cluster, but composed sequences (a dead key plus a
    /// base letter) can resolve to several codepoints, and emoji or CJK IME
    /// output can be longer still.
    Text(String),
    /// A key with no textual meaning.
    Special(SpecialKey),
    /// Escape hatch: a raw platform keycode for cases the resolver could not
    /// map. The receiver may well not honour it, but forwarding something beats
    /// dropping the keystroke silently.
    RawKeyCode(u32),
}

/// A single keyboard event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub payload: KeyPayload,
    pub action: KeyAction,
    /// Modifier state as observed by the sender.
    ///
    /// For [`KeyPayload::Text`] the shift/AltGr bits are already baked into the
    /// resolved characters, so the receiver must **not** re-apply them — doing so
    /// turns `A` into `Shift+A` and thence, in many apps, into a hotkey. They are
    /// still transmitted because chords like `Ctrl+C` need them.
    pub modifiers: Modifiers,
}

impl KeyEvent {
    pub fn text(s: impl Into<String>, action: KeyAction, modifiers: Modifiers) -> Self {
        Self {
            payload: KeyPayload::Text(s.into()),
            action,
            modifiers,
        }
    }

    pub fn special(key: SpecialKey, action: KeyAction, modifiers: Modifiers) -> Self {
        Self {
            payload: KeyPayload::Special(key),
            action,
            modifiers,
        }
    }

    /// Modifiers the receiver should physically hold while injecting.
    ///
    /// Shift and AltGr are stripped for text payloads because the sender already
    /// consumed them during resolution; see the note on
    /// [`KeyEvent::modifiers`].
    pub fn injection_modifiers(&self) -> Modifiers {
        match self.payload {
            KeyPayload::Text(_) => self
                .modifiers
                .without(Modifiers::SHIFT)
                .without(Modifiers::ALT_GR),
            _ => self.modifiers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_set_operations() {
        let m = Modifiers::CTRL | Modifiers::SHIFT;
        assert!(m.contains(Modifiers::CTRL));
        assert!(m.contains(Modifiers::SHIFT));
        assert!(!m.contains(Modifiers::ALT));
        assert!(m.contains(Modifiers::CTRL | Modifiers::SHIFT));
        assert!(Modifiers::NONE.is_empty());
        assert_eq!(m.without(Modifiers::SHIFT), Modifiers::CTRL);
    }

    #[test]
    fn every_modifier_bit_is_distinct() {
        let all = [
            Modifiers::SHIFT,
            Modifiers::CTRL,
            Modifiers::ALT,
            Modifiers::SUPER,
            Modifiers::ALT_GR,
            Modifiers::CAPS_LOCK,
            Modifiers::NUM_LOCK,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_eq!(a.0 & b.0, 0, "bits {i} and {j} overlap");
                }
            }
        }
    }

    #[test]
    fn text_events_do_not_reapply_shift() {
        // The sender already used Shift to produce the capital; replaying it
        // would turn a typed 'A' into the Shift+A chord.
        let ev = KeyEvent::text("A", KeyAction::Press, Modifiers::SHIFT);
        assert_eq!(ev.injection_modifiers(), Modifiers::NONE);
    }

    #[test]
    fn text_events_do_not_reapply_altgr() {
        let ev = KeyEvent::text("€", KeyAction::Press, Modifiers::ALT_GR);
        assert_eq!(ev.injection_modifiers(), Modifiers::NONE);
    }

    #[test]
    fn text_events_preserve_chord_modifiers() {
        // Ctrl+C must stay a chord: Ctrl is not consumed by layout resolution.
        let ev = KeyEvent::text("c", KeyAction::Press, Modifiers::CTRL);
        assert_eq!(ev.injection_modifiers(), Modifiers::CTRL);
    }

    #[test]
    fn shifted_chords_keep_ctrl_but_drop_shift() {
        let ev = KeyEvent::text("C", KeyAction::Press, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(ev.injection_modifiers(), Modifiers::CTRL);
    }

    #[test]
    fn special_events_keep_all_modifiers() {
        // Shift+Tab has no text form, so Shift is genuinely needed here.
        let ev = KeyEvent::special(SpecialKey::Tab, KeyAction::Press, Modifiers::SHIFT);
        assert_eq!(ev.injection_modifiers(), Modifiers::SHIFT);
    }

    #[test]
    fn dead_key_composition_travels_as_one_payload() {
        // `'` then `a` composes on the sender and arrives as a single á.
        let ev = KeyEvent::text("á", KeyAction::Press, Modifiers::NONE);
        assert_eq!(ev.payload, KeyPayload::Text("á".into()));
    }

    #[test]
    fn modifier_keys_report_their_bit() {
        assert_eq!(SpecialKey::CtrlLeft.modifier_bit(), Some(Modifiers::CTRL));
        assert_eq!(
            SpecialKey::ShiftRight.modifier_bit(),
            Some(Modifiers::SHIFT)
        );
        assert_eq!(SpecialKey::F1.modifier_bit(), None);
        assert!(SpecialKey::SuperLeft.is_modifier());
        assert!(!SpecialKey::CapsLock.is_modifier());
    }

    #[test]
    fn key_event_serde_round_trips() {
        for ev in [
            KeyEvent::text("å", KeyAction::Press, Modifiers::NONE),
            KeyEvent::special(SpecialKey::VolumeUp, KeyAction::Release, Modifiers::CTRL),
            KeyEvent {
                payload: KeyPayload::RawKeyCode(0x1234),
                action: KeyAction::Repeat,
                modifiers: Modifiers::ALT,
            },
        ] {
            let bytes = postcard::to_allocvec(&ev).unwrap();
            assert_eq!(postcard::from_bytes::<KeyEvent>(&bytes).unwrap(), ev);
        }
    }

    #[test]
    fn multi_codepoint_text_survives_the_wire() {
        let ev = KeyEvent::text("👍🏽", KeyAction::Press, Modifiers::NONE);
        let bytes = postcard::to_allocvec(&ev).unwrap();
        assert_eq!(postcard::from_bytes::<KeyEvent>(&bytes).unwrap(), ev);
    }
}
