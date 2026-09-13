//! The `call-capture` seam: joining a telephone call the owner is already
//! on so its audio becomes a source (`design/call-capture.md`). A provider
//! owns a telephone number reachable through a programmable-voice service
//! and, on request, dials the owner's verified phone; the owner answers
//! and merges the incoming call into the conversation they are having, and
//! from that moment the provider's leg hears the call and records it. The
//! recordings then arrive through the same provider's `connections`
//! registration like any other host's sources.
//!
//! The seam carries only what a transport needs to drive that from a
//! button: where the owner's number stands, one call to start, and one
//! status to show. Credentials and the service's vocabulary stay in the
//! provider.

use std::fmt;

use serde::{Deserialize, Serialize};

use inseam_kernel::address::Timestamp;
use inseam_kernel::substrate::ServiceKey;

use crate::SeamError;

pub const CALL_CAPTURE: ServiceKey<dyn CallCapture> = ServiceKey::new("call-capture");

/// Digits an E.164 number may have after the `+`: the ITU ceiling.
pub const PHONE_DIGITS_MAX: usize = 15;
/// Fewest digits a number the node will ever dial may have: shorter is a
/// short code or a typo, and never someone's phone.
pub const PHONE_DIGITS_MIN: usize = 7;
/// Length of a verification code the owner reads back.
pub const VERIFICATION_CODE_DIGITS: usize = 6;

/// A telephone number in E.164 form (`+14155550123`): parsed once at the
/// boundary so nothing downstream ever formats a raw string into a dial
/// request. Spaces, dashes, dots, and parentheses in the input are
/// dropped; a leading `+` is required so a national number is never
/// dialed into the wrong country.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PhoneNumber(String);

impl PhoneNumber {
    pub fn parse(input: &str) -> Result<Self, SeamError> {
        let compact: String = input
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')'))
            .collect();
        let Some(digits) = compact.strip_prefix('+') else {
            return Err(SeamError::Invalid(format!(
                "phone number `{input}` must start with `+` and a country code"
            )));
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(SeamError::Invalid(format!(
                "phone number `{input}` may only contain digits after `+`"
            )));
        }
        if digits.len() < PHONE_DIGITS_MIN || digits.len() > PHONE_DIGITS_MAX {
            return Err(SeamError::Invalid(format!(
                "phone number `{input}` must have {PHONE_DIGITS_MIN} to {PHONE_DIGITS_MAX} digits"
            )));
        }
        if digits.starts_with('0') {
            return Err(SeamError::Invalid(format!(
                "phone number `{input}`: a country code never starts with 0"
            )));
        }
        Ok(Self(compact))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The number with all but its last digits masked, for status lines
    /// that must not print a whole phone number.
    pub fn masked(&self) -> String {
        let digits = &self.0[1..];
        let keep = 4.min(digits.len());
        let hidden = digits.len() - keep;
        format!("+{}{}", "•".repeat(hidden), &digits[hidden..])
    }
}

impl TryFrom<String> for PhoneNumber {
    type Error = SeamError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<PhoneNumber> for String {
    fn from(number: PhoneNumber) -> Self {
        number.0
    }
}

impl fmt::Display for PhoneNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where the owner's own phone number stands: nothing set, a number the
/// node has called with a code that has not been read back yet, or a
/// verified number the node may dial on request. Verification exists
/// because anyone holding the owner token could otherwise make the node
/// ring an arbitrary phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OwnerNumber {
    Unset,
    Pending {
        number: PhoneNumber,
        expires_at: Timestamp,
    },
    Verified {
        number: PhoneNumber,
    },
}

/// The call the provider most recently placed to the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureCall {
    /// The provider's own id for the call.
    pub id: String,
    pub started_at: Timestamp,
}

/// Everything a button needs to render: the number the owner merges
/// (also dialable directly), where their own number stands, the last call
/// placed, and how many recordings have landed in the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub capture_number: PhoneNumber,
    pub owner_number: OwnerNumber,
    pub last_call: Option<CaptureCall>,
    pub recordings_archived: u64,
}

impl CaptureStatus {
    /// The owner's number for a status line, masked, or a dash when none
    /// is set.
    pub fn owner_number_masked(&self) -> String {
        match &self.owner_number {
            OwnerNumber::Unset => "—".to_string(),
            OwnerNumber::Pending { number, .. } | OwnerNumber::Verified { number } => {
                number.masked()
            }
        }
    }
}

#[async_trait::async_trait]
pub trait CallCapture: Send + Sync {
    async fn status(&self) -> Result<CaptureStatus, SeamError>;

    /// Begin verifying `number` as the owner's: the provider calls it and
    /// speaks a code, which the owner reads back through
    /// [`CallCapture::verify_owner_number`]. Replaces any pending attempt.
    async fn set_owner_number(&self, number: PhoneNumber) -> Result<CaptureStatus, SeamError>;

    /// Finish verification with the code the owner heard. A wrong or
    /// expired code is `Refused`.
    async fn verify_owner_number(&self, code: &str) -> Result<CaptureStatus, SeamError>;

    /// Place the capture call to the verified owner number. Refused when
    /// no number is verified, when a call was started too recently, or
    /// when the hourly ceiling is spent.
    async fn start(&self) -> Result<CaptureStatus, SeamError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_numbers_parse_e164_and_drop_punctuation() {
        let number = PhoneNumber::parse("+1 (415) 555-0123").expect("parses");
        assert_eq!(number.as_str(), "+14155550123");
        assert_eq!(number.masked(), "+•••••••0123");
        let json = serde_json::to_string(&number).expect("serializes");
        assert_eq!(json, "\"+14155550123\"");
    }

    #[test]
    fn phone_numbers_refuse_shapeless_input() {
        for bad in [
            "",
            "4155550123",
            "+",
            "+0123456789",
            "+1234",
            "+1234567890123456",
            "+1415abc0123",
        ] {
            assert!(PhoneNumber::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn owner_number_serializes_with_a_state_tag() {
        let pending = OwnerNumber::Pending {
            number: PhoneNumber::parse("+14155550123").expect("parses"),
            expires_at: Timestamp(10),
        };
        let json = serde_json::to_value(&pending).expect("serializes");
        assert_eq!(json["state"], "pending");
        assert_eq!(json["number"], "+14155550123");
        let unset = serde_json::to_value(OwnerNumber::Unset).expect("serializes");
        assert_eq!(unset["state"], "unset");
    }
}
