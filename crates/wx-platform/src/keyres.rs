//! Layout resolution: turning a platform key event into a wire [`KeyEvent`].
//!
//! This module is where WinXtend's cross-layout guarantee actually lives. The
//! protocol promises that a Norwegian keyboard types `å` on a US machine, and
//! that promise is only kept if resolution happens **here**, on the sending
//! machine, against the sender's own layout. Ship a scancode instead and the
//! receiver types whatever sits at that position in *its* layout, which is the
//! defect every Synergy-lineage tool has.
//!
//! # What the backends must supply
//!
//! Each OS backend does the syscall part — `ToUnicodeEx` on Windows,
//! `UCKeyTranslate` on macOS, `xkb_state_key_get_utf8` on X11/Wayland — and hands
//! the result over as a [`RawKey`]. Everything after that is platform-independent
//! and lives here, so the composition rules cannot drift between backends.
//!
//! The one thing here that runs on the *receiving* side is [`decompose`], the
//! inverse of [`compose`]: an injector whose layout has no single key for `é` types
//! the dead acute and then `e` instead. It shares these tables precisely so the two
//! directions cannot disagree about which accent produces which character.
//!
//! # Why composition is our problem and not the OS's
//!
//! Dead keys (`'` then `a` giving `á`) are normally resolved by the OS as part of
//! delivering the keystroke to the focused window. WinXtend cannot use that: while
//! the cursor is on a remote machine the local keystrokes are suppressed, so no
//! window ever receives them and the OS composition machinery never runs. Worse,
//! the naive fix — letting the OS compose by *not* passing the no-state flag to
//! `ToUnicodeEx` — corrupts the user's own local composition state, so typing
//! locally afterwards produces stray accents. So the state machine is ours, and
//! [`KeyResolver`] is it.

use wx_proto::{KeyAction, KeyEvent, KeyPayload, Modifiers, SpecialKey};

/// A key event as a platform backend observes it, before it becomes wire traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawKey {
    /// Platform keycode (a Windows virtual key, a macOS `CGKeyCode`, an X11
    /// keycode). Only used for the [`KeyPayload::RawKeyCode`] escape hatch and to
    /// pair a release with the press it belongs to.
    pub code: u32,
    pub action: KeyAction,
    pub modifiers: Modifiers,
    /// The semantic key, if this one has no textual meaning.
    ///
    /// Takes precedence over `text`: backends that report both for Tab or Enter
    /// get the [`SpecialKey`], because a receiver injecting `\t` as a unicode
    /// character does not move focus.
    pub special: Option<SpecialKey>,
    /// Text the layout produces for this keystroke.
    ///
    /// Backends must resolve this with Shift and AltGr applied but Ctrl, Alt, and
    /// Super masked out. That is what makes `Ctrl+C` cross layouts: the wire
    /// event needs the base character `c` plus a Ctrl modifier bit, not the
    /// control character `0x03`, which no receiver can usefully inject. Leaked
    /// control characters are recovered defensively below, but backends should not
    /// rely on it.
    pub text: Option<String>,
    /// Set when the layout says this keystroke is a dead key: it produces no text
    /// yet and instead modifies the next one. The character is the accent the
    /// platform reported, spacing or combining form.
    pub dead: Option<char>,
}

impl RawKey {
    pub fn special(code: u32, key: SpecialKey, action: KeyAction, modifiers: Modifiers) -> Self {
        Self {
            code,
            action,
            modifiers,
            special: Some(key),
            text: None,
            dead: None,
        }
    }

    pub fn text(
        code: u32,
        text: impl Into<String>,
        action: KeyAction,
        modifiers: Modifiers,
    ) -> Self {
        Self {
            code,
            action,
            modifiers,
            special: None,
            text: Some(text.into()),
            dead: None,
        }
    }

    pub fn dead(code: u32, accent: char, action: KeyAction, modifiers: Modifiers) -> Self {
        Self {
            code,
            action,
            modifiers,
            special: None,
            text: None,
            dead: Some(accent),
        }
    }

    /// A keystroke the layout maps to nothing at all.
    pub fn unmapped(code: u32, action: KeyAction, modifiers: Modifiers) -> Self {
        Self {
            code,
            action,
            modifiers,
            special: None,
            text: None,
            dead: None,
        }
    }
}

/// Resolves [`RawKey`]s into wire [`KeyEvent`]s, carrying the state that spans
/// keystrokes.
///
/// Two pieces of state, each fixing a specific failure:
///
/// * **Pending dead key**, so `'` + `a` becomes one `á` rather than two useless
///   events.
/// * **What each held key resolved to**, so a release carries the same payload as
///   its press. Re-resolving at release time is wrong: the user may have let go of
///   Shift first, and the receiver would then get `press A` / `release a` and be
///   left with `A` stuck down.
#[derive(Debug, Default)]
pub struct KeyResolver {
    pending_dead: Option<char>,
    /// Press-ordered, so releases can be replayed newest-first with modifiers
    /// last. `None` payload means the press was deliberately swallowed (a dead
    /// key), and its release must be swallowed too.
    down: Vec<(u32, Option<KeyPayload>)>,
}

