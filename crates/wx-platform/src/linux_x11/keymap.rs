//! Reading the receiving desktop's keyboard layout, so text can be injected into it.
//!
//! # Why this is not the Wayland keymap parser, and not `libxkbcommon` either
//!
//! The skeleton this backend replaced said to reuse
//! [`crate::linux_wayland::keymap`] by way of `xkb_x11_get_keymap_as_string`,
//! "rather than linking `libxkbcommon`". That instruction was self-contradictory
//! and is not followed. No such symbol exists; the API it gestures at is
//! `xkb_x11_keymap_new_from_device` followed by `xkb_keymap_get_as_string`, and
//! both live in **libxkbcommon-x11** — precisely the C library the sentence says
//! not to link. Following it would put a C toolchain on every machine that builds
//! this crate, which is the cost `reis`, the hand-written Wayland keymap parser and
//! `x11rb`'s `default-features = false` were each chosen to avoid.
//!
//! It is also unnecessary, and that is the useful half. X11 hands over the same
//! information in two **core** requests that need no extension and no library:
//!
//! * `GetKeyboardMapping` — every keycode's keysyms, one row per key.
//! * `GetModifierMapping` — which keycodes are bound to Shift and to the level-3
//!   modifier, which is what makes those levels reachable.
//!
//! What is left is to read the rows the right way round, which is this module. It
//! is pure, so it is compiled and tested on every platform rather than only where
//! an X server exists.
//!
//! Keysym *numbering* is shared with the Wayland side and is not duplicated: xkb
//! keysyms are X11 keysyms, so [`crate::linux_wayland::keys`] answers "which keysym
//! is this character" for both backends. Only the lookup direction differs.
//!
//! # How a core keyboard row is laid out
//!
//! The core protocol defines the first four entries of a row as group 1 level 1,
//! group 1 level 2, group 2 level 1, group 2 level 2. XKB — which every current X
//! server runs — writes its extra levels after those, so a full row reads:
//!
//! ```text
//!   index   0     1     2     3     4     5     6     7
//!   group   1     1     2     2     1     1     2     2
//!   level   1     2     1     2     3     4     3     4
//! ```
//!
//! Measured rather than assumed, on a live server: the ISO key on a US layout
//! reports `less greater less greater bar brokenbar bar`, so `bar` — AltGr and that
//! key — sits at index 4 and `brokenbar` — AltGr, Shift and that key — at index 5.
//!
//! **Only group 1 is used.** A keysym that appears solely in a group-2 slot belongs
//! to the user's *other* keyboard layout, and reaching it would mean switching the
//! group out from under them. So indices 2, 3, 6 and 7 are ignored, and a character
//! that lives only there is reported as unavailable rather than typed as whatever
//! group 1 has at that position.
//!
//! The consequence, stated plainly because it is a real limit rather than an
//! oversight: on a machine whose active layout group is not the first, this resolves
//! against the wrong layout. A single-layout desktop — which is what a server nobody
//! sits at has — is unaffected, and every group-2 slot on one duplicates group 1
//! anyway.
//!
//! # What this deliberately does not do
//!
//! Remap a scratch keycode with `ChangeKeyboardMapping` to type a character the
//! layout does not have. X11 is the one backend where that is possible, and it is
//! the piece worth building next — but a remap that is not restored on every exit
//! path leaves the user's keyboard corrupted until they log out, so it is not
//! something to bolt on. Until then this backend refuses the character out loud,
//! exactly as the Wayland injector already does.

use std::collections::HashMap;

use wx_proto::SpecialKey;

use crate::linux_wayland::keys::{char_for_keysym, XK_ISO_LEVEL3_SHIFT};

/// `Mode_switch`, the older spelling of the level-3 modifier. Layouts still use it.
const XK_MODE_SWITCH: u32 = 0xff7e;

/// `Caps_Lock`.
const XK_CAPS_LOCK: u32 = 0xffe5;

/// How many modifier positions the protocol defines: Shift, Lock, Control, and
/// Mod1 through Mod5.
pub const MODIFIER_COUNT: usize = 8;

/// The `Shift` position in a `GetModifierMapping` reply.
const MOD_SHIFT: usize = 0;
/// The `Lock` position.
const MOD_LOCK: usize = 1;
/// The first position a layout may put the level-3 modifier in. `Control` sits
/// between `Lock` and `Mod1` and is never it.
const FIRST_MOD: usize = 3;

