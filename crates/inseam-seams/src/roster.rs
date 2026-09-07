//! The `roster` seam: the network's synchronized self-description
//! (`design/roster.md`) as this node reads it — who exists, how each node
//! is dialed, which hosts exist, and who stewards what — plus the
//! admission ceremony: invitations in, expulsions out. The records
//! themselves are the kernel's (`inseam_kernel::network`) and replicate
//! through the `sync` seam like catalog entries; this seam is the typed
//! view over what the store holds and the only writer of this node's own
//! roster records.
//!
//! An [`Invitation`] is the whole join: the inviting node's identity and
//! dialing hints (endpoint plus key pinning, as the design assumed) and a
//! one-time token that its admission policy honors once. The owner copies
//! one string from one node to another; the joining node dials, presents
//! the token, and syncs — after which it is a roster node like any other
//! and the token is spent.

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use inseam_kernel::address::{HostId, Timestamp};
use inseam_kernel::network::{
    ENDPOINTS_MAX, Endpoint, HostRecord, NodeId, NodeRecord, StewardshipRecord,
};
use inseam_kernel::substrate::{Notify, ServiceKey};

use crate::SeamError;
use crate::transport::{InvitationToken, PeerAddress};

pub const ROSTER: ServiceKey<dyn Roster> = ServiceKey::new("roster");

/// Fired after the roster tables changed: the sync seam emits it once an
/// exchange applied a roster record from a peer, and the roster provider
/// emits it after publishing or expelling. It carries no payload on
/// purpose — a listener re-reads the roster, so a burst of applied entries
/// collapses into one read and nothing it holds can go stale. The roster
/// provider itself listens here to refresh the in-memory admission view
/// its synchronous `admit` answers from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RosterChanged;

impl Notify for RosterChanged {}

/// How long an invitation stays open: a day covers "I'll set the laptop up
/// tonight" without leaving a live token around for a week.
pub const INVITATION_TTL_SECS: i64 = 24 * 60 * 60;
/// Most unredeemed invitations one node holds at once; a 33rd is refused
/// until one is redeemed or expires.
pub const INVITATIONS_OPEN_MAX: usize = 32;
/// The text form's prefix, so a pasted string is recognizable as ours.
pub const INVITATION_PREFIX: &str = "inseam-invite:";
/// Longest invitation text accepted. A maximal invitation — sixteen
/// endpoints of 256 characters, a 128-character token, the id and the
/// expiry — is under 4.5 KiB of JSON and under 6 KiB once base64
/// encoded, so 8 KiB admits every well-formed one and nothing wild.
pub const INVITATION_TEXT_CHARS_MAX: usize = 8 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvitationError {
    #[error("invitation text is {0} characters; the bound is {INVITATION_TEXT_CHARS_MAX}")]
    TooLong(usize),
    #[error("invitation text does not start with `{INVITATION_PREFIX}`")]
    MissingPrefix,
    #[error("invitation text is not base64url: {0}")]
    Encoding(String),
    #[error("invitation is malformed: {0}")]
    Malformed(String),
    #[error("invitation carries more than {ENDPOINTS_MAX} endpoints")]
    TooManyEndpoints,
}

/// What one node hands another to join: dial `node` at `endpoints`,
/// present `token` once, before `expires`. Rendered as
/// `inseam-invite:<base64url of the JSON>` by `Display` and parsed back by
/// `FromStr`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invitation {
    pub node: NodeId,
    pub endpoints: Vec<Endpoint>,
    pub token: InvitationToken,
    pub expires: Timestamp,
}

impl Invitation {
    /// Whether the invitation can still be redeemed at `now`.
    pub fn is_open_at(&self, now: Timestamp) -> bool {
        now < self.expires
    }

    /// Bounds an invitation must satisfy, checked when minted and when
    /// parsed: the two sides of the copy.
    pub fn check_bounds(&self) -> Result<(), InvitationError> {
        if self.endpoints.len() > ENDPOINTS_MAX {
            return Err(InvitationError::TooManyEndpoints);
        }
        Ok(())
    }
}

impl fmt::Display for Invitation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A struct of validated fields always serializes; the map is for
        // the signature, not an expected path.
        let json = serde_json::to_vec(self).map_err(|_| fmt::Error)?;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        write!(f, "{INVITATION_PREFIX}{encoded}")
    }
}

impl FromStr for Invitation {
    type Err = InvitationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // The bound comes first, before anything decodes: a pasted string
        // is untrusted input.
        let text = s.trim();
        if text.chars().count() > INVITATION_TEXT_CHARS_MAX {
            return Err(InvitationError::TooLong(text.chars().count()));
        }
        let encoded = text
            .strip_prefix(INVITATION_PREFIX)
            .ok_or(InvitationError::MissingPrefix)?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|e| InvitationError::Encoding(e.to_string()))?;
        let invitation: Invitation =
            serde_json::from_slice(&json).map_err(|e| InvitationError::Malformed(e.to_string()))?;
        invitation.check_bounds()?;
        Ok(invitation)
    }
}

impl From<Invitation> for PeerAddress {
    /// A join is one exchange with the inviting node, token in hand.
    fn from(invitation: Invitation) -> Self {
        Self {
            id: invitation.node,
            endpoints: invitation.endpoints,
            invitation: Some(invitation.token),
        }
    }
}

/// One host and every steward that currently claims it. A host with no
/// stewards is unreachable but still known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStewards {
    pub host: HostRecord,
    pub stewards: Vec<StewardshipRecord>,
}