impl KeyResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a dead key is waiting for its base character.
    ///
    /// Exposed so the agent can avoid handing control to another machine
    /// mid-composition, which would send the accent to one screen and the base
    /// letter to another.
    pub fn has_pending_composition(&self) -> bool {
        self.pending_dead.is_some()
    }

    /// Number of keys currently believed held down.
    pub fn held_count(&self) -> usize {
        self.down.iter().filter(|(_, p)| p.is_some()).count()
    }

    /// Resolve one platform key event.
    ///
    /// Returns zero, one, or several wire events: zero when a dead key is
    /// absorbed, several when a pending accent has to be flushed ahead of the key
    /// that cancelled it.
    pub fn resolve(&mut self, raw: &RawKey) -> Vec<KeyEvent> {
        match raw.action {
            KeyAction::Release => self.resolve_release(raw),
            KeyAction::Press | KeyAction::Repeat => self.resolve_press(raw),
        }
    }

    fn resolve_release(&mut self, raw: &RawKey) -> Vec<KeyEvent> {
        match self.take_down(raw.code) {
            // Swallowed at press time (a dead key); swallow the release to match.
            Some(None) => Vec::new(),
            Some(Some(payload)) => vec![KeyEvent {
                payload,
                action: KeyAction::Release,
                modifiers: raw.modifiers,
            }],
            // Never seen pressed: capture started with the key already held, or
            // the press went to a different resolver. Forward it anyway —
            // dropping a release is the one unrecoverable mistake, because the
            // receiver is then stuck with a key down that no local action clears.
            None => {
                let payload = self
                    .payload_for(raw)
                    .unwrap_or(KeyPayload::RawKeyCode(raw.code));
                vec![KeyEvent {
                    payload,
                    action: KeyAction::Release,
                    modifiers: raw.modifiers,
                }]
            }
        }
    }

    fn resolve_press(&mut self, raw: &RawKey) -> Vec<KeyEvent> {
        let mut out = Vec::new();

        // A second dead key in a row: the first one is no longer going to compose
        // with anything, so emit it as a standalone accent (`^` `^` gives `^^`,
        // matching Windows and X11 behaviour).
        if let Some(accent) = raw.dead {
            if raw.action == KeyAction::Repeat {
                // Auto-repeat of a held dead key composes nothing; absorbing it
                // avoids a burst of stray accents on the receiver.
                return out;
            }
            if let Some(previous) = self.pending_dead.take() {
                out.extend(flush_accent(previous));
            }
            self.pending_dead = Some(accent);
            self.record_down(raw, None);
            return out;
        }

        // Escape cancels a composition rather than committing it, which is what
        // every desktop does and what a user reaching for Escape intends.
        if raw.special == Some(SpecialKey::Escape) {
            self.pending_dead = None;
        }

        let payload = match self.payload_for(raw) {
            Some(payload) => payload,
            // Nothing the layout could give us. Forwarding the raw code beats
            // dropping the keystroke: some receivers can still honour it, and a
            // silently vanished key is indistinguishable from a network fault.
            None => KeyPayload::RawKeyCode(raw.code),
        };

        // A pending accent that cannot compose with this key (a function key, an
        // arrow, an unmapped code) is flushed rather than discarded: the user
        // typed it and losing input silently is worse than an extra character.
        //
        // A level modifier is not one of those keys: see [`holds_composition`].
        let composing = matches!(payload, KeyPayload::Text(_))
            || matches!(payload, KeyPayload::Special(k) if holds_composition(k));
        if self.pending_dead.is_some() && !composing {
            if let Some(previous) = self.pending_dead.take() {
                out.extend(flush_accent(previous));
            }
        }

        let payload = match payload {
            KeyPayload::Text(text) => match self.pending_dead.take() {
                Some(accent) => KeyPayload::Text(compose(accent, &text)),
                None => KeyPayload::Text(text),
            },
            // The accent stays pending across a level modifier; every other payload
            // has already flushed it above.
            payload => payload,
        };

        self.record_down(raw, Some(payload.clone()));
        out.push(KeyEvent {
            payload,
            action: raw.action,
            modifiers: raw.modifiers,
        });
        out
    }

    /// Decide the payload for a keystroke, ignoring composition state.
    fn payload_for(&self, raw: &RawKey) -> Option<KeyPayload> {
        if let Some(key) = raw.special {
            return Some(KeyPayload::Special(key));
        }
        let text = raw.text.as_deref()?;

        // A single control character means the backend did not mask Ctrl out, or
        // the layout maps this key to a C0 code. Recover rather than forward
        // something no receiver can type.
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if is_control(c) {
                return recover_control_char(c, raw.modifiers);
            }
        }

        let scrubbed: String = text.chars().filter(|c| !is_control(*c)).collect();
        if scrubbed.is_empty() {
            return None;
        }
        Some(KeyPayload::Text(scrubbed))
    }

    fn record_down(&mut self, raw: &RawKey, payload: Option<KeyPayload>) {
        // Auto-repeat must not append a second entry, or `release_held` would
        // emit one release per repeat.
        if let Some(slot) = self.down.iter_mut().find(|(code, _)| *code == raw.code) {
            slot.1 = payload;
            return;
        }
        self.down.push((raw.code, payload));
    }

    fn take_down(&mut self, code: u32) -> Option<Option<KeyPayload>> {
        let idx = self.down.iter().position(|(c, _)| *c == code)?;
        Some(self.down.remove(idx).1)
    }

    /// Releases for every key still held, newest first.
    ///
    /// Sent before yielding control and on disconnect. Newest-first matters
    /// because modifiers are pressed before the keys they modify, so this order
    /// releases `Ctrl` last — releasing it first can turn the remaining releases
    /// into unintended chords on the receiver.
    ///
    /// Also clears any pending composition: an accent left pending across a
    /// handoff would attach itself to the first letter typed after control comes
    /// back, minutes later.
    pub fn release_held(&mut self) -> Vec<KeyEvent> {
        self.pending_dead = None;
        let held = core::mem::take(&mut self.down);
        held.into_iter()
            .rev()
            .filter_map(|(_, payload)| payload)
            .map(|payload| KeyEvent {
                payload,
                action: KeyAction::Release,
                // Modifier state is not replayed: by definition we are tearing
                // state down, and asserting a modifier here would re-latch what
                // we are trying to clear.
                modifiers: Modifiers::NONE,
            })
            .collect()
    }

    /// Forget all state, without emitting anything.
    ///
    /// For a hard reset — capture restarting, a peer vanishing — where the
    /// releases could not be delivered anyway.
    pub fn reset(&mut self) {
        self.pending_dead = None;
        self.down.clear();
    }
}

