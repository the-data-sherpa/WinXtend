//! On-disk configuration, and the zero-config defaults that make it optional.
//!
//! The design requirement here is that a fresh install works with **no config
//! file at all**: start the agent on two machines, they find each other over
//! mDNS, one offers to pair, and the mesh works. Everything in this module
//! therefore has a defensible default, and [`Config::load_or_default`] treats a
//! missing file as success.
//!
//! A file only appears once something has to persist across restarts: the layout
//! the user arranged, a renamed peer, a changed port. That is written by the
//! agent, not by the user, so the format is chosen to survive round-tripping
//! rather than to be pleasant to hand-write — though it is readable, and node ids
//! are stored as hex precisely so a human can match an entry against what the UI
//! shows.
//!
//! # What is deliberately *not* here
//!
//! The device keypair and the trust store. Those live in
//! [`wx_net::identity`] under the same config directory, because a secret and a
//! list of who is trusted must not be casually editable alongside cosmetic
//! preferences — and because losing this file must not silently un-pair every
//! machine on the desk.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use wx_proto::{
    GlobalMonitorId, KeyAction, KeyEvent, KeyPayload, Layout, Modifiers, MonitorId, NodeId,
    Placement, Rect, SpecialKey, DEFAULT_PORT,
};

/// File name inside the config directory.
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid configuration: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("serialising configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("no per-user configuration directory is available")]
    NoConfigDir,
}

/// Everything the agent persists.
///
/// Every field has `#[serde(default)]`, so a config file containing a single line
/// is valid and an older file keeps working when a new section is added. Unknown
/// keys are ignored rather than rejected: a newer build writing a field this one
/// does not know must not make the file unloadable, because the two builds share
/// one config directory on a machine mid-upgrade.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub node: NodeConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub hotkeys: Hotkeys,
    /// Layout the user arranged, if any.
    ///
    /// `None` on a fresh install, which is the signal for
    /// [`crate::autolayout`] to invent one. Kept as an `Option` rather than an
    /// empty layout so that "never configured" is distinguishable from
    /// "deliberately empty" — the first wants a guess, the second must not be
    /// overwritten by one.
    #[serde(default)]
    pub layout: Option<SavedLayout>,
    /// Per-peer settings, keyed by hex node id.
    ///
    /// Absent peers get [`PeerConfig::default`], so pairing a machine needs no
    /// entry here at all. An entry exists only once something diverges from the
    /// default.
    #[serde(default, rename = "peer")]
    pub peers: BTreeMap<String, PeerConfig>,
}

impl Config {
    /// Default location: `<config dir>/config.toml`, beside the identity key.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        wx_net::default_config_dir()
            .map(|dir| dir.join(CONFIG_FILE))
            .map_err(|_| ConfigError::NoConfigDir)
    }

    /// Load, treating "no file" as "all defaults".
    ///
    /// A malformed file is an error rather than a silent fall back to defaults:
    /// defaults would look like the agent had forgotten the user's layout and
    /// peer names, and the user would have no idea why. Better to refuse to start
    /// and say which line is wrong.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Write the config, creating the directory if needed.
    ///
    /// Written to a temporary file and renamed, so an interrupted write cannot
    /// leave a half-truncated file that refuses to parse on next start — which
    /// would take the user's whole layout with it.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|source| ConfigError::Write {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Settings for one peer, defaulted when it has no entry.
    pub fn peer(&self, node: &NodeId) -> PeerConfig {
        self.peers.get(&node.to_hex()).cloned().unwrap_or_default()
    }

    /// Mutable settings for one peer, creating the entry on demand.
    pub fn peer_mut(&mut self, node: &NodeId) -> &mut PeerConfig {
        self.peers.entry(node.to_hex()).or_default()
    }
}

/// Identity of this machine as presented to the mesh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Name shown to other machines and in the UI. Defaults to the hostname,
    /// which is a poor label but a familiar one; the user renames it once.
    #[serde(default = "default_node_name")]
    pub name: String,
    /// Whether the agent has been registered to start with the session.
    ///
    /// Recorded here as well as in the OS so that `--status` can report it
    /// without needing to read the registry, and so that an OS-level uninstall
    /// (a wiped profile, a reimaged machine) does not leave the config claiming
    /// something untrue for long — the agent reconciles it at startup.
    #[serde(default)]
    pub autostart: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: default_node_name(),
            autostart: false,
        }
    }
}

