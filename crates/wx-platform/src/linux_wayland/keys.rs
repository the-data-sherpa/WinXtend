//! The fixed halves of Linux input: evdev codes and X11 keysyms.
//!
//! Everything here is a constant of the platform rather than of the desktop, which
//! is why it is compiled and tested everywhere. `linux/input-event-codes.h` is the
//! same on every Linux machine and libei carries exactly those numbers, so a
//! [`SpecialKey`] or a [`MouseButton`] needs no keymap and no compositor to resolve
//! — F5 is 63 whatever the user has their keyboard set to.
//!
//! Text is the opposite case and lives in [`super::keymap`]: a character has no
//! fixed keycode, only a keysym, and where that keysym sits depends entirely on
//! the receiving desktop's layout.

use wx_proto::{MouseButton, SpecialKey};

/// xkb keycodes are evdev keycodes plus this.
///
/// The offset exists because X11 keycodes had to start at 8. It matters here
/// because a keymap read off the `ei_keyboard` device is expressed in xkb
/// keycodes, while [`ei_keyboard.key`] takes evdev ones — the protocol says so
/// explicitly ("the key codes must match the defines in linux/input-event-codes.h").
/// Getting it wrong shifts every keystroke eight places along the keyboard, which
/// types plausible-looking rubbish rather than failing.
///
/// [`ei_keyboard.key`]: reis::ei::Keyboard::key
pub const XKB_KEYCODE_OFFSET: u32 = 8;

/// `KEY_LEFTSHIFT`, the shift this backend presses to reach level 2.
pub const KEY_LEFTSHIFT: u32 = 42;
/// `KEY_RIGHTALT`. AltGr on every layout that has one, and where
/// `ISO_Level3_Shift` sits unless the keymap says otherwise.
pub const KEY_RIGHTALT: u32 = 100;
/// `KEY_LEFTCTRL`.
pub const KEY_LEFTCTRL: u32 = 29;
/// `KEY_LEFTALT`.
pub const KEY_LEFTALT: u32 = 56;
/// `KEY_LEFTMETA`.
pub const KEY_LEFTMETA: u32 = 125;

/// The keysym for `ISO_Level3_Shift`, looked up in the keymap to find the real
/// AltGr key on layouts that put it somewhere other than right Alt.
pub const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;

/// Keysyms with no legacy name are expressed as this plus the codepoint.
///
/// The X11 convention, and the one `xkb_utf32_to_keysym` falls back to. Keymaps
/// may write either form for a character, so lookups try both.
const UNICODE_KEYSYM_BASE: u32 = 0x0100_0000;

/// Whether X11 gives this codepoint a legacy keysym numerically equal to itself.
///
/// Two blocks do, and they are the two that matter for a Latin-script desktop:
/// Latin-1 (`XK_aring` really is `0xe5`) and the currency block, whose last entry
/// is `XK_EuroSign` at `0x20ac` — the same number as `U+20AC`. Real keymaps use
/// the legacy spelling for both, so a lookup that only ever tried the unicode
/// escape reports `€` as untypable on every European layout that has it.
///
/// The list stops there deliberately, and the restriction is the point rather
/// than laziness: X11's other legacy blocks — Latin-2/3/4, Greek, Cyrillic — pick
/// numbers *unrelated* to Unicode, and several of them overlap the codepoints of
/// scripts they have nothing to do with. Trying a bare value from those ranges
/// would sometimes find a key and type a character from another alphabet, which
/// is worse than the honest refusal in [`super::keymap`]. Supporting one of those
/// layouts means adding its keysym-to-character table, not widening this test.
fn is_direct_keysym(code: u32) -> bool {
    (0x20..=0xff).contains(&code) || (0x20a0..=0x20ac).contains(&code)
}

/// The keysym a character is asked for by, preferring the legacy spelling.
pub fn keysym_for_char(c: char) -> u32 {
    let code = c as u32;
    if is_direct_keysym(code) {
        code
    } else {
        UNICODE_KEYSYM_BASE | code
    }
}

/// Every keysym a character might be listed under, best first.
///
/// A keymap is free to write `0xe5` or `0x010000e5` for the same `å`, and a
/// lookup that only tried one of them would report a character as untypable on a
/// layout that plainly has it.
pub fn char_keysyms(c: char) -> [u32; 2] {
    let code = c as u32;
    [keysym_for_char(c), UNICODE_KEYSYM_BASE | code]
}