/// Whether a bare modifier press leaves a composition in progress alone.
///
/// The *level* modifiers, and only those. Nothing cancels or commits a composition
/// because the user reached for the shift that types the letter the accent is going
/// to compose with: `^` then Shift then `E` is `Ê` on every desktop, not `^E`, and
/// on a European layout the accent itself is often reached through AltGr.
///
/// Ctrl, Alt and Super are the opposite case and are deliberately excluded. Holding
/// one of those means a shortcut, and composing the accent onto the shortcut's
/// letter sends the peer a character its layout cannot resolve, carrying the CTRL
/// bit — so the shortcut silently never fires. A stray accent is a blemish; a dead
/// `Ctrl+S` is a broken feature. GTK draws the same line: it declines to feed a
/// keypress into the compose table while Ctrl or Alt is held.
///
/// The residual wart, accepted rather than overlooked: on a layout where right Alt
/// is a plain Alt rather than AltGr, a pending accent survives an `AltRight` chord.
/// The alternative is deciding by which physical key it is rather than by what it
/// means, which would break AltGr composition on every European layout — the far
/// commoner case.
fn holds_composition(key: SpecialKey) -> bool {
    matches!(
        key,
        SpecialKey::ShiftLeft | SpecialKey::ShiftRight | SpecialKey::AltRight
    )
}

/// Emit a pending accent as a standalone character, press and release together.
///
/// Modifiers are cleared: the accent is plain text, and carrying a stale Ctrl bit
/// would turn it into a chord on the receiver.
fn flush_accent(accent: char) -> Vec<KeyEvent> {
    let text = Accent::classify(accent)
        .map(|a| a.spacing().to_string())
        .unwrap_or_else(|| accent.to_string());
    vec![
        KeyEvent::text(text.clone(), KeyAction::Press, Modifiers::NONE),
        KeyEvent::text(text, KeyAction::Release, Modifiers::NONE),
    ]
}

/// C0 and C1 control characters, which are never useful as wire text.
fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// Turn a leaked control character back into something injectable.
fn recover_control_char(c: char, modifiers: Modifiers) -> Option<KeyPayload> {
    // Ctrl+letter arrives as 0x01..0x1A when the backend forgot to mask Ctrl.
    // Recovering the letter keeps the chord intact: the receiver holds Ctrl and
    // types the base character, which is what `Ctrl+C` has to become.
    let ctrl_like = modifiers.contains(Modifiers::CTRL) || modifiers.contains(Modifiers::SUPER);
    let code = c as u32;
    if ctrl_like && (0x01..=0x1a).contains(&code) {
        let letter = (b'a' + (code as u8 - 1)) as char;
        return Some(KeyPayload::Text(letter.to_string()));
    }
    // Otherwise the layout genuinely produced a control code for a key the
    // backend did not classify. Map the well-known ones to their semantic key.
    Some(KeyPayload::Special(match c {
        '\r' | '\n' => SpecialKey::Enter,
        '\t' => SpecialKey::Tab,
        '\u{8}' => SpecialKey::Backspace,
        '\u{1b}' => SpecialKey::Escape,
        '\u{7f}' => SpecialKey::Delete,
        _ => return None,
    }))
}

/// Split a precomposed character back into a base letter and the combining accent
/// that produces it.
///
/// The inverse of [`compose`], and the one thing a *receiving* backend needs from
/// these tables. A keyboard layout with no key for `é` — which is most of them —
/// almost always still has a dead acute, and pressing that followed by `e` is how
/// the character is typed on that machine. Without this the receiver can only
/// refuse a composed character outright, which is precisely the outcome resolving
/// keystrokes to text was meant to avoid.
///
/// `None` for anything these tables do not cover, including characters that are
/// already a base letter.
pub fn decompose(c: char) -> Option<(char, char)> {
    for accent in Accent::ALL {
        let (bases, composed) = accent.table();
        if let Some(index) = composed.chars().position(|x| x == c) {
            return Some((bases.chars().nth(index)?, accent.combining()));
        }
    }
    None
}

/// Compose a pending accent with the text that followed it.
///
/// Prefers a precomposed codepoint where Unicode has one, because a few older
/// Windows applications render `a` + U+0301 as two glyphs. Falls back to the
/// decomposed form, which still displays and still types everywhere, rather than
/// dropping the accent.
fn compose(accent: char, following: &str) -> String {
    let Some(base) = following.chars().next() else {
        return following.to_string();
    };
    let rest: String = following.chars().skip(1).collect();

    let Some(class) = Accent::classify(accent) else {
        // An accent we do not recognise. Concatenating is lossless and lets the
        // receiver's font do what it can.
        return format!("{accent}{following}");
    };

    // Space commits the accent on its own; this is how every layout lets you type
    // a bare `^` or `~`.
    if base == ' ' {
        return format!("{}{}", class.spacing(), rest);
    }

    match class.precomposed(base) {
        Some(c) => format!("{c}{rest}"),
        None => format!("{base}{}{rest}", class.combining()),
    }
}

