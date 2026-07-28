//! Trust-on-first-use pairing with a short PIN.
//!
//! Pairing exists because there is no CA and no pre-shared secret: the first time
//! two machines meet, something has to convince each of them that the key the
//! other advertises belongs to a machine the user actually owns. A six-digit PIN
//! shown on both screens is the strongest thing a user will reliably do, so the
//! job here is to get the most out of it.
//!
//! Six digits is only about twenty bits, so the PIN must never be usable outside
//! the connection it was typed for:
//!
//! * The proof hashes the PIN together with **both** handshake nonces. Those are
//!   fresh per connection, so a proof captured off the wire is worthless on any
//!   other connection — there is nothing to record and replay later.
//! * The proof is also tagged with the prover's role, so the value one side sends
//!   is not the value the other side owes. Without that, a man in the middle
//!   could reflect a proof straight back and be accepted by its author.
//! * Verification is constant time. A comparison that returns on the first wrong
//!   byte lets an attacker learn a correct prefix, which turns twenty bits of
//!   guessing into about sixty tries.
//!
//! What this does *not* do is make the PIN a key: it authenticates the pairing
//! exchange only. Confidentiality comes from the QUIC/TLS session underneath.

use core::fmt;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use wx_proto::ControlMsg;

use crate::handshake::{Established, Role};

/// Digits in a pairing PIN.
///
/// Six is a deliberate compromise: long enough that an online guess against a
/// single connection is unlikely, short enough that a user reads it off one
/// screen and types it on another without a mistake.
pub const PIN_DIGITS: usize = 6;

/// Domain-separation tag for pairing proofs, kept distinct from the handshake
/// signature context so neither construction can ever be fed the other's bytes.
pub const PAIRING_CONTEXT: &[u8] = b"winxtend-pairing-v1";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PairingError {
    #[error("a pairing PIN is exactly {PIN_DIGITS} digits, got {got}")]
    WrongLength { got: usize },
    /// Rejects anything that is not `0`-`9` ASCII, including non-Latin digit
    /// characters: those look like digits to the user on one machine and hash to
    /// different bytes on the other, which would present as "the right PIN does
    /// not work".
    #[error("a pairing PIN may only contain the ASCII digits 0-9")]
    NotAsciiDigits,
    #[error("expected a PairConfirm message")]
    NotAPairConfirm,
    #[error("the operating system random number generator failed: {0}")]
    Random(getrandom::Error),
}

/// A pairing PIN: exactly [`PIN_DIGITS`] ASCII digits.
#[derive(Clone, PartialEq, Eq)]
pub struct Pin([u8; PIN_DIGITS]);

impl Pin {
    /// A fresh PIN from the OS CSPRNG.
    pub fn generate() -> Result<Self, PairingError> {
        let mut digits = [0u8; PIN_DIGITS];
        let mut filled = 0;
        while filled < PIN_DIGITS {
            let mut buf = [0u8; PIN_DIGITS * 2];
            getrandom::fill(&mut buf).map_err(PairingError::Random)?;
            for byte in buf {
                if filled == PIN_DIGITS {
                    break;
                }
                // Rejection sampling above 249 (25 * 10). Taking `byte % 10`
                // over the whole range would make 0-5 appear more often than
                // 6-9, shaving entropy off an already short secret.
                if byte < 250 {
                    digits[filled] = b'0' + (byte % 10);
                    filled += 1;
                }
            }
        }
        Ok(Self(digits))
    }

    pub fn parse(s: &str) -> Result<Self, PairingError> {
        let bytes = s.as_bytes();
        if bytes.len() != PIN_DIGITS {
            return Err(PairingError::WrongLength {
                got: s.chars().count(),
            });
        }
        if !bytes.iter().all(u8::is_ascii_digit) {
            return Err(PairingError::NotAsciiDigits);
        }
        let mut digits = [0u8; PIN_DIGITS];
        digits.copy_from_slice(bytes);
        Ok(Self(digits))
    }

    pub fn as_str(&self) -> &str {
        // The only constructors enforce ASCII digits.
        core::str::from_utf8(&self.0).expect("a Pin is always ASCII digits")
    }
}

impl fmt::Display for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// Redacted, because pairing runs with tracing turned up and a PIN logged next to
// the two nonces it is hashed with is the one combination that would let a reader
// of the log finish the pairing themselves.
impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pin(******)")
    }
}