/// The character a keysym produces, for the blocks [`is_direct_keysym`] covers
/// and for the unicode escape.
///
/// Used to decide whether a key is alphabetic — whether Caps Lock will flip the
/// level out from under an injected letter.
pub fn char_for_keysym(keysym: u32) -> Option<char> {
    let code = if is_direct_keysym(keysym) {
        keysym
    } else if keysym & UNICODE_KEYSYM_BASE == UNICODE_KEYSYM_BASE {
        keysym & 0x00ff_ffff
    } else {
        return None;
    };
    char::from_u32(code)
}

/// The `XK_dead_*` keysym for a combining accent.
///
/// Dead keys are how a layout types the accented characters it has no single key
/// for: on a Norwegian keyboard `é` is AltGr+ø then `e`. When
/// [`crate::keyres::decompose`] splits a character into a base and a mark, this
/// says which key to press first.
///
/// The block is contiguous from `XK_dead_grave` but not in an order worth
/// arithmetic on, and it covers exactly the accents `keyres` composes — the two
/// tables are inverses of each other and a gap here shows up as a character that
/// composes on the sender and refuses on the receiver.
pub fn dead_keysym(combining: char) -> Option<u32> {
    Some(match combining {
        '\u{0300}' => 0xfe50, // dead_grave
        '\u{0301}' => 0xfe51, // dead_acute
        '\u{0302}' => 0xfe52, // dead_circumflex
        '\u{0303}' => 0xfe53, // dead_tilde
        '\u{0304}' => 0xfe54, // dead_macron
        '\u{0306}' => 0xfe55, // dead_breve
        '\u{0307}' => 0xfe56, // dead_abovedot
        '\u{0308}' => 0xfe57, // dead_diaeresis
        '\u{030a}' => 0xfe58, // dead_abovering
        '\u{030b}' => 0xfe59, // dead_doubleacute
        '\u{030c}' => 0xfe5a, // dead_caron
        '\u{0327}' => 0xfe5b, // dead_cedilla
        '\u{0328}' => 0xfe5c, // dead_ogonek
        _ => return None,
    })
}

/// The combining accent an `XK_dead_*` keysym stands for.
///
/// The inverse of [`dead_keysym`], and the capture side's half of the same
/// contract: a keymap that puts `dead_acute` on a key must be reported upwards as
/// [`crate::keyres::RawKey::dead`] so [`crate::keyres::KeyResolver`] can compose
/// it with the next keystroke, rather than as a character nobody typed.
///
/// Written as its own match rather than by searching [`dead_keysym`] so that the
/// two are checked against each other by test — a table walked in both directions
/// hides a wrong entry, because it is wrong identically each way.
pub fn dead_accent(keysym: u32) -> Option<char> {
    Some(match keysym {
        0xfe50 => '\u{0300}', // dead_grave
        0xfe51 => '\u{0301}', // dead_acute
        0xfe52 => '\u{0302}', // dead_circumflex
        0xfe53 => '\u{0303}', // dead_tilde
        0xfe54 => '\u{0304}', // dead_macron
        0xfe55 => '\u{0306}', // dead_breve
        0xfe56 => '\u{0307}', // dead_abovedot
        0xfe57 => '\u{0308}', // dead_diaeresis
        0xfe58 => '\u{030a}', // dead_abovering
        0xfe59 => '\u{030b}', // dead_doubleacute
        0xfe5a => '\u{030c}', // dead_caron
        0xfe5b => '\u{0327}', // dead_cedilla
        0xfe5c => '\u{0328}', // dead_ogonek
        _ => return None,
    })
}

