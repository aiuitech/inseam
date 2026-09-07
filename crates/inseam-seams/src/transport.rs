//! The `transport` seam: node↔node connections (`design/connections.md`).
//! The flagship provider is iroh — QUIC dialed by the peer's public key,
//! with hole punching and relayed fallback — but nothing above this seam
//! knows that. A consumer names a peer by its [`NodeId`] plus the roster's
//! dialing hints, names the protocol it speaks per request, and gets one
//! request/response exchange back. Handlers register per protocol name as
//! fiber effects, the way connections and grants register into their
//! registries, and one QUIC ALPN carries them all: the [`ProtocolName`]
//! is the routing key inside the stream, never a second ALPN.
//!
//! Who may connect is not the transport's call. It asks its [`Admission`]
//! policy — the roster's, which knows who is invited and who is expelled —
//! and with no policy set it refuses everyone, so a misconfigured node is
//! closed rather than open.
//!
//! Sessions are local knowledge (`design/network.md`): a session is
//! whatever is live right now, in either direction, and liveness is
//! learned by trying — never read from a synced record.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use inseam_kernel::address::Timestamp;
use inseam_kernel::network::{Endpoint, NodeId, NodeRecord};
use inseam_kernel::substrate::{ApplyCx, PluginError, ServiceKey};

use crate::SeamError;
use crate::operations::FETCH_BYTES_MAX;

pub const TRANSPORT: ServiceKey<dyn Transport> = ServiceKey::new("transport");

/// Longest protocol name; names are short paths (`inseam/sync/1`), never
/// descriptions.
pub const PROTOCOL_NAME_CHARS_MAX: usize = 64;
/// Longest one-time admission token an invitation carries.
pub const INVITATION_TOKEN_CHARS_MAX: usize = 128;
/// Bytes of framing a routed message may add around the largest payload
/// the operations seam carries: the request envelope, an address, a
/// content type. Generous, never a second payload.
pub const MESSAGE_FRAMING_BYTES_MAX: u64 = 8 * 1024 * 1024;
/// Most bytes one request or one response may carry: a routed
/// `fetch_bytes` ships up to [`FETCH_BYTES_MAX`] plus framing. A routed
/// wire form must carry bytes raw, in a binary encoding — base64 inside
/// JSON would inflate a maximal fetch by a third and past this bound.
pub const MESSAGE_BYTES_MAX: u64 = FETCH_BYTES_MAX + MESSAGE_FRAMING_BYTES_MAX;
const _: () = assert!(
    MESSAGE_BYTES_MAX == 40 * 1024 * 1024,
    "the message bound is the fetch bound plus framing, 40 MiB in all"
);
const _: () = assert!(
    MESSAGE_BYTES_MAX > FETCH_BYTES_MAX,
    "a maximal fetch must fit in one message with room for its framing"
);
/// How long one exchange may take before the caller gives up on the peer.
pub const REQUEST_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);
const _: () = assert!(
    REQUEST_TIMEOUT_DEFAULT.as_secs() > 0,
    "a zero timeout would refuse every request"
);
/// Most live sessions one node holds, in both directions together; a
/// personal network is tens of nodes, and a 257th session is a bug or an
/// attack, not a bigger network.
pub const SESSIONS_MAX: usize = 256;
const _: () = assert!(
    SESSIONS_MAX > 0,
    "a node with no sessions could reach nothing"
);

/// The undo a registration returns; calling it withdraws what was
/// registered.
pub type Disposer = Box<dyn FnOnce() + Send>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolNameError {
    #[error("protocol name may not be empty")]
    Empty,
    #[error(
        "protocol name `{0}` may only contain lowercase ASCII letters, digits, `/`, `.` and `-`"
    )]
    InvalidCharacters(String),
    #[error("protocol name `{0}` is longer than {PROTOCOL_NAME_CHARS_MAX} characters")]
    TooLong(String),
}

/// Names the protocol a request speaks (`inseam/sync/1`, `inseam/route/1`):
/// the per-stream routing key the transport multiplexes over its one ALPN.
/// Validated so it is safe on the wire and unambiguous in a listing; a
/// version belongs in the name, so a breaking change is a new name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProtocolName(String);