/// The accent a dead key applies.
///
/// Classified rather than used verbatim because platforms report dead keys
/// inconsistently: Windows `ToUnicodeEx` hands back the spacing form (`^`, `´`),
/// X11 keysyms use `dead_circumflex`, and some layouts report the combining
/// codepoint. All three have to reach the same composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Accent {
    Acute,
    Grave,
    Circumflex,
    Tilde,
    Diaeresis,
    Ring,
    Cedilla,
    Caron,
    Macron,
    Breve,
    DotAbove,
    DoubleAcute,
    Ogonek,
}

impl Accent {
    /// Every accent, so the tables can be walked without a list that drifts out
    /// of step with the enum.
    const ALL: [Accent; 13] = [
        Accent::Acute,
        Accent::Grave,
        Accent::Circumflex,
        Accent::Tilde,
        Accent::Diaeresis,
        Accent::Ring,
        Accent::Cedilla,
        Accent::Caron,
        Accent::Macron,
        Accent::Breve,
        Accent::DotAbove,
        Accent::DoubleAcute,
        Accent::Ogonek,
    ];

    fn classify(c: char) -> Option<Self> {
        Some(match c {
            '\'' | '\u{00b4}' | '\u{0301}' => Accent::Acute,
            '`' | '\u{02cb}' | '\u{0300}' => Accent::Grave,
            '^' | '\u{02c6}' | '\u{0302}' => Accent::Circumflex,
            '~' | '\u{02dc}' | '\u{0303}' => Accent::Tilde,
            '"' | '\u{00a8}' | '\u{0308}' => Accent::Diaeresis,
            '\u{00b0}' | '\u{02da}' | '\u{030a}' => Accent::Ring,
            ',' | '\u{00b8}' | '\u{0327}' => Accent::Cedilla,
            '\u{02c7}' | '\u{030c}' => Accent::Caron,
            '\u{00af}' | '\u{02c9}' | '\u{0304}' => Accent::Macron,
            '\u{02d8}' | '\u{0306}' => Accent::Breve,
            '\u{02d9}' | '\u{0307}' => Accent::DotAbove,
            '\u{02dd}' | '\u{030b}' => Accent::DoubleAcute,
            '\u{02db}' | '\u{0328}' => Accent::Ogonek,
            _ => return None,
        })
    }

    /// The standalone spacing character, for "dead key then space".
    fn spacing(self) -> char {
        match self {
            Accent::Acute => '\u{00b4}',
            Accent::Grave => '`',
            Accent::Circumflex => '^',
            Accent::Tilde => '~',
            Accent::Diaeresis => '\u{00a8}',
            Accent::Ring => '\u{00b0}',
            Accent::Cedilla => '\u{00b8}',
            Accent::Caron => '\u{02c7}',
            Accent::Macron => '\u{00af}',
            Accent::Breve => '\u{02d8}',
            Accent::DotAbove => '\u{02d9}',
            Accent::DoubleAcute => '\u{02dd}',
            Accent::Ogonek => '\u{02db}',
        }
    }

    /// The combining mark, used for the decomposed fallback.
    fn combining(self) -> char {
        match self {
            Accent::Acute => '\u{0301}',
            Accent::Grave => '\u{0300}',
            Accent::Circumflex => '\u{0302}',
            Accent::Tilde => '\u{0303}',
            Accent::Diaeresis => '\u{0308}',
            Accent::Ring => '\u{030a}',
            Accent::Cedilla => '\u{0327}',
            Accent::Caron => '\u{030c}',
            Accent::Macron => '\u{0304}',
            Accent::Breve => '\u{0306}',
            Accent::DotAbove => '\u{0307}',
            Accent::DoubleAcute => '\u{030b}',
            Accent::Ogonek => '\u{0328}',
        }
    }

    /// Base letters this accent has a precomposed form for, paired positionally
    /// with those forms.
    ///
    /// Two parallel strings rather than a table of tuples: it stays readable, and
    /// `compose_tables_are_positionally_aligned` fails loudly if the two ever
    /// disagree in length, which is the only way to get silent mis-composition
    /// here.
    fn table(self) -> (&'static str, &'static str) {
        match self {
            Accent::Acute => ("aeiouyclnrszAEIOUYCLNRSZ", "áéíóúýćĺńŕśźÁÉÍÓÚÝĆĹŃŔŚŹ"),
            Accent::Grave => ("aeiouAEIOU", "àèìòùÀÈÌÒÙ"),
            Accent::Circumflex => ("aceghijosuwyzACEGHIJOSUWYZ", "âĉêĝĥîĵôŝûŵŷẑÂĈÊĜĤÎĴÔŜÛŴŶẐ"),
            Accent::Tilde => ("anoiuANOIU", "ãñõĩũÃÑÕĨŨ"),
            Accent::Diaeresis => ("aeiouyAEIOUY", "äëïöüÿÄËÏÖÜŸ"),
            Accent::Ring => ("auAU", "åůÅŮ"),
            Accent::Cedilla => ("cgklnrstCGKLNRST", "çģķļņŗşţÇĢĶĻŅŖŞŢ"),
            Accent::Caron => ("cdelnrstzCDELNRSTZ", "čďěľňřšťžČĎĚĽŇŘŠŤŽ"),
            Accent::Macron => ("aeiouAEIOU", "āēīōūĀĒĪŌŪ"),
            Accent::Breve => ("aegiouAEGIOU", "ăĕğĭŏŭĂĔĞĬŎŬ"),
            Accent::DotAbove => ("cegzCEGZ", "ċėġżĊĖĠŻ"),
            Accent::DoubleAcute => ("ouOU", "őűŐŰ"),
            Accent::Ogonek => ("aeiuAEIU", "ąęįųĄĘĮŲ"),
        }
    }