/// The evdev keycode for a key with no textual meaning.
///
/// `None` for keys Linux has no code for, which is reported as
/// [`crate::PlatformError::Unsupported`] rather than guessed at: pressing an
/// arbitrary nearby key would be worse than saying the key could not be sent.
pub fn evdev_from_special(key: SpecialKey) -> Option<u32> {
    Some(match key {
        SpecialKey::Escape => 1,
        SpecialKey::Backspace => 14,
        SpecialKey::Tab => 15,
        SpecialKey::Enter => 28,
        SpecialKey::Delete => 111,
        SpecialKey::Insert => 110,
        SpecialKey::Home => 102,
        SpecialKey::End => 107,
        SpecialKey::PageUp => 104,
        SpecialKey::PageDown => 109,
        SpecialKey::Up => 103,
        SpecialKey::Down => 108,
        SpecialKey::Left => 105,
        SpecialKey::Right => 106,
        // F1..F10 are contiguous from KEY_F1; F11 and F12 are not, because the
        // original PC keyboard had ten function keys and the extra two were
        // bolted on later.
        SpecialKey::F1 => 59,
        SpecialKey::F2 => 60,
        SpecialKey::F3 => 61,
        SpecialKey::F4 => 62,
        SpecialKey::F5 => 63,
        SpecialKey::F6 => 64,
        SpecialKey::F7 => 65,
        SpecialKey::F8 => 66,
        SpecialKey::F9 => 67,
        SpecialKey::F10 => 68,
        SpecialKey::F11 => 87,
        SpecialKey::F12 => 88,
        SpecialKey::ShiftLeft => KEY_LEFTSHIFT,
        SpecialKey::ShiftRight => 54,
        SpecialKey::CtrlLeft => KEY_LEFTCTRL,
        SpecialKey::CtrlRight => 97,
        SpecialKey::AltLeft => KEY_LEFTALT,
        SpecialKey::AltRight => KEY_RIGHTALT,
        SpecialKey::SuperLeft => KEY_LEFTMETA,
        SpecialKey::SuperRight => 126,
        SpecialKey::CapsLock => 58,
        SpecialKey::NumLock => 69,
        SpecialKey::ScrollLock => 70,
        SpecialKey::PrintScreen => 99,
        SpecialKey::Pause => 119,
        SpecialKey::Menu => 127,
        SpecialKey::VolumeUp => 115,
        SpecialKey::VolumeDown => 114,
        SpecialKey::VolumeMute => 113,
        SpecialKey::MediaPlayPause => 164,
        SpecialKey::MediaNext => 163,
        SpecialKey::MediaPrev => 165,
        SpecialKey::MediaStop => 166,
        SpecialKey::BrightnessUp => 225,
        SpecialKey::BrightnessDown => 224,
    })
}

/// The semantic key an evdev keycode stands for, if it has no textual meaning.
///
/// The inverse of [`evdev_from_special`], and the first question capture asks of
/// every keycode: a [`SpecialKey`] beats whatever the layout would resolve the key
/// to, because a receiver injecting `\t` as a character does not move focus and
/// one injecting `\r` does not press a button. [`crate::keyres::RawKey`] states
/// the same precedence.
///
/// `None` means "ask the layout" — every letter, digit and punctuation key lands
/// here, which is the whole point: those are the keys whose meaning depends on the
/// keyboard the user actually has.
///
/// Written as its own match rather than derived from [`evdev_from_special`] so
/// that a keycode is never silently claimed by two keys; the round trip is a test.
pub fn special_from_evdev(code: u32) -> Option<SpecialKey> {
    Some(match code {
        1 => SpecialKey::Escape,
        14 => SpecialKey::Backspace,
        15 => SpecialKey::Tab,
        28 => SpecialKey::Enter,
        111 => SpecialKey::Delete,
        110 => SpecialKey::Insert,
        102 => SpecialKey::Home,
        107 => SpecialKey::End,
        104 => SpecialKey::PageUp,
        109 => SpecialKey::PageDown,
        103 => SpecialKey::Up,
        108 => SpecialKey::Down,
        105 => SpecialKey::Left,
        106 => SpecialKey::Right,
        59 => SpecialKey::F1,
        60 => SpecialKey::F2,
        61 => SpecialKey::F3,
        62 => SpecialKey::F4,
        63 => SpecialKey::F5,
        64 => SpecialKey::F6,
        65 => SpecialKey::F7,
        66 => SpecialKey::F8,
        67 => SpecialKey::F9,
        68 => SpecialKey::F10,
        87 => SpecialKey::F11,
        88 => SpecialKey::F12,
        KEY_LEFTSHIFT => SpecialKey::ShiftLeft,
        54 => SpecialKey::ShiftRight,
        KEY_LEFTCTRL => SpecialKey::CtrlLeft,
        97 => SpecialKey::CtrlRight,
        KEY_LEFTALT => SpecialKey::AltLeft,
        KEY_RIGHTALT => SpecialKey::AltRight,
        KEY_LEFTMETA => SpecialKey::SuperLeft,
        126 => SpecialKey::SuperRight,
        58 => SpecialKey::CapsLock,
        69 => SpecialKey::NumLock,
        70 => SpecialKey::ScrollLock,
        99 => SpecialKey::PrintScreen,
        119 => SpecialKey::Pause,
        127 => SpecialKey::Menu,
        115 => SpecialKey::VolumeUp,
        114 => SpecialKey::VolumeDown,
        113 => SpecialKey::VolumeMute,
        164 => SpecialKey::MediaPlayPause,
        163 => SpecialKey::MediaNext,
        165 => SpecialKey::MediaPrev,
        166 => SpecialKey::MediaStop,
        225 => SpecialKey::BrightnessUp,
        224 => SpecialKey::BrightnessDown,
        _ => return None,
    })
}