/// The modifiers this backend will hold down to reach a shifted level.
///
/// Only two, and for the same reason the Wayland backend gives: ordinary layouts
/// reach every printable level with Shift and the level-3 modifier. A level that
/// wanted Control or Alt would be a VT switch or a compose sequence, never a
/// character, and pressing Control to "reach" it would fire a shortcut instead of
/// typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LevelMods {
    pub shift: bool,
    pub level3: bool,
}

impl LevelMods {
    /// How many keys have to be held, so the cheapest way to type a character can
    /// be preferred when a layout offers several.
    fn cost(self) -> u8 {
        u8::from(self.shift) + u8::from(self.level3)
    }
}

/// How to produce one keysym on this layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    /// An X11 keycode, ready for `xtest_fake_input`.
    pub keycode: u8,
    pub mods: LevelMods,
    /// Whether Caps Lock swaps this key's first two levels.
    ///
    /// Tracked because Caps Lock is a *locked* modifier on the receiving desktop
    /// that this injector did not set and must not clear. With it on, pressing the
    /// `a` key unshifted produces `A`, so the shift has to be inverted to type what
    /// was actually asked for.
    pub alphabetic: bool,
}

/// The two replies this module reads, kept apart from the X11 types that carry
/// them so the interpretation is testable with no server.
#[derive(Debug, Clone, Default)]
pub struct RawKeymap {
    /// The first keycode `keysyms` describes; `GetKeyboardMapping` is asked for the
    /// whole range starting here.
    pub min_keycode: u8,
    /// Row width. Four on a plain layout, more where XKB has extra levels to
    /// spill.
    pub keysyms_per_keycode: u8,
    /// One row per keycode, concatenated. `NoSymbol` is zero.
    pub keysyms: Vec<u32>,
    /// Keycodes bound to each modifier position, in protocol order: Shift, Lock,
    /// Control, Mod1..Mod5. Unbound slots (keycode zero) are stripped.
    pub modifiers: [Vec<u8>; MODIFIER_COUNT],
}

/// The receiving desktop's layout, indexed the way injection needs to ask about it.
#[derive(Debug, Default)]
pub struct Keymap {
    /// Keysym to the cheapest stroke that produces it.
    strokes: HashMap<u32, Stroke>,
    /// The keycode to press for Shift.
    shift: Option<u8>,
    /// The keycode to press to reach level 3, when the layout has one.
    level3: Option<u8>,
    /// Whether the `Lock` modifier on this keyboard is Caps Lock rather than Shift
    /// Lock. Only Caps Lock inverts an alphabetic key's level.
    caps_is_lock: bool,
}

impl Keymap {
    /// Build the reverse map from the two replies.
    pub fn from_raw(raw: &RawKeymap) -> Self {
        let per = raw.keysyms_per_keycode as usize;
        if per == 0 || raw.keysyms.is_empty() {
            return Self::default();
        }

        let mut strokes: HashMap<u32, Stroke> = HashMap::new();
        for (index, row) in raw.keysyms.chunks(per).enumerate() {
            let Some(keycode) = index
                .try_into()
                .ok()
                .and_then(|i: u8| raw.min_keycode.checked_add(i))
            else {
                // Past keycode 255, which the protocol cannot address.
                break;
            };
            let levels = group_one_levels(row);
            let alphabetic = is_alphabetic(&levels);
            for (keysym, mods) in levels {
                let Some(keysym) = keysym else { continue };
                let candidate = Stroke {
                    keycode,
                    mods,
                    alphabetic,
                };
                // Cheapest wins, and a tie keeps the incumbent so the answer does
                // not depend on which of two identical keys the server listed
                // first — a keymap that resolved `a` to a different keycode between
                // two reads would make injection non-reproducible for no reason.
                match strokes.get(&keysym) {
                    Some(existing) if existing.mods.cost() <= mods.cost() => {}
                    _ => {
                        strokes.insert(keysym, candidate);
                    }
                }
            }
        }

        let shift = raw.modifiers[MOD_SHIFT].first().copied();
        let level3 = level3_keycode(raw, per);
        let caps_is_lock = raw.modifiers[MOD_LOCK]
            .iter()
            .any(|code| row_contains(raw, per, *code, XK_CAPS_LOCK));

        Self {
            strokes,
            shift,
            level3,
            caps_is_lock,
        }
    }

