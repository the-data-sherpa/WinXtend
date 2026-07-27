//! Reading the receiving desktop's keyboard layout, so text can be injected into it.
//!
//! # Why this exists at all
//!
//! WinXtend's central promise is that keys cross the wire as **text**, and the
//! receiver's only job is "produce this codepoint" (see [`crate::keyres`]). libei
//! makes that hard: `ei_keyboard.key` carries an evdev keycode, not a character,
//! and there is no request for "type this string".
//!
//! Three ways out were considered, and only one survives contact with the alpha
//! target:
//!
//! * **Upload our own xkb keymap.** Not possible. `ei_keyboard.keymap` is an
//!   *event*, sent by the compositor to us; the client has no request to set one.
//!   The protocol says so plainly: "for clients of `ei_handshake.context_type`
//!   sender it is the client's responsibility to send the correct
//!   `ei_keyboard.key` keycodes to generate the expected keysym in the EIS
//!   implementation."
//! * **`ei_text.utf8`**, which is exactly the request this design wants. It exists
//!   in the protocol and in `reis`, but the alpha target ships libei 1.5.0, whose
//!   EIS side does not implement `ei_text` — the interface is not offered, so the
//!   capability is never negotiated. Worth re-testing whenever the target moves:
//!   [`super::inject`] already binds the capability and will use it the day a
//!   compositor offers it.
//! * **`RemoteDesktop.NotifyKeyboardKeysym`**, the portal's own D-Bus route, which
//!   would hand the whole problem to the compositor. Measured on the alpha target:
//!   once `ConnectToEIS` has been called the portal refuses it outright —
//!   *"Session is not allowed to call NotifyKeyboard methods"*. The two transports
//!   are mutually exclusive, and the session is already committed to libei.
//!
//! So the answer is this module: read the keymap the compositor hands us, and find
//! the keycode and modifier level that produces the wanted keysym **on the
//! receiver's layout**. That is still the cross-layout guarantee — a Norwegian
//! machine sending `[` gets `[`, not the `å` that shares its position, because the
//! character is resolved here against the layout that is actually loaded.
//!
//! # The limit, stated honestly
//!
//! A character the receiver's layout cannot produce at any level cannot be
//! injected on Wayland. Nothing in the portal, libei, or the compositor offers a
//! way to remap; that is the same reason `wtype` does not work on GNOME. The
//! injector logs a `warn` naming the codepoint rather than pressing something else,
//! and this is the one acceptance criterion of #6 that the platform, not the
//! implementation, refuses.
//!
//! # Why the parser is written here rather than pulled in
//!
//! `libxkbcommon` would do this properly, but binding to it means a C toolchain
//! and `libxkbcommon-dev` on every machine that builds WinXtend, including the
//! three CI runners — the same cost `reis` was chosen to avoid (see the note at the
//! top of [`super::driver`]). What is needed is a fraction of xkb: which keysym
//! sits at which keycode and level, and which modifiers reach that level. That is a
//! text scan, and it is pure, so it is compiled and tested on every platform rather
//! than only where a compositor exists.

use std::collections::HashMap;

use super::keys::{char_for_keysym, KEY_RIGHTALT, XKB_KEYCODE_OFFSET, XK_ISO_LEVEL3_SHIFT};

/// The modifiers this backend will hold down to reach a shifted level.
///
/// Only two, deliberately. Level 3 and above on ordinary layouts are reached with
/// Shift and `ISO_Level3_Shift` and nothing else; a level that wants Control or
/// Alt is a VT switch or a compose sequence, never a character, and pressing
/// Control to "reach" it would fire a shortcut instead of typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LevelMods {
    pub shift: bool,
    pub level3: bool,
}

impl LevelMods {
    const NONE: Self = Self {
        shift: false,
        level3: false,
    };
    const SHIFT: Self = Self {
        shift: true,
        level3: false,
    };
    const LEVEL3: Self = Self {
        shift: false,
        level3: true,
    };
    const SHIFT_LEVEL3: Self = Self {
        shift: true,
        level3: true,
    };

    /// How many keys have to be held. Used to prefer the cheapest way to type a
    /// character when a layout offers several.
    fn cost(self) -> u8 {
        u8::from(self.shift) + u8::from(self.level3)
    }
}

/// How to produce one keysym on this layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    /// evdev keycode, ready for `ei_keyboard.key`.
    pub keycode: u32,
    pub mods: LevelMods,
    /// Whether Caps Lock swaps this key's first two levels.
    ///
    /// Tracked because Caps Lock is a *locked* modifier on the receiving desktop
    /// that this injector did not set and must not clear. With it on, pressing the
    /// `a` key unshifted produces `A`, so the shift has to be inverted to type
    /// what was asked for.
    pub alphabetic: bool,
}

/// What one key produces, indexed the way *capture* needs to ask about it.
///
/// The mirror image of [`Stroke`]. Injection asks "which key produces this
/// character"; capture is handed a keycode by libei and has to ask "what does this
/// key produce right now", which needs the levels in the other order and the
/// modifiers that reach each of them.
#[derive(Debug, Clone, Default)]
struct KeyLevels {
    /// Keysym per zero-based level, `None` where the level is empty.
    keysyms: Vec<Option<u32>>,
    /// Modifiers that reach each level, `None` where the level needs something
    /// this backend does not track as a level shift (a Control or Alt level is a
    /// VT switch, never a character).
    mods: Vec<Option<LevelMods>>,
    /// Whether Caps Lock swaps this key's first two levels.
    alphabetic: bool,
}

/// One keyboard layout, indexed both ways: by keysym for injection, and by
/// keycode for capture.
#[derive(Debug, Default)]
pub struct Keymap {
    strokes: HashMap<u32, Stroke>,
    keys: HashMap<u32, KeyLevels>,
    level3_keycode: Option<u32>,
}

impl Keymap {
    /// Read the keymap text the compositor sent on `ei_keyboard.keymap`.
    ///
    /// `group` is the layout group in effect, zero-based, as reported by
    /// `ei_keyboard.modifiers`. A user with two input sources gets a keymap holding
    /// both, and only the active one will actually be interpreted by the
    /// compositor, so resolving against the wrong one types the other layout's
    /// characters.
    pub fn parse(text: &str, group: usize) -> Self {
        let types = parse_types(section(text, "xkb_types").unwrap_or(""));
        let codes = parse_keycodes(section(text, "xkb_keycodes").unwrap_or(""));
        let symbols = section(text, "xkb_symbols").unwrap_or("");

        let mut strokes: HashMap<u32, Stroke> = HashMap::new();
        let mut keys: HashMap<u32, KeyLevels> = HashMap::new();
        let mut level3_keycode = None;

        for key in parse_keys(symbols) {
            let Some(xkb_code) = codes.get(key.name.as_str()).copied() else {
                continue;
            };
            // A keymap that starts below the offset is malformed rather than
            // interesting; skipping beats subtracting into a wrapped keycode.
            let Some(keycode) = xkb_code.checked_sub(XKB_KEYCODE_OFFSET) else {
                continue;
            };

            let levels = key.group(group);
            if levels.is_empty() {
                continue;
            }
            if levels.first() == Some(&Some(XK_ISO_LEVEL3_SHIFT)) {
                // Right Alt wins when it is one of the candidates. A keymap can
                // carry a `<LVL3>` keycode that no physical keyboard has — the US
                // layout does — and while the compositor would honour it, the key
                // a user's AltGr actually sits under is the safer thing to press.
                if level3_keycode.is_none() || keycode == KEY_RIGHTALT {
                    level3_keycode = Some(keycode);
                }
            }

            let type_name = key.type_for(group);
            let alphabetic = is_alphabetic(&levels, type_name);

            // The capture index, built in the same pass so the two directions
            // cannot disagree about a key's levels or its type.
            keys.insert(
                keycode,
                KeyLevels {
                    keysyms: levels.clone(),
                    mods: (0..levels.len())
                        .map(|index| level_mods(&types, type_name, index, levels.len()))
                        .collect(),
                    alphabetic,
                },
            );

            for (index, keysym) in levels.iter().enumerate() {
                let Some(keysym) = *keysym else { continue };
                let Some(mods) = level_mods(&types, type_name, index, levels.len()) else {
                    continue;
                };
                let stroke = Stroke {
                    keycode,
                    mods,
                    alphabetic,
                };
                match strokes.get(&keysym) {
                    // The cheapest way to type a character wins: a layout that has
                    // `a` unshifted and also on some AltGr level should use the
                    // plain key, and lower keycodes break ties so the result does
                    // not depend on HashMap iteration order.
                    Some(existing)
                        if (existing.mods.cost(), existing.keycode) <= (mods.cost(), keycode) => {}
                    _ => {
                        strokes.insert(keysym, stroke);
                    }
                }
            }
        }

        Self {
            strokes,
            keys,
            level3_keycode,
        }
    }

