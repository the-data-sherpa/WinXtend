//! Node capabilities and version negotiation.
//!
//! Nodes in a WinXtend mesh are not statically divided into masters and slaves.
//! A machine advertises what it *can* do and control flows to whoever currently
//! owns the cursor, so a laptop can drive a mini-PC in the morning and be driven
//! by a desktop in the afternoon with no config change. A headless Raspberry Pi
//! simply advertises input capture without injection or displays.

use serde::{Deserialize, Serialize};

/// Wire format version.
///
/// Bumped only for changes that older peers cannot parse. Additive changes —
/// appending an enum variant or a capability bit — do not require a bump,
/// because peers ignore capabilities they do not recognise and never send
/// variants the other side did not advertise support for.
pub const PROTOCOL_VERSION: u16 = 1;

/// Oldest version this build can still talk to.
pub const MIN_COMPATIBLE_VERSION: u16 = 1;

// A build whose floor is above its own version could not talk to anything, itself
// included. Checked at compile time rather than in a test, so the mistake cannot
// be committed in the first place.
const _: () = assert!(MIN_COMPATIBLE_VERSION <= PROTOCOL_VERSION);

/// What a node is able to do.
///
/// A bitset so that unknown future bits survive a round trip through an old
/// build untouched, rather than being dropped by a struct-shaped decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities(pub u32);

impl Capabilities {
    pub const NONE: Self = Self(0);
    /// Can capture local keyboard and mouse to drive other nodes.
    pub const CAPTURE_INPUT: Self = Self(1 << 0);
    /// Can inject received input into its own OS.
    pub const INJECT_INPUT: Self = Self(1 << 1);
    /// Has at least one display and participates in the layout.
    pub const HAS_DISPLAYS: Self = Self(1 << 2);
    pub const CLIPBOARD_TEXT: Self = Self(1 << 3);
    pub const CLIPBOARD_IMAGE: Self = Self(1 << 4);
    pub const FILE_TRANSFER: Self = Self(1 << 5);
    /// Can encode and send its screen as video.
    pub const VIDEO_SOURCE: Self = Self(1 << 6);
    /// Can decode and display a peer's screen.
    pub const VIDEO_SINK: Self = Self(1 << 7);
    /// Can lock its session when the controlling node's screensaver engages.
    pub const SCREENSAVER_SYNC: Self = Self(1 << 8);
    /// Runs with enough privilege to inject at the login/lock screen.
    pub const PRIVILEGED_INJECT: Self = Self(1 << 9);
    /// Can relay traffic between peers that cannot reach each other directly.
    pub const RELAY: Self = Self(1 << 10);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Capabilities both nodes share.
    ///
    /// The basis for every feature decision: clipboard images are only attempted
    /// when both ends advertise them, and so on.
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::ops::BitOr for Capabilities {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// Which OS a node runs, for display and for platform-specific workarounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
    Other,
}

/// Kind of display session, which decides what input backend is usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayServer {
    Windows,
    Quartz,
    X11,
    Wayland,
    /// No display server: a headless forwarder driving peers via evdev.
    Headless,
}

/// Result of comparing two peers' protocol versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCheck {
    Compatible,
    /// Peer speaks an older version than this build supports.
    PeerTooOld {
        peer: u16,
        min_supported: u16,
    },
    /// Peer speaks a newer version than this build understands.
    PeerTooNew {
        peer: u16,
        ours: u16,
    },
}

/// Decide whether a peer's protocol version can be spoken.
pub fn check_version(peer_version: u16) -> VersionCheck {
    if peer_version < MIN_COMPATIBLE_VERSION {
        VersionCheck::PeerTooOld {
            peer: peer_version,
            min_supported: MIN_COMPATIBLE_VERSION,
        }
    } else if peer_version > PROTOCOL_VERSION {
        VersionCheck::PeerTooNew {
            peer: peer_version,
            ours: PROTOCOL_VERSION,
        }
    } else {
        VersionCheck::Compatible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_bits_are_distinct() {
        let all = [
            Capabilities::CAPTURE_INPUT,
            Capabilities::INJECT_INPUT,
            Capabilities::HAS_DISPLAYS,
            Capabilities::CLIPBOARD_TEXT,
            Capabilities::CLIPBOARD_IMAGE,
            Capabilities::FILE_TRANSFER,
            Capabilities::VIDEO_SOURCE,
            Capabilities::VIDEO_SINK,
            Capabilities::SCREENSAVER_SYNC,
            Capabilities::PRIVILEGED_INJECT,
            Capabilities::RELAY,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_eq!(a.0 & b.0, 0, "capability bits {i} and {j} overlap");
                }
            }
        }
    }

    #[test]
    fn intersect_finds_the_shared_feature_set() {
        // A headless Pi captures input but has no screen and cannot show video.
        let pi = Capabilities::CAPTURE_INPUT | Capabilities::CLIPBOARD_TEXT;
        let mac = Capabilities::INJECT_INPUT
            | Capabilities::HAS_DISPLAYS
            | Capabilities::CLIPBOARD_TEXT
            | Capabilities::CLIPBOARD_IMAGE
            | Capabilities::VIDEO_SOURCE;

        let shared = pi.intersect(mac);
        assert!(shared.contains(Capabilities::CLIPBOARD_TEXT));
        assert!(!shared.contains(Capabilities::CLIPBOARD_IMAGE));
        assert!(!shared.contains(Capabilities::VIDEO_SOURCE));
    }

    #[test]
    fn unknown_future_bits_survive_a_round_trip() {
        // An old build must not silently clear bits it does not recognise, or
        // relayed handshakes would lose features between peers.
        let future = Capabilities(1 << 30);
        let bytes = postcard::to_allocvec(&future).unwrap();
        assert_eq!(
            postcard::from_bytes::<Capabilities>(&bytes).unwrap(),
            future
        );
    }

    #[test]
    fn same_version_is_compatible() {
        assert_eq!(check_version(PROTOCOL_VERSION), VersionCheck::Compatible);
    }

    #[test]
    fn newer_peer_is_rejected_with_both_versions() {
        assert_eq!(
            check_version(PROTOCOL_VERSION + 1),
            VersionCheck::PeerTooNew {
                peer: PROTOCOL_VERSION + 1,
                ours: PROTOCOL_VERSION,
            }
        );
    }

    #[test]
    fn version_zero_is_too_old() {
        assert_eq!(
            check_version(0),
            VersionCheck::PeerTooOld {
                peer: 0,
                min_supported: MIN_COMPATIBLE_VERSION,
            }
        );
    }
}