/// This machine's hostname, or a stable-looking placeholder.
///
/// Never fails: a node with no name is worse than a node called `winxtend-node`,
/// because the UI would show an empty row that the user cannot identify.
pub fn default_node_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "winxtend-node".to_string())
}

/// Transport and discovery settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// UDP port for the QUIC listener. One port, one firewall rule.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Address to bind. `0.0.0.0` by default because a KVM that only listens on
    /// loopback is useless, and binding a specific interface is the unusual case.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Announce on mDNS and browse for peers.
    ///
    /// On by default: this is the entire zero-config story. Turned off on
    /// networks where multicast is blocked or unwelcome, which then requires
    /// peers to be dialled by address.
    #[serde(default = "yes")]
    pub discovery: bool,
    /// Dial discovered peers that are already paired, without being asked.
    #[serde(default = "yes")]
    pub auto_connect: bool,
    /// Let an unpaired peer reach the pairing exchange.
    ///
    /// On by default, and it is narrower than it sounds: such a session can send
    /// nothing but pairing messages, the peer still has to prove possession of
    /// the key it advertises, and a human still has to confirm a six-digit PIN
    /// before anything is trusted. Off means pairing can only be started from
    /// this machine.
    #[serde(default = "yes")]
    pub accept_pairing_requests: bool,
    /// Static peer addresses, for networks with no working multicast.
    ///
    /// `host:port` strings, tried in addition to whatever discovery finds.
    #[serde(default)]
    pub extra_addresses: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            bind: default_bind(),
            discovery: true,
            auto_connect: true,
            accept_pairing_requests: true,
            extra_addresses: Vec::new(),
        }
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_bind() -> String {
    "0.0.0.0".to_string()
}

fn yes() -> bool {
    true
}

fn one() -> f32 {
    1.0
}

/// Per-peer overrides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Locally chosen label, overriding the name the peer advertises.
    #[serde(default)]
    pub name: Option<String>,
    /// Participate in the mesh at all. A paired-but-disabled peer stays in the
    /// trust store, so switching it back on needs no re-pairing.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Share the clipboard with this peer.
    #[serde(default = "yes")]
    pub clipboard: bool,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            name: None,
            enabled: true,
            clipboard: true,
        }
    }
}

/// A layout as persisted, and as exchanged with the UI.
///
/// Not [`wx_proto::Layout`] directly: that has a 32-byte array for each node id,
/// which serialises to a wall of integers in both TOML and JSON. Hex strings cost
/// one conversion and make both the config file and the IPC channel legible.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SavedLayout {
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub placements: Vec<SavedPlacement>,
}

/// One monitor's rectangle in the global virtual desktop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedPlacement {
    /// Owning node, hex encoded.
    pub node: String,
    pub monitor: u32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    #[serde(default = "one")]
    pub cursor_scale: f32,
}

impl SavedLayout {
    pub fn from_layout(layout: &Layout) -> Self {
        Self {
            revision: layout.revision,
            placements: layout
                .placements
                .iter()
                .map(|p| SavedPlacement {
                    node: p.monitor.node.to_hex(),
                    monitor: p.monitor.monitor.0,
                    x: p.global_bounds.x,
                    y: p.global_bounds.y,
                    w: p.global_bounds.w,
                    h: p.global_bounds.h,
                    cursor_scale: p.cursor_scale,
                })
                .collect(),
        }
    }

    /// Convert back, skipping entries whose node id is not valid hex.
    ///
    /// One hand-mangled line must not cost the user every other screen in the
    /// layout, so bad entries are dropped and logged rather than failing the
    /// whole load. The layout engine tolerates missing monitors — the cursor is
    /// rehomed — where it cannot tolerate a placement with no node.
    pub fn to_layout(&self) -> Layout {
        let mut placements = Vec::with_capacity(self.placements.len());
        for p in &self.placements {
            match NodeId::from_hex(&p.node) {
                Ok(node) => placements.push(Placement {
                    monitor: GlobalMonitorId::new(node, MonitorId(p.monitor)),
                    global_bounds: Rect::new(p.x, p.y, p.w, p.h),
                    cursor_scale: p.cursor_scale,
                }),
                Err(e) => tracing::warn!(
                    node = %p.node,
                    error = %e,
                    "dropping a saved placement with an unreadable node id"
                ),
            }
        }
        Layout {
            placements,
            revision: self.revision,
        }
    }
}

