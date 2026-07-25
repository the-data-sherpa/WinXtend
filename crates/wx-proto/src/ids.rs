//! Stable identifiers for nodes and their monitors.

use core::fmt;

use serde::{Deserialize, Serialize};

/// A node's permanent identity: the raw bytes of its ed25519 public key.
///
/// Nodes are identified by key, never by hostname or IP, so a machine keeps its
/// identity across DHCP leases, renames, and network changes. Pairing is
/// trust-on-first-use against this value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// Short form shown in UI and logs. Eight hex chars is plenty to
    /// disambiguate the handful of machines on one desk, and it fits in a
    /// column without wrapping.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Self, IdParseError> {
        let bytes = hex::decode(s).map_err(|_| IdParseError::NotHex)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| IdParseError::WrongLength)?;
        Ok(Self(arr))
    }
}

// Logs are full of these; the short form keeps lines readable, and the full key
// is always recoverable via `to_hex`.
impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.short())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.short())
    }
}

/// An ed25519 signature.
///
/// A newtype with hand-written serde impls rather than a bare `[u8; 64]`, because
/// serde's blanket array impls stop at 32 elements. Encoded as a byte string, so
/// postcard writes a length prefix and the raw bytes rather than 64 separate
/// varint-encoded elements.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature(pub [u8; 64]);

impl Signature {
    pub const fn to_bytes(self) -> [u8; 64] {
        self.0
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Full 128 hex chars in a log line is noise; the prefix is enough to
        // correlate a signature across two machines' logs.
        write!(f, "Signature({}…)", hex::encode(&self.0[..4]))
    }
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct SigVisitor;

        impl<'de> serde::de::Visitor<'de> for SigVisitor {
            type Value = Signature;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 64-byte ed25519 signature")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Signature, E> {
                let arr: [u8; 64] = v
                    .try_into()
                    .map_err(|_| E::invalid_length(v.len(), &self))?;
                Ok(Signature(arr))
            }

            // Self-describing formats (JSON in the config file, say) present the
            // value as a sequence rather than a byte string.
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Signature, A::Error> {
                let mut out = [0u8; 64];
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                Ok(Signature(out))
            }
        }

        d.deserialize_bytes(SigVisitor)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdParseError {
    #[error("identifier is not valid hex")]
    NotHex,
    #[error("identifier must be 32 bytes")]
    WrongLength,
}

/// Identifies a monitor within a single node.
///
/// Only unique per node — pair it with a [`NodeId`] to address a monitor
/// anywhere in the mesh. See [`GlobalMonitorId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MonitorId(pub u32);

impl fmt::Display for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mon{}", self.0)
    }
}

/// A monitor addressed across the whole mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GlobalMonitorId {
    pub node: NodeId,
    pub monitor: MonitorId,
}

impl GlobalMonitorId {
    pub const fn new(node: NodeId, monitor: MonitorId) -> Self {
        Self { node, monitor }
    }
}

impl fmt::Display for GlobalMonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.node, self.monitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_hex_round_trips() {
        let id = NodeId([7u8; 32]);
        assert_eq!(NodeId::from_hex(&id.to_hex()), Ok(id));
    }

    #[test]
    fn short_form_is_eight_hex_chars() {
        assert_eq!(NodeId([0xab; 32]).short(), "abababab");
    }

    #[test]
    fn rejects_malformed_hex() {
        assert_eq!(NodeId::from_hex("zzzz"), Err(IdParseError::NotHex));
        assert_eq!(NodeId::from_hex("abcd"), Err(IdParseError::WrongLength));
    }

    #[test]
    fn signature_round_trips_through_postcard() {
        let mut raw = [0u8; 64];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = i as u8;
        }
        let sig = Signature(raw);
        let bytes = postcard::to_allocvec(&sig).unwrap();
        assert_eq!(postcard::from_bytes::<Signature>(&bytes).unwrap(), sig);
    }

    #[test]
    fn signature_encodes_as_bytes_not_sixty_four_varints() {
        // A byte string is 64 bytes plus a short length prefix. Element-wise
        // encoding would push high bytes to two varints each.
        let bytes = postcard::to_allocvec(&Signature([0xff; 64])).unwrap();
        assert!(
            bytes.len() <= 66,
            "signature encoded to {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn signature_of_wrong_length_is_rejected() {
        // A truncated signature must fail to decode rather than silently
        // zero-pad into something that could pass verification.
        let short = postcard::to_allocvec(&[0u8; 32].as_slice()).unwrap();
        assert!(postcard::from_bytes::<Signature>(&short).is_err());
    }
}