    /// How to type `keysym`, or `None` if this layout cannot.
    pub fn stroke(&self, keysym: u32) -> Option<Stroke> {
        self.strokes.get(&keysym).copied()
    }

    /// What pressing `keycode` produces with `held` down, or `None` if this
    /// layout gives the key no meaning.
    ///
    /// The capture direction, and the one that keeps WinXtend's central promise:
    /// the keysym comes from *this* machine's layout, so a Norwegian `å` is
    /// resolved here and crosses the wire as text rather than as the keycode a US
    /// desktop would read as `[`.
    ///
    /// `caps_locked` is applied the way the compositor would: on an alphabetic key
    /// it swaps the first two levels, so `a` with Caps Lock on really does resolve
    /// to `A`. It is deliberately *not* folded into `held` by the caller, because
    /// Caps Lock does nothing at all to a key the layout did not mark alphabetic —
    /// pressing `1` with it on still gives `1`.
    ///
    /// Only Shift and Level 3 are consulted. Ctrl, Alt and Super are masked out on
    /// purpose and [`crate::keyres::RawKey::text`] says why: `Ctrl+C` has to cross
    /// the wire as the character `c` plus a Ctrl bit, because the control character
    /// `0x03` is not something any receiver can inject.
    pub fn keysym(&self, keycode: u32, held: LevelMods, caps_locked: bool) -> Option<u32> {
        let key = self.keys.get(&keycode)?;
        let mut wanted = held;
        if key.alphabetic && caps_locked {
            wanted.shift = !wanted.shift;
        }
        // Exact first, then give up one modifier at a time. A key with two levels
        // pressed with AltGr held has no level-3 entry to find, and the compositor
        // would produce its shifted or base symbol rather than nothing; reporting
        // `None` there would drop a keystroke the user really typed.
        for candidate in [
            wanted,
            LevelMods {
                shift: wanted.shift,
                level3: false,
            },
            LevelMods {
                shift: false,
                level3: wanted.level3,
            },
            LevelMods::NONE,
        ] {
            if let Some(keysym) = key
                .mods
                .iter()
                .position(|m| *m == Some(candidate))
                .and_then(|index| key.keysyms.get(index).copied().flatten())
            {
                return Some(keysym);
            }
        }
        None
    }

    /// Whether `keycode` is the layout's level-3 shift.
    ///
    /// Asked of every captured modifier press, so that right Alt on a Norwegian
    /// layout reports [`wx_proto::Modifiers::ALT_GR`] and right Alt on a layout
    /// that has no level 3 does not.
    pub fn is_level3(&self, keycode: u32) -> bool {
        self.level3_keycode == Some(keycode)
    }

    /// The first keysym in `candidates` this layout can produce.
    pub fn stroke_for_any(&self, candidates: impl IntoIterator<Item = u32>) -> Option<Stroke> {
        candidates.into_iter().find_map(|k| self.stroke(k))
    }

    /// The keycode that acts as AltGr here.
    ///
    /// Read out of the keymap rather than assumed to be right Alt, because a
    /// layout is free to put `ISO_Level3_Shift` elsewhere — and pressing right Alt
    /// on a layout that treats it as a plain Alt turns every level-3 character
    /// into an Alt chord.
    pub fn level3_keycode(&self) -> Option<u32> {
        self.level3_keycode
    }

    /// Whether anything was understood. An empty keymap means the parse found
    /// nothing usable, which the injector reports rather than silently typing
    /// nothing.
    pub fn is_empty(&self) -> bool {
        self.strokes.is_empty()
    }
}

/// Whether Caps Lock will flip this key's first two levels.
///
/// Decided from the shape of the symbols rather than only from the declared type,
/// because keys usually declare no type at all and the layout compiler infers
/// `ALPHABETIC` from exactly this: level 1 a lowercase letter, level 2 its
/// uppercase.
fn is_alphabetic(levels: &[Option<u32>], type_name: Option<&str>) -> bool {
    if let Some(name) = type_name {
        return name.contains("ALPHABETIC");
    }
    let (Some(Some(lower)), Some(Some(upper))) = (levels.first(), levels.get(1)) else {
        return false;
    };
    let (Some(lower), Some(upper)) = (char_for_keysym(*lower), char_for_keysym(*upper)) else {
        return false;
    };
    lower.is_lowercase() && lower.to_uppercase().eq(std::iter::once(upper))
}

/// Modifiers for a zero-based level index on a key.
///
/// The declared type is authoritative when there is one; `xkb_types` states the
/// mapping outright and some keys really do use Control or Alt to change level.
/// Without a declaration the level count settles it, which is the same inference
/// the layout compiler makes.
///
/// `None` means the level is out of reach — either its type wants a modifier this
/// backend will not press, or the key has more levels than the standard four.
fn level_mods(
    types: &HashMap<String, Vec<Option<LevelMods>>>,
    type_name: Option<&str>,
    index: usize,
    level_count: usize,
) -> Option<LevelMods> {
    if index == 0 {
        return Some(LevelMods::NONE);
    }
    if let Some(name) = type_name {
        if let Some(levels) = types.get(name) {
            return levels.get(index).copied().flatten();
        }
    }
    // The conventional ladder, and the only one worth guessing: Shift, then AltGr,
    // then both. Anything past level 4 belongs to a type that had better have
    // declared itself.
    match (index, level_count) {
        (1, _) => Some(LevelMods::SHIFT),
        (2, _) => Some(LevelMods::LEVEL3),
        (3, _) => Some(LevelMods::SHIFT_LEVEL3),
        _ => None,
    }
}

