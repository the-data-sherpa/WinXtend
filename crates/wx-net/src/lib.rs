//! Transport, discovery, and pairing.
//!
//! QUIC carries both planes of the protocol over a single connection: unreliable
//! datagrams for pointer motion, reliable streams for control. See
//! [`wx_proto`](../wx_proto/index.html) for why that split is the whole point.
//!
//! Identity is an ed25519 keypair generated on first run. Peers are trusted on
//! first use after the user confirms a short code shown on both machines, so
//! there is no shared password sitting in a config file — the failure mode of
//! every KVM tool that ships one.
//!
//! Discovery is DNS-SD over mDNS, so machines find each other with no IP
//! addresses typed anywhere.
//!
//! # Where the security actually lives
//!
//! Worth stating plainly, because it is unusual and it looks wrong at a glance:
//!
//! 1. TLS provides the session key and encryption only. Certificates are
//!    self-signed throwaways and are not verified against anything — see
//!    [`transport`], which explains at length why that is not the hole it looks
//!    like.
//! 2. The peer is authenticated in the *application* handshake, by signing a
//!    nonce this side chose, together with keying material exported from the TLS
//!    session itself, using the key behind the [`wx_proto::NodeId`] it advertises
//!    ([`handshake`]). The nonce makes the proof unreplayable; the exported
//!    material makes it unrelayable, which a nonce alone does not — a machine in
//!    the middle can forward someone else's proof without forging anything.
//! 3. That key must already be in the trust store ([`identity::TrustStore`]),
//!    which is only written after a successful PIN exchange ([`pairing`]).
//! 4. Discovery is a hint and grants nothing ([`discovery`]).
//!
//! Steps 2 and 3 are pure functions over messages, so every hostile case has a
//! unit test rather than needing two machines and a packet capture.

pub mod discovery;
pub mod handshake;
pub mod identity;
pub mod pairing;
pub mod transport;

pub use discovery::{
    Advertiser, Browser, DiscoveredPeer, DiscoveryError, DiscoveryEvent, DiscoveryState, PeerTrust,
    ServiceRecord,
};
pub use handshake::{
    fresh_nonce, Established, Handshake, HandshakeError, Outcome, Party, Role, Step,
};
pub use identity::{
    default_config_dir, verify_handshake, ChannelBinding, Identity, IdentityError, TrustStore,
    TrustedPeer,
};
pub use pairing::{
    pairing_proof, verify_pairing_proof, PairingError, PairingSession, Pin, PIN_DIGITS,
};
pub use transport::{Endpoint, Events, Session, SessionEvent, SessionSetup, TransportError};

/// Fill an array from the OS CSPRNG.
///
/// Every secret in this crate — key seeds, handshake nonces, pairing PINs — comes
/// from here rather than a userspace RNG, so there is exactly one place to audit
/// and no chance of a time-seeded generator sneaking in behind a convenience API.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N], getrandom::Error> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_bytes_are_neither_zero_nor_repeated() {
        // A backend that silently no-opped would make every nonce and every key
        // identical, and nothing downstream would notice.
        let a = random_bytes::<32>().unwrap();
        assert_ne!(a, [0u8; 32]);
        assert_ne!(a, random_bytes::<32>().unwrap());
    }
}