/// Derive the proof that `prover` owes, for one specific connection.
///
/// The two nonces are passed in role order rather than local/remote order: both
/// machines must hash the same byte string, and "who dialled" is the only
/// ordering they already agree on. Getting this wrong does not weaken anything —
/// it simply makes pairing fail every time, with no clue why.
pub fn pairing_proof(
    pin: &Pin,
    initiator_nonce: &[u8; 32],
    responder_nonce: &[u8; 32],
    prover: Role,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PAIRING_CONTEXT);
    hasher.update([prover.tag()]);
    // Length-prefix the PIN so that a future longer secret cannot produce the
    // same hash input as a short one followed by the start of a nonce.
    hasher.update([PIN_DIGITS as u8]);
    hasher.update(pin.as_str().as_bytes());
    hasher.update(initiator_nonce);
    hasher.update(responder_nonce);
    hasher.finalize().into()
}

/// Check an offered proof against the one `prover` should have produced.
pub fn verify_pairing_proof(
    pin: &Pin,
    initiator_nonce: &[u8; 32],
    responder_nonce: &[u8; 32],
    prover: Role,
    offered: &[u8; 32],
) -> bool {
    let expected = pairing_proof(pin, initiator_nonce, responder_nonce, prover);
    constant_time_eq(&expected, offered)
}

/// Compare two byte slices without revealing where they first differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Length is not a secret here: both sides are fixed-size hashes.
    a.len() == b.len() && fold_difference(a.iter().copied().zip(b.iter().copied()))
}

/// Accumulate the difference across *every* pair of bytes, then make a single
/// branch on the aggregate.
///
/// Written over an iterator specifically so a test can prove no pair is skipped.
/// Returning early on the first mismatch would leak the length of a correct
/// prefix, and a six-digit PIN falls to roughly sixty guesses once prefixes leak.
fn fold_difference<I: Iterator<Item = (u8, u8)>>(pairs: I) -> bool {
    let mut diff = 0u8;
    for (x, y) in pairs {
        diff |= x ^ y;
    }
    // `subtle`'s comparison keeps the optimiser from reintroducing a branch on
    // the accumulated value; either way the branch is on the whole difference
    // rather than on any byte position.
    bool::from(diff.ct_eq(&0))
}

/// One side of a pairing exchange, bound to one established handshake.
///
/// Holding the nonces from the handshake is what ties the PIN to this connection.
/// Constructing this from anything else would reintroduce the replay the design
/// exists to prevent, which is why the only constructor takes an [`Established`].
#[derive(Debug)]
pub struct PairingSession {
    role: Role,
    pin: Pin,
    initiator_nonce: [u8; 32],
    responder_nonce: [u8; 32],
}

impl PairingSession {
    pub fn new(established: &Established, pin: Pin) -> Self {
        Self {
            role: established.role,
            pin,
            initiator_nonce: established.initiator_nonce(),
            responder_nonce: established.responder_nonce(),
        }
    }

    pub fn pin(&self) -> &Pin {
        &self.pin
    }

    /// The message this side sends to prove it knows the PIN.
    pub fn confirm(&self) -> ControlMsg {
        ControlMsg::PairConfirm {
            code_proof: pairing_proof(
                &self.pin,
                &self.initiator_nonce,
                &self.responder_nonce,
                self.role,
            ),
        }
    }