/// The body of a top-level `xkb_<name> "..." { ... }` block.
///
/// Brace-counted rather than matched on the closing line, because `xkb_symbols`
/// contains braces on nearly every line of its own.
fn section<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(name)?;
    let open = text[start..].find('{')? + start;
    let mut depth = 0usize;
    for (offset, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `<AE01> = 10;` and `alias <LatQ> = <AD01>;`, flattened to name → xkb keycode.
///
/// Split on `;` rather than on lines: the statement, not the line, is the unit
/// xkb is written in, and a compositor is free to put several on one line.
fn parse_keycodes(body: &str) -> HashMap<String, u32> {
    let mut codes: HashMap<String, u32> = HashMap::new();
    let mut aliases: Vec<(String, String)> = Vec::new();

    for statement in body.split(';') {
        let Some((left, right)) = statement.split_once('=') else {
            continue;
        };
        // A statement can carry the tail of the enclosing block or an `alias`
        // keyword — `{ <A>`, `alias <ALGR>` — so the name is the last `<...>` on
        // the left rather than the whole of it.
        let left = left.trim();
        let Some(at) = left.rfind('<') else { continue };
        let name = &left[at..];
        if !name.ends_with('>') {
            continue;
        }
        let right = right.trim();
        // An alias names another key rather than a number; both spellings end up
        // in the same table, so `<ALGR>` resolves wherever `<RALT>` does.
        match right.parse::<u32>() {
            Ok(code) => {
                codes.insert(name.to_string(), code);
            }
            Err(_) if right.starts_with('<') && right.ends_with('>') => {
                aliases.push((name.to_string(), right.to_string()));
            }
            Err(_) => {}
        }
    }
    // Applied afterwards so an alias can name a key defined further down.
    for (from, to) in aliases {
        if let Some(code) = codes.get(&to).copied() {
            codes.insert(from, code);
        }
    }
    codes
}

/// `type "FOUR_LEVEL" { map[Shift+LevelThree]= 4; ... }`, flattened to
/// name → level index → modifiers.
///
/// A `map` naming any modifier outside Shift and `LevelThree` leaves the level
/// unreachable rather than approximating it: that is how a `CTRL+ALT` key's
/// second level stops being mistaken for a shifted character.
///
/// A level is routinely mapped more than once, and the entries do not agree. The
/// standard `ALPHABETIC` type is `map[Shift]= 2` followed by `map[Lock]= 2`, and
/// `KEYPAD` pairs Shift with NumLock the same way; whichever of those this
/// backend cannot press must never displace the one it can, in either order, or
/// the level records as reachable with nothing held and every capital types
/// lowercase.
fn parse_types(body: &str) -> HashMap<String, Vec<Option<LevelMods>>> {
    let mut types: HashMap<String, Vec<Option<LevelMods>>> = HashMap::new();
    let mut current: Option<String> = None;

    // Statements, not lines: `type "X" { modifiers= ...; map[Shift]= 2; }` is
    // legal on one line, and a line-based scan silently understands none of it.
    for statement in body.split(';') {
        if let Some(at) = statement.find("type") {
            if let Some(name) = quoted(&statement[at..]) {
                current = Some(name.to_string());
                // Level 1 is the base level on every type there is.
                types
                    .entry(name.to_string())
                    .or_insert_with(|| vec![Some(LevelMods::NONE)]);
            }
        }
        let Some(name) = current.as_deref() else {
            continue;
        };
        let Some(at) = statement.find("map[") else {
            continue;
        };
        let Some((mods, level)) = statement[at + 4..].split_once(']') else {
            continue;
        };
        let Some(level) = parse_level(level) else {
            continue;
        };

        let mut wanted = LevelMods::NONE;
        // Whether this backend can hold everything the entry names. Caps Lock is
        // the desktop's own state rather than a key to press — it is handled by
        // inverting the shift on an alphabetic key — so a level reached only
        // through it is not a level this backend can reach at all.
        let mut pressable = true;
        for token in mods.split('+') {
            match token.trim() {
                "Shift" => wanted.shift = true,
                "LevelThree" => wanted.level3 = true,
                "" => {}
                _ => pressable = false,
            }
        }

        let levels = types.entry(name.to_string()).or_default();
        if levels.len() < level {
            levels.resize(level, None);
        }
        let slot = &mut levels[level - 1];
        let better = match *slot {
            Some(existing) => wanted.cost() < existing.cost(),
            None => true,
        };
        if pressable && better {
            *slot = Some(wanted);
        }
    }
    types
}

/// The level a `map[...]=` entry names.
///
/// Both spellings occur and mean the same thing: mutter serialises `= 2`, and
/// `xkbcomp -xkb` writes `= Level2`. Only understanding the bare number leaves
/// every shifted level on a keymap in the other spelling unreachable — which does
/// not fail, it silently types the base character, so `A` comes out as `a` and
/// `Å` as `å`.
fn parse_level(token: &str) -> Option<usize> {
    let token = token.trim().trim_start_matches('=').trim();
    let digits = token.strip_prefix("Level").unwrap_or(token);
    digits.parse::<usize>().ok().filter(|l| *l >= 1)
}

/// One `key <NAME> { ... }` entry, as far as this module cares.
#[derive(Debug, Default)]
struct KeyEntry {
    name: String,
    /// Symbol lists, one per layout group.
    groups: Vec<Vec<Option<u32>>>,
    /// Declared type per group; index 0 also holds a type declared for the whole
    /// key with no group index.
    types: Vec<Option<String>>,
    /// A `type= "..."` with no group index, which applies to every group.
    shared_type: Option<String>,
}

impl KeyEntry {
    fn group(&self, group: usize) -> Vec<Option<u32>> {
        // Falling back to the first group rather than giving up: a keymap can
        // report a group index for which a particular key defines nothing, and the
        // compositor resolves that key through group 1.
        self.groups
            .get(group)
            .or_else(|| self.groups.first())
            .cloned()
            .unwrap_or_default()
    }

    fn type_for(&self, group: usize) -> Option<&str> {
        self.types
            .get(group)
            .and_then(Option::as_deref)
            .or(self.shared_type.as_deref())
    }
}

/// Pull every `key <NAME> { ... };` out of an `xkb_symbols` body.
fn parse_keys(body: &str) -> Vec<KeyEntry> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;

    while let Some(found) = body[i..].find("key ") {
        let mut cursor = i + found + 4;
        // `key` only starts a definition at the start of a statement; `key.repeat`
        // and the tail of an identifier must not be mistaken for one.
        let preceded_by = body[..i + found].trim_end().chars().last();
        if !matches!(preceded_by, None | Some('{') | Some(';') | Some('}')) {
            i = i + found + 4;
            continue;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let Some(name_end) = body[cursor..].find('>') else {
            break;
        };
        let name = body[cursor..cursor + name_end + 1].to_string();
        if !name.starts_with('<') {
            i = cursor;
            continue;
        }
        cursor += name_end + 1;

        let Some(open) = body[cursor..].find('{') else {
            break;
        };
        let start = cursor + open + 1;
        let Some(end) = matching_brace(body, start) else {
            break;
        };
        out.push(parse_key_body(name, &body[start..end]));
        i = end;
    }
    out
}

fn matching_brace(body: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (offset, c) in body[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// The inside of a `key { ... }`.
///
/// Both spellings occur in the same keymap and have to be read the same way:
/// `symbols[1]= [ a, A ]` with an explicit group index, and a bare `[ a, A ]`
/// whose position *is* its group. Bracketed things that are not symbol lists —
/// `actions[1]= [ ... ]`, `type[1]= "..."` — have to be stepped over rather than
/// counted as groups, or every key with actions gains a phantom layout.
fn parse_key_body(name: String, body: &str) -> KeyEntry {
    let mut entry = KeyEntry {
        name,
        ..Default::default()
    };
    let bytes = body.as_bytes();
    let mut i = 0;
    // The keyword a bracket or string belongs to, and the group index it named.
    let mut keyword: Option<(String, usize)> = None;
    let mut bare_group = 0usize;

    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = body[start..i].to_ascii_lowercase();
            // An index bracket binds tighter than anything else: `symbols[1]`.
            let mut index = 0usize;
            if bytes.get(i) == Some(&b'[') {
                let close = body[i..].find(']').map(|o| i + o);
                if let Some(close) = close {
                    index = body[i + 1..close].trim().parse::<usize>().unwrap_or(1);
                    index = index.saturating_sub(1);
                    i = close + 1;
                }
            }
            keyword = Some((word, index));
            continue;
        }
        match c {
            '[' => {
                let Some(close) = matching_bracket(body, i + 1) else {
                    break;
                };
                let list = &body[i + 1..close];
                i = close + 1;
                match keyword.take() {
                    Some((word, index)) if word == "symbols" => {
                        set_group(&mut entry.groups, index, parse_symbol_list(list));
                        bare_group = index + 1;
                    }
                    // Anything else bracketed is not symbols; skip it whole.
                    Some(_) => {}
                    None => {
                        set_group(&mut entry.groups, bare_group, parse_symbol_list(list));
                        bare_group += 1;
                    }
                }
            }
            '"' => {
                let close = body[i + 1..].find('"').map(|o| i + 1 + o);
                let Some(close) = close else { break };
                let value = &body[i + 1..close];
                i = close + 1;
                if let Some((word, index)) = keyword.take() {
                    if word == "type" {
                        if index == 0 && !body[..i].contains("type[") {
                            entry.shared_type = Some(value.to_string());
                        }
                        set_type(&mut entry.types, index, value.to_string());
                    }
                }
            }
            // `=` and `,` merely separate; the keyword they follow still stands.
            '=' | ',' | ' ' | '\t' | '\n' | '\r' => i += 1,
            _ => {
                keyword = None;
                i += 1;
            }
        }
    }
    entry
}

fn matching_bracket(body: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (offset, c) in body[start..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn set_group(groups: &mut Vec<Vec<Option<u32>>>, index: usize, value: Vec<Option<u32>>) {
    if groups.len() <= index {
        groups.resize(index + 1, Vec::new());
    }
    groups[index] = value;
}

fn set_type(types: &mut Vec<Option<String>>, index: usize, value: String) {
    if types.len() <= index {
        types.resize(index + 1, None);
    }
    types[index] = Some(value);
}

/// One comma-separated symbol list, `None` where the level is empty.
fn parse_symbol_list(list: &str) -> Vec<Option<u32>> {
    list.split(',')
        .map(|token| parse_keysym(token.trim()))
        .collect()
}

/// One keysym token.
///
/// The alpha target's compositor writes plain hex, which is why that form comes
/// first. The named forms are here so a keymap serialised by `libxkbcommon` — what
/// another compositor is likely to send — is not a wall of unresolvable names; the
/// ASCII block covers the printable characters every Latin layout is built from.
fn parse_keysym(token: &str) -> Option<u32> {
    if token.is_empty() || token == "NoSymbol" || token == "VoidSymbol" {
        return None;
    }
    if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16).ok();
    }
    // `U00E5`: the unicode spelling xkb allows in a symbols file.
    if let Some(hex) = token.strip_prefix('U') {
        if hex.len() >= 4 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            if let Ok(code) = u32::from_str_radix(hex, 16) {
                return Some(super::keys::keysym_for_char(char::from_u32(code)?));
            }
        }
    }
    // A one-character name is that ASCII character: `a`, `A`, `7`.
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    if let Ok(decimal) = token.parse::<u32>() {
        return Some(decimal);
    }
    named_keysym(token)
}