    /// How to type `keysym`, if this layout can.
    pub fn stroke(&self, keysym: u32) -> Option<Stroke> {
        self.strokes.get(&keysym).copied()
    }

    /// The first of `candidates` this layout can type.
    ///
    /// A character has more than one legal keysym spelling — see
    /// [`crate::linux_wayland::keys::char_keysyms`] — and a lookup that tried only
    /// one would report a character as untypable on a layout that plainly has it.
    pub fn stroke_for_any(&self, candidates: impl IntoIterator<Item = u32>) -> Option<Stroke> {
        candidates.into_iter().find_map(|k| self.stroke(k))
    }

    /// The keycode a keysym sits on, whatever level it needs.
    ///
    /// For keys that are pressed rather than typed — Enter, F5, a modifier — where
    /// the level is irrelevant because the keysym is the same at every level.
    pub fn keycode(&self, keysym: u32) -> Option<u8> {
        self.strokes.get(&keysym).map(|s| s.keycode)
    }

    pub fn shift_keycode(&self) -> Option<u8> {
        self.shift
    }

    /// The keycode that reaches level 3, or `None` on a layout with no such key.
    ///
    /// `None` is not a detail to paper over: pressing right Alt on the chance it
    /// works turns the keystroke into an Alt chord and opens a menu, so a character
    /// that needs a level this layout cannot reach is refused instead.
    pub fn level3_keycode(&self) -> Option<u8> {
        self.level3
    }

    /// Whether this keyboard's `Lock` modifier is Caps Lock.
    pub fn caps_is_lock(&self) -> bool {
        self.caps_is_lock
    }

    /// Whether the server described any key at all.
    ///
    /// An empty keymap is not fatal — special keys and chords resolve through fixed
    /// keycodes and go on working — but every text payload will be refused, so the
    /// caller says so once rather than per keystroke.
    pub fn is_empty(&self) -> bool {
        self.strokes.is_empty()
    }
}

/// The group-1 entries of one keycode's row, paired with the modifiers that reach
/// them.
///
/// See the module docs for the index layout. The core protocol's own rule for a
/// half-filled pair is applied here: a level whose partner is `NoSymbol` and whose
/// keysym is a cased letter stands for the pair `[lowercase, uppercase]`. Real XKB
/// keymaps spell both out, but the rule is in the protocol and a server is entitled
/// to rely on it — and where it applies, ignoring it would make every capital
/// letter untypable.
fn group_one_levels(row: &[u32]) -> [(Option<u32>, LevelMods); 4] {
    let at = |i: usize| row.get(i).copied().filter(|k| *k != 0);
    let pair = |lower: Option<u32>, upper: Option<u32>| -> (Option<u32>, Option<u32>) {
        match (lower, upper) {
            (Some(k), None) => match cased_pair(k) {
                Some((low, up)) => (Some(low), Some(up)),
                None => (Some(k), None),
            },
            other => other,
        }
    };

    let (l1, l2) = pair(at(0), at(1));
    let (l3, l4) = pair(at(4), at(5));
    [
        (
            l1,
            LevelMods {
                shift: false,
                level3: false,
            },
        ),
        (
            l2,
            LevelMods {
                shift: true,
                level3: false,
            },
        ),
        (
            l3,
            LevelMods {
                shift: false,
                level3: true,
            },
        ),
        (
            l4,
            LevelMods {
                shift: true,
                level3: true,
            },
        ),
    ]
}

/// The lowercase and uppercase keysyms of a cased letter, if it is one.
///
/// Restricted to the characters [`char_for_keysym`] can name, which is exactly the
/// range where a keysym and its codepoint agree — anything else would need a
/// keysym-to-character table this backend deliberately does not carry.
fn cased_pair(keysym: u32) -> Option<(u32, u32)> {
    let c = char_for_keysym(keysym)?;
    let mut lower = c.to_lowercase();
    let mut upper = c.to_uppercase();
    // Multi-character case mappings (`ß` uppercases to `SS`) have no single keysym
    // and are not what this rule is about.
    let (Some(low), None, Some(up), None) =
        (lower.next(), lower.next(), upper.next(), upper.next())
    else {
        return None;
    };
    if low == up {
        return None;
    }
    Some((
        crate::linux_wayland::keys::keysym_for_char(low),
        crate::linux_wayland::keys::keysym_for_char(up),
    ))
}