/// What a matched hotkey means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    /// Pin the cursor to the machine it is on, or release it.
    ToggleLock,
    /// Bring the cursor back to this machine from wherever it is.
    ReclaimCursor,
    /// Lock this session and every peer's, skipping machines that have not
    /// advertised they can. Runs the same path as [`crate::ipc::Request::LockAll`],
    /// which states the contract.
    LockAll,
}

/// Locally recognised chords, never forwarded to a peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hotkeys {
    /// Pin the cursor to the current machine. The one hotkey that has to exist:
    /// full-screen games and VMs are exactly where sliding off the edge onto
    /// another machine is never what the user meant.
    #[serde(default = "default_toggle_lock", with = "opt_hotkey")]
    pub toggle_lock: Option<Hotkey>,
    /// Recover control when the cursor is stranded on a machine that has stopped
    /// responding but has not yet been declared dead.
    #[serde(default = "default_reclaim", with = "opt_hotkey")]
    pub reclaim_cursor: Option<Hotkey>,
    /// Lock every machine on the desk at once.
    #[serde(default = "default_lock_all", with = "opt_hotkey")]
    pub lock_all: Option<Hotkey>,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            toggle_lock: default_toggle_lock(),
            reclaim_cursor: default_reclaim(),
            lock_all: default_lock_all(),
        }
    }
}

impl Hotkeys {
    /// Which action, if any, this key event triggers.
    ///
    /// Only a press counts. Matching a repeat would fire the action many times a
    /// second while the chord is held, and matching a release would fire it twice
    /// per use.
    pub fn action_for(&self, ev: &KeyEvent) -> Option<HotkeyAction> {
        if ev.action != KeyAction::Press {
            return None;
        }
        for (hotkey, action) in [
            (self.toggle_lock, HotkeyAction::ToggleLock),
            (self.reclaim_cursor, HotkeyAction::ReclaimCursor),
            (self.lock_all, HotkeyAction::LockAll),
        ] {
            if hotkey.is_some_and(|h| h.matches(ev)) {
                return Some(action);
            }
        }
        None
    }
}

fn default_toggle_lock() -> Option<Hotkey> {
    Hotkey::parse("ctrl+alt+super+l").ok()
}

fn default_reclaim() -> Option<Hotkey> {
    Hotkey::parse("ctrl+alt+super+home").ok()
}

fn default_lock_all() -> Option<Hotkey> {
    // Deliberately not Ctrl+Alt+Del: Windows reserves the secure attention
    // sequence and no low-level hook ever sees it, so binding it would produce a
    // hotkey that silently never fires.
    Hotkey::parse("ctrl+alt+super+k").ok()
}

/// A chord: some modifiers plus exactly one ordinary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: Modifiers,
    pub key: HotkeyKey,
}

/// The non-modifier half of a chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyKey {
    /// Matched against the text the sender's layout produced, case-insensitively.
    ///
    /// Text rather than a keycode for the same reason the protocol sends text:
    /// `l` is `l` on every layout, where a scancode is not.
    Char(char),
    Special(SpecialKey),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HotkeyParseError {
    #[error("a hotkey needs at least one key")]
    Empty,
    #[error("a hotkey needs exactly one non-modifier key, found {0} and {1}")]
    TooManyKeys(String, String),
    #[error("a hotkey needs a non-modifier key, not just modifiers")]
    ModifiersOnly,
    #[error("unrecognised key name {0}")]
    UnknownKey(String),
}