/// The X11 name for a keysym, for the Latin-1 block and the one modifier this
/// module has to recognise by name.
///
/// Latin-1 is the whole of what a Latin-script layout is built from, and in that
/// block the keysym *is* the codepoint — so the table is (name, character) and
/// nothing has to be memorised beyond the spellings. It stops there on purpose:
/// Latin-2 and beyond are thousands of names for layouts the alpha target does not
/// reach, and the compositor it does reach writes hex anyway.
fn named_keysym(name: &str) -> Option<u32> {
    // The one non-character name that matters here: without it, AltGr cannot be
    // found in a keymap serialised with names, and every level-3 character
    // becomes untypable.
    if name == "ISO_Level3_Shift" {
        return Some(XK_ISO_LEVEL3_SHIFT);
    }
    // A dead key is not a character and must not be read as one. Without these,
    // every accented character on a layout that composes them is unreachable in
    // both directions: injection cannot find the accent to press, and capture
    // reports the key as producing nothing at all.
    if let Some(keysym) = dead_named_keysym(name) {
        return Some(keysym);
    }
    LATIN1_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c as u32)
}

/// `dead_acute` and its neighbours, by name.
///
/// The same block [`super::keys::dead_keysym`] and [`super::keys::dead_accent`]
/// cover by number, spelled the way a keymap serialised with names writes them.
/// Kept here rather than there because it is a *parsing* concern — what a token in
/// a keymap file means — and the round trip through all three is a test.
fn dead_named_keysym(name: &str) -> Option<u32> {
    let combining = match name {
        "dead_grave" => '\u{0300}',
        "dead_acute" => '\u{0301}',
        "dead_circumflex" => '\u{0302}',
        "dead_tilde" => '\u{0303}',
        "dead_macron" => '\u{0304}',
        "dead_breve" => '\u{0306}',
        "dead_abovedot" => '\u{0307}',
        "dead_diaeresis" => '\u{0308}',
        "dead_abovering" => '\u{030a}',
        "dead_doubleacute" => '\u{030b}',
        "dead_caron" => '\u{030c}',
        "dead_cedilla" => '\u{0327}',
        "dead_ogonek" => '\u{0328}',
        _ => return None,
    };
    super::keys::dead_keysym(combining)
}

