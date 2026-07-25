//! Device identity and the trust store.
//!
//! A node's identity is an ed25519 keypair generated once, on first run, and
//! never transmitted. The public key *is* the [`NodeId`], so identity survives
//! hostname changes, DHCP leases, and moving between networks — the things that
//! make IP- or name-based peer configuration rot.
//!
//! Authentication is deliberately not delegated to TLS. The TLS layer uses a
//! throwaway self-signed certificate (see [`crate::transport`]) because a
//! peer-to-peer mesh on a desk has no certificate authority to anchor trust in.
//! Instead the peer proves possession of the private key behind its advertised
//! `NodeId` by signing a nonce this side chose, inside the application
//! handshake. That check is the whole of peer authentication, so everything in
//! this module is on the critical security path.
//!
//! Signing a nonce alone is not enough, and the difference is the difference
//! between a secure design and a broken one. A nonce proves the peer is live and
//! holds the key; it says nothing about *which connection* the proof belongs to.
//! Because the certificate is not checked, a machine on the path can terminate
//! TLS twice — once towards each peer — and relay the `Hello`, `Welcome` and
//! `AuthProof` bytes verbatim. Both peers then verify a genuine signature over
//! their own fresh nonce and each believes it is talking to the other, while the
//! attacker holds both session keys and reads and rewrites every keystroke. The
//! nonce cannot detect that, because the attacker never has to forge anything.
//!
//! So every handshake signature also covers a [`ChannelBinding`]: keying material
//! exported from the TLS session the handshake is running over. A relay
//! necessarily has two distinct TLS sessions, so the value the signer bound to is
//! not the value the verifier computes, and the signature does not verify.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use wx_proto::{NodeId, Signature};

/// Domain-separation tag prefixed to every handshake signature.
///
/// Without a tag, a signature produced for one purpose is a valid signature for
/// any other protocol that also signs a bare 32-byte value — including a future
/// version of this one. The tag is versioned so that changing what gets signed
/// cannot make an old signature verify under new rules.
///
/// `v2` covers the channel binding as well as the nonce. The bump is what stops a
/// `v1` proof — which was bound to no connection at all — from being replayed
/// into a `v2` handshake by a relay.
pub const HANDSHAKE_CONTEXT: &[u8] = b"winxtend-handshake-v2";

/// File name of the persisted secret key, inside the config directory.
pub const KEY_FILE: &str = "identity.key";

/// File name of the persisted trust store, inside the config directory.
pub const TRUST_FILE: &str = "trusted-peers.toml";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("no per-user config directory is available on this platform")]
    NoConfigDir,
    #[error("reading or writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the stored key in {path} is not 32 bytes of hex")]
    MalformedKey { path: PathBuf },
    #[error("the trust store at {path} is not valid TOML: {source}")]
    MalformedStore {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "the trust store at {path} has an entry whose key is not a 32-byte hex node id: {key}"
    )]
    MalformedStoreEntry { path: PathBuf, key: String },
    #[error("serialising the trust store: {0}")]
    SerializeStore(toml::ser::Error),
    #[error("the operating system random number generator failed: {0}")]
    Random(getrandom::Error),
}

/// Keying material identifying one specific TLS session.
///
/// Produced by both ends of a connection from the TLS exporter (RFC 5705), which
/// guarantees two properties this design depends on: both peers of the *same*
/// session derive the same value without exchanging it, and no two sessions
/// derive the same value. Covering it by the handshake signature is what makes a
/// relayed proof useless — see the module documentation.
///
/// Never sent on the wire. Putting it in a message would let an attacker claim
/// whatever value made its relay work; the point is that each side computes it
/// locally from secrets only that session's endpoints have.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ChannelBinding([u8; 32]);

impl ChannelBinding {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

// Redacted: this is derived from the TLS session secrets, and a log line
// containing it plus a captured handshake is more than a reader should ever have.
impl fmt::Debug for ChannelBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChannelBinding(redacted)")
    }
}