/// The semantic key a *modifier* keysym stands for.
///
/// A layout is free to put a modifier on any keycode it likes — `lv3:lsgt_switch`
/// puts `ISO_Level3_Shift` on the key beside left Shift, `ctrl:nocaps` puts Control
/// where Caps Lock is — and [`special_from_evdev`] only knows the usual positions.
/// Without this the key falls through to the layout, resolves to a keysym no
/// character can be made of, and leaves this backend as a raw keycode: the peer
/// then presses whatever *its* keyboard has at that position, which types a stray
/// character rather than modifying anything. [`crate`] states the rule this keeps:
/// a backend that hands raw keycodes upwards has moved the problem to the wrong
/// side of the wire.
///
/// The mapping is by intent rather than by name. `ISO_Level3_Shift` and
/// `Mode_switch` both become [`SpecialKey::AltRight`], which is where every
/// receiver's AltGr is, and which `wx_core`'s router already pairs with
/// `ALT | ALT_GR` so releasing it clears both. That loses nothing: the sender has
/// already resolved its text through its own layout, so a receiver never needs a
/// level-3 shift to *produce* a character — only to know the modifier is down.
///
/// The locks are deliberately absent. Caps Lock and Num Lock are toggles this
/// backend tracks from the evdev code, and a payload without the matching state
/// change would put the two sides out of step about which level is live.
pub fn special_from_keysym(keysym: u32) -> Option<SpecialKey> {
    Some(match keysym {
        XK_ISO_LEVEL3_SHIFT => SpecialKey::AltRight,
        0xff7e => SpecialKey::AltRight, // Mode_switch, the older spelling of AltGr
        0xffe1 => SpecialKey::ShiftLeft,
        0xffe2 => SpecialKey::ShiftRight,
        0xffe3 => SpecialKey::CtrlLeft,
        0xffe4 => SpecialKey::CtrlRight,
        // Meta is where X11 put the Alt keys on PC hardware, and keymaps still
        // write it there.
        0xffe7 | 0xffe9 => SpecialKey::AltLeft,
        0xffe8 | 0xffea => SpecialKey::AltRight,
        0xffeb => SpecialKey::SuperLeft,
        0xffec => SpecialKey::SuperRight,
        _ => return None,
    })
}

/// `BTN_LEFT`. The mouse button block starts here.
const BTN_LEFT: u32 = 0x110;
/// `BTN_SIDE` and `BTN_EXTRA`: what a mouse's thumb buttons report, and what
/// browsers read as Back and Forward.
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;
/// `BTN_FORWARD`, `BTN_BACK`, `BTN_TASK` — the three codes left in the block,
/// which is where a gaming mouse's extra buttons land.
const BTN_FORWARD: u32 = 0x115;
const BTN_TASK: u32 = 0x117;

/// The evdev button code for a wire mouse button.
///
/// Back and Forward are `BTN_SIDE`/`BTN_EXTRA` rather than the identically-named
/// `BTN_BACK`/`BTN_FORWARD`, because that is what real mice report and therefore
/// what applications watch for — the similarly named codes are almost unused.
pub fn evdev_from_button(button: MouseButton) -> Option<u32> {
    Some(match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
        // Past the end of the block there is no code to send, so this is refused
        // rather than wrapped onto something else that would click.
        MouseButton::Extra(n) => {
            let code = BTN_FORWARD.checked_add(u32::from(n))?;
            if code > BTN_TASK {
                return None;
            }
            code
        }
    })
}

