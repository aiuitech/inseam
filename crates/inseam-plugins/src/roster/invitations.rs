//! The open invitations one node holds (`design/roster.md`): a bounded
//! ledger of one-time tokens, each with an expiry. It lives in memory only
//! — a restart forgets every open invitation, and the owner mints a fresh
//! one — because an invitation is a short-lived ceremony, not a fact the
//! roster should carry or sync. The ledger is pure: it takes the clock as
//! an argument, so its behavior is checkable without one.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore as _;

use inseam_kernel::address::Timestamp;
use inseam_seams::roster::{INVITATIONS_OPEN_MAX, INVITATION_TTL_SECS};
use inseam_seams::transport::{InvitationToken, INVITATION_TOKEN_CHARS_MAX};
use inseam_seams::SeamError;

/// Random bytes behind one token: 256 bits, so a guess is hopeless within
/// the day a token stays open.
pub const TOKEN_RANDOM_BYTES: usize = 32;
/// Characters base64url spends on [`TOKEN_RANDOM_BYTES`] without padding.
const TOKEN_CHARS: usize = 43;
const _: () = assert!(
    TOKEN_CHARS == TOKEN_RANDOM_BYTES.div_ceil(3) * 4 - 1,
    "32 bytes encode to 43 base64url characters without padding"
);
const _: () = assert!(
    TOKEN_CHARS <= INVITATION_TOKEN_CHARS_MAX,
    "a minted token must fit the wire bound"
);

struct OpenInvitation {
    token: InvitationToken,
    expires: Timestamp,
}

/// Every unredeemed, unexpired invitation this node minted since it
/// started, bounded by [`INVITATIONS_OPEN_MAX`].
pub(crate) struct OpenInvitations {
    open: Vec<OpenInvitation>,
}

impl OpenInvitations {
    pub(crate) fn new() -> Self {
        Self { open: Vec::new() }
    }

    /// Mint a fresh token open until `now + INVITATION_TTL_SECS`. Expired
    /// invitations are pruned first, so a ledger full of stale ones never
    /// blocks a new invitation; a ledger full of live ones refuses.
    pub(crate) fn mint(&mut self, now: Timestamp) -> Result<(InvitationToken, Timestamp), SeamError> {
        self.prune(now);
        if self.open.len() >= INVITATIONS_OPEN_MAX {
            return Err(SeamError::Refused(format!(
                "{INVITATIONS_OPEN_MAX} invitations are already open; redeem or wait out one first"
            )));
        }
        let token = random_token();
        let expires = Timestamp(now.0.saturating_add(INVITATION_TTL_SECS));
        assert!(expires > now, "an invitation opens before it expires");
        self.open.push(OpenInvitation {
            token: token.clone(),
            expires,
        });
        assert!(self.open_count() <= INVITATIONS_OPEN_MAX);
        Ok((token, expires))
    }

    /// Consume the invitation `token` names: true once, then never. The
    /// comparison is the token type's constant-time equality; walking the
    /// ledger reveals at most which slot matched, never how much of a
    /// guess did.
    pub(crate) fn redeem(&mut self, token: &InvitationToken, now: Timestamp) -> bool {
        self.prune(now);
        let position = self.open.iter().position(|open| open.token == *token);
        match position {
            Some(index) => {
                self.open.swap_remove(index);
                true
            }
            None => false,
        }
    }

    pub(crate) fn open_count(&self) -> usize {
        self.open.len()
    }

    fn prune(&mut self, now: Timestamp) {
        self.open.retain(|open| now < open.expires);
    }
}

/// A fresh base64url token of [`TOKEN_RANDOM_BYTES`] from the OS CSPRNG.
fn random_token() -> InvitationToken {
    let mut bytes = [0u8; TOKEN_RANDOM_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    assert_eq!(encoded.len(), TOKEN_CHARS);
    // The base64url alphabet has no whitespace and no control character,
    // and the length is under the bound (checked at compile time above),
    // so the constructor cannot refuse.
    InvitationToken::new(encoded).expect("a base64url token is a valid invitation token")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = Timestamp(1_800_000_000);

    #[test]
    fn a_minted_token_redeems_once() {
        let mut ledger = OpenInvitations::new();
        let (token, expires) = ledger.mint(NOW).expect("mints");
        assert_eq!(expires, Timestamp(NOW.0 + INVITATION_TTL_SECS));
        assert_eq!(ledger.open_count(), 1);
        assert!(ledger.redeem(&token, NOW));
        assert!(!ledger.redeem(&token, NOW), "a token is spent by its first redeem");
        assert_eq!(ledger.open_count(), 0);
    }

    #[test]
    fn tokens_are_unique_and_plain() {
        let mut ledger = OpenInvitations::new();
        let (a, _) = ledger.mint(NOW).expect("mints");
        let (b, _) = ledger.mint(NOW).expect("mints");
        assert_ne!(a, b);
        assert_eq!(a.secret().len(), TOKEN_CHARS);
        assert!(a.secret().chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn an_expired_token_does_not_redeem() {
        let mut ledger = OpenInvitations::new();
        let (token, expires) = ledger.mint(NOW).expect("mints");
        assert!(!ledger.redeem(&token, expires), "expiry is exclusive");
        assert_eq!(ledger.open_count(), 0, "the expired token was pruned");
    }

    #[test]
    fn a_wrong_token_is_refused_and_leaves_the_ledger_intact() {
        let mut ledger = OpenInvitations::new();
        let _ = ledger.mint(NOW).expect("mints");
        let wrong = InvitationToken::new("not-the-token").expect("valid");
        assert!(!ledger.redeem(&wrong, NOW));
        assert_eq!(ledger.open_count(), 1);
    }

    #[test]
    fn a_full_ledger_refuses_until_one_expires() {
        let mut ledger = OpenInvitations::new();
        for _ in 0..INVITATIONS_OPEN_MAX {
            ledger.mint(NOW).expect("mints within the bound");
        }
        assert!(matches!(ledger.mint(NOW), Err(SeamError::Refused(_))));
        assert_eq!(ledger.open_count(), INVITATIONS_OPEN_MAX);
        let later = Timestamp(NOW.0 + INVITATION_TTL_SECS);
        ledger.mint(later).expect("the expired ones made room");
        assert_eq!(ledger.open_count(), 1);
    }
}