/// This node's keypair.
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// Generate a fresh identity.
    ///
    /// Fills the seed straight from the OS CSPRNG rather than a userspace RNG:
    /// the seed is the single secret the whole security model rests on, and a
    /// seeded-from-time RNG would make every node on a freshly imaged fleet
    /// generate the same key.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(IdentityError::Random)?;
        Ok(Self::from_secret_bytes(&seed))
    }

    pub fn from_secret_bytes(secret: &[u8; 32]) -> Self {
        Self {
            signing: SigningKey::from_bytes(secret),
        }
    }

    /// The raw seed. Only for persisting; never send this anywhere.
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn node_id(&self) -> NodeId {
        NodeId(self.signing.verifying_key().to_bytes())
    }

    /// Sign a peer-chosen handshake nonce *on one specific TLS session*, proving
    /// possession of the private key behind [`Identity::node_id`].
    ///
    /// Both halves are required. The nonce stops a proof captured from an earlier
    /// connection being replayed; the binding stops a proof from being relayed
    /// live through a machine in the middle, which the nonce alone does not.
    pub fn sign_handshake(&self, binding: &ChannelBinding, nonce: &[u8; 32]) -> Signature {
        Signature(
            self.signing
                .sign(&handshake_message(binding, nonce))
                .to_bytes(),
        )
    }

    /// Load the persisted identity from `dir`, generating and saving one if there
    /// is none yet.
    ///
    /// First run and "user deleted the key file" are the same case on purpose:
    /// the node comes back with a new identity and has to be re-paired, which is
    /// the honest outcome — a silently reused stale key would be worse.
    pub fn load_or_create(dir: &Path) -> Result<Self, IdentityError> {
        let path = dir.join(KEY_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => {
                let bytes = hex::decode(text.trim())
                    .map_err(|_| IdentityError::MalformedKey { path: path.clone() })?;
                let seed: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| IdentityError::MalformedKey { path: path.clone() })?;
                Ok(Self::from_secret_bytes(&seed))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                identity.save(dir)?;
                Ok(identity)
            }
            Err(source) => Err(IdentityError::Io { path, source }),
        }
    }

    /// Persist the secret key into `dir`, creating the directory if needed.
    ///
    /// Stored as hex text rather than raw bytes so that a support request can
    /// include "does the file have 64 characters in it" without a hex editor,
    /// and so an editor cannot corrupt it by appending a newline.
    pub fn save(&self, dir: &Path) -> Result<(), IdentityError> {
        fs::create_dir_all(dir).map_err(|source| IdentityError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = dir.join(KEY_FILE);
        fs::write(&path, hex::encode(self.secret_bytes())).map_err(|source| IdentityError::Io {
            path: path.clone(),
            source,
        })?;
        restrict_to_owner(&path)?;
        Ok(())
    }
}

// Hand-written so that a stray `{:?}` in a log line cannot print the private
// key. `SigningKey`'s own Debug does render its bytes, so deriving here would
// silently turn any debug log of a session into a key disclosure.
impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", self.node_id())
    }
}

/// Tighten permissions on the key file so other users on the machine cannot read
/// it. A no-op on Windows, where the per-user config directory is already
/// ACL-restricted to the owning account.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<(), IdentityError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        IdentityError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<(), IdentityError> {
    Ok(())
}

/// Exactly the bytes that [`Identity::sign_handshake`] signs and
/// [`verify_handshake`] checks. Kept in one place so the two can never drift.
///
/// Both fields are fixed-width, so concatenation is unambiguous with no length
/// prefixes: there is no pair of inputs that produces the same byte string.
fn handshake_message(binding: &ChannelBinding, nonce: &[u8; 32]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(HANDSHAKE_CONTEXT.len() + 64);
    msg.extend_from_slice(HANDSHAKE_CONTEXT);
    msg.extend_from_slice(binding.as_bytes());
    msg.extend_from_slice(nonce);
    msg
}

/// Check that `sig` was produced by the private key behind `node`, over `nonce`,
/// on the TLS session `binding` was derived from.
///
/// Returns `false` for every failure, including a `NodeId` that is not a valid
/// curve point: a hostile peer controls those bytes, so an unparseable key must
/// be an ordinary rejection rather than an error path a caller might mishandle.
pub fn verify_handshake(
    node: &NodeId,
    binding: &ChannelBinding,
    nonce: &[u8; 32],
    sig: &Signature,
) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(&node.0) else {
        return false;
    };
    let sig = ed25519_dalek::Signature::from_bytes(&sig.0);
    // `verify_strict` rather than `verify`: it rejects small-order public keys,
    // which otherwise allow one signature to verify under several different
    // "identities" and would let a peer be impersonated by a key it does not own.
    key.verify_strict(&handshake_message(binding, nonce), &sig)
        .is_ok()
}

/// The per-user directory holding the key and the trust store.
pub fn default_config_dir() -> Result<PathBuf, IdentityError> {
    ProjectDirs::from("dev", "WinXtend", "WinXtend")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .ok_or(IdentityError::NoConfigDir)
}