/// X11 keysym names for printable Latin-1, in codepoint order.
///
/// `quoteright`/`quoteleft` are the deprecated spellings of `apostrophe` and
/// `grave`; older layout files still use them, and they cost two lines.
const LATIN1_NAMES: &[(&str, char)] = &[
    ("space", ' '),
    ("exclam", '!'),
    ("quotedbl", '"'),
    ("numbersign", '#'),
    ("dollar", '$'),
    ("percent", '%'),
    ("ampersand", '&'),
    ("apostrophe", '\''),
    ("quoteright", '\''),
    ("parenleft", '('),
    ("parenright", ')'),
    ("asterisk", '*'),
    ("plus", '+'),
    ("comma", ','),
    ("minus", '-'),
    ("period", '.'),
    ("slash", '/'),
    ("colon", ':'),
    ("semicolon", ';'),
    ("less", '<'),
    ("equal", '='),
    ("greater", '>'),
    ("question", '?'),
    ("at", '@'),
    ("bracketleft", '['),
    ("backslash", '\\'),
    ("bracketright", ']'),
    ("asciicircum", '^'),
    ("underscore", '_'),
    ("grave", '`'),
    ("quoteleft", '`'),
    ("braceleft", '{'),
    ("bar", '|'),
    ("braceright", '}'),
    ("asciitilde", '~'),
    ("nobreakspace", '\u{a0}'),
    ("exclamdown", '¡'),
    ("cent", '¢'),
    ("sterling", '£'),
    ("currency", '¤'),
    ("yen", '¥'),
    ("brokenbar", '¦'),
    ("section", '§'),
    ("diaeresis", '¨'),
    ("copyright", '©'),
    ("ordfeminine", 'ª'),
    ("guillemotleft", '«'),
    ("notsign", '¬'),
    ("hyphen", '\u{ad}'),
    ("registered", '®'),
    ("macron", '¯'),
    ("degree", '°'),
    ("plusminus", '±'),
    ("twosuperior", '²'),
    ("threesuperior", '³'),
    ("acute", '´'),
    ("mu", 'µ'),
    ("paragraph", '¶'),
    ("periodcentered", '·'),
    ("cedilla", '¸'),
    ("onesuperior", '¹'),
    ("masculine", 'º'),
    ("guillemotright", '»'),
    ("onequarter", '¼'),
    ("onehalf", '½'),
    ("threequarters", '¾'),
    ("questiondown", '¿'),
    ("Agrave", 'À'),
    ("Aacute", 'Á'),
    ("Acircumflex", 'Â'),
    ("Atilde", 'Ã'),
    ("Adiaeresis", 'Ä'),
    ("Aring", 'Å'),
    ("AE", 'Æ'),
    ("Ccedilla", 'Ç'),
    ("Egrave", 'È'),
    ("Eacute", 'É'),
    ("Ecircumflex", 'Ê'),
    ("Ediaeresis", 'Ë'),
    ("Igrave", 'Ì'),
    ("Iacute", 'Í'),
    ("Icircumflex", 'Î'),
    ("Idiaeresis", 'Ï'),
    ("ETH", 'Ð'),
    ("Ntilde", 'Ñ'),
    ("Ograve", 'Ò'),
    ("Oacute", 'Ó'),
    ("Ocircumflex", 'Ô'),
    ("Otilde", 'Õ'),
    ("Odiaeresis", 'Ö'),
    ("multiply", '×'),
    ("Oslash", 'Ø'),
    ("Ooblique", 'Ø'),
    ("Ugrave", 'Ù'),
    ("Uacute", 'Ú'),
    ("Ucircumflex", 'Û'),
    ("Udiaeresis", 'Ü'),
    ("Yacute", 'Ý'),
    ("THORN", 'Þ'),
    ("ssharp", 'ß'),
    ("agrave", 'à'),
    ("aacute", 'á'),
    ("acircumflex", 'â'),
    ("atilde", 'ã'),
    ("adiaeresis", 'ä'),
    ("aring", 'å'),
    ("ae", 'æ'),
    ("ccedilla", 'ç'),
    ("egrave", 'è'),
    ("eacute", 'é'),
    ("ecircumflex", 'ê'),
    ("ediaeresis", 'ë'),
    ("igrave", 'ì'),
    ("iacute", 'í'),
    ("icircumflex", 'î'),
    ("idiaeresis", 'ï'),
    ("eth", 'ð'),
    ("ntilde", 'ñ'),
    ("ograve", 'ò'),
    ("oacute", 'ó'),
    ("ocircumflex", 'ô'),
    ("otilde", 'õ'),
    ("odiaeresis", 'ö'),
    ("division", '÷'),
    ("oslash", 'ø'),
    ("ugrave", 'ù'),
    ("uacute", 'ú'),
    ("ucircumflex", 'û'),
    ("udiaeresis", 'ü'),
    ("yacute", 'ý'),
    ("thorn", 'þ'),
    ("ydiaeresis", 'ÿ'),
];