impl ProtocolName {
    pub fn new(name: impl Into<String>) -> Result<Self, ProtocolNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ProtocolNameError::Empty);
        }
        if name.len() > PROTOCOL_NAME_CHARS_MAX {
            return Err(ProtocolNameError::TooLong(name));
        }
        let valid = name.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '/' || c == '.' || c == '-'
        });
        if !valid {
            return Err(ProtocolNameError::InvalidCharacters(name));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProtocolName {
    type Error = ProtocolNameError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<ProtocolName> for String {
    fn from(name: ProtocolName) -> String {
        name.0
    }
}

impl fmt::Display for ProtocolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvitationTokenError {
    #[error("invitation token may not be empty")]
    Empty,
    #[error("invitation token contains whitespace or control characters")]
    InvalidCharacters,
    #[error("invitation token is longer than {INVITATION_TOKEN_CHARS_MAX} characters")]
    TooLong,
}

/// The one-time admission token a joining node presents on first contact
/// (`design/roster.md`): minted by the inviting node, carried inside the
/// invitation, redeemed exactly once. `Debug` redacts it and the errors
/// never echo it; equality is constant-time so a redeem cannot leak how
/// much of a guess matched.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InvitationToken(String);

impl InvitationToken {
    pub fn new(token: impl Into<String>) -> Result<Self, InvitationTokenError> {
        let token = token.into();
        if token.is_empty() {
            return Err(InvitationTokenError::Empty);
        }
        if token.chars().count() > INVITATION_TOKEN_CHARS_MAX {
            return Err(InvitationTokenError::TooLong);
        }
        let plain = token.chars().all(|c| !c.is_whitespace() && !c.is_control());
        if !plain {
            return Err(InvitationTokenError::InvalidCharacters);
        }
        Ok(Self(token))
    }

    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl PartialEq for InvitationToken {
    fn eq(&self, other: &Self) -> bool {
        if self.0.len() != other.0.len() {
            return false;
        }
        let difference = self
            .0
            .bytes()
            .zip(other.0.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        difference == 0
    }
}

impl Eq for InvitationToken {}

impl fmt::Debug for InvitationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InvitationToken(<redacted>)")
    }
}

impl TryFrom<String> for InvitationToken {
    type Error = InvitationTokenError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<InvitationToken> for String {
    fn from(token: InvitationToken) -> String {
        token.0
    }
}

/// Where to reach a peer: its identity, the roster's dialing hints, and —
/// on a joining node's first contact only — the admission token. An
/// outbound-only peer has no endpoints, and is reached only through a
/// session it opened itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAddress {
    pub id: NodeId,
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation: Option<InvitationToken>,
}

impl From<&NodeRecord> for PeerAddress {
    /// Dialing an admitted roster node needs no invitation.
    fn from(record: &NodeRecord) -> Self {
        Self {
            id: record.id,
            endpoints: record.endpoints.clone(),
            invitation: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDirection {
    /// The peer dialed us.
    Inbound,
    /// We dialed the peer.
    Outbound,
}

/// One live session as this node observes it — local knowledge, never
/// synced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionView {
    pub peer: NodeId,
    pub direction: SessionDirection,
    pub since: Timestamp,
    pub last_used: Timestamp,
}

/// Serves one protocol: a request body in, a response body out. The
/// transport has already authenticated `peer` and admitted it; the
/// handler decides what the bytes mean.
#[async_trait::async_trait]
pub trait RequestHandler: Send + Sync {
    async fn handle(&self, peer: NodeId, body: Vec<u8>) -> Result<Vec<u8>, SeamError>;
}

/// An admission policy's answer for one connecting peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    Admitted,
    /// Why, for the transport's log; the peer learns only that it was
    /// refused.
    Refused(String),
}

impl Admit {
    pub fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted)
    }
}

/// Decides who may connect: the roster, which knows who is admitted, who
/// is expelled, and which one-time tokens are still open. Called on the
/// transport's accept path, so it is synchronous and must answer from
/// memory. A transport with no policy set refuses every peer — the safe
/// default for a node whose roster has not come up yet.
pub trait Admission: Send + Sync {
    fn admit(&self, peer: &NodeId, invitation: Option<&InvitationToken>) -> Admit;
}

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    fn local_id(&self) -> NodeId;

    /// This node's current dialing hints in the transport's own endpoint
    /// vocabulary — what its roster node record publishes. Empty for a
    /// node that cannot be dialed.
    fn endpoints(&self) -> Vec<Endpoint>;

    /// One handler per protocol name; a second registration for a name is
    /// refused by name, never silently replaced.
    fn register(
        &self,
        protocol: ProtocolName,
        handler: Arc<dyn RequestHandler>,
    ) -> Result<Disposer, SeamError>;

    /// Install the admission policy; a second policy while one is set is
    /// refused. Disposing it returns the transport to refusing everyone.
    fn set_admission(&self, policy: Arc<dyn Admission>) -> Result<Disposer, SeamError>;

    /// One request/response exchange. Reuses a live session with the peer
    /// in *either* direction — an accepted inbound connection serves
    /// outbound requests too, which is how a dialable node reaches an
    /// outbound-only one — else dials the endpoints. Bodies are bounded by
    /// [`MESSAGE_BYTES_MAX`] both ways.
    async fn request(
        &self,
        to: &PeerAddress,
        protocol: &ProtocolName,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, SeamError>;

    /// Every live session, in both directions.
    fn sessions(&self) -> Vec<SessionView>;

    /// Drop any session with the peer — expulsion's local half; the
    /// admission policy keeps it out afterwards.
    fn disconnect(&self, peer: &NodeId);
}

/// Register a protocol handler as a fiber effect: a plugin's `apply` calls
/// this once per protocol it serves, and unmounting the plugin withdraws
/// the handler through the disposer.
pub fn register_as_effect(
    cx: &mut ApplyCx<'_>,
    protocol: ProtocolName,
    handler: Arc<dyn RequestHandler>,
) -> Result<(), PluginError> {
    let label = format!("register transport protocol `{protocol}`");
    let transport = cx.get(&TRANSPORT)?;
    let disposer = transport
        .register(protocol, handler)
        .map_err(|e| PluginError(e.to_string()))?;
    cx.effect(label, disposer);
    Ok(())
}

/// Install the admission policy as a fiber effect: the roster's `apply`
/// calls this, and unmounting it closes the transport again.
pub fn admission_as_effect(
    cx: &mut ApplyCx<'_>,
    policy: Arc<dyn Admission>,
) -> Result<(), PluginError> {
    let transport = cx.get(&TRANSPORT)?;
    let disposer = transport
        .set_admission(policy)
        .map_err(|e| PluginError(e.to_string()))?;
    cx.effect("set transport admission policy", disposer);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_name_accepts_versioned_paths() {
        for name in ["inseam/sync/1", "inseam/route/1", "a", "x.y-z/0"] {
            assert_eq!(ProtocolName::new(name).expect("valid").as_str(), name);
        }
    }

    #[test]
    fn protocol_name_rejects_empty_long_and_odd_names() {
        assert_eq!(ProtocolName::new(""), Err(ProtocolNameError::Empty));
        for bad in ["Inseam/sync", "sync 1", "sync_1", "sync:1", "sÿnc"] {
            assert!(
                matches!(
                    ProtocolName::new(bad),
                    Err(ProtocolNameError::InvalidCharacters(_))
                ),
                "{bad} should be refused"
            );
        }
        let long = "p".repeat(PROTOCOL_NAME_CHARS_MAX + 1);
        assert!(matches!(
            ProtocolName::new(long),
            Err(ProtocolNameError::TooLong(_))
        ));
        let longest = "p".repeat(PROTOCOL_NAME_CHARS_MAX);
        assert!(ProtocolName::new(longest).is_ok());
    }

    #[test]
    fn protocol_name_roundtrips_through_serde() {
        let name = ProtocolName::new("inseam/sync/1").expect("valid");
        let json = serde_json::to_string(&name).expect("serializes");
        assert_eq!(json, "\"inseam/sync/1\"");
        let back: ProtocolName = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, name);
        assert!(serde_json::from_str::<ProtocolName>("\"Bad Name\"").is_err());
    }

    #[test]
    fn invitation_token_is_bounded_and_redacted() {
        let token = InvitationToken::new("abc-123").expect("valid");
        assert_eq!(token.secret(), "abc-123");
        assert_eq!(format!("{token:?}"), "InvitationToken(<redacted>)");
        assert_eq!(InvitationToken::new(""), Err(InvitationTokenError::Empty));
        assert_eq!(
            InvitationToken::new("a b"),
            Err(InvitationTokenError::InvalidCharacters)
        );
        assert_eq!(
            InvitationToken::new("a\nb"),
            Err(InvitationTokenError::InvalidCharacters)
        );
        let long = "t".repeat(INVITATION_TOKEN_CHARS_MAX + 1);
        assert_eq!(
            InvitationToken::new(long),
            Err(InvitationTokenError::TooLong)
        );
    }

    #[test]
    fn invitation_tokens_compare_by_value() {
        let a = InvitationToken::new("same").expect("valid");
        let b = InvitationToken::new("same").expect("valid");
        let c = InvitationToken::new("sam").expect("valid");
        let d = InvitationToken::new("samf").expect("valid");
        assert_eq!(a, b);
        assert_ne!(a, c, "a prefix is not the token");
        assert_ne!(a, d, "one byte off is not the token");
    }

    #[test]
    fn peer_address_from_a_record_carries_no_invitation() {
        let record = NodeRecord {
            id: NodeId::from_bytes([3; 32]),
            display_name: "mini".to_string(),
            endpoints: vec![Endpoint::new("relay:https://r.example").expect("valid")],
            capabilities: inseam_kernel::network::NodeCapabilities {
                always_on: true,
                deep_index: true,
                relays: true,
            },
        };
        let address = PeerAddress::from(&record);
        assert_eq!(address.id, record.id);
        assert_eq!(address.endpoints, record.endpoints);
        assert_eq!(address.invitation, None);
        let json = serde_json::to_value(&address).expect("serializes");
        assert!(
            json.get("invitation").is_none(),
            "absent tokens are not written"
        );
    }
}
