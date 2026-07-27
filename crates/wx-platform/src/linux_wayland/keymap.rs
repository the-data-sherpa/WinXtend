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

/// One keyboard layout, indexed the way injection needs to ask about it.
#[derive(Debug, Default)]
pub struct Keymap {
    strokes: HashMap<u32, Stroke>,
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
            level3_keycode,
        }
    }

    /// How to type `keysym`, or `None` if this layout cannot.
    pub fn stroke(&self, keysym: u32) -> Option<Stroke> {
        self.strokes.get(&keysym).copied()
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
/// A `map` naming any modifier outside Shift, `LevelThree` and Lock records the
/// level as unreachable rather than approximating it: that is how a `CTRL+ALT`
/// key's second level stops being mistaken for a shifted character.
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
        let Some(level) = level
            .trim()
            .trim_start_matches('=')
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|l| *l >= 1)
        else {
            continue;
        };

        let mut wanted = LevelMods::NONE;
        let mut reachable = true;
        for token in mods.split('+') {
            match token.trim() {
                "Shift" => wanted.shift = true,
                "LevelThree" => wanted.level3 = true,
                // Caps Lock is the desktop's state, not something to press; a
                // level reached with it is reached without it too once the
                // alphabetic inversion has been applied.
                "Lock" | "" => {}
                _ => reachable = false,
            }
        }

        let levels = types.entry(name.to_string()).or_default();
        if levels.len() < level {
            levels.resize(level, None);
        }
        levels[level - 1] = reachable.then_some(wanted);
    }
    types
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
    LATIN1_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c as u32)
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
mod tests {
    use super::*;
    use crate::linux_wayland::keys::{char_keysyms, keysym_for_char};

    /// The shape mutter actually sends, cut down to the keys under test: numeric
    /// keysyms, tab-indented, `symbols[N]=` for multi-group keys and a bare list
    /// for single-group ones.
    pub(super) const US: &str = r#"
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
    pub(super) const NO: &str = r#"
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