fn quoted(s: &str) -> Option<&str> {
    let start = s.find('"')? + 1;
    let end = s[start..].find('"')? + start;
    Some(&s[start..end])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::linux_wayland::keys::{char_keysyms, keysym_for_char, KEY_LEFTALT};

    /// The shape mutter actually sends, cut down to the keys under test: numeric
    /// keysyms, tab-indented, `symbols[N]=` for multi-group keys and a bare list
    /// for single-group ones.
    pub(crate) const US: &str = r#"
xkb_keymap {
xkb_keycodes "evdev" {
	minimum = 8;
	maximum = 708;
	<ESC> = 9;
	<AE01> = 10;
	<AD01> = 24;
	<AD11> = 34;
	<AD12> = 35;
	<AC01> = 38;
	<LFSH> = 50;
	<RALT> = 108;
	<FK01> = 67;
	alias <ALGR> = <RALT>;
};
xkb_types "complete" {
	type "ONE_LEVEL" {
		modifiers= none;
		level_name[1]= "Any";
	};
	type "ALPHABETIC" {
		modifiers= Shift+Lock;
		map[Shift]= 2;
		map[Lock]= 2;
	};
	type "FOUR_LEVEL" {
		modifiers= Shift+LevelThree;
		map[Shift]= 2;
		map[LevelThree]= 3;
		map[Shift+LevelThree]= 4;
	};
	type "CTRL+ALT" {
		modifiers= Control+Alt;
		map[Control+Alt]= 2;
	};
};
xkb_compatibility "complete" {
	interpret Any+AnyOf(all) {
		action= SetMods(modifiers=modMapMods);
	};
};
xkb_symbols "pc_us" {
	name[1]="English (US)";
	key <ESC> {	[ 0xff1b ] };
	key <AE01> {
		symbols[1]= [ 0x31, 0x21 ]
	};
	key <AD01> {
		type= "ALPHABETIC",
		symbols[1]= [ 0x71, 0x51 ]
	};
	key <AD11> {
		symbols[1]= [ 0x5b, 0x7b ]
	};
	key <AD12> {
		symbols[1]= [ 0x5d, 0x7d ]
	};
	key <AC01> {
		type= "FOUR_LEVEL",
		symbols[1]= [ 0x61, 0x41, 0xe1, 0xc1 ]
	};
	key <LFSH> {	[ 0xffe1 ] };
	key <RALT> {	[ 0xfe03 ] };
	key <FK01> {
		type= "CTRL+ALT",
		symbols[1]= [ 0xffbe, 0x1008fe01 ]
	};
};
};
"#;

    /// A Norwegian layout in the other spelling: named keysyms, bare symbol lists,
    /// which is what a `libxkbcommon`-serialised keymap looks like.
    pub(crate) const NO: &str = r#"
xkb_keymap {
xkb_keycodes "evdev" {
	<AD11> = 34;
	<AD12> = 35;
	<AC10> = 47;
	<AC11> = 48;
	<AE08> = 17;
	<AE09> = 18;
	<RALT> = 108;
};
xkb_types "complete" {
	type "FOUR_LEVEL" {
		modifiers= Shift+LevelThree;
		map[Shift]= 2;
		map[LevelThree]= 3;
		map[Shift+LevelThree]= 4;
	};
};
xkb_symbols "pc_no" {
	key <AD11> { [ aring, Aring ] };
	key <AD12> { [ diaeresis, asciicircum, asciitilde ] };
	key <AC10> { [ oslash, Oslash ] };
	key <AC11> { [ ae, AE ] };
	key <AE08> {
		type= "FOUR_LEVEL",
		symbols[1]= [ 7, slash, braceleft, NoSymbol ]
	};
	key <AE09> {
		type= "FOUR_LEVEL",
		symbols[1]= [ 8, parenleft, bracketleft, NoSymbol ]
	};
	key <RALT> { [ ISO_Level3_Shift ] };
};
};
"#;

    /// The third spelling, and the one that broke: `xkbcomp -xkb` writes levels as
    /// `Level2` rather than `2`, and dead keys by name. Taken from the output of
    /// `setxkbmap -layout no -print | xkbcomp -xkb`, cut down to the keys under
    /// test.
    ///
    /// Both differences fail *silently* rather than loudly. An unparsed `Level2`
    /// leaves every shifted level unreachable, so `Å` types as `å`; an unparsed
    /// `dead_diaeresis` leaves the key producing nothing at all, so every accented
    /// character on the layout becomes untypable in both directions.
    pub(crate) const NO_NAMED: &str = r#"
xkb_keymap {
xkb_keycodes "evdev+aliases(qwerty)" {
    <AD01> = 24;
    <AD11> = 34;
    <AD12> = 35;
    <RALT> = 108;
};
xkb_types "complete" {
    type "ALPHABETIC" {
        modifiers= Shift+Lock;
        map[Shift]= Level2;
        map[Lock]= Level2;
    };
    type "FOUR_LEVEL" {
        modifiers= Shift+LevelThree;
        map[Shift]= Level2;
        map[LevelThree]= Level3;
        map[Shift+LevelThree]= Level4;
    };
};
xkb_symbols "pc_no" {
    key <AD01> {
        type[Group1]= "ALPHABETIC",
        symbols[Group1]= [ q, Q ]
    };
    key <AD11> {
        type[Group1]= "ALPHABETIC",
        symbols[Group1]= [ aring, Aring ]
    };
    key <AD12> {
        type[Group1]= "FOUR_LEVEL",
        symbols[Group1]= [ dead_diaeresis, dead_circumflex, dead_tilde, dead_caron ]
    };
    key <RALT> {
        symbols[Group1]= [ ISO_Level3_Shift ]
    };
};
};
"#;

    fn us() -> Keymap {
        Keymap::parse(US, 0)
    }

    #[test]
    fn a_plain_letter_needs_no_modifier() {
        let km = us();
        let a = km.stroke(keysym_for_char('a')).unwrap();
        // xkb 38 minus the eight-code offset. Sending 38 would type F4.
        assert_eq!(a.keycode, 30);
        assert_eq!(a.mods, LevelMods::NONE);
        // FOUR_LEVEL, not FOUR_LEVEL_ALPHABETIC: Caps Lock does not reach it, and
        // pretending otherwise would invert the shift on every letter with a
        // declared four-level type.
        assert!(!a.alphabetic);
    }

    #[test]
    fn a_capital_is_the_same_key_with_shift() {
        let km = us();
        let upper = km.stroke(keysym_for_char('A')).unwrap();
        assert_eq!(upper.keycode, 30);
        assert_eq!(upper.mods, LevelMods::SHIFT);
    }

    #[test]
    fn the_third_and_fourth_levels_are_reached_with_altgr() {
        let km = us();
        assert_eq!(
            km.stroke(keysym_for_char('á')).unwrap().mods,
            LevelMods::LEVEL3
        );
        assert_eq!(
            km.stroke(keysym_for_char('Á')).unwrap().mods,
            LevelMods::SHIFT_LEVEL3
        );
    }

    #[test]
    fn altgr_is_found_where_the_keymap_puts_it() {
        // Read out of the keymap rather than assumed: pressing right Alt on a
        // layout that treats it as plain Alt turns every level-3 character into an
        // Alt chord.
        assert_eq!(us().level3_keycode(), Some(KEY_RIGHTALT));
        assert_eq!(Keymap::parse(NO, 0).level3_keycode(), Some(KEY_RIGHTALT));

        // A keymap that also defines the phantom `<LVL3>` keycode the US layout
        // carries must still prefer the key a user's AltGr is actually under.
        let both = r#"
xkb_keymap {
xkb_keycodes "e" { <LVL3> = 92; <RALT> = 108; };
xkb_symbols "s" {
	key <LVL3> { [ 0xfe03 ] };
	key <RALT> { [ 0xfe03 ] };
};
};
"#;
        assert_eq!(Keymap::parse(both, 0).level3_keycode(), Some(KEY_RIGHTALT));
    }

    #[test]
    fn a_lock_mapping_does_not_steal_the_level_from_shift() {
        // `ALPHABETIC` maps level 2 twice — `map[Shift]= 2` and `map[Lock]= 2` —
        // and `<AD01>` declares that type. Letting the Lock entry win records the
        // level as reachable with nothing held, so `Q` presses the `q` key bare
        // and types a lowercase letter with Caps Lock off *and* on.
        let upper = us().stroke(keysym_for_char('Q')).unwrap();
        assert_eq!(upper.keycode, 16, "AD01 is evdev 16, the `q` key");
        assert_eq!(upper.mods, LevelMods::SHIFT);
        assert!(upper.alphabetic);
        assert_eq!(
            us().stroke(keysym_for_char('q')).unwrap().mods,
            LevelMods::NONE
        );
    }

    #[test]
    fn an_unpressable_mapping_never_displaces_a_pressable_one() {
        // Order must not decide it: the same collision arrives Lock-first on some
        // keymaps, and `KEYPAD` spells it with NumLock, which is not a modifier
        // this backend recognises at all.
        let text = r#"
xkb_keymap {
xkb_keycodes "e" { <A> = 10; <B> = 11; };
xkb_types "c" {
	type "LOCK_FIRST" { modifiers= Shift+Lock; map[Lock]= 2; map[Shift]= 2; };
	type "KEYPAD" { modifiers= Shift+NumLock; map[Shift]= 2; map[NumLock]= 2; };
};
xkb_symbols "s" {
	key <A> { type= "LOCK_FIRST", symbols[1]= [ 0x61, 0x41 ] };
	key <B> { type= "KEYPAD", symbols[1]= [ 0x62, 0x42 ] };
};
};
"#;
        let km = Keymap::parse(text, 0);
        assert_eq!(
            km.stroke(keysym_for_char('A')).unwrap().mods,
            LevelMods::SHIFT
        );
        assert_eq!(
            km.stroke(keysym_for_char('B')).unwrap().mods,
            LevelMods::SHIFT
        );
    }

    #[test]
    fn the_fourth_level_of_an_alphabetic_type_still_wants_shift() {
        // `FOUR_LEVEL_ALPHABETIC` maps level 4 as `Shift+LevelThree` and again as
        // `Lock+LevelThree`. Taking the second reading holds AltGr alone and types
        // the third level's character instead.
        let text = r#"
xkb_keymap {
xkb_keycodes "e" { <A> = 10; };
xkb_types "c" {
	type "FOUR_LEVEL_ALPHABETIC" {
		modifiers= Shift+Lock+LevelThree;
		map[Shift]= 2;
		map[Lock]= 2;
		map[LevelThree]= 3;
		map[Shift+LevelThree]= 4;
		map[Lock+LevelThree]= 4;
	};
};
xkb_symbols "s" {
	key <A> { type= "FOUR_LEVEL_ALPHABETIC", symbols[1]= [ 0x61, 0x41, 0xe1, 0xc1 ] };
};
};
"#;
        let km = Keymap::parse(text, 0);
        assert_eq!(
            km.stroke(keysym_for_char('Á')).unwrap().mods,
            LevelMods::SHIFT_LEVEL3
        );
        assert_eq!(
            km.stroke(keysym_for_char('á')).unwrap().mods,
            LevelMods::LEVEL3
        );
    }

    #[test]
    fn a_level_that_wants_control_is_left_unreachable() {
        // `CTRL+ALT` level 2 on F1 is a VT switch. Treating it as a shifted level
        // would press Shift and fire something else entirely.
        let km = us();
        assert!(km.stroke(0x1008fe01).is_none());
        // The base level of the same key is still perfectly usable.
        assert_eq!(km.stroke(0xffbe).unwrap().mods, LevelMods::NONE);
    }

    #[test]
    fn caps_lock_sensitivity_is_recognised_without_a_declared_type() {
        // Most keys declare no type at all; the layout compiler infers ALPHABETIC
        // from a lowercase/uppercase pair, and so must this.
        let km = Keymap::parse(NO, 0);
        assert!(km.stroke(keysym_for_char('å')).unwrap().alphabetic);
        // A digit key is not alphabetic, so Caps Lock must not invert it.
        assert!(!km.stroke(keysym_for_char('7')).unwrap().alphabetic);
    }

    #[test]
    fn named_keysyms_resolve_as_well_as_numeric_ones() {
        // The alpha target writes hex, but a keymap from libxkbcommon is names.
        let km = Keymap::parse(NO, 0);
        for c in ['å', 'Å', 'ø', 'Ø', 'æ', 'Æ'] {
            assert!(
                km.stroke_for_any(char_keysyms(c)).is_some(),
                "{c:?} unresolvable"
            );
        }
    }

    #[test]
    fn the_norwegian_collision_resolves_to_the_character_that_was_asked_for() {
        // The whole reason text crosses the wire instead of scancodes. On this
        // layout `[` lives at AltGr+8 and `å` has the position a US keyboard gives
        // `[`. Asking for `[` must produce `[`.
        let km = Keymap::parse(NO, 0);
        let bracket = km.stroke_for_any(char_keysyms('[')).unwrap();
        let aring = km.stroke_for_any(char_keysyms('å')).unwrap();
        assert_ne!(bracket.keycode, aring.keycode);
        assert_eq!(bracket.mods, LevelMods::LEVEL3);
        assert_eq!(bracket.keycode, 10, "AE09 is evdev 10, the `9` key");
        assert_eq!(aring.mods, LevelMods::NONE);
    }

    #[test]
    fn a_character_the_layout_cannot_type_is_reported_as_missing() {
        // The honest half of the cross-layout promise on Wayland: there is no
        // remapping available, so this has to be a `None` the caller can warn
        // about rather than a stroke that types something else.
        let km = us();
        assert!(km.stroke_for_any(char_keysyms('å')).is_none());
        assert!(km.stroke_for_any(char_keysyms('€')).is_none());
    }

    #[test]
    fn the_cheapest_way_to_type_a_character_wins() {
        // A layout offering the same character unshifted and on an AltGr level
        // should use the plain key; holding AltGr needlessly turns some
        // applications' shortcuts on.
        let text = r#"
xkb_keymap {
xkb_keycodes "e" { <A> = 10; <B> = 11; };
xkb_types "c" { type "FOUR_LEVEL" { modifiers= Shift+LevelThree; map[Shift]= 2; map[LevelThree]= 3; }; };
xkb_symbols "s" {
	key <A> { type= "FOUR_LEVEL", symbols[1]= [ 0x30, 0x31, 0x7a ] };
	key <B> { [ 0x7a ] };
};
};
"#;
        let km = Keymap::parse(text, 0);
        let z = km.stroke(keysym_for_char('z')).unwrap();
        assert_eq!(z.mods, LevelMods::NONE);
        assert_eq!(z.keycode, 3, "the plain key, not the AltGr level");
    }

    #[test]
    fn the_active_group_is_the_one_resolved_against() {
        // Two input sources means one keymap with two groups, and only the active
        // one is what the compositor will interpret our keycodes through.
        let text = r#"
xkb_keymap {
xkb_keycodes "e" { <AD11> = 34; };
xkb_types "c" { type "ALPHABETIC" { modifiers= Shift; map[Shift]= 2; }; };
xkb_symbols "s" {
	key <AD11> {
		symbols[1]= [ 0x5b, 0x7b ],
		symbols[2]= [ 0xe5, 0xc5 ]
	};
};
};
"#;
        assert!(Keymap::parse(text, 0)
            .stroke(keysym_for_char('['))
            .is_some());
        assert!(Keymap::parse(text, 0)
            .stroke(keysym_for_char('å'))
            .is_none());
        assert!(Keymap::parse(text, 1)
            .stroke(keysym_for_char('å'))
            .is_some());
        assert!(Keymap::parse(text, 1)
            .stroke(keysym_for_char('['))
            .is_none());
        // A group the keymap does not have falls back to the first rather than
        // resolving nothing at all.
        assert!(Keymap::parse(text, 7)
            .stroke(keysym_for_char('['))
            .is_some());
    }

    #[test]
    fn bracketed_things_that_are_not_symbols_do_not_become_groups() {
        // `actions[1]= [ ... ]` sits between the symbol lists in a real keymap.
        // Counting it as a group shifts every later group along by one, which
        // silently resolves text against the wrong layout.
        let text = r#"
xkb_keymap {
xkb_keycodes "e" { <A> = 10; };
xkb_types "c" { type "ALPHABETIC" { modifiers= Shift; map[Shift]= 2; }; };
xkb_symbols "s" {
	key <A> {
		type[1]= "ALPHABETIC",
		symbols[1]= [ 0x61, 0x41 ],
		actions[1]= [ NoAction(), NoAction() ]
	};
};
};
"#;
        let km = Keymap::parse(text, 0);
        assert_eq!(km.stroke(keysym_for_char('a')).unwrap().keycode, 2);
        assert_eq!(
            km.stroke(keysym_for_char('A')).unwrap().mods,
            LevelMods::SHIFT
        );
    }

    #[test]
    fn nothing_understood_is_reported_rather_than_pretended() {
        assert!(Keymap::parse("", 0).is_empty());
        assert!(Keymap::parse("not a keymap at all", 0).is_empty());
        assert!(!us().is_empty());
    }

    #[test]
    fn a_truncated_keymap_does_not_panic() {
        // The keymap arrives over a file descriptor; a short read or a compositor
        // bug must not take the agent down with it.
        for cut in [10, 200, 600, US.len() - 1] {
            let mut end = cut.min(US.len());
            while !US.is_char_boundary(end) {
                end -= 1;
            }
            let _ = Keymap::parse(&US[..end], 0);
        }
    }

    // -- the capture direction -------------------------------------------
    //
    // The same fixtures read the other way round. Every assertion below is the
    // inverse of one above it, which is the point: a keymap that injects `å`
    // correctly and captures it as `[` would keep the cross-layout promise in one
    // direction only, and the failure would look like the peer's bug.

    #[test]
    fn a_captured_keycode_resolves_through_this_machines_layout() {
        // The whole guarantee in one assertion. Keycode 34 is `[` on a US layout
        // and `å` on a Norwegian one; the answer must come from the keymap the
        // compositor handed over, not from the number.
        assert_eq!(
            us().keysym(26, LevelMods::NONE, false),
            Some(keysym_for_char('['))
        );
        assert_eq!(
            Keymap::parse(NO, 0).keysym(26, LevelMods::NONE, false),
            Some(keysym_for_char('å'))
        );
    }

    #[test]
    fn shift_and_altgr_select_the_level_they_reach() {
        let km = us();
        // <AC01>, FOUR_LEVEL: a A á Á.
        for (mods, expected) in [
            (LevelMods::NONE, 'a'),
            (LevelMods::SHIFT, 'A'),
            (LevelMods::LEVEL3, 'á'),
            (LevelMods::SHIFT_LEVEL3, 'Á'),
        ] {
            assert_eq!(
                km.keysym(30, mods, false),
                Some(keysym_for_char(expected)),
                "{mods:?}"
            );
        }
    }

    #[test]
    fn caps_lock_flips_a_letter_and_leaves_a_digit_alone() {
        let km = us();
        // <AD01> is ALPHABETIC, so Caps Lock reaches it...
        assert_eq!(km.keysym(16, LevelMods::NONE, true), Some('Q' as u32));
        assert_eq!(km.keysym(16, LevelMods::SHIFT, true), Some('q' as u32));
        // ...and <AE01> is not, so it does not. Folding Caps Lock into the shift
        // for every key would turn `1` into `!` whenever the light was on.
        assert_eq!(km.keysym(2, LevelMods::NONE, true), Some('1' as u32));
        assert_eq!(km.keysym(2, LevelMods::SHIFT, true), Some('!' as u32));
    }

    #[test]
    fn a_modifier_this_backend_will_not_reach_does_not_swallow_the_keystroke() {
        // <AE01> has two levels and no level-3 entry. AltGr+1 still produces `1`
        // on the desktop, so reporting nothing would drop a key the user pressed.
        let km = us();
        assert_eq!(km.keysym(2, LevelMods::LEVEL3, false), Some('1' as u32));
        assert_eq!(
            km.keysym(2, LevelMods::SHIFT_LEVEL3, false),
            Some('!' as u32)
        );
    }

    #[test]
    fn a_level_that_needs_control_is_not_mistaken_for_a_shifted_character() {
        // <FK01> declares CTRL+ALT, whose second level is a VT switch. Its base
        // level is still F1 and must resolve; the second must never be reachable
        // by Shift, or holding Shift on F1 would report an XF86 keysym.
        let km = us();
        assert_eq!(km.keysym(59, LevelMods::NONE, false), Some(0xffbe));
        assert_eq!(km.keysym(59, LevelMods::SHIFT, false), Some(0xffbe));
    }

    #[test]
    fn a_keycode_the_layout_says_nothing_about_resolves_to_nothing() {
        // Not a panic and not a guess: the caller reports it as an unmapped key
        // and lets `KeyResolver` forward the raw code.
        assert_eq!(us().keysym(200, LevelMods::NONE, false), None);
    }

    #[test]
    fn the_layout_names_its_own_altgr_key() {
        // Right Alt is `ISO_Level3_Shift` on both fixtures, so capture reports
        // ALT_GR for it. A layout that put level 3 elsewhere must not have right
        // Alt claim the bit.
        assert!(us().is_level3(KEY_RIGHTALT));
        assert!(Keymap::parse(NO, 0).is_level3(KEY_RIGHTALT));
        assert!(!us().is_level3(KEY_LEFTALT));
    }

    #[test]
    fn a_dead_key_on_the_layout_is_reported_as_the_keysym_it_is() {
        // The Norwegian `¨` key: level 1 is `diaeresis`, which is where dead-key
        // composition starts. Capture has to hand the keysym up unchanged so
        // `keys::dead_accent` can classify it.
        let km = Keymap::parse(NO, 0);
        assert_eq!(km.keysym(27, LevelMods::NONE, false), Some(0xa8));
    }

    #[test]
    fn what_injection_types_is_what_capture_reads_back() {
        // Round trip through both indexes on both fixtures. A disagreement here is
        // exactly the bug that makes a character survive one hop and not the
        // return journey.
        for (name, text) in [("us", US), ("no", NO)] {
            let km = Keymap::parse(text, 0);
            for c in ['a', 'A', '[', 'å', 'ø', 'æ', '7', '/'] {
                let Some(stroke) = km.stroke_for_any(char_keysyms(c)) else {
                    continue;
                };
                let back = km.keysym(stroke.keycode, stroke.mods, false);
                assert_eq!(
                    back.and_then(crate::linux_wayland::keys::char_for_keysym),
                    Some(c),
                    "{name}: {c:?} typed on {} {:?} read back as {back:?}",
                    stroke.keycode,
                    stroke.mods
                );
            }
        }
    }

    #[test]
    fn a_level_named_rather_than_numbered_is_still_a_level() {
        // Measured against a real `xkbcomp -xkb` keymap: without this every
        // shifted character silently types its base instead, so `Å` comes out as
        // `å` and the failure looks like a stuck Shift rather than a parse gap.
        assert_eq!(parse_level("Level2"), Some(2));
        assert_eq!(parse_level("= Level4"), Some(4));
        assert_eq!(parse_level(" 2 "), Some(2));
        assert_eq!(parse_level("Level0"), None);
        assert_eq!(parse_level("Any"), None);

        let km = Keymap::parse(NO_NAMED, 0);
        assert_eq!(km.keysym(26, LevelMods::NONE, false), Some(0xe5)); // å
        assert_eq!(km.keysym(26, LevelMods::SHIFT, false), Some(0xc5)); // Å
        assert_eq!(km.stroke(0xc5).unwrap().mods, LevelMods::SHIFT);
    }

    #[test]
    fn a_dead_key_written_by_name_is_a_dead_key_and_not_nothing() {
        // The other half of the same real keymap. A `dead_diaeresis` nobody
        // recognises leaves the key with no symbol at any level, which makes every
        // accented character on the layout untypable in both directions — and
        // reports the keystroke as unmapped rather than as the start of a
        // composition.
        let km = Keymap::parse(NO_NAMED, 0);
        assert_eq!(km.keysym(27, LevelMods::NONE, false), Some(0xfe57));
        assert_eq!(km.keysym(27, LevelMods::SHIFT, false), Some(0xfe52));
        assert_eq!(km.keysym(27, LevelMods::LEVEL3, false), Some(0xfe53));
        // And injection can find them, which is what makes a character the
        // receiver has no single key for reachable at all.
        assert_eq!(km.stroke(0xfe57).unwrap().keycode, 27);
    }

    #[test]
    fn every_dead_key_this_backend_composes_is_readable_by_name() {
        // Three tables have to agree: the name a keymap writes, the keysym it
        // stands for, and the combining mark `keyres` composes with. A gap in any
        // of them is a character that works on one layout and not another.
        for combining in [
            '\u{0300}', '\u{0301}', '\u{0302}', '\u{0303}', '\u{0304}', '\u{0306}', '\u{0307}',
            '\u{0308}', '\u{030a}', '\u{030b}', '\u{030c}', '\u{0327}', '\u{0328}',
        ] {
            let keysym = crate::linux_wayland::keys::dead_keysym(combining).unwrap();
            let name = DEAD_NAMES
                .iter()
                .find(|n| dead_named_keysym(n) == Some(keysym))
                .unwrap_or_else(|| panic!("{combining:?} has no name"));
            assert_eq!(parse_keysym(name), Some(keysym), "{name}");
        }
    }

    /// Every `dead_*` spelling this module claims to read.
    const DEAD_NAMES: &[&str] = &[
        "dead_grave",
        "dead_acute",
        "dead_circumflex",
        "dead_tilde",
        "dead_macron",
        "dead_breve",
        "dead_abovedot",
        "dead_diaeresis",
        "dead_abovering",
        "dead_doubleacute",
        "dead_caron",
        "dead_cedilla",
        "dead_ogonek",
    ];

    #[test]
    fn keysym_tokens_cover_the_spellings_a_keymap_uses() {
        assert_eq!(parse_keysym("0xff1b"), Some(0xff1b));
        assert_eq!(parse_keysym("a"), Some(0x61));
        assert_eq!(parse_keysym("7"), Some(0x37));
        assert_eq!(parse_keysym("bracketleft"), Some(0x5b));
        assert_eq!(parse_keysym("aring"), Some(0xe5));
        assert_eq!(parse_keysym("ISO_Level3_Shift"), Some(0xfe03));
        assert_eq!(parse_keysym("U00E5"), Some(0xe5));
        assert_eq!(parse_keysym("NoSymbol"), None);
        assert_eq!(parse_keysym(""), None);
        assert_eq!(parse_keysym("Hangul_Jeonja"), None);
    }
}