#[async_trait::async_trait]
pub trait Roster: Send + Sync {
    /// This node's own record as last published.
    fn local(&self) -> NodeRecord;

    /// Every known node including this one, excluding the expelled.
    async fn nodes(&self) -> Result<Vec<NodeRecord>, SeamError>;

    async fn node(&self, id: &NodeId) -> Result<Option<NodeRecord>, SeamError>;

    /// Every known host with its stewards, ordered by host id.
    async fn hosts(&self) -> Result<Vec<HostStewards>, SeamError>;

    async fn stewards_of(&self, host: &HostId) -> Result<Vec<StewardshipRecord>, SeamError>;

    /// Known and not expelled.
    async fn is_admitted(&self, id: &NodeId) -> Result<bool, SeamError>;

    /// Re-publish this node's record with the transport's current
    /// endpoints — the rotation path (`design/roster.md`).
    async fn republish(&self) -> Result<NodeRecord, SeamError>;

    /// Mint an invitation: this node's identity and endpoints, a fresh
    /// one-time token, and an expiry [`INVITATION_TTL_SECS`] out. Refused
    /// past [`INVITATIONS_OPEN_MAX`] open ones.
    async fn invite(&self) -> Result<Invitation, SeamError>;

    /// Consume a token for `peer`: true once, then never. Synchronous
    /// because the transport's admission hook is.
    fn redeem(&self, peer: &NodeId, token: &InvitationToken) -> bool;

    /// Publish the expulsion: every node stops admitting `node` and drops
    /// its logs; this node disconnects it now.
    async fn expel(&self, node: &NodeId) -> Result<(), SeamError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::INVITATION_TOKEN_CHARS_MAX;
    use inseam_kernel::network::ENDPOINT_CHARS_MAX;

    fn invitation(endpoints: usize) -> Invitation {
        Invitation {
            node: NodeId::from_bytes([9; 32]),
            endpoints: (0..endpoints)
                .map(|i| Endpoint::new(format!("relay:https://relay-{i}.example")).expect("valid"))
                .collect(),
            token: InvitationToken::new("one-time-token").expect("valid"),
            expires: Timestamp(1_800_000_000),
        }
    }

    #[test]
    fn invitation_roundtrips_through_its_text_form() {
        let invitation = invitation(2);
        let text = invitation.to_string();
        assert!(text.starts_with(INVITATION_PREFIX));
        assert!(
            !text.contains("one-time-token"),
            "the text is encoded, not plain"
        );
        let back: Invitation = text.parse().expect("parses");
        assert_eq!(back, invitation);
        let padded: Invitation = format!("  {text}\n").parse().expect("trims");
        assert_eq!(padded, invitation);
    }

    #[test]
    fn invitation_rejects_text_that_is_not_one() {
        assert_eq!(
            "not-an-invite:abc".parse::<Invitation>(),
            Err(InvitationError::MissingPrefix)
        );
        assert!(matches!(
            format!("{INVITATION_PREFIX}***").parse::<Invitation>(),
            Err(InvitationError::Encoding(_))
        ));
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"node\":1}");
        assert!(matches!(
            format!("{INVITATION_PREFIX}{json}").parse::<Invitation>(),
            Err(InvitationError::Malformed(_))
        ));
        let long = format!(
            "{INVITATION_PREFIX}{}",
            "a".repeat(INVITATION_TEXT_CHARS_MAX)
        );
        assert!(matches!(
            long.parse::<Invitation>(),
            Err(InvitationError::TooLong(_))
        ));
    }

    #[test]
    fn invitation_bounds_its_endpoints_on_both_sides() {
        let crowded = invitation(ENDPOINTS_MAX + 1);
        assert_eq!(
            crowded.check_bounds(),
            Err(InvitationError::TooManyEndpoints)
        );
        assert_eq!(
            crowded.to_string().parse::<Invitation>(),
            Err(InvitationError::TooManyEndpoints)
        );
        assert_eq!(invitation(ENDPOINTS_MAX).check_bounds(), Ok(()));
    }

    #[test]
    fn a_maximal_invitation_fits_the_text_bound() {
        let maximal = Invitation {
            node: NodeId::from_bytes([0xff; 32]),
            endpoints: (0..ENDPOINTS_MAX)
                .map(|_| Endpoint::new("e".repeat(ENDPOINT_CHARS_MAX)).expect("valid"))
                .collect(),
            token: InvitationToken::new("t".repeat(INVITATION_TOKEN_CHARS_MAX)).expect("valid"),
            expires: Timestamp(i64::MAX),
        };
        let text = maximal.to_string();
        assert!(
            text.len() <= INVITATION_TEXT_CHARS_MAX,
            "{} chars",
            text.len()
        );
        assert_eq!(text.parse::<Invitation>().expect("parses"), maximal);
    }

    #[test]
    fn invitation_is_open_until_it_expires() {
        let invitation = invitation(1);
        assert!(invitation.is_open_at(Timestamp(invitation.expires.0 - 1)));
        assert!(!invitation.is_open_at(invitation.expires));
        assert!(!invitation.is_open_at(Timestamp(invitation.expires.0 + 1)));
    }

    #[test]
    fn a_join_dials_the_inviter_with_the_token() {
        let invitation = invitation(1);
        let address = PeerAddress::from(invitation.clone());
        assert_eq!(address.id, invitation.node);
        assert_eq!(address.endpoints, invitation.endpoints);
        assert_eq!(address.invitation, Some(invitation.token));
    }
}