/// The wire mouse button an evdev button code stands for.
///
/// The inverse of [`evdev_from_button`], and deliberately narrow: `BTN_BACK` and
/// `BTN_FORWARD` are *not* mapped to [`MouseButton::Back`]/[`MouseButton::Forward`]
/// here even though their names invite it, because [`evdev_from_button`] sends
/// those two on `BTN_SIDE`/`BTN_EXTRA` and a capture that disagreed would round
/// trip a thumb button onto a different one. They arrive as
/// [`MouseButton::Extra`], which is what they are.
///
/// `None` for a code outside the block — a gaming mouse's macro keys, a digitiser
/// stylus — because there is no wire button to call them and inventing one would
/// have a peer click something.
pub fn button_from_evdev(code: u32) -> Option<MouseButton> {
    Some(match code {
        BTN_LEFT => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        BTN_SIDE => MouseButton::Back,
        BTN_EXTRA => MouseButton::Forward,
        c if (BTN_FORWARD..=BTN_TASK).contains(&c) => MouseButton::Extra((c - BTN_FORWARD) as u8),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin1_characters_are_their_own_keysym() {
        // The one range where the keysym is not the unicode escape. Getting this
        // wrong makes every accented character unfindable in a keymap that has it.
        assert_eq!(keysym_for_char('a'), 0x61);
        assert_eq!(keysym_for_char('å'), 0xe5);
        assert_eq!(keysym_for_char('ø'), 0xf8);
        assert_eq!(keysym_for_char('æ'), 0xe6);
        assert_eq!(keysym_for_char('['), 0x5b);
        assert_eq!(keysym_for_char(' '), 0x20);
    }

    #[test]
    fn characters_beyond_latin1_take_the_unicode_escape() {
        assert_eq!(keysym_for_char('ł'), 0x0100_0142);
        assert_eq!(keysym_for_char('😀'), 0x0101_f600);
    }

    #[test]
    fn the_euro_uses_its_legacy_keysym_rather_than_the_escape() {
        // Measured on a real Norwegian keymap: mutter writes `0x20ac` for the
        // AltGr+E level. Asking only for `0x010020ac` reported `€` as untypable on
        // a layout that has it, which is the exact failure the text-over-the-wire
        // design is supposed to prevent.
        assert_eq!(keysym_for_char('€'), 0x20ac);
        assert_eq!(char_keysyms('€'), [0x20ac, 0x0100_20ac]);
    }

    #[test]
    fn legacy_blocks_that_disagree_with_unicode_are_not_guessed_at() {
        // X11's Latin-2, Greek and Cyrillic keysyms are numbers unrelated to their
        // codepoints, and some of them collide with the codepoints of other
        // scripts. Trying a bare value there would occasionally find a key and type
        // a character from the wrong alphabet — worse than refusing.
        for c in ['ł', 'ą', 'α', 'ф', 'ה'] {
            assert_eq!(
                keysym_for_char(c),
                UNICODE_KEYSYM_BASE | c as u32,
                "{c:?} was given a bare keysym"
            );
        }
    }

    #[test]
    fn a_lookup_tries_both_spellings_of_a_latin1_keysym() {
        // Keymaps write either form for the same character; only trying one would
        // report `å` as untypable on a Norwegian layout that plainly has it.
        assert_eq!(char_keysyms('å'), [0xe5, 0x0100_00e5]);
        // With no legacy keysym there is only one spelling, so both entries agree.
        assert_eq!(char_keysyms('ł'), [0x0100_0142, 0x0100_0142]);
    }

    #[test]
    fn keysyms_round_trip_back_to_their_character() {
        for c in ['a', 'A', 'å', '[', '€', '\u{1f600}'] {
            for keysym in char_keysyms(c) {
                assert_eq!(char_for_keysym(keysym), Some(c), "{c:?} via {keysym:#x}");
            }
        }
    }

    #[test]
    fn function_keysyms_are_not_mistaken_for_characters() {
        // 0xff0d is Return, not a codepoint. Treating the function-key block as
        // text would make the alphabetic test below fire on Enter.
        assert_eq!(char_for_keysym(0xff0d), None);
        assert_eq!(char_for_keysym(0xfe03), None);
    }

    #[test]
    fn a_modifier_keysym_resolves_to_the_modifier_and_not_to_a_position() {
        // The keysym block a layout uses when it moves a modifier off its usual
        // key. Left unmapped, each of these leaves capture as a raw keycode and has
        // the peer press whatever its own keyboard has at that position.
        assert_eq!(
            special_from_keysym(XK_ISO_LEVEL3_SHIFT),
            Some(SpecialKey::AltRight)
        );
        assert_eq!(special_from_keysym(0xff7e), Some(SpecialKey::AltRight));
        assert_eq!(special_from_keysym(0xffe1), Some(SpecialKey::ShiftLeft));
        assert_eq!(special_from_keysym(0xffe4), Some(SpecialKey::CtrlRight));
        assert_eq!(special_from_keysym(0xffe9), Some(SpecialKey::AltLeft));
        assert_eq!(special_from_keysym(0xffeb), Some(SpecialKey::SuperLeft));

        // Characters and the locks are not this function's business: the first are
        // text, and the second are toggles tracked from the evdev code.
        assert_eq!(special_from_keysym(keysym_for_char('a')), None);
        assert_eq!(special_from_keysym(0xffe5), None);
    }

    #[test]
    fn every_special_key_has_a_linux_keycode() {
        // The table is the whole reason `SpecialKey` exists, so a variant added to
        // the protocol without a code here would silently become uninjectable.
        for key in EVERY_SPECIAL {
            assert!(evdev_from_special(*key).is_some(), "{key:?} has no keycode");
        }
    }

    #[test]
    fn distinct_special_keys_get_distinct_keycodes() {
        // Two keys sharing a code would make one of them silently type the other.
        let mut seen = std::collections::HashMap::new();
        for key in [
            SpecialKey::F10,
            SpecialKey::F11,
            SpecialKey::F12,
            SpecialKey::NumLock,
            SpecialKey::ScrollLock,
            SpecialKey::AltLeft,
            SpecialKey::AltRight,
            SpecialKey::ShiftLeft,
            SpecialKey::ShiftRight,
            SpecialKey::SuperLeft,
            SpecialKey::SuperRight,
        ] {
            let code = evdev_from_special(key).unwrap();
            assert_eq!(seen.insert(code, key), None, "{key:?} collides on {code}");
        }
    }

    #[test]
    fn the_named_mouse_buttons_map_onto_what_a_real_mouse_reports() {
        assert_eq!(evdev_from_button(MouseButton::Left), Some(0x110));
        assert_eq!(evdev_from_button(MouseButton::Right), Some(0x111));
        assert_eq!(evdev_from_button(MouseButton::Middle), Some(0x112));
        // Thumb buttons: BTN_SIDE/BTN_EXTRA, not the near-unused
        // BTN_BACK/BTN_FORWARD that share their names.
        assert_eq!(evdev_from_button(MouseButton::Back), Some(BTN_SIDE));
        assert_eq!(evdev_from_button(MouseButton::Forward), Some(BTN_EXTRA));
    }

    /// Every `SpecialKey`, so a variant added to the protocol is caught by the
    /// exhaustive match in `evdev_from_special` and by the round trips below.
    const EVERY_SPECIAL: &[SpecialKey] = &[
        SpecialKey::Escape,
        SpecialKey::Backspace,
        SpecialKey::Tab,
        SpecialKey::Enter,
        SpecialKey::Delete,
        SpecialKey::Insert,
        SpecialKey::Home,
        SpecialKey::End,
        SpecialKey::PageUp,
        SpecialKey::PageDown,
        SpecialKey::Up,
        SpecialKey::Down,
        SpecialKey::Left,
        SpecialKey::Right,
        SpecialKey::F1,
        SpecialKey::F2,
        SpecialKey::F3,
        SpecialKey::F4,
        SpecialKey::F5,
        SpecialKey::F6,
        SpecialKey::F7,
        SpecialKey::F8,
        SpecialKey::F9,
        SpecialKey::F10,
        SpecialKey::F11,
        SpecialKey::F12,
        SpecialKey::ShiftLeft,
        SpecialKey::ShiftRight,
        SpecialKey::CtrlLeft,
        SpecialKey::CtrlRight,
        SpecialKey::AltLeft,
        SpecialKey::AltRight,
        SpecialKey::SuperLeft,
        SpecialKey::SuperRight,
        SpecialKey::CapsLock,
        SpecialKey::NumLock,
        SpecialKey::ScrollLock,
        SpecialKey::PrintScreen,
        SpecialKey::Pause,
        SpecialKey::Menu,
        SpecialKey::VolumeUp,
        SpecialKey::VolumeDown,
        SpecialKey::VolumeMute,
        SpecialKey::MediaPlayPause,
        SpecialKey::MediaNext,
        SpecialKey::MediaPrev,
        SpecialKey::MediaStop,
        SpecialKey::BrightnessUp,
        SpecialKey::BrightnessDown,
    ];

    #[test]
    fn a_captured_keycode_resolves_to_the_key_injection_would_send_it_on() {
        // The two tables are hand-written on purpose — see the note on
        // `special_from_evdev` — so this is what stops them drifting. A gap makes
        // a key that can be injected but not captured, which reads as "F5 does
        // nothing when I press it here but works from the other machine".
        for key in EVERY_SPECIAL {
            let code = evdev_from_special(*key).unwrap();
            assert_eq!(
                special_from_evdev(code),
                Some(*key),
                "{key:?} on evdev {code} came back as something else"
            );
        }
    }

    #[test]
    fn the_keys_a_layout_owns_are_left_to_the_layout() {
        // Letters, digits and punctuation must fall through to the keymap, or the
        // cross-layout guarantee never gets a chance: a Norwegian `å` would be
        // reported as whatever the US layout calls that position.
        for code in [
            30, // KEY_A
            2,  // KEY_1
            26, // KEY_LEFTBRACE — `å` on a Norwegian layout
            57, // KEY_SPACE
            39, // KEY_SEMICOLON — `ø` on a Norwegian layout
        ] {
            assert_eq!(special_from_evdev(code), None, "keycode {code}");
        }
    }

    #[test]
    fn the_dead_key_tables_are_inverses_of_each_other() {
        // A gap here composes on one side and not the other: the sender emits a
        // combining accent the receiver has no key for, or worse, capture reports
        // a standalone `´` where the user meant to start composing `á`.
        for combining in [
            '\u{0300}', '\u{0301}', '\u{0302}', '\u{0303}', '\u{0304}', '\u{0306}', '\u{0307}',
            '\u{0308}', '\u{030a}', '\u{030b}', '\u{030c}', '\u{0327}', '\u{0328}',
        ] {
            let keysym = dead_keysym(combining).expect("a combining mark with no dead keysym");
            assert_eq!(dead_accent(keysym), Some(combining), "{keysym:#x}");
        }
    }

    #[test]
    fn a_keysym_that_is_not_a_dead_key_is_not_treated_as_one() {
        // The block sits just below the modifier keysyms, and `ISO_Level3_Shift`
        // is the neighbour that would otherwise arrive as a stray accent.
        assert_eq!(dead_accent(XK_ISO_LEVEL3_SHIFT), None);
        assert_eq!(dead_accent('a' as u32), None);
        assert_eq!(dead_accent(0xff0d), None); // Return
    }

    #[test]
    fn a_captured_button_code_resolves_to_the_button_injection_sends_it_on() {
        for button in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
            MouseButton::Extra(0),
            MouseButton::Extra(1),
            MouseButton::Extra(2),
        ] {
            let code = evdev_from_button(button).unwrap();
            assert_eq!(button_from_evdev(code), Some(button), "{button:?}");
        }
    }

    #[test]
    fn a_button_outside_the_block_is_reported_as_nothing_rather_than_guessed() {
        // A stylus tip, a joystick trigger, a macro key. Mapping one onto Left
        // would have a peer click where the user only put a pen down.
        assert_eq!(button_from_evdev(0x100), None); // BTN_0
        assert_eq!(button_from_evdev(0x140), None); // BTN_TOOL_PEN
        assert_eq!(button_from_evdev(0), None);
    }

    #[test]
    fn buttons_past_the_end_of_the_block_are_refused_rather_than_wrapped() {
        // Wrapping would click something else entirely, which is worse than a
        // button that reports it cannot be sent.
        assert_eq!(evdev_from_button(MouseButton::Extra(0)), Some(BTN_FORWARD));
        assert_eq!(evdev_from_button(MouseButton::Extra(2)), Some(BTN_TASK));
        assert_eq!(evdev_from_button(MouseButton::Extra(3)), None);
        assert_eq!(evdev_from_button(MouseButton::Extra(255)), None);
    }
}