/// A peer the user has paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    /// User-assigned label. Seeded from the peer's advertised name at pairing
    /// time, then editable, because hostnames like `DESKTOP-8K3QJ1` help nobody.
    pub name: String,
    /// Set when the user explicitly refuses a peer. Kept as an entry rather than
    /// deleted so that a blocked machine cannot get itself re-offered for
    /// pairing every time it reappears on the network.
    #[serde(default)]
    pub blocked: bool,
    /// Unix seconds when the peer was paired, for display in the UI.
    #[serde(default)]
    pub paired_at_unix: u64,
}

/// Paired peers, keyed by node id.
///
/// Persisted as TOML with hex node ids as table keys. Hex rather than raw bytes
/// because TOML keys are strings, and because a user editing this file by hand
/// needs to be able to match an entry against the id the UI shows them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default, rename = "peer")]
    peers: BTreeMap<String, TrustedPeer>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a peer as paired, or update the name of one already paired.
    ///
    /// Trusting clears any previous block: the user has just said yes to this
    /// machine, and leaving a stale `blocked` flag would make pairing appear to
    /// succeed while every connection was refused.
    pub fn trust(&mut self, node: NodeId, name: impl Into<String>) {
        self.peers.insert(
            node.to_hex(),
            TrustedPeer {
                name: name.into(),
                blocked: false,
                paired_at_unix: now_unix(),
            },
        );
    }

    pub fn block(&mut self, node: NodeId, name: impl Into<String>) {
        let entry = self.peers.entry(node.to_hex()).or_insert(TrustedPeer {
            name: name.into(),
            blocked: false,
            paired_at_unix: now_unix(),
        });
        entry.blocked = true;
    }

    /// Drop a peer entirely. Returns whether there was anything to drop.
    pub fn forget(&mut self, node: &NodeId) -> bool {
        self.peers.remove(&node.to_hex()).is_some()
    }

    /// Whether this peer may open a session. A blocked peer is *known* but not
    /// paired, so callers must never test membership as a proxy for this.
    pub fn is_paired(&self, node: &NodeId) -> bool {
        matches!(self.peers.get(&node.to_hex()), Some(p) if !p.blocked)
    }

    pub fn is_blocked(&self, node: &NodeId) -> bool {
        matches!(self.peers.get(&node.to_hex()), Some(p) if p.blocked)
    }

    pub fn name_of(&self, node: &NodeId) -> Option<&str> {
        self.peers.get(&node.to_hex()).map(|p| p.name.as_str())
    }

    /// Rename an already-known peer. Returns `false` if it is not known.
    pub fn rename(&mut self, node: &NodeId, name: impl Into<String>) -> bool {
        match self.peers.get_mut(&node.to_hex()) {
            Some(peer) => {
                peer.name = name.into();
                true
            }
            None => false,
        }
    }

    /// Every known peer, paired or blocked, in node-id order.
    pub fn peers(&self) -> impl Iterator<Item = (NodeId, &TrustedPeer)> {
        // Keys were validated on load and are only ever written by `to_hex`, so
        // a failure here means in-memory corruption; skipping is the safe choice
        // over panicking inside a UI listing.
        self.peers
            .iter()
            .filter_map(|(k, v)| NodeId::from_hex(k).ok().map(|id| (id, v)))
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Load from `dir`, returning an empty store when the file does not exist.
    ///
    /// A malformed store is an error rather than an empty store on purpose. The
    /// silent-empty behaviour would present as every peer suddenly being
    /// unpaired with no explanation, which is indistinguishable from an attack.
    pub fn load(dir: &Path) -> Result<Self, IdentityError> {
        let path = dir.join(TRUST_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(IdentityError::Io { path, source }),
        };
        let store: Self =
            toml::from_str(&text).map_err(|source| IdentityError::MalformedStore {
                path: path.clone(),
                source,
            })?;
        for key in store.peers.keys() {
            if NodeId::from_hex(key).is_err() {
                return Err(IdentityError::MalformedStoreEntry {
                    path,
                    key: key.clone(),
                });
            }
        }
        Ok(store)
    }

    pub fn save(&self, dir: &Path) -> Result<(), IdentityError> {
        fs::create_dir_all(dir).map_err(|source| IdentityError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = dir.join(TRUST_FILE);
        let text = toml::to_string_pretty(self).map_err(IdentityError::SerializeStore)?;
        fs::write(&path, text).map_err(|source| IdentityError::Io { path, source })
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wx-net-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn binding(tag: u8) -> ChannelBinding {
        ChannelBinding::from_bytes([tag; 32])
    }

    #[test]
    fn a_node_verifies_its_own_handshake_signature() {
        let id = Identity::generate().unwrap();
        let nonce = [42u8; 32];
        let b = binding(0xaa);
        assert!(verify_handshake(
            &id.node_id(),
            &b,
            &nonce,
            &id.sign_handshake(&b, &nonce)
        ));
    }

    #[test]
    fn a_signature_does_not_verify_against_a_different_nonce() {
        // This is what makes each handshake unreplayable: the verifier picks the
        // nonce, so a proof captured from an earlier connection is worthless.
        let id = Identity::generate().unwrap();
        let b = binding(0xaa);
        let sig = id.sign_handshake(&b, &[1u8; 32]);
        assert!(!verify_handshake(&id.node_id(), &b, &[2u8; 32], &sig));
    }

    #[test]
    fn a_signature_does_not_verify_on_a_different_tls_session() {
        // The relay attack: a man in the middle terminates TLS on both sides and
        // forwards the proof unchanged. The signature is genuine and the nonce is
        // the verifier's own, so only the channel binding can tell them apart.
        let id = Identity::generate().unwrap();
        let nonce = [7u8; 32];
        let sig = id.sign_handshake(&binding(0x01), &nonce);
        assert!(!verify_handshake(
            &id.node_id(),
            &binding(0x02),
            &nonce,
            &sig
        ));
    }

    #[test]
    fn a_single_flipped_bit_in_the_binding_refuses_the_handshake() {
        // Two TLS sessions differ in far more than one bit, but a signature that
        // survived any change to the binding would not be covering it at all.
        let id = Identity::generate().unwrap();
        let nonce = [3u8; 32];
        let mut bytes = [0x5au8; 32];
        let sig = id.sign_handshake(&ChannelBinding::from_bytes(bytes), &nonce);
        for byte in 0..32 {
            bytes[byte] ^= 0x01;
            assert!(
                !verify_handshake(
                    &id.node_id(),
                    &ChannelBinding::from_bytes(bytes),
                    &nonce,
                    &sig
                ),
                "byte {byte} of the channel binding is not covered by the signature"
            );
            bytes[byte] ^= 0x01;
        }
    }

    #[test]
    fn another_nodes_signature_is_rejected() {
        let alice = Identity::generate().unwrap();
        let bob = Identity::generate().unwrap();
        let nonce = [7u8; 32];
        let b = binding(0xbb);
        assert!(!verify_handshake(
            &alice.node_id(),
            &b,
            &nonce,
            &bob.sign_handshake(&b, &nonce)
        ));
    }

    #[test]
    fn a_tampered_signature_is_rejected() {
        let id = Identity::generate().unwrap();
        let nonce = [9u8; 32];
        let b = binding(0xcc);
        let mut sig = id.sign_handshake(&b, &nonce);
        sig.0[0] ^= 0x01;
        assert!(!verify_handshake(&id.node_id(), &b, &nonce, &sig));
    }

    #[test]
    fn hostile_node_id_bytes_are_rejected_without_panicking() {
        // NodeId comes straight off the wire, so these are attacker-chosen.
        let id = Identity::generate().unwrap();
        let nonce = [3u8; 32];
        let b = binding(0xdd);
        let sig = id.sign_handshake(&b, &nonce);
        for bytes in [[0u8; 32], [0xffu8; 32], [0x01u8; 32]] {
            assert!(!verify_handshake(&NodeId(bytes), &b, &nonce, &sig));
        }
    }

    #[test]
    fn signature_over_the_bare_nonce_does_not_verify() {
        // Guards the domain-separation tag: if the tag were dropped, a signature
        // from some other protocol that signs 32 raw bytes would authenticate a
        // WinXtend handshake.
        let id = Identity::generate().unwrap();
        let nonce = [5u8; 32];
        let untagged = Signature(id.signing.sign(&nonce).to_bytes());
        assert!(!verify_handshake(
            &id.node_id(),
            &binding(0xee),
            &nonce,
            &untagged
        ));
    }

    #[test]
    fn the_binding_and_the_nonce_are_not_interchangeable() {
        // Both are 32 bytes. If they were concatenated in the other order, or the
        // signature covered only their combination, swapping them would verify —
        // which would let a relay pick a binding that made its session work.
        let id = Identity::generate().unwrap();
        let sig = id.sign_handshake(&binding(0x11), &[0x22u8; 32]);
        assert!(!verify_handshake(
            &id.node_id(),
            &binding(0x22),
            &[0x11u8; 32],
            &sig
        ));
    }

    #[test]
    fn a_channel_binding_never_prints_its_bytes() {
        let b = ChannelBinding::from_bytes([0x7fu8; 32]);
        let rendered = format!("{b:?}");
        assert!(!rendered.contains("7f"), "{rendered}");
    }

    #[test]
    fn secret_bytes_round_trip_to_the_same_node_id() {
        let id = Identity::generate().unwrap();
        let restored = Identity::from_secret_bytes(&id.secret_bytes());
        assert_eq!(restored.node_id(), id.node_id());
    }

    #[test]
    fn identity_debug_never_prints_the_secret() {
        let id = Identity::generate().unwrap();
        let rendered = format!("{id:?}");
        assert!(
            !rendered.contains(&hex::encode(id.secret_bytes())),
            "secret key leaked into Debug output"
        );
    }

    #[test]
    fn identity_survives_a_restart() {
        let dir = temp_dir("identity-restart");
        let first = Identity::load_or_create(&dir).unwrap();
        let second = Identity::load_or_create(&dir).unwrap();
        assert_eq!(first.node_id(), second.node_id());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_key_file_is_an_error_not_a_new_identity() {
        // Silently regenerating would un-pair the node and look like an attack.
        let dir = temp_dir("identity-truncated");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(KEY_FILE), "abcd").unwrap();
        assert!(matches!(
            Identity::load_or_create(&dir),
            Err(IdentityError::MalformedKey { .. })
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_peer_is_not_paired() {
        let store = TrustStore::new();
        assert!(!store.is_paired(&NodeId([1u8; 32])));
        assert!(!store.is_blocked(&NodeId([1u8; 32])));
    }

    #[test]
    fn a_blocked_peer_is_known_but_not_paired() {
        let mut store = TrustStore::new();
        let node = NodeId([2u8; 32]);
        store.block(node, "junk laptop");
        assert!(store.is_blocked(&node));
        assert!(!store.is_paired(&node));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn trusting_a_blocked_peer_clears_the_block() {
        let mut store = TrustStore::new();
        let node = NodeId([3u8; 32]);
        store.block(node, "laptop");
        store.trust(node, "laptop");
        assert!(store.is_paired(&node));
        assert!(!store.is_blocked(&node));
    }

    #[test]
    fn forgetting_a_peer_reports_whether_it_was_known() {
        let mut store = TrustStore::new();
        let node = NodeId([4u8; 32]);
        assert!(!store.forget(&node));
        store.trust(node, "pi");
        assert!(store.forget(&node));
        assert!(!store.is_paired(&node));
    }

    #[test]
    fn trust_store_round_trips_through_disk() {
        let dir = temp_dir("trust-round-trip");
        let alice = NodeId([0xaa; 32]);
        let bob = NodeId([0xbb; 32]);

        let mut store = TrustStore::new();
        store.trust(alice, "owen-mac");
        store.block(bob, "unknown thing");
        store.save(&dir).unwrap();

        let loaded = TrustStore::load(&dir).unwrap();
        assert_eq!(loaded.name_of(&alice), Some("owen-mac"));
        assert!(loaded.is_paired(&alice));
        assert!(loaded.is_blocked(&bob));
        assert_eq!(loaded.peers().count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_trust_store_is_an_empty_store() {
        let dir = temp_dir("trust-missing");
        assert!(TrustStore::load(&dir).unwrap().is_empty());
    }

    #[test]
    fn a_corrupt_trust_store_is_an_error_not_an_empty_store() {
        let dir = temp_dir("trust-corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(TRUST_FILE), "this is not [ toml").unwrap();
        assert!(matches!(
            TrustStore::load(&dir),
            Err(IdentityError::MalformedStore { .. })
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_trust_store_entry_with_a_bad_node_id_is_rejected() {
        let dir = temp_dir("trust-bad-key");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(TRUST_FILE), "[peer.notahexnodeid]\nname = \"x\"\n").unwrap();
        assert!(matches!(
            TrustStore::load(&dir),
            Err(IdentityError::MalformedStoreEntry { .. })
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn renaming_only_affects_known_peers() {
        let mut store = TrustStore::new();
        let node = NodeId([6u8; 32]);
        assert!(!store.rename(&node, "nope"));
        store.trust(node, "old");
        assert!(store.rename(&node, "new"));
        assert_eq!(store.name_of(&node), Some("new"));
    }

    #[test]
    fn two_generated_identities_differ() {
        // A shared seed across a freshly imaged fleet would make every node the
        // same peer, so this is worth asserting rather than assuming.
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.node_id(), b.node_id());
    }
}
