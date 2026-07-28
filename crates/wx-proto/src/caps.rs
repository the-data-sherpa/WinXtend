//! Node capabilities and version negotiation.
//!
//! Nodes in a WinXtend mesh are not statically divided into masters and slaves.
//! A machine advertises what it *can* do and control flows to whoever currently
//! owns the cursor, so a laptop can drive a mini-PC in the morning and be driven
//! by a desktop in the afternoon with no config change. A headless Raspberry Pi
//! simply advertises input capture without injection or displays.

use serde::{Deserialize, Serialize};

/// Newest wire format this build speaks.
///
/// Bumped only for changes that older peers cannot parse. Additive changes —
/// appending an enum variant or a capability bit — do not require a bump,
/// because peers ignore capabilities they do not recognise and never send
/// variants the other side did not advertise support for.
///
/// This is the *top* of a range, not a single value: the build speaks every
/// version in `MIN_COMPATIBLE_VERSION..=PROTOCOL_VERSION`, and it is the number
/// each side advertises in [`crate::ControlMsg::Hello`] and
/// [`crate::ControlMsg::Welcome`] so that [`check_version`] can pick one.
pub const PROTOCOL_VERSION: u16 = 1;

/// Oldest wire format this build still speaks.
///
/// The bottom of that range, and a real promise rather than a politeness: raising
/// it says the code for every version below it is gone, so a peer down there is
/// refused outright instead of being misparsed.
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
    const NAMED: [(Self, &'static str); 12] = [
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
        (Self::CAPABILITY_UPDATES, "CAPABILITY_UPDATES"),
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

/// Outcome of negotiating a wire format with a peer.
///
/// There is no `PeerTooNew`: a peer advertising a version this build has never
/// heard of is not a refusal, it is a peer to negotiate *down* with. See
/// [`check_version`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCheck {
    /// A wire format both ends speak was found.
    Compatible {
        /// The version this connection runs at.
        ///
        /// Both machines compute this, independently, from the same two numbers,
        /// and arrive at the same answer — which is the whole reason negotiation
        /// needs no extra round trip and no extra message.
        effective: u16,
    },
    /// The peer's newest version is older than anything this build still speaks.
    PeerTooOld { peer: u16, min_supported: u16 },
}

/// Pick the wire format a connection will use.
///
/// # Why a newer peer is not refused
///
/// Refusing anything above [`PROTOCOL_VERSION`] means the *first* bump severs
/// interop: an updated machine and a not-yet-updated one could not connect at
/// all, rather than continuing at the older feature level. For a tool whose only
/// job is joining two machines, that is the worst available failure.
///
/// So a build advertises the top of a range it speaks and both ends settle on
/// `min(ours, theirs)` — the newest format neither has to guess at.
///
/// # Why `min` is safe to compute unilaterally
///
/// Negotiation that only one end honours is worse than a clean refusal: the
/// tolerant side would still be handed messages it cannot parse. That cannot
/// happen here. [`crate::ControlMsg::Hello`] and [`crate::ControlMsg::Welcome`]
/// both carry the sender's `PROTOCOL_VERSION`, so by the time either side calls
/// this it holds *both* numbers, and `min` is commutative. Neither end has to
/// announce the result or trust the other to have agreed — they cannot disagree.
///
/// # Why the floor survives
///
/// Only the top of each range crosses the wire, so this cannot see whether the
/// peer's own floor admits the negotiated version. It does not need to: each side
/// checks the result against its own [`MIN_COMPATIBLE_VERSION`], and a peer whose
/// floor is above what we offer refuses us with
/// [`crate::RejectReason::ProtocolTooOld`]. Every incompatible pairing still ends
/// in exactly one clear refusal naming a version number, from whichever end can
/// see the problem.
///
/// # What an "older effective version" does and does not buy you
///
/// It buys the wire *format* and nothing else. It does not make a newer peer's
/// extra messages harmless to send: postcard identifies enum variants by index
/// and is not self-describing, so a variant an older build has never seen is a
/// hard decode error, not something it can skip — and a control stream that fails
/// to decode is torn down.
/// `codec::tests::an_unknown_variant_is_a_hard_decode_failure` pins that.
///
/// Feature differences are therefore *not* this function's job; they are
/// [`Capabilities`]' job, which is already the policy at the top of this file and
/// is already enforced before send by `Engine::peer_supports` in
/// `crates/wx-agent/src/engine.rs`. [`Capabilities::CAPABILITY_UPDATES`] is the
/// worked example: appending [`crate::ControlMsg::CapabilitiesChanged`] was made
/// safe by a capability bit, not by a version bump.
///
/// The division that follows, and that a future bump must respect:
///
/// * **New feature, existing encoding** — add a capability bit, gate the send on
///   it, and *do not* bump. Nothing here changes.
/// * **Changed encoding of something already on the wire** — bump, keep the code
///   that emits the old encoding, and choose between them on `effective`. Drop
///   the old encoding only by raising [`MIN_COMPATIBLE_VERSION`] in the same
///   breath, which converts silent misparsing into the refusal above.
pub fn check_version(peer_version: u16) -> VersionCheck {
    negotiate(peer_version, PROTOCOL_VERSION, MIN_COMPATIBLE_VERSION)
}

/// [`check_version`] with this build's constants lifted into arguments.
///
/// Private, and it exists for one test: the claim above is that *two different
/// builds* agree on the effective version, and a test that can only instantiate
/// this build's constants cannot check that. Parameterising lets the test stand
/// up both ends of a mixed-version pair for real instead of re-deriving `min`
/// alongside the code it is meant to be checking.
fn negotiate(peer_version: u16, ours: u16, floor: u16) -> VersionCheck {
    if peer_version < floor {
        VersionCheck::PeerTooOld {
            peer: peer_version,
            min_supported: floor,
        }
    } else {
        VersionCheck::Compatible {
            effective: peer_version.min(ours),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bit this build defines, in one place.
    ///
    /// One list rather than one per test: the bug this guards against is a bit
    /// added to the impl and forgotten somewhere else, and a second copy of the
    /// list is just another place to forget it.
    const ALL: [Capabilities; 12] = [
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

    #[test]
    fn capability_bits_are_distinct() {
        let all = ALL;
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
        // which tells the user nothing about what their machine cannot do. The
        // fallback is there for a *peer* built against a newer protocol, so a bit
        // this build advertises itself reaching it is always a mistake.
        for cap in ALL {
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
        assert_eq!(
            check_version(PROTOCOL_VERSION),
            VersionCheck::Compatible {
                effective: PROTOCOL_VERSION
            }
        );
    }

    #[test]
    fn a_newer_peer_negotiates_down_rather_than_being_refused() {
        // The regression this whole module exists for: the first version bump must
        // not sever interop between an updated machine and a stale one. Both a
        // one-step and a far-future bump land on what this build can actually
        // speak, rather than on a refusal.
        for ahead in [1, 5, 1000] {
            assert_eq!(
                check_version(PROTOCOL_VERSION + ahead),
                VersionCheck::Compatible {
                    effective: PROTOCOL_VERSION
                },
                "a peer {ahead} versions ahead"
            );
        }
        assert_eq!(
            check_version(u16::MAX),
            VersionCheck::Compatible {
                effective: PROTOCOL_VERSION
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

    #[test]
    fn tolerance_for_newer_peers_did_not_remove_the_floor() {
        // Newer-peer tolerance is one-directional on purpose. Everything below the
        // floor is still refused, and named, however far below it sits.
        for peer in 0..MIN_COMPATIBLE_VERSION {
            assert_eq!(
                check_version(peer),
                VersionCheck::PeerTooOld {
                    peer,
                    min_supported: MIN_COMPATIBLE_VERSION,
                },
                "peer at version {peer}"
            );
        }
    }

    #[test]
    fn the_negotiated_version_is_one_this_build_can_actually_speak() {
        // "Tolerant" must never mean agreeing to a format we do not implement.
        for peer in 0..=64u16 {
            if let VersionCheck::Compatible { effective } = check_version(peer) {
                assert!(
                    (MIN_COMPATIBLE_VERSION..=PROTOCOL_VERSION).contains(&effective),
                    "negotiated {effective} with a peer at {peer}, outside \
                     {MIN_COMPATIBLE_VERSION}..={PROTOCOL_VERSION}"
                );
            }
        }
    }

    #[test]
    fn two_different_builds_never_disagree_about_the_effective_version() {
        // The property the design rests on. Each side runs this check against the
        // other's advertised version, alone, with no message announcing the
        // result — so if the two could ever land on different answers, the
        // tolerant end would go on to be handed messages it cannot parse, which
        // is worse than the refusal this replaces.
        //
        // Every mixed pair of builds in a small grid, rather than one example:
        // the failure would be an off-by-one at a range edge.
        for a_max in 1..=6u16 {
            for a_floor in 1..=a_max {
                for b_max in 1..=6u16 {
                    for b_floor in 1..=b_max {
                        let a_sees = negotiate(b_max, a_max, a_floor);
                        let b_sees = negotiate(a_max, b_max, b_floor);
                        let pair = format!(
                            "A {a_floor}..={a_max} vs B {b_floor}..={b_max}: \
                             {a_sees:?} / {b_sees:?}"
                        );

                        let (
                            VersionCheck::Compatible { effective: a },
                            VersionCheck::Compatible { effective: b },
                        ) = (a_sees, b_sees)
                        else {
                            // At least one end refuses. That is a clean outcome —
                            // it sends a Reject naming a version — so there is
                            // nothing to agree about.
                            continue;
                        };

                        assert_eq!(a, b, "ends disagree: {pair}");
                        // And the agreed version is one *both* implement, which is
                        // what makes proceeding safe rather than merely mutual.
                        assert!((a_floor..=a_max).contains(&a), "A cannot speak it: {pair}");
                        assert!((b_floor..=b_max).contains(&a), "B cannot speak it: {pair}");
                    }
                }
            }
        }
    }
}