impl Hotkey {
    /// Parse `ctrl+alt+super+l`.
    ///
    /// Case-insensitive, tolerant of spaces, and accepts the aliases each
    /// platform's users expect (`win`, `cmd`, `option`, `return`) so a config
    /// copied between machines does not have to be rewritten.
    pub fn parse(s: &str) -> Result<Self, HotkeyParseError> {
        let mut mods = Modifiers::NONE;
        let mut key: Option<(String, HotkeyKey)> = None;

        for raw in s.split('+') {
            let token = raw.trim().to_ascii_lowercase();
            if token.is_empty() {
                continue;
            }
            if let Some(bit) = modifier_named(&token) {
                mods = mods.union(bit);
                continue;
            }
            let parsed = parse_key_token(&token)
                .ok_or_else(|| HotkeyParseError::UnknownKey(raw.trim().to_string()))?;
            if let Some((first, _)) = &key {
                return Err(HotkeyParseError::TooManyKeys(first.clone(), token));
            }
            key = Some((token, parsed));
        }

        match key {
            Some((_, key)) => Ok(Self { mods, key }),
            None if mods.is_empty() => Err(HotkeyParseError::Empty),
            None => Err(HotkeyParseError::ModifiersOnly),
        }
    }

    /// Whether this chord is the one being pressed.
    ///
    /// The modifier set must match *exactly*, so Ctrl+Alt+Super+L does not fire
    /// when the user is holding Shift as well — an inexact match would swallow
    /// keystrokes the user meant for the remote machine. Lock keys are excluded
    /// from the comparison because Caps Lock being on is not part of any chord,
    /// and requiring it off would make the hotkey mysteriously stop working.
    pub fn matches(&self, ev: &KeyEvent) -> bool {
        let seen = ev
            .modifiers
            .without(Modifiers::CAPS_LOCK)
            .without(Modifiers::NUM_LOCK);
        if seen != self.mods {
            return false;
        }
        match (&self.key, &ev.payload) {
            (HotkeyKey::Char(want), KeyPayload::Text(text)) => {
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => c.eq_ignore_ascii_case(want),
                    _ => false,
                }
            }
            (HotkeyKey::Special(want), KeyPayload::Special(got)) => want == got,
            _ => false,
        }
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bit, name) in MODIFIER_NAMES {
            if self.mods.contains(bit) {
                write!(f, "{name}+")?;
            }
        }
        match self.key {
            HotkeyKey::Char(' ') => f.write_str("space"),
            HotkeyKey::Char(c) => write!(f, "{c}"),
            HotkeyKey::Special(k) => f.write_str(special_name(k).unwrap_or("unknown")),
        }
    }
}

impl FromStr for Hotkey {
    type Err = HotkeyParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Hotkey::parse(&text).map_err(D::Error::custom)
    }
}

/// Serde shim so a hotkey can be switched off by writing an empty string.
///
/// TOML has no natural way to say "this binding is disabled" for a value that is
/// otherwise a string, and `toggle_lock = ""` is what a user will try.
mod opt_hotkey {
    use super::Hotkey;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Hotkey>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(h) => s.collect_str(h),
            None => s.serialize_str(""),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Hotkey>, D::Error> {
        let text = String::deserialize(d)?;
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        Hotkey::parse(trimmed).map(Some).map_err(D::Error::custom)
    }
}

/// Canonical modifier spellings, in the order they are printed.
const MODIFIER_NAMES: [(Modifiers, &str); 5] = [
    (Modifiers::CTRL, "ctrl"),
    (Modifiers::ALT, "alt"),
    (Modifiers::ALT_GR, "altgr"),
    (Modifiers::SHIFT, "shift"),
    (Modifiers::SUPER, "super"),
];

fn modifier_named(token: &str) -> Option<Modifiers> {
    Some(match token {
        "ctrl" | "control" => Modifiers::CTRL,
        "alt" | "option" | "opt" => Modifiers::ALT,
        "altgr" | "alt_gr" | "ralt" => Modifiers::ALT_GR,
        "shift" => Modifiers::SHIFT,
        "super" | "win" | "windows" | "cmd" | "command" | "meta" => Modifiers::SUPER,
        _ => return None,
    })
}