    /// Check the peer's `PairConfirm`.
    ///
    /// `Ok(false)` means the user typed a different PIN — a
    /// [`wx_proto::RejectReason::BadPairingCode`] answer. `Err` means the peer
    /// sent something else entirely, which is a protocol violation.
    pub fn accepts(&self, msg: &ControlMsg) -> Result<bool, PairingError> {
        let ControlMsg::PairConfirm { code_proof } = msg else {
            return Err(PairingError::NotAPairConfirm);
        };
        Ok(verify_pairing_proof(
            &self.pin,
            &self.initiator_nonce,
            &self.responder_nonce,
            self.role.peer(),
            code_proof,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use wx_proto::{Capabilities, DisplayServer, NodeId, NodeInfo, Platform};

    fn info(id: NodeId) -> NodeInfo {
        NodeInfo {
            id,
            name: "peer".into(),
            platform: Platform::Linux,
            display_server: DisplayServer::Wayland,
            capabilities: Capabilities::NONE,
            monitors: Vec::new(),
            agent_version: "0.1.0".into(),
        }
    }

    /// The two `Established` views of one connection, one per machine.
    fn connection(nonce_i: [u8; 32], nonce_r: [u8; 32]) -> (Established, Established) {
        let initiator = Established {
            role: Role::Initiator,
            peer: info(NodeId([2u8; 32])),
            protocol: wx_proto::PROTOCOL_VERSION,
            peer_protocol: wx_proto::PROTOCOL_VERSION,
            local_nonce: nonce_i,
            peer_nonce: nonce_r,
            peer_was_paired: false,
        };
        let responder = Established {
            role: Role::Responder,
            peer: info(NodeId([1u8; 32])),
            protocol: wx_proto::PROTOCOL_VERSION,
            peer_protocol: wx_proto::PROTOCOL_VERSION,
            local_nonce: nonce_r,
            peer_nonce: nonce_i,
            peer_was_paired: false,
        };
        (initiator, responder)
    }

    #[test]
    fn matching_pins_pair_successfully() {
        let (a, b) = connection([1u8; 32], [2u8; 32]);
        let pin = Pin::parse("314159").unwrap();
        let side_a = PairingSession::new(&a, pin.clone());
        let side_b = PairingSession::new(&b, pin);
        assert!(side_b.accepts(&side_a.confirm()).unwrap());
        assert!(side_a.accepts(&side_b.confirm()).unwrap());
    }

    #[test]
    fn a_wrong_pin_is_rejected() {
        let (a, b) = connection([1u8; 32], [2u8; 32]);
        let side_a = PairingSession::new(&a, Pin::parse("314159").unwrap());
        let side_b = PairingSession::new(&b, Pin::parse("314158").unwrap());
        assert!(!side_b.accepts(&side_a.confirm()).unwrap());
        assert!(!side_a.accepts(&side_b.confirm()).unwrap());
    }

    #[test]
    fn a_proof_from_one_connection_is_rejected_on_another() {
        // The attack this closes: record a pairing exchange, then present the
        // captured proof to the same machine later with the same PIN still set.
        let pin = Pin::parse("246810").unwrap();
        let (a_old, _b_old) = connection([1u8; 32], [2u8; 32]);
        let (_a_new, b_new) = connection([1u8; 32], [99u8; 32]);

        let captured = PairingSession::new(&a_old, pin.clone()).confirm();
        let victim = PairingSession::new(&b_new, pin);
        assert!(!victim.accepts(&captured).unwrap());
    }

    #[test]
    fn a_proof_is_rejected_when_reflected_at_its_author() {
        // Without the role tag, both sides would owe the same value and a man in
        // the middle could simply echo the proof back.
        let (a, _b) = connection([1u8; 32], [2u8; 32]);
        let side_a = PairingSession::new(&a, Pin::parse("111111").unwrap());
        let own = side_a.confirm();
        assert!(!side_a.accepts(&own).unwrap());
    }

    #[test]
    fn swapping_the_nonce_roles_changes_the_proof() {
        let pin = Pin::parse("777777").unwrap();
        let straight = pairing_proof(&pin, &[1u8; 32], &[2u8; 32], Role::Initiator);
        let swapped = pairing_proof(&pin, &[2u8; 32], &[1u8; 32], Role::Initiator);
        assert_ne!(straight, swapped);
    }

    #[test]
    fn each_role_owes_a_different_proof() {
        let pin = Pin::parse("135790").unwrap();
        assert_ne!(
            pairing_proof(&pin, &[1u8; 32], &[2u8; 32], Role::Initiator),
            pairing_proof(&pin, &[1u8; 32], &[2u8; 32], Role::Responder)
        );
    }

    #[test]
    fn a_proof_is_stable_for_the_same_inputs() {
        // Both machines derive it independently; a non-deterministic proof would
        // make pairing fail intermittently.
        let pin = Pin::parse("000123").unwrap();
        assert_eq!(
            pairing_proof(&pin, &[3u8; 32], &[4u8; 32], Role::Responder),
            pairing_proof(&pin, &[3u8; 32], &[4u8; 32], Role::Responder)
        );
    }

    #[test]
    fn anything_other_than_a_pair_confirm_is_a_protocol_error() {
        let (a, _b) = connection([1u8; 32], [2u8; 32]);
        let side = PairingSession::new(&a, Pin::parse("222222").unwrap());
        assert_eq!(
            side.accepts(&ControlMsg::LayoutRequest).unwrap_err(),
            PairingError::NotAPairConfirm
        );
    }

    #[test]
    fn comparison_examines_every_byte_even_when_the_first_differs() {
        // A short-circuiting compare would leak the length of a correct prefix,
        // and a six-digit PIN does not survive that.
        let seen = Cell::new(0usize);
        let a = [0u8; 32];
        let mut b = [0u8; 32];
        b[0] = 1;
        let pairs = a
            .iter()
            .copied()
            .zip(b.iter().copied())
            .inspect(|_| seen.set(seen.get() + 1));
        assert!(!fold_difference(pairs));
        assert_eq!(seen.get(), 32, "compare stopped after {} bytes", seen.get());
    }

    #[test]
    fn comparison_catches_a_difference_in_the_last_byte() {
        let a = [0u8; 32];
        let mut b = [0u8; 32];
        b[31] = 1;
        assert!(!constant_time_eq(&a, &b));
        assert!(constant_time_eq(&a, &[0u8; 32]));
    }

    #[test]
    fn comparison_rejects_a_prefix_of_the_expected_value() {
        assert!(!constant_time_eq(&[1, 2, 3], &[1, 2, 3, 4]));
        assert!(!constant_time_eq(&[], &[0]));
        assert!(constant_time_eq(&[], &[]));
    }

    #[test]
    fn pin_parsing_rejects_the_wrong_number_of_digits() {
        assert_eq!(
            Pin::parse("12345").unwrap_err(),
            PairingError::WrongLength { got: 5 }
        );
        assert_eq!(
            Pin::parse("1234567").unwrap_err(),
            PairingError::WrongLength { got: 7 }
        );
        assert_eq!(
            Pin::parse("").unwrap_err(),
            PairingError::WrongLength { got: 0 }
        );
    }

    #[test]
    fn pin_parsing_rejects_non_ascii_digits() {
        // Arabic-Indic digits read as "123456" to a user but are three bytes each,
        // so accepting them would let two machines disagree about the same PIN.
        assert!(matches!(
            Pin::parse("١٢٣٤٥٦"),
            Err(PairingError::WrongLength { .. } | PairingError::NotAsciiDigits)
        ));
        assert_eq!(
            Pin::parse("12a456").unwrap_err(),
            PairingError::NotAsciiDigits
        );
        assert_eq!(
            Pin::parse("12 456").unwrap_err(),
            PairingError::NotAsciiDigits
        );
    }

    #[test]
    fn a_pin_of_all_zeros_is_valid_and_keeps_its_leading_zeros() {
        // Treating a PIN as a number would print "0" for this and never match.
        let pin = Pin::parse("000000").unwrap();
        assert_eq!(pin.as_str(), "000000");
        assert_eq!(Pin::parse("000042").unwrap().to_string(), "000042");
    }

    #[test]
    fn generated_pins_are_six_ascii_digits() {
        for _ in 0..64 {
            let pin = Pin::generate().unwrap();
            assert_eq!(pin.as_str().len(), PIN_DIGITS);
            assert!(pin.as_str().bytes().all(|b| b.is_ascii_digit()));
            // Round-tripping proves the display form is what a user would type.
            assert_eq!(Pin::parse(pin.as_str()).unwrap(), pin);
        }
    }

    #[test]
    fn generated_pins_use_the_whole_digit_range() {
        // Catches a modulo-bias or off-by-one that silently confines a position
        // to a subset of digits.
        let mut seen = [false; 10];
        for _ in 0..2000 {
            for b in Pin::generate().unwrap().as_str().bytes() {
                seen[(b - b'0') as usize] = true;
            }
        }
        assert!(seen.iter().all(|s| *s), "digits never generated: {seen:?}");
    }

    #[test]
    fn pin_debug_never_prints_the_pin() {
        let pin = Pin::parse("987654").unwrap();
        assert!(!format!("{pin:?}").contains("987654"));
    }
}