    fn precomposed(self, base: char) -> Option<char> {
        let (bases, composed) = self.table();
        let index = bases.chars().position(|c| c == base)?;
        composed.chars().nth(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a platform key source, so the resolver can be driven through
    /// realistic sequences without a keyboard or a display server.
    struct FakeKeyboard {
        resolver: KeyResolver,
        next_code: u32,
        codes: std::collections::HashMap<String, u32>,
    }

    impl FakeKeyboard {
        fn new() -> Self {
            Self {
                resolver: KeyResolver::new(),
                next_code: 100,
                codes: std::collections::HashMap::new(),
            }
        }

        /// A stable keycode per logical key, so presses and releases pair up the
        /// way a real keyboard's do.
        fn code_for(&mut self, key: &str) -> u32 {
            if let Some(c) = self.codes.get(key) {
                return *c;
            }
            self.next_code += 1;
            self.codes.insert(key.to_string(), self.next_code);
            self.next_code
        }

        fn press_text(&mut self, text: &str, modifiers: Modifiers) -> Vec<KeyEvent> {
            let code = self.code_for(text);
            self.resolver
                .resolve(&RawKey::text(code, text, KeyAction::Press, modifiers))
        }

        fn release_text(&mut self, text: &str, modifiers: Modifiers) -> Vec<KeyEvent> {
            let code = self.code_for(text);
            self.resolver
                .resolve(&RawKey::text(code, text, KeyAction::Release, modifiers))
        }

        fn press_dead(&mut self, accent: char) -> Vec<KeyEvent> {
            let code = self.code_for(&format!("dead{accent}"));
            self.resolver.resolve(&RawKey::dead(
                code,
                accent,
                KeyAction::Press,
                Modifiers::NONE,
            ))
        }

        fn release_dead(&mut self, accent: char) -> Vec<KeyEvent> {
            let code = self.code_for(&format!("dead{accent}"));
            self.resolver.resolve(&RawKey::dead(
                code,
                accent,
                KeyAction::Release,
                Modifiers::NONE,
            ))
        }

        fn press_special(&mut self, key: SpecialKey, modifiers: Modifiers) -> Vec<KeyEvent> {
            let code = self.code_for(&format!("{key:?}"));
            self.resolver
                .resolve(&RawKey::special(code, key, KeyAction::Press, modifiers))
        }

        fn release_special(&mut self, key: SpecialKey, modifiers: Modifiers) -> Vec<KeyEvent> {
            let code = self.code_for(&format!("{key:?}"));
            self.resolver
                .resolve(&RawKey::special(code, key, KeyAction::Release, modifiers))
        }
    }

    fn texts(events: &[KeyEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match (&e.payload, e.action) {
                (KeyPayload::Text(t), KeyAction::Press) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn compose_tables_are_positionally_aligned() {
        // A length mismatch would silently pair the wrong accent with the wrong
        // letter, and only some users of some layouts would ever notice.
        for accent in Accent::ALL {
            let (bases, composed) = accent.table();
            assert_eq!(
                bases.chars().count(),
                composed.chars().count(),
                "{accent:?} table lengths disagree"
            );
            let mut seen = std::collections::HashSet::new();
            for b in bases.chars() {
                assert!(seen.insert(b), "{accent:?} lists base {b:?} twice");
            }
        }
    }

    #[test]
    fn every_accent_classifies_its_own_spacing_and_combining_forms() {
        // Round-tripping matters because a platform may report either form, and a
        // form we do not classify degrades to naive concatenation.
        for accent in Accent::ALL {
            assert_eq!(Accent::classify(accent.spacing()), Some(accent));
            assert_eq!(Accent::classify(accent.combining()), Some(accent));
        }
    }

    #[test]
    fn a_composed_character_splits_back_into_its_dead_key_and_base() {
        // What a receiver does when its layout has no single key for the
        // character: type the dead accent and then the letter, which is how
        // somebody sitting at that keyboard would.
        assert_eq!(decompose('é'), Some(('e', '\u{301}')));
        assert_eq!(decompose('ô'), Some(('o', '\u{302}')));
        assert_eq!(decompose('ñ'), Some(('n', '\u{303}')));
        assert_eq!(decompose('Ü'), Some(('U', '\u{308}')));
        assert_eq!(decompose('ç'), Some(('c', '\u{327}')));
    }

    #[test]
    fn a_character_that_is_already_a_base_letter_does_not_decompose() {
        for c in ['a', 'Z', '[', 'ß', '€'] {
            assert_eq!(decompose(c), None, "{c:?}");
        }
    }

    #[test]
    fn composition_and_decomposition_are_inverses() {
        // The two directions share one table, so a drift between them would mean
        // a receiver typing a different character from the one the sender
        // resolved.
        for accent in Accent::ALL {
            let (bases, _) = accent.table();
            for base in bases.chars() {
                let composed = compose(accent.combining(), &base.to_string());
                let one = composed.chars().next().unwrap();
                assert_eq!(
                    decompose(one),
                    Some((base, accent.combining())),
                    "{accent:?} + {base:?}"
                );
            }
        }
    }

    #[test]
    fn a_plain_letter_travels_as_text() {
        let mut kb = FakeKeyboard::new();
        let events = kb.press_text("a", Modifiers::NONE);
        assert_eq!(
            events,
            vec![KeyEvent::text("a", KeyAction::Press, Modifiers::NONE)]
        );
    }

    #[test]
    fn a_nordic_letter_survives_resolution_unchanged() {
        // The whole point of the crate: the sender's layout already produced this
        // character, so nothing here may reinterpret it.
        let mut kb = FakeKeyboard::new();
        assert_eq!(texts(&kb.press_text("å", Modifiers::NONE)), vec!["å"]);
        assert_eq!(texts(&kb.press_text("€", Modifiers::ALT_GR)), vec!["€"]);
    }

    #[test]
    fn dead_key_press_emits_nothing_until_a_base_letter_arrives() {
        let mut kb = FakeKeyboard::new();
        assert!(kb.press_dead('\u{00b4}').is_empty());
        assert!(kb.release_dead('\u{00b4}').is_empty());
        assert_eq!(texts(&kb.press_text("a", Modifiers::NONE)), vec!["á"]);
    }

    #[test]
    fn composition_yields_one_precomposed_codepoint_not_two() {
        // Two codepoints would render as two glyphs in some Windows apps, and the
        // receiver would type the accent as a separate keystroke.
        let mut kb = FakeKeyboard::new();
        kb.press_dead('^');
        let out = texts(&kb.press_text("o", Modifiers::NONE));
        assert_eq!(out, vec!["ô"]);
        assert_eq!(out[0].chars().count(), 1);
    }

    #[test]
    fn dead_key_then_space_types_the_bare_accent() {
        let mut kb = FakeKeyboard::new();
        kb.press_dead('~');
        assert_eq!(texts(&kb.press_text(" ", Modifiers::NONE)), vec!["~"]);
    }

    #[test]
    fn dead_key_with_no_precomposed_form_falls_back_to_the_combining_mark() {
        // `q` has no precomposed acute form; losing the accent silently would be
        // worse than emitting the decomposed sequence.
        let mut kb = FakeKeyboard::new();
        kb.press_dead('\u{00b4}');
        let out = texts(&kb.press_text("q", Modifiers::NONE));
        assert_eq!(out, vec!["q\u{0301}"]);
    }

    #[test]
    fn two_dead_keys_in_a_row_emit_the_first_as_a_standalone_accent() {
        let mut kb = FakeKeyboard::new();
        assert!(kb.press_dead('^').is_empty());
        assert_eq!(texts(&kb.press_dead('^')), vec!["^"]);
        // The second one is still pending and composes with what follows.
        assert_eq!(texts(&kb.press_text("e", Modifiers::NONE)), vec!["ê"]);
    }

    #[test]
    fn escape_cancels_a_pending_composition() {
        let mut kb = FakeKeyboard::new();
        kb.press_dead('\u{00b4}');
        let out = kb.press_special(SpecialKey::Escape, Modifiers::NONE);
        assert_eq!(
            out,
            vec![KeyEvent::special(
                SpecialKey::Escape,
                KeyAction::Press,
                Modifiers::NONE
            )]
        );
        assert!(!kb.resolver.has_pending_composition());
        // The accent is gone, so the next letter is unaccented.
        assert_eq!(texts(&kb.press_text("a", Modifiers::NONE)), vec!["a"]);
    }

    #[test]
    fn a_pending_accent_is_flushed_before_a_key_it_cannot_compose_with() {
        // An arrow, a function key, Enter: none of them composes, and the accent
        // the user typed is emitted rather than dropped, because losing input
        // silently is worse than an extra character.
        for key in [SpecialKey::Right, SpecialKey::F5, SpecialKey::Enter] {
            let mut kb = FakeKeyboard::new();
            kb.press_dead('~');
            let out = kb.press_special(key, Modifiers::NONE);
            assert_eq!(texts(&out), vec!["~"], "{key:?}");
            assert_eq!(out.last().unwrap().payload, KeyPayload::Special(key));
            assert!(!kb.resolver.has_pending_composition(), "{key:?}");
        }
    }

    #[test]
    fn a_chord_modifier_flushes_the_accent_rather_than_composing_onto_the_shortcut() {
        // Keeping it pending would compose the accent onto the shortcut's letter,
        // so the peer receives a character its layout cannot resolve carrying the
        // CTRL bit, and Save never fires. The stray `^` is the lesser harm.
        for modifier in [
            SpecialKey::CtrlLeft,
            SpecialKey::AltLeft,
            SpecialKey::SuperLeft,
        ] {
            let mut kb = FakeKeyboard::new();
            kb.press_dead('^');
            let out = kb.press_special(modifier, Modifiers::NONE);
            assert_eq!(texts(&out), vec!["^"], "{modifier:?}");
            assert_eq!(out.last().unwrap().payload, KeyPayload::Special(modifier));
            assert!(!kb.resolver.has_pending_composition(), "{modifier:?}");
        }

        // And the shortcut itself still arrives intact.
        let mut kb = FakeKeyboard::new();
        kb.press_dead('^');
        kb.press_special(SpecialKey::CtrlLeft, Modifiers::NONE);
        let save = kb.press_text("s", Modifiers::CTRL);
        assert_eq!(texts(&save), vec!["s"]);
        assert_eq!(save[0].modifiers, Modifiers::CTRL);
    }

    #[test]
    fn reaching_for_a_modifier_does_not_break_a_composition_in_progress() {
        // The AZERTY case: `^`, then Shift, then E is `Ê` on every desktop. Nothing
        // cancels a composition because the user reached for the shift that types
        // the letter it is going to compose with — and a level-3 shift arrives here
        // as `AltRight`, so an accent reached with AltGr would flush itself.
        for modifier in [SpecialKey::ShiftLeft, SpecialKey::AltRight] {
            let mut kb = FakeKeyboard::new();
            kb.press_dead('^');
            let out = kb.press_special(modifier, Modifiers::NONE);
            assert!(texts(&out).is_empty(), "{modifier:?} flushed the accent");
            assert_eq!(out.last().unwrap().payload, KeyPayload::Special(modifier));
            assert!(kb.resolver.has_pending_composition(), "{modifier:?}");
            assert_eq!(
                texts(&kb.press_text("E", Modifiers::SHIFT)),
                vec!["Ê"],
                "{modifier:?}"
            );
        }
    }

    #[test]
    fn a_release_carries_the_payload_its_press_resolved_to() {
        // Shift is released before the letter on real keyboards. Re-resolving at
        // release time would send `press A` then `release a`, and the receiver
        // would be left holding `A`.
        let mut kb = FakeKeyboard::new();
        let code = kb.code_for("shifted");
        let press =
            kb.resolver
                .resolve(&RawKey::text(code, "A", KeyAction::Press, Modifiers::SHIFT));
        assert_eq!(press[0].payload, KeyPayload::Text("A".into()));

        let release = kb.resolver.resolve(&RawKey::text(
            code,
            "a",
            KeyAction::Release,
            Modifiers::NONE,
        ));
        assert_eq!(release[0].payload, KeyPayload::Text("A".into()));
        assert_eq!(release[0].action, KeyAction::Release);
    }

    #[test]
    fn a_composed_release_carries_the_composed_text() {
        let mut kb = FakeKeyboard::new();
        kb.press_dead('\u{00b4}');
        kb.press_text("e", Modifiers::NONE);
        let release = kb.release_text("e", Modifiers::NONE);
        assert_eq!(release[0].payload, KeyPayload::Text("é".into()));
    }

    #[test]
    fn release_of_a_key_pressed_before_capture_started_is_still_forwarded() {
        // Capture can begin with a modifier already held. Swallowing its release
        // strands that modifier down on every peer.
        let mut kb = FakeKeyboard::new();
        let out = kb.release_special(SpecialKey::CtrlLeft, Modifiers::NONE);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].action, KeyAction::Release);
        assert_eq!(out[0].payload, KeyPayload::Special(SpecialKey::CtrlLeft));
    }

    #[test]
    fn release_of_an_unmappable_key_still_produces_an_event() {
        let mut kb = FakeKeyboard::new();
        let out = kb
            .resolver
            .resolve(&RawKey::unmapped(0xfe, KeyAction::Release, Modifiers::NONE));
        assert_eq!(out[0].payload, KeyPayload::RawKeyCode(0xfe));
    }

    #[test]
    fn every_held_key_is_released_on_handoff_so_none_stick() {
        let mut kb = FakeKeyboard::new();
        kb.press_special(SpecialKey::CtrlLeft, Modifiers::NONE);
        kb.press_special(SpecialKey::ShiftLeft, Modifiers::CTRL);
        kb.press_text("c", Modifiers::CTRL | Modifiers::SHIFT);

        let releases = kb.resolver.release_held();
        assert_eq!(releases.len(), 3);
        assert!(releases.iter().all(|e| e.action == KeyAction::Release));
        // Newest first, so Ctrl goes last: releasing it first would turn the
        // remaining releases into bare keys mid-chord.
        assert_eq!(releases[0].payload, KeyPayload::Text("c".into()));
        assert_eq!(
            releases[2].payload,
            KeyPayload::Special(SpecialKey::CtrlLeft)
        );
        assert_eq!(kb.resolver.held_count(), 0);
    }

    #[test]
    fn release_held_reports_nothing_twice() {
        let mut kb = FakeKeyboard::new();
        kb.press_text("x", Modifiers::NONE);
        assert_eq!(kb.resolver.release_held().len(), 1);
        assert!(kb.resolver.release_held().is_empty());
    }

    #[test]
    fn release_held_clears_a_pending_composition() {
        // Otherwise the accent reattaches to the first letter typed after control
        // returns, possibly minutes later.
        let mut kb = FakeKeyboard::new();
        kb.press_dead('^');
        assert!(kb.resolver.has_pending_composition());
        kb.resolver.release_held();
        assert!(!kb.resolver.has_pending_composition());
        assert_eq!(texts(&kb.press_text("a", Modifiers::NONE)), vec!["a"]);
    }

    #[test]
    fn auto_repeat_does_not_multiply_the_held_key_count() {
        let mut kb = FakeKeyboard::new();
        let code = kb.code_for("r");
        for action in [KeyAction::Press, KeyAction::Repeat, KeyAction::Repeat] {
            kb.resolver
                .resolve(&RawKey::text(code, "r", action, Modifiers::NONE));
        }
        assert_eq!(kb.resolver.held_count(), 1);
        assert_eq!(kb.resolver.release_held().len(), 1);
    }

    #[test]
    fn auto_repeat_is_forwarded_as_repeat_not_press() {
        // The receiver may prefer to generate its own repeats; it can only decide
        // that if the distinction survives.
        let mut kb = FakeKeyboard::new();
        let code = kb.code_for("r");
        kb.resolver
            .resolve(&RawKey::text(code, "r", KeyAction::Press, Modifiers::NONE));
        let out = kb
            .resolver
            .resolve(&RawKey::text(code, "r", KeyAction::Repeat, Modifiers::NONE));
        assert_eq!(out[0].action, KeyAction::Repeat);
    }

    #[test]
    fn repeated_dead_key_does_not_emit_a_burst_of_accents() {
        let mut kb = FakeKeyboard::new();
        let code = kb.code_for("dead");
        kb.resolver
            .resolve(&RawKey::dead(code, '^', KeyAction::Press, Modifiers::NONE));
        for _ in 0..5 {
            assert!(kb
                .resolver
                .resolve(&RawKey::dead(code, '^', KeyAction::Repeat, Modifiers::NONE))
                .is_empty());
        }
        assert_eq!(texts(&kb.press_text("a", Modifiers::NONE)), vec!["â"]);
    }

    #[test]
    fn special_keys_win_over_text_the_backend_also_reported() {
        // Enter reported as both `Enter` and "\r": injecting "\r" as a unicode
        // character does not activate a button or submit a form.
        let mut kb = FakeKeyboard::new();
        let raw = RawKey {
            code: 13,
            action: KeyAction::Press,
            modifiers: Modifiers::NONE,
            special: Some(SpecialKey::Enter),
            text: Some("\r".into()),
            dead: None,
        };
        let out = kb.resolver.resolve(&raw);
        assert_eq!(out[0].payload, KeyPayload::Special(SpecialKey::Enter));
    }

    #[test]
    fn control_characters_are_never_sent_as_text() {
        let mut kb = FakeKeyboard::new();
        for (text, expected) in [
            ("\r", SpecialKey::Enter),
            ("\t", SpecialKey::Tab),
            ("\u{8}", SpecialKey::Backspace),
            ("\u{1b}", SpecialKey::Escape),
            ("\u{7f}", SpecialKey::Delete),
        ] {
            let out = kb.press_text(text, Modifiers::NONE);
            assert_eq!(out[0].payload, KeyPayload::Special(expected), "{text:?}");
        }
    }

    #[test]
    fn ctrl_chords_recover_the_base_letter_a_backend_leaked_as_a_control_code() {
        // A backend that forgets to mask Ctrl before calling the OS resolver
        // yields 0x03 for Ctrl+C. Forwarding that types nothing on the receiver;
        // `Ctrl` plus `c` is what actually copies.
        let mut kb = FakeKeyboard::new();
        let out = kb.press_text("\u{3}", Modifiers::CTRL);
        assert_eq!(out[0].payload, KeyPayload::Text("c".into()));
        assert_eq!(out[0].modifiers, Modifiers::CTRL);
        assert_eq!(out[0].injection_modifiers(), Modifiers::CTRL);
    }

    #[test]
    fn an_unclassifiable_control_code_degrades_to_a_raw_keycode() {
        // Better a keycode some receivers can honour than a silently dropped key.
        let mut kb = FakeKeyboard::new();
        let out = kb.resolver.resolve(&RawKey::text(
            0x99,
            "\u{0}",
            KeyAction::Press,
            Modifiers::NONE,
        ));
        assert_eq!(out[0].payload, KeyPayload::RawKeyCode(0x99));
    }

    #[test]
    fn embedded_control_characters_are_stripped_from_longer_text() {
        let mut kb = FakeKeyboard::new();
        let out = kb.press_text("ab\u{0}c", Modifiers::NONE);
        assert_eq!(out[0].payload, KeyPayload::Text("abc".into()));
    }

    #[test]
    fn a_key_the_layout_maps_to_nothing_degrades_to_a_raw_keycode() {
        let mut kb = FakeKeyboard::new();
        let out = kb
            .resolver
            .resolve(&RawKey::unmapped(0x7b, KeyAction::Press, Modifiers::NONE));
        assert_eq!(out[0].payload, KeyPayload::RawKeyCode(0x7b));
    }

    #[test]
    fn ime_output_of_several_codepoints_travels_as_one_event() {
        let mut kb = FakeKeyboard::new();
        let out = kb.press_text("👍🏽", Modifiers::NONE);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload, KeyPayload::Text("👍🏽".into()));
    }

    #[test]
    fn composition_before_multi_codepoint_text_only_touches_the_first_char() {
        let mut kb = FakeKeyboard::new();
        kb.press_dead('\u{00b4}');
        assert_eq!(texts(&kb.press_text("ab", Modifiers::NONE)), vec!["áb"]);
    }

    #[test]
    fn reset_forgets_state_without_emitting_events() {
        let mut kb = FakeKeyboard::new();
        kb.press_special(SpecialKey::CtrlLeft, Modifiers::NONE);
        kb.press_dead('^');
        kb.resolver.reset();
        assert_eq!(kb.resolver.held_count(), 0);
        assert!(!kb.resolver.has_pending_composition());
        assert!(kb.resolver.release_held().is_empty());
    }

    #[test]
    fn a_realistic_norwegian_sentence_resolves_to_the_characters_typed() {
        // End-to-end through the fake source: the receiver's layout is irrelevant
        // because only these characters ever cross the wire.
        let mut kb = FakeKeyboard::new();
        let mut typed = String::new();
        for ch in ["b", "l", "å", "b", "æ", "r"] {
            for t in texts(&kb.press_text(ch, Modifiers::NONE)) {
                typed.push_str(&t);
            }
            kb.release_text(ch, Modifiers::NONE);
        }
        assert_eq!(typed, "blåbær");
        assert_eq!(kb.resolver.held_count(), 0);
    }

    #[test]
    fn modifier_presses_travel_as_their_own_events() {
        // An application can observe "Ctrl is down" with no other key involved.
        let mut kb = FakeKeyboard::new();
        let out = kb.press_special(SpecialKey::CtrlLeft, Modifiers::NONE);
        assert_eq!(out[0].payload, KeyPayload::Special(SpecialKey::CtrlLeft));
        assert_eq!(kb.resolver.held_count(), 1);
    }
}
