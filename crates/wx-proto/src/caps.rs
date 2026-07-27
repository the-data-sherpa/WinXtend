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
    /// Defined, but advertised by no backend: nothing implements the
    /// `FileTransfer*` half of the protocol, and `wx-platform`'s
    /// `no_backend_claims_a_capability_nothing_implements` keeps it that way.
    /// The bit itself stays. Reusing a bit index would read an older peer's
    /// advertisement as a capability it never claimed, so a capability that
    /// turns out to be unimplemented stops being advertised rather than being
    /// removed.
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
    /// Understands [`crate::ControlMsg::CapabilitiesChanged`].
    ///
    /// A statement about this build's wire implementation rather than about the
    /// machine it runs on, and the thing that makes appending that variant safe
    /// under the policy at the top of this file: peers ignore capability bits they
    /// do not recognise, so a build that predates the message simply never
    /// advertises this and is never sent it. Without the bit there would be no way
    /// to honour "never send a variant the other side did not advertise support
    /// for", and an older peer would meet a variant it cannot decode on a stream
    /// where a decode failure ends the session.
    pub const CAPABILITY_UPDATES: Self = Self(1 << 11);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Capabilities both nodes share.
    ///
    /// For reporting what two machines have in common. It is not where a feature
    /// is permitted or refused: that is done one machine at a time, by
    /// `Engine::peer_supports` in `crates/wx-agent/src/engine.rs`, so that the
    /// refusal can name which machine was missing which bit. A feature needing
    /// both ends — clipboard images, say — asks about each end in turn.
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Every bit this build knows, with the name used in logs and in the UI.
    ///
    /// Ordered by bit so that two machines' capability lists can be read side by
    /// side without re-sorting them in the reader's head.
    const NAMED: [(Self, &'static str); 11] = [
        (Self::CAPTURE_INPUT, "CAPTURE_INPUT"),
        (Self::INJECT_INPUT, "INJECT_INPUT"),
        (Self::HAS_DISPLAYS, "HAS_DISPLAYS"),
        (Self::CLIPBOARD_TEXT, "CLIPBOARD_TEXT"),
        (Self::CLIPBOARD_IMAGE, "CLIPBOARD_IMAGE"),
        (Self::FILE_TRANSFER, "FILE_TRANSFER"),
        (Self::VIDEO_SOURCE, "VIDEO_SOURCE"),
        (Self::VIDEO_SINK, "VIDEO_SINK"),
        (Self::SCREENSAVER_SYNC, "SCREENSAVER_SYNC"),
        (Self::PRIVILEGED_INJECT, "PRIVILEGED_INJECT"),
        (Self::RELAY, "RELAY"),
    ];

    /// Names of the bits that are set.
    ///
    /// A bit this build has never heard of is reported as `bit N` rather than
    /// dropped. A peer built against a newer protocol advertising something new is
    /// exactly the case where "I do not know what that is" beats silence: the whole
    /// reason these strings exist is so that a refusal, or a UI row, names what was
    /// actually claimed.
    pub fn names(self) -> Vec<String> {
        let mut out = Vec::new();
        for (bit, name) in Self::NAMED {
            if self.contains(bit) {
                out.push(name.to_string());
            }
        }
        let known = Self::NAMED.iter().fold(0u32, |acc, (bit, _)| acc | bit.0);
        for index in 0..u32::BITS {
            let bit = 1u32 << index;
            if self.0 & bit != 0 && known & bit == 0 {
                out.push(format!("bit {index}"));
            }
        }
        out
    }

    /// The set as one line of prose, for a log field or a status row.
    pub fn describe(self) -> String {
        let names = self.names();
        if names.is_empty() {
            "nothing".to_string()
        } else {
            names.join(", ")
        }
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
            Capabilities::CAPABILITY_UPDATES,
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
    fn every_known_bit_has_a_name() {
        // A bit added without a name would be reported as "bit N" in a refusal,
        // which tells the user nothing about what their machine cannot do.
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
        for cap in all {
            let names = cap.names();
            assert_eq!(names.len(), 1, "{cap:?} named {names:?}");
            assert!(!names[0].starts_with("bit "), "{cap:?} has no name");
        }
    }

    #[test]
    fn names_are_listed_lowest_bit_first() {
        let caps = Capabilities::SCREENSAVER_SYNC | Capabilities::CAPTURE_INPUT;
        assert_eq!(caps.names(), vec!["CAPTURE_INPUT", "SCREENSAVER_SYNC"]);
    }

    #[test]
    fn an_unknown_bit_is_reported_rather_than_dropped() {
        // A peer built against a newer protocol claims something this build has
        // never heard of. Showing the bit index is the honest answer; showing an
        // empty list would make the peer look less capable than it is.
        let future = Capabilities(1 << 30) | Capabilities::INJECT_INPUT;
        assert_eq!(future.names(), vec!["INJECT_INPUT", "bit 30"]);
    }

    #[test]
    fn an_empty_set_describes_itself_as_nothing() {
        // Used in log lines, where an empty string would read as a missing field.
        assert_eq!(Capabilities::NONE.describe(), "nothing");
        assert_eq!(
            (Capabilities::CAPTURE_INPUT | Capabilities::RELAY).describe(),
            "CAPTURE_INPUT, RELAY"
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
