//! The `node` seam: this node's own identity (`design/roster.md`). A node
//! *is* its keypair — the public half is its [`NodeId`], the dial target
//! and the origin of every record it publishes — and the seam is where
//! that identity lives, so the transport, the roster, and the sync layer
//! all read one answer instead of each keeping a key file. The provider
//! mints the key once, keeps it in the node's data directory beside the
//! other owner-private files, and never syncs or configures it.

use std::fmt;

use inseam_kernel::network::{Endpoint, NodeCapabilities, NodeId, NodeRecord};
use inseam_kernel::substrate::ServiceKey;

pub const NODE: ServiceKey<dyn Node> = ServiceKey::new("node");

/// The Ed25519 secret whose public half is the node id, handed to the
/// transport so it can build its own key type from it. `Debug` redacts
/// it and it has no serde form: the secret leaves the process only inside
/// the provider's own key file.
#[derive(Clone)]
pub struct SecretKeyBytes([u8; 32]);

impl SecretKeyBytes {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SecretKeyBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKeyBytes(<redacted>)")
    }
}

/// This node, as it knows itself. Every answer is stable for the life of
/// the process: the id never changes, and a renamed node or a changed
/// capability set is a restarted node.
pub trait Node: Send + Sync {
    /// The public key, which is the identity.
    fn id(&self) -> NodeId;

    fn display_name(&self) -> String;

    /// What this node advertises about itself to discovery and routing.
    fn capabilities(&self) -> NodeCapabilities;

    /// The secret half of the identity, for the transport only.
    fn secret_key(&self) -> SecretKeyBytes;

    /// This node's roster record with the given dialing hints — exactly
    /// what the roster publishes on a republish, so it never re-derives
    /// the identity fields.
    fn record(&self, endpoints: Vec<Endpoint>) -> NodeRecord {
        NodeRecord {
            id: self.id(),
            display_name: self.display_name(),
            endpoints,
            capabilities: self.capabilities(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_debug_redacts() {
        let secret = SecretKeyBytes::from_bytes([7; 32]);
        assert_eq!(format!("{secret:?}"), "SecretKeyBytes(<redacted>)");
        assert_eq!(secret.as_bytes(), &[7; 32]);
    }

    struct Fixed;

    impl Node for Fixed {
        fn id(&self) -> NodeId {
            NodeId::from_bytes([1; 32])
        }
        fn display_name(&self) -> String {
            "laptop".to_string()
        }
        fn capabilities(&self) -> NodeCapabilities {
            NodeCapabilities {
                always_on: false,
                deep_index: true,
                relays: false,
            }
        }
        fn secret_key(&self) -> SecretKeyBytes {
            SecretKeyBytes::from_bytes([0; 32])
        }
    }

    #[test]
    fn record_carries_the_identity_and_the_given_endpoints() {
        let endpoint = Endpoint::new("relay:https://relay.example").expect("valid");
        let record = Fixed.record(vec![endpoint.clone()]);
        assert_eq!(record.id, NodeId::from_bytes([1; 32]));
        assert_eq!(record.display_name, "laptop");
        assert_eq!(record.endpoints, vec![endpoint]);
        assert!(record.capabilities.deep_index);
        assert!(!record.capabilities.always_on);
    }
}