/// Named keys accepted in a hotkey, and how they are printed back.
///
/// Only keys a user might plausibly bind. Media keys are absent on purpose: a
/// hotkey that swallows Volume Up locally would be a surprising thing to
/// configure by accident.
const SPECIAL_NAMES: [(&str, SpecialKey); 26] = [
    ("escape", SpecialKey::Escape),
    ("backspace", SpecialKey::Backspace),
    ("tab", SpecialKey::Tab),
    ("enter", SpecialKey::Enter),
    ("delete", SpecialKey::Delete),
    ("insert", SpecialKey::Insert),
    ("home", SpecialKey::Home),
    ("end", SpecialKey::End),
    ("pageup", SpecialKey::PageUp),
    ("pagedown", SpecialKey::PageDown),
    ("up", SpecialKey::Up),
    ("down", SpecialKey::Down),
    ("left", SpecialKey::Left),
    ("right", SpecialKey::Right),
    ("f1", SpecialKey::F1),
    ("f2", SpecialKey::F2),
    ("f3", SpecialKey::F3),
    ("f4", SpecialKey::F4),
    ("f5", SpecialKey::F5),
    ("f6", SpecialKey::F6),
    ("f7", SpecialKey::F7),
    ("f8", SpecialKey::F8),
    ("f9", SpecialKey::F9),
    ("f10", SpecialKey::F10),
    ("f11", SpecialKey::F11),
    ("f12", SpecialKey::F12),
];

fn parse_key_token(token: &str) -> Option<HotkeyKey> {
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(HotkeyKey::Char(c.to_ascii_lowercase()));
    }
    let alias = match token {
        "esc" => "escape",
        "return" => "enter",
        "del" => "delete",
        "ins" => "insert",
        "pgup" | "page_up" => "pageup",
        "pgdn" | "pagedown" | "page_down" => "pagedown",
        "space" | "spacebar" => return Some(HotkeyKey::Char(' ')),
        other => other,
    };
    SPECIAL_NAMES
        .iter()
        .find(|(name, _)| *name == alias)
        .map(|(_, key)| HotkeyKey::Special(*key))
}