/// Whether Caps Lock swaps this key's first two levels.
///
/// True exactly when the two are the same letter in the two cases. A digit key is
/// not alphabetic even though it has two levels, so Caps Lock must not invert it —
/// inverting would type `!` where the sender pressed `1`.
fn is_alphabetic(levels: &[(Option<u32>, LevelMods); 4]) -> bool {
    let (Some(lower), Some(upper)) = (levels[0].0, levels[1].0) else {
        return false;
    };
    match cased_pair(lower) {
        Some((low, up)) => low == lower && up == upper,
        None => false,
    }
}

/// The keycode to press to reach level 3.
///
/// Both halves are required. The keysym alone is not enough: a layout can carry
/// `ISO_Level3_Shift` on a key that is bound to no modifier at all, and pressing
/// that key would shift nothing while looking like it had. And a modifier position
/// alone says nothing about which level it selects. So this looks for a keycode
/// that is both bound to one of Mod1..Mod5 and carries the level-3 keysym.
fn level3_keycode(raw: &RawKeymap, per: usize) -> Option<u8> {
    raw.modifiers[FIRST_MOD..]
        .iter()
        .flatten()
        .copied()
        .find(|code| {
            row_contains(raw, per, *code, XK_ISO_LEVEL3_SHIFT)
                || row_contains(raw, per, *code, XK_MODE_SWITCH)
        })
}

/// Whether a keycode's row carries a keysym anywhere in it.
///
/// The whole row rather than group 1, because a modifier is the same key at every
/// level and a layout is free to list it only in a group-2 slot.
fn row_contains(raw: &RawKeymap, per: usize, keycode: u8, keysym: u32) -> bool {
    let Some(index) = keycode.checked_sub(raw.min_keycode).map(usize::from) else {
        return false;
    };
    raw.keysyms
        .chunks(per)
        .nth(index)
        .is_some_and(|row| row.contains(&keysym))
}