fn special_name(key: SpecialKey) -> Option<&'static str> {
    SPECIAL_NAMES
        .iter()
        .find(|(_, k)| *k == key)
        .map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wx_proto::{KeyEvent, MonitorId, NodeId};

    fn press(payload: KeyPayload, mods: Modifiers) -> KeyEvent {
        KeyEvent {
            payload,
            action: KeyAction::Press,
            modifiers: mods,
        }
    }

    fn chord() -> Modifiers {
        Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER
    }

    #[test]
    fn a_fresh_install_needs_no_file_to_be_usable() {
        // The zero-config claim, asserted rather than assumed: defaults alone
        // must produce a node that can be discovered, dialled, and paired.
        let c = Config::default();
        assert_eq!(c.network.port, DEFAULT_PORT);
        assert!(c.network.discovery, "peers could never be found");
        assert!(
            c.network.auto_connect,
            "a paired peer would never reconnect"
        );
        assert!(
            c.network.accept_pairing_requests,
            "no machine could ever offer to pair with this one"
        );
        assert!(
            !c.node.name.trim().is_empty(),
            "an unnamed node is unusable"
        );
        assert!(c.layout.is_none(), "a guessed layout must not be persisted");
        assert!(c.hotkeys.toggle_lock.is_some());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempdir("missing");
        let path = dir.join("nope.toml");
        assert_eq!(Config::load_or_default(&path).unwrap(), Config::default());
    }

    #[test]
    fn a_malformed_file_is_refused_rather_than_silently_defaulted() {
        // Falling back to defaults here would present as the agent having
        // forgotten the user's layout, with nothing on screen to explain it.
        let dir = tempdir("malformed");
        let path = dir.join("config.toml");
        std::fs::write(&path, "network = \"not a table\"\n").unwrap();
        assert!(matches!(
            Config::load_or_default(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn a_partial_file_keeps_the_defaults_for_everything_else() {
        let dir = tempdir("partial");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[network]\nport = 9999\n").unwrap();
        let c = Config::load_or_default(&path).unwrap();
        assert_eq!(c.network.port, 9999);
        assert!(c.network.discovery);
        assert_eq!(c.node.name, default_node_name());
    }

    #[test]
    fn unknown_keys_are_ignored_so_a_newer_build_can_share_the_file() {
        let dir = tempdir("unknown");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[network]\nport = 1\nquantum_mode = true\n").unwrap();
        assert_eq!(Config::load_or_default(&path).unwrap().network.port, 1);
    }

    #[test]
    fn config_round_trips_through_the_file() {
        let dir = tempdir("roundtrip");
        let path = dir.join("config.toml");

        let mut c = Config::default();
        c.node.name = "owen-desktop".into();
        c.node.autostart = true;
        c.network.port = 24801;
        c.network.extra_addresses = vec!["10.0.0.5:24800".into()];
        c.hotkeys.lock_all = None;
        c.layout = Some(SavedLayout {
            revision: 4,
            placements: vec![SavedPlacement {
                node: NodeId([7u8; 32]).to_hex(),
                monitor: 2,
                x: -1920,
                y: 40,
                w: 1920,
                h: 1080,
                cursor_scale: 1.25,
            }],
        });
        c.peer_mut(&NodeId([9u8; 32])).name = Some("mac mini".into());
        c.peer_mut(&NodeId([9u8; 32])).enabled = false;

        c.save(&path).unwrap();
        assert_eq!(Config::load_or_default(&path).unwrap(), c);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = tempdir("atomic");
        let path = dir.join("config.toml");
        Config::default().save(&path).unwrap();
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn an_absent_peer_still_has_working_settings() {
        // Pairing must not require a config entry, or the zero-config path breaks
        // the moment a second machine appears.
        let p = Config::default().peer(&NodeId([1u8; 32]));
        assert!(p.enabled);
        assert!(p.clipboard);
        assert!(p.name.is_none());
    }

    #[test]
    fn saved_layout_round_trips_through_the_protocol_type() {
        let layout = Layout {
            revision: 11,
            placements: vec![
                Placement {
                    monitor: GlobalMonitorId::new(NodeId([1u8; 32]), MonitorId(0)),
                    global_bounds: Rect::new(0, 0, 1920, 1080),
                    cursor_scale: 1.0,
                },
                Placement {
                    monitor: GlobalMonitorId::new(NodeId([2u8; 32]), MonitorId(3)),
                    global_bounds: Rect::new(1920, -180, 2560, 1440),
                    cursor_scale: 1.4,
                },
            ],
        };
        assert_eq!(SavedLayout::from_layout(&layout).to_layout(), layout);
    }

    #[test]
    fn one_unreadable_placement_does_not_discard_the_others() {
        let saved = SavedLayout {
            revision: 2,
            placements: vec![
                SavedPlacement {
                    node: "not hex".into(),
                    monitor: 0,
                    x: 0,
                    y: 0,
                    w: 100,
                    h: 100,
                    cursor_scale: 1.0,
                },
                SavedPlacement {
                    node: NodeId([3u8; 32]).to_hex(),
                    monitor: 1,
                    x: 0,
                    y: 0,
                    w: 800,
                    h: 600,
                    cursor_scale: 1.0,
                },
            ],
        };
        let layout = saved.to_layout();
        assert_eq!(layout.placements.len(), 1);
        assert_eq!(layout.placements[0].monitor.monitor, MonitorId(1));
        assert_eq!(layout.revision, 2);
    }

    #[test]
    fn hotkeys_parse_and_print_the_same_way_round() {
        for text in [
            "ctrl+alt+super+l",
            "ctrl+shift+f5",
            "alt+home",
            "super+space",
        ] {
            let h = Hotkey::parse(text).unwrap();
            assert_eq!(Hotkey::parse(&h.to_string()).unwrap(), h, "{text}");
        }
    }

    #[test]
    fn hotkey_parsing_accepts_the_aliases_each_platform_uses() {
        let win = Hotkey::parse("Control+Windows+ESC").unwrap();
        let mac = Hotkey::parse("ctrl+cmd+escape").unwrap();
        assert_eq!(win, mac);
        assert_eq!(win.key, HotkeyKey::Special(SpecialKey::Escape));
    }

    #[test]
    fn hotkey_parsing_rejects_the_ambiguous_cases() {
        assert_eq!(Hotkey::parse(""), Err(HotkeyParseError::Empty));
        assert_eq!(
            Hotkey::parse("ctrl+alt"),
            Err(HotkeyParseError::ModifiersOnly)
        );
        assert!(matches!(
            Hotkey::parse("ctrl+a+b"),
            Err(HotkeyParseError::TooManyKeys(..))
        ));
        assert!(matches!(
            Hotkey::parse("ctrl+nonsense"),
            Err(HotkeyParseError::UnknownKey(_))
        ));
    }

    #[test]
    fn the_lock_chord_fires_on_the_key_it_is_bound_to() {
        let keys = Hotkeys::default();
        let ev = press(KeyPayload::Text("l".into()), chord());
        assert_eq!(keys.action_for(&ev), Some(HotkeyAction::ToggleLock));
    }

    #[test]
    fn a_shifted_capital_still_matches_a_lowercase_binding() {
        // Text arrives already resolved through the sender's layout, so holding
        // Shift turns `l` into `L`; the chord must still be recognised.
        let keys = Hotkeys::default();
        let ev = press(KeyPayload::Text("L".into()), chord() | Modifiers::SHIFT);
        // Shift is not part of the binding, so this must NOT fire.
        assert_eq!(keys.action_for(&ev), None);

        let bound = Hotkey::parse("ctrl+shift+l").unwrap();
        assert!(bound.matches(&press(
            KeyPayload::Text("L".into()),
            Modifiers::CTRL | Modifiers::SHIFT
        )));
    }

    #[test]
    fn an_extra_modifier_does_not_trigger_a_hotkey() {
        // An inexact match would swallow keystrokes meant for the remote machine.
        let keys = Hotkeys::default();
        let ev = press(KeyPayload::Text("l".into()), chord() | Modifiers::SHIFT);
        assert_eq!(keys.action_for(&ev), None);
    }

    #[test]
    fn caps_lock_being_on_does_not_break_a_hotkey() {
        let keys = Hotkeys::default();
        let ev = press(KeyPayload::Text("l".into()), chord() | Modifiers::CAPS_LOCK);
        assert_eq!(keys.action_for(&ev), Some(HotkeyAction::ToggleLock));
    }

    #[test]
    fn only_a_press_fires_a_hotkey() {
        // A repeat would toggle the lock dozens of times a second while held.
        let keys = Hotkeys::default();
        for action in [KeyAction::Release, KeyAction::Repeat] {
            let ev = KeyEvent {
                payload: KeyPayload::Text("l".into()),
                action,
                modifiers: chord(),
            };
            assert_eq!(keys.action_for(&ev), None, "{action:?}");
        }
    }

    #[test]
    fn a_special_key_binding_does_not_match_a_text_event() {
        let home = Hotkey::parse("ctrl+alt+super+home").unwrap();
        assert!(!home.matches(&press(KeyPayload::Text("home".into()), chord())));
        assert!(home.matches(&press(KeyPayload::Special(SpecialKey::Home), chord())));
    }

    #[test]
    fn multi_character_text_never_matches_a_single_key_binding() {
        // An IME or emoji payload is not a chord, and must be forwarded.
        let keys = Hotkeys::default();
        assert_eq!(
            keys.action_for(&press(KeyPayload::Text("lo".into()), chord())),
            None
        );
        assert_eq!(
            keys.action_for(&press(KeyPayload::Text(String::new()), chord())),
            None
        );
    }

    #[test]
    fn a_hotkey_can_be_switched_off_in_the_file() {
        let dir = tempdir("disable");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[hotkeys]\ntoggle_lock = \"\"\nlock_all = \"none\"\n",
        )
        .unwrap();
        let c = Config::load_or_default(&path).unwrap();
        assert!(c.hotkeys.toggle_lock.is_none());
        assert!(c.hotkeys.lock_all.is_none());
        // Untouched bindings keep their defaults.
        assert_eq!(c.hotkeys.reclaim_cursor, default_reclaim());
    }

    #[test]
    fn a_disabled_hotkey_survives_a_save_and_reload() {
        let dir = tempdir("disable-roundtrip");
        let path = dir.join("config.toml");
        let mut c = Config::default();
        c.hotkeys.toggle_lock = None;
        c.save(&path).unwrap();
        assert!(Config::load_or_default(&path)
            .unwrap()
            .hotkeys
            .toggle_lock
            .is_none());
    }

    /// A scratch directory that does not collide with other tests in the binary.
    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wx-agent-config-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