/// The X11 keysym for a key with no textual meaning.
///
/// Injection resolves these through the keymap rather than by adding eight to the
/// evdev code, because the keysym is what the key *means* and the position is not:
/// `ctrl:nocaps` moves Control onto the Caps Lock key, and a receiver that pressed
/// the usual position would toggle the sender's capitals instead of holding
/// Control. The offset is still the fallback — see
/// [`super::inject::Injector::special_keycode`] — for the keys a minimal keymap
/// leaves out.
///
/// Written as its own table rather than derived from the evdev one, so that a
/// wrong entry is visible rather than wrong identically in both directions. Every
/// value was checked against a live evdev keymap.
pub fn keysym_from_special(key: SpecialKey) -> u32 {
    match key {
        SpecialKey::Escape => 0xff1b,
        SpecialKey::Backspace => 0xff08,
        SpecialKey::Tab => 0xff09,
        SpecialKey::Enter => 0xff0d,
        SpecialKey::Delete => 0xffff,
        SpecialKey::Insert => 0xff63,
        SpecialKey::Home => 0xff50,
        SpecialKey::End => 0xff57,
        // `Prior` and `Next` are the protocol's names for these two.
        SpecialKey::PageUp => 0xff55,
        SpecialKey::PageDown => 0xff56,
        SpecialKey::Up => 0xff52,
        SpecialKey::Down => 0xff54,
        SpecialKey::Left => 0xff51,
        SpecialKey::Right => 0xff53,
        SpecialKey::F1 => 0xffbe,
        SpecialKey::F2 => 0xffbf,
        SpecialKey::F3 => 0xffc0,
        SpecialKey::F4 => 0xffc1,
        SpecialKey::F5 => 0xffc2,
        SpecialKey::F6 => 0xffc3,
        SpecialKey::F7 => 0xffc4,
        SpecialKey::F8 => 0xffc5,
        SpecialKey::F9 => 0xffc6,
        SpecialKey::F10 => 0xffc7,
        SpecialKey::F11 => 0xffc8,
        SpecialKey::F12 => 0xffc9,
        SpecialKey::ShiftLeft => 0xffe1,
        SpecialKey::ShiftRight => 0xffe2,
        SpecialKey::CtrlLeft => 0xffe3,
        SpecialKey::CtrlRight => 0xffe4,
        SpecialKey::AltLeft => 0xffe9,
        SpecialKey::AltRight => 0xffea,
        SpecialKey::SuperLeft => 0xffeb,
        SpecialKey::SuperRight => 0xffec,
        SpecialKey::CapsLock => XK_CAPS_LOCK,
        SpecialKey::NumLock => 0xff7f,
        SpecialKey::ScrollLock => 0xff14,
        SpecialKey::PrintScreen => 0xff61,
        SpecialKey::Pause => 0xff13,
        SpecialKey::Menu => 0xff67,
        SpecialKey::VolumeUp => 0x1008_ff13,
        SpecialKey::VolumeDown => 0x1008_ff11,
        SpecialKey::VolumeMute => 0x1008_ff12,
        // `XF86AudioPlay`, which is what an evdev keymap puts on `KEY_PLAYPAUSE`;
        // `XF86AudioPause` sits beside it on the shifted level and is a different
        // key as far as applications are concerned.
        SpecialKey::MediaPlayPause => 0x1008_ff14,
        SpecialKey::MediaNext => 0x1008_ff17,
        SpecialKey::MediaPrev => 0x1008_ff16,
        SpecialKey::MediaStop => 0x1008_ff15,
        SpecialKey::BrightnessUp => 0x1008_ff02,
        SpecialKey::BrightnessDown => 0x1008_ff03,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux_wayland::keys::{char_keysyms, evdev_from_special, keysym_for_char};

    const NONE: LevelMods = LevelMods {
        shift: false,
        level3: false,
    };
    const SHIFT: LevelMods = LevelMods {
        shift: true,
        level3: false,
    };
    const LEVEL3: LevelMods = LevelMods {
        shift: false,
        level3: true,
    };
    const SHIFT_LEVEL3: LevelMods = LevelMods {
        shift: true,
        level3: true,
    };

    /// A keymap built from rows written the way `xmodmap -pke` prints them.
    ///
    /// `rows` starts at `min_keycode`; each row is padded out to the widest.
    fn keymap(min_keycode: u8, rows: &[&[u32]], modifiers: [&[u8]; MODIFIER_COUNT]) -> Keymap {
        let per = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut keysyms = Vec::new();
        for row in rows {
            keysyms.extend_from_slice(row);
            keysyms.extend(std::iter::repeat_n(0, per - row.len()));
        }
        Keymap::from_raw(&RawKeymap {
            min_keycode,
            keysyms_per_keycode: per as u8,
            keysyms,
            modifiers: modifiers.map(<[u8]>::to_vec),
        })
    }

    /// The same, from rows given by keycode rather than packed together.
    ///
    /// Real keycodes are sparse — the keys these tests care about are spread from 9
    /// to 133 — and writing the gaps out by hand is how a fixture ends up disagreeing
    /// with the modifier map beside it.
    fn sparse(rows: &[(u8, &[u32])], modifiers: [&[u8]; MODIFIER_COUNT]) -> Keymap {
        let min = rows.iter().map(|(k, _)| *k).min().unwrap_or(8);
        let max = rows.iter().map(|(k, _)| *k).max().unwrap_or(8);
        let per = rows.iter().map(|(_, r)| r.len()).max().unwrap_or(1);
        let mut keysyms = vec![0u32; usize::from(max - min + 1) * per];
        for (keycode, row) in rows {
            let base = usize::from(keycode - min) * per;
            keysyms[base..base + row.len()].copy_from_slice(row);
        }
        Keymap::from_raw(&RawKeymap {
            min_keycode: min,
            keysyms_per_keycode: per as u8,
            keysyms,
            modifiers: modifiers.map(<[u8]>::to_vec),
        })
    }

    /// The modifier rows a stock evdev keymap has, at the keycodes a live server
    /// reports them on.
    fn stock_modifiers() -> [&'static [u8]; MODIFIER_COUNT] {
        [&[50], &[66], &[37], &[64], &[77], &[], &[133], &[92]]
    }

    /// Enough of a real US layout to resolve against, with the keycodes and keysyms
    /// a live `xmodmap -pke` reports.
    fn us_layout() -> Keymap {
        sparse(
            &[
                (9, &[0xff1b, 0, 0xff1b, 0]),                  // Escape
                (10, &[b'1' as u32, 0x21, b'1' as u32, 0x21]), // 1 exclam
                (11, &[b'2' as u32, 0x40, b'2' as u32, 0x40]), // 2 at
                (22, &[0xff08, 0xff08, 0xff08, 0xff08]),       // BackSpace
                (23, &[0xff09, 0xfe20, 0xff09, 0xfe20]),       // Tab
                (24, &[b'q' as u32, b'Q' as u32, 0, 0]),
                (26, &[b'e' as u32, b'E' as u32, 0, 0]),
                (34, &[0x5b, 0x7b, 0x5b, 0x7b]), // bracketleft braceleft
                (36, &[0xff0d, 0, 0xff0d, 0]),   // Return
                (37, &[0xffe3, 0, 0xffe3, 0]),   // Control_L
                (38, &[b'a' as u32, b'A' as u32, 0, 0]),
                (50, &[0xffe1, 0, 0xffe1, 0]), // Shift_L
                (66, &[XK_CAPS_LOCK, 0, XK_CAPS_LOCK, 0]),
                (92, &[XK_ISO_LEVEL3_SHIFT, 0, XK_ISO_LEVEL3_SHIFT, 0]),
            ],
            stock_modifiers(),
        )
    }

    #[test]
    fn an_unshifted_letter_needs_no_modifier() {
        let km = us_layout();
        let a = km
            .stroke(keysym_for_char('a'))
            .expect("`a` is on this layout");
        assert_eq!(a.keycode, 38);
        assert_eq!(a.mods, NONE);
        assert!(a.alphabetic);
    }

    #[test]
    fn a_capital_is_the_same_key_with_shift() {
        // The report's own probe: `A` resolves to keycode 38 with shift.
        let km = us_layout();
        let upper = km
            .stroke(keysym_for_char('A'))
            .expect("`A` is on this layout");
        assert_eq!(upper.keycode, 38);
        assert_eq!(upper.mods, SHIFT);
    }

    #[test]
    fn punctuation_resolves_the_way_the_probe_measured_it() {
        // Pinned against the numbers in the scoping report, which were read off a
        // live X server: `[` and `{` share keycode 34, `@` is Shift and keycode 11.
        let km = us_layout();
        assert_eq!(
            km.stroke(keysym_for_char('[')).map(|s| (s.keycode, s.mods)),
            Some((34, NONE))
        );
        assert_eq!(
            km.stroke(keysym_for_char('{')).map(|s| (s.keycode, s.mods)),
            Some((34, SHIFT))
        );
        assert_eq!(
            km.stroke(keysym_for_char('@')).map(|s| (s.keycode, s.mods)),
            Some((11, SHIFT))
        );
    }

    #[test]
    fn a_character_the_layout_does_not_have_is_not_resolved_to_something_else() {
        // The whole point of refusing: pressing a nearby key would type a character
        // nobody asked for, which is worse than saying it could not be sent.
        let km = us_layout();
        for c in ['€', 'é', 'ß', '中', '🦀'] {
            assert_eq!(
                km.stroke_for_any(char_keysyms(c)),
                None,
                "{c:?} was resolved on a layout that does not have it"
            );
        }
    }

    #[test]
    fn a_level_three_character_is_reached_through_the_fifth_and_sixth_entries() {
        // Measured on a live server: the ISO key reads
        // `less greater less greater bar brokenbar bar`, so `bar` is AltGr and
        // `brokenbar` is AltGr with Shift. Reading indices 2 and 3 instead would
        // resolve both to the unshifted `less`.
        let km = keymap(
            94,
            &[&[0x3c, 0x3e, 0x3c, 0x3e, 0x7c, 0xa6, 0x7c]],
            stock_modifiers(),
        );
        assert_eq!(
            km.stroke(keysym_for_char('|')).map(|s| (s.keycode, s.mods)),
            Some((94, LEVEL3))
        );
        assert_eq!(
            km.stroke(keysym_for_char('¦')).map(|s| (s.keycode, s.mods)),
            Some((94, SHIFT_LEVEL3))
        );
    }

    #[test]
    fn a_keysym_that_lives_only_in_the_second_group_is_not_reachable() {
        // The user's other keyboard layout. Typing it would mean switching their
        // active group, and resolving it against group 1 would press a key that
        // produces something else entirely.
        let km = keymap(
            10,
            &[&[b'q' as u32, b'Q' as u32, 0x00e5, 0x00c5]],
            stock_modifiers(),
        );
        assert_eq!(km.stroke(keysym_for_char('q')).map(|s| s.keycode), Some(10));
        assert_eq!(km.stroke_for_any(char_keysyms('å')), None);
    }

    #[test]
    fn the_cheapest_way_to_type_a_character_is_the_one_chosen() {
        // A layout can offer the same character twice. Holding two modifiers where
        // none are needed is visible: an application watching for a bare keypress
        // sees a chord.
        let km = keymap(
            10,
            &[&[0, 0, 0, 0, b'x' as u32, 0], &[b'x' as u32, 0, 0, 0, 0, 0]],
            stock_modifiers(),
        );
        let x = km.stroke(keysym_for_char('x')).unwrap();
        assert_eq!((x.keycode, x.mods), (11, NONE));
    }

    #[test]
    fn a_tie_keeps_the_first_key_the_server_listed() {
        // Two keycodes, same cost. Picking differently between two reads of the
        // same keyboard would make injection non-reproducible for no reason.
        let km = keymap(
            10,
            &[&[b'x' as u32, 0, 0, 0], &[b'x' as u32, 0, 0, 0]],
            stock_modifiers(),
        );
        assert_eq!(km.stroke(keysym_for_char('x')).unwrap().keycode, 10);
    }

    #[test]
    fn a_lone_letter_stands_for_its_own_pair() {
        // The core protocol's rule for a half-filled row. Ignoring it makes every
        // capital untypable on a server that relies on it.
        let km = keymap(10, &[&[b'a' as u32, 0, 0, 0]], stock_modifiers());
        assert_eq!(
            km.stroke(keysym_for_char('a')).map(|s| (s.keycode, s.mods)),
            Some((10, NONE))
        );
        assert_eq!(
            km.stroke(keysym_for_char('A')).map(|s| (s.keycode, s.mods)),
            Some((10, SHIFT))
        );
    }

    #[test]
    fn a_lone_symbol_is_not_given_an_invented_shifted_form() {
        // `Escape` and `Return` are listed alone too, and neither has a case.
        let km = keymap(9, &[&[0xff1b, 0, 0, 0]], stock_modifiers());
        assert_eq!(km.keycode(0xff1b), Some(9));
        assert_eq!(km.strokes.len(), 1);
    }

    #[test]
    fn only_a_key_whose_two_levels_are_one_letter_is_alphabetic() {
        // Caps Lock must invert `a`/`A` and must not invert `1`/`!` — inverting the
        // digit would type an exclamation mark where the sender pressed one.
        let km = us_layout();
        assert!(km.stroke(keysym_for_char('a')).unwrap().alphabetic);
        assert!(!km.stroke(keysym_for_char('1')).unwrap().alphabetic);
        assert!(!km.stroke(keysym_for_char('[')).unwrap().alphabetic);
    }

    #[test]
    fn the_level_three_key_is_the_one_bound_to_a_modifier() {
        // Both halves matter. A layout can carry `ISO_Level3_Shift` on a key bound
        // to nothing, and pressing that key shifts no level while looking as though
        // it had.
        let bound = keymap(
            92,
            &[&[XK_ISO_LEVEL3_SHIFT, 0, XK_ISO_LEVEL3_SHIFT, 0]],
            [&[50], &[66], &[37], &[64], &[77], &[], &[133], &[92]],
        );
        assert_eq!(bound.level3_keycode(), Some(92));

        let unbound = keymap(
            92,
            &[&[XK_ISO_LEVEL3_SHIFT, 0, XK_ISO_LEVEL3_SHIFT, 0]],
            [&[50], &[66], &[37], &[64], &[77], &[], &[133], &[]],
        );
        assert_eq!(unbound.level3_keycode(), None);
    }

    #[test]
    fn the_older_spelling_of_the_level_three_modifier_is_recognised() {
        // Layouts predating `ISO_Level3_Shift` write `Mode_switch`, and a receiver
        // that did not know it would refuse every AltGr character on them.
        let km = keymap(
            92,
            &[&[XK_MODE_SWITCH, 0, XK_MODE_SWITCH, 0]],
            [&[50], &[66], &[37], &[64], &[77], &[], &[133], &[92]],
        );
        assert_eq!(km.level3_keycode(), Some(92));
    }

    #[test]
    fn control_is_never_mistaken_for_the_level_three_modifier() {
        // `Control` sits between `Lock` and `Mod1` in the reply, so a scan that
        // started at the wrong index would hold Control down to reach a level and
        // fire a shortcut instead of typing.
        let km = keymap(
            37,
            &[&[XK_ISO_LEVEL3_SHIFT, 0, 0, 0]],
            [&[], &[], &[37], &[], &[], &[], &[], &[]],
        );
        assert_eq!(km.level3_keycode(), None);
    }

    #[test]
    fn the_shift_key_comes_from_the_modifier_map_rather_than_a_fixed_position() {
        let km = us_layout();
        assert_eq!(km.shift_keycode(), Some(50));

        let moved = keymap(
            10,
            &[&[0xffe1, 0, 0, 0]],
            [&[10], &[], &[], &[], &[], &[], &[], &[]],
        );
        assert_eq!(moved.shift_keycode(), Some(10));
    }

    #[test]
    fn a_keyboard_whose_lock_is_not_caps_lock_does_not_invert_letters() {
        // `Shift_Lock` on the Lock modifier is a different key with different
        // semantics, and inverting for it would type capitals nobody asked for.
        let caps = us_layout();
        assert!(caps.caps_is_lock());

        let shift_lock = keymap(
            66,
            &[&[0xffe6, 0, 0, 0]],
            [&[50], &[66], &[], &[], &[], &[], &[], &[]],
        );
        assert!(!shift_lock.caps_is_lock());
    }

    #[test]
    fn a_server_that_described_no_keys_is_empty_rather_than_a_panic() {
        // The headless case, and a keyboard the server has not finished setting up.
        assert!(Keymap::from_raw(&RawKeymap::default()).is_empty());
        let km = keymap(8, &[&[0, 0, 0, 0]], stock_modifiers());
        assert!(km.is_empty());
        assert!(!us_layout().is_empty());
    }

    #[test]
    fn a_row_running_past_keycode_255_stops_rather_than_wrapping() {
        // The protocol cannot address a keycode above 255, and wrapping would
        // silently give a high key the identity of a low one.
        let rows: Vec<&[u32]> = std::iter::repeat_n(&[b'a' as u32, 0, 0, 0][..], 40).collect();
        let km = keymap(250, &rows, stock_modifiers());
        assert!(km.strokes.values().all(|s| s.keycode >= 250));
    }

    #[test]
    fn every_special_key_has_a_keysym_and_they_are_all_distinct() {
        // Two keys sharing a keysym would make one of them silently press the
        // other, and the table is only checked against the protocol by eye.
        let mut seen = std::collections::HashMap::new();
        for key in EVERY_SPECIAL {
            let keysym = keysym_from_special(*key);
            assert_ne!(keysym, 0, "{key:?} has no keysym");
            assert_eq!(seen.insert(keysym, *key), None, "{key:?} collides");
        }
    }

    #[test]
    fn a_special_keys_keysym_and_its_evdev_code_name_the_same_key() {
        // The two routes to a keycode have to agree, or the fallback in
        // `Injector::special_keycode` presses a different key from the keymap
        // lookup it stands in for. Checked against the offset every Linux X server
        // uses, on a row set copied from a live `xmodmap -pke`.
        for (key, keycode, keysym) in [
            (SpecialKey::Escape, 9u8, 0xff1bu32),
            (SpecialKey::Backspace, 22, 0xff08),
            (SpecialKey::Tab, 23, 0xff09),
            (SpecialKey::Enter, 36, 0xff0d),
            (SpecialKey::CtrlLeft, 37, 0xffe3),
            (SpecialKey::F1, 67, 0xffbe),
            (SpecialKey::F12, 96, 0xffc9),
            (SpecialKey::MediaPlayPause, 172, 0x1008_ff14),
            (SpecialKey::BrightnessUp, 233, 0x1008_ff02),
        ] {
            assert_eq!(keysym_from_special(key), keysym, "{key:?}");
            assert_eq!(
                evdev_from_special(key).map(|c| c + 8),
                Some(u32::from(keycode)),
                "{key:?} is not at keycode {keycode} by the evdev offset"
            );
        }
    }

    /// Every `SpecialKey`, so a variant added to the protocol is caught by the
    /// exhaustive match rather than silently becoming uninjectable.
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
}
