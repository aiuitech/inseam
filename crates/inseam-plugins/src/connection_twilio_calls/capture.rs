//! The `call-capture` provider: verifying the owner's phone and placing the
//! capture call. Both are outbound calls the provider makes to the owner's
//! number with inline TwiML — a spoken code for verification, a spoken
//! notice and a long recording for capture — so the node never serves a
//! webhook or opens a port. State that must survive a restart (the
//! verified number, the pending code, recent call starts) lives in the
//! kernel's per-plugin state namespace; nothing here is a credential.

use std::sync::Arc;

use rand::RngCore as _;

use inseam_kernel::address::Timestamp;
use inseam_kernel::state::StateNamespace;
use inseam_seams::SeamError;
use inseam_seams::call_capture::{
    CallCapture, CaptureCall, CaptureStatus, OwnerNumber, PhoneNumber, VERIFICATION_CODE_DIGITS,
};

use super::archive::Archive;
use super::twilio::TwilioClient;

/// The state namespace's schema version: bump to discard every key.
pub const STATE_VERSION: &str = "1";

const KEY_OWNER_NUMBER: &str = "owner_number";
const KEY_PENDING_NUMBER: &str = "pending_number";
const KEY_PENDING_CODE: &str = "pending_code";
const KEY_PENDING_EXPIRES_AT: &str = "pending_expires_at";
const KEY_LAST_CALL_SID: &str = "last_call_sid";
const KEY_LAST_CALL_STARTED_AT: &str = "last_call_started_at";
const KEY_CALL_STARTS: &str = "call_starts";

/// Seconds a capture call must be apart from the previous one: two taps
/// on a button are one call.
pub const CALL_START_GAP_SECS_MIN: i64 = 60;
const HOUR_SECS: i64 = 3600;

/// Longest a spoken notice may be: TwiML is inlined into the call request,
/// which Twilio caps at 4000 characters.
pub const NOTICE_CHARS_MAX: usize = 1000;

/// The knobs the capture flow reads from config.
#[derive(Debug, Clone)]
pub struct CapturePolicy {
    pub capture_number: PhoneNumber,
    pub notice: String,
    pub recording_secs_max: u32,
    pub ring_secs: u32,
    pub calls_per_hour_max: u32,
    pub verification_ttl_secs: i64,
}

/// Wall-clock reads go through this so tests drive time by hand.
pub trait Clock: Send + Sync {
    fn now_epoch(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_epoch(&self) -> i64 {
        Timestamp::from(std::time::SystemTime::now()).0
    }
}

pub struct CaptureService {
    twilio: Arc<TwilioClient>,
    state: StateNamespace,
    archive: Arc<Archive>,
    policy: CapturePolicy,
    clock: Arc<dyn Clock>,
}

impl CaptureService {
    pub fn new(
        twilio: Arc<TwilioClient>,
        state: StateNamespace,
        archive: Arc<Archive>,
        policy: CapturePolicy,
        clock: Arc<dyn Clock>,
    ) -> Self {
        assert!(policy.recording_secs_max >= 1);
        assert!(policy.calls_per_hour_max >= 1);
        assert!(policy.verification_ttl_secs >= 1);
        Self {
            twilio,
            state,
            archive,
            policy,
            clock,
        }
    }

    async fn owner_number(&self) -> Result<OwnerNumber, SeamError> {
        if let Some(number) = self.state.get(KEY_OWNER_NUMBER).await? {
            return Ok(OwnerNumber::Verified {
                number: PhoneNumber::parse(&number)?,
            });
        }
        let pending = self.state.get(KEY_PENDING_NUMBER).await?;
        let expires_at = self.state.get(KEY_PENDING_EXPIRES_AT).await?;
        match (pending, expires_at) {
            (Some(number), Some(expires_at)) => Ok(OwnerNumber::Pending {
                number: PhoneNumber::parse(&number)?,
                expires_at: Timestamp(expires_at.parse().unwrap_or(0)),
            }),
            _ => Ok(OwnerNumber::Unset),
        }
    }

    async fn last_call(&self) -> Result<Option<CaptureCall>, SeamError> {
        let sid = self.state.get(KEY_LAST_CALL_SID).await?;
        let started_at = self.state.get(KEY_LAST_CALL_STARTED_AT).await?;
        match (sid, started_at) {
            (Some(id), Some(started_at)) => Ok(Some(CaptureCall {
                id,
                started_at: Timestamp(started_at.parse().unwrap_or(0)),
            })),
            _ => Ok(None),
        }
    }

    /// Epoch seconds of capture calls started in the last hour, oldest
    /// first, bounded by the hourly ceiling.
    async fn recent_call_starts(&self, now: i64) -> Result<Vec<i64>, SeamError> {
        let raw = self.state.get(KEY_CALL_STARTS).await?.unwrap_or_default();
        Ok(recent_starts(&raw, now, self.policy.calls_per_hour_max))
    }

    async fn clear_pending(&self) -> Result<(), SeamError> {
        self.state.delete(KEY_PENDING_NUMBER).await?;
        self.state.delete(KEY_PENDING_CODE).await?;
        self.state.delete(KEY_PENDING_EXPIRES_AT).await?;
        Ok(())
    }

    async fn record_call(
        &self,
        sid: &str,
        now: i64,
        mut starts: Vec<i64>,
    ) -> Result<(), SeamError> {
        starts.push(now);
        let rendered: Vec<String> = starts.iter().map(i64::to_string).collect();
        self.state.put(KEY_CALL_STARTS, &rendered.join(",")).await?;
        self.state.put(KEY_LAST_CALL_SID, sid).await?;
        self.state
            .put(KEY_LAST_CALL_STARTED_AT, &now.to_string())
            .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl CallCapture for CaptureService {
    async fn status(&self) -> Result<CaptureStatus, SeamError> {
        let recordings_archived = u64::try_from(self.archive.list()?.len()).unwrap_or(u64::MAX);
        Ok(CaptureStatus {
            capture_number: self.policy.capture_number.clone(),
            owner_number: self.owner_number().await?,
            last_call: self.last_call().await?,
            recordings_archived,
        })
    }

    async fn set_owner_number(&self, number: PhoneNumber) -> Result<CaptureStatus, SeamError> {
        if number == self.policy.capture_number {
            return Err(SeamError::Invalid(
                "the owner's number cannot be the capture number itself".to_string(),
            ));
        }
        let now = self.clock.now_epoch();
        let starts = self.recent_call_starts(now).await?;
        check_call_allowed(&starts, now, self.policy.calls_per_hour_max)?;
        let code = verification_code();
        let twiml = twiml_verification(&code);
        let sid = self
            .twilio
            .create_call(
                self.policy.capture_number.as_str(),
                number.as_str(),
                &twiml,
                self.policy.ring_secs,
            )
            .await?;
        self.state.delete(KEY_OWNER_NUMBER).await?;
        self.state.put(KEY_PENDING_NUMBER, number.as_str()).await?;
        self.state.put(KEY_PENDING_CODE, &code).await?;
        let expires_at = now + self.policy.verification_ttl_secs;
        self.state
            .put(KEY_PENDING_EXPIRES_AT, &expires_at.to_string())
            .await?;
        self.record_call(&sid, now, starts).await?;
        self.status().await
    }

    async fn verify_owner_number(&self, code: &str) -> Result<CaptureStatus, SeamError> {
        let OwnerNumber::Pending { number, expires_at } = self.owner_number().await? else {
            return Err(SeamError::Refused(
                "no verification is pending; set the owner number first".to_string(),
            ));
        };
        let now = self.clock.now_epoch();
        if now > expires_at.0 {
            self.clear_pending().await?;
            return Err(SeamError::Refused(
                "the verification code has expired; set the owner number again".to_string(),
            ));
        }
        let expected = self.state.get(KEY_PENDING_CODE).await?.unwrap_or_default();
        if !codes_match(&expected, code) {
            return Err(SeamError::Refused(
                "the verification code does not match".to_string(),
            ));
        }
        self.clear_pending().await?;
        self.state.put(KEY_OWNER_NUMBER, number.as_str()).await?;
        self.status().await
    }

    async fn start(&self) -> Result<CaptureStatus, SeamError> {
        let OwnerNumber::Verified { number } = self.owner_number().await? else {
            return Err(SeamError::Refused(
                "no verified owner number; set and verify one first".to_string(),
            ));
        };
        let now = self.clock.now_epoch();
        let starts = self.recent_call_starts(now).await?;
        check_call_allowed(&starts, now, self.policy.calls_per_hour_max)?;
        let twiml = twiml_capture(&self.policy.notice, self.policy.recording_secs_max);
        let sid = self
            .twilio
            .create_call(
                self.policy.capture_number.as_str(),
                number.as_str(),
                &twiml,
                self.policy.ring_secs,
            )
            .await?;
        self.record_call(&sid, now, starts).await?;
        self.status().await
    }
}

/// Parse the stored start list and keep only the last hour's entries,
/// newest `max` at most.
fn recent_starts(raw: &str, now: i64, max: u32) -> Vec<i64> {
    let mut starts: Vec<i64> = raw
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .filter(|start| now - *start < HOUR_SECS)
        .collect();
    starts.sort_unstable();
    let keep = usize::try_from(max).unwrap_or(usize::MAX);
    if starts.len() > keep {
        starts.drain(..starts.len() - keep);
    }
    starts
}

/// Refuse a call that would follow the last one too closely or pass the
/// hourly ceiling.
fn check_call_allowed(starts: &[i64], now: i64, per_hour_max: u32) -> Result<(), SeamError> {
    if let Some(last) = starts.last()
        && now - *last < CALL_START_GAP_SECS_MIN
    {
        return Err(SeamError::Refused(format!(
            "a capture call was placed {} seconds ago; wait {CALL_START_GAP_SECS_MIN}",
            now - *last
        )));
    }
    if starts.len() >= usize::try_from(per_hour_max).unwrap_or(usize::MAX) {
        return Err(SeamError::Refused(format!(
            "{per_hour_max} capture calls were placed in the last hour; that is the ceiling"
        )));
    }
    Ok(())
}

/// Six random digits, uniformly drawn.
fn verification_code() -> String {
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    let value = u64::from_le_bytes(bytes) % 1_000_000;
    let code = format!("{value:0width$}", width = VERIFICATION_CODE_DIGITS);
    assert_eq!(code.len(), VERIFICATION_CODE_DIGITS);
    code
}

fn codes_match(expected: &str, given: &str) -> bool {
    let given: String = given.chars().filter(char::is_ascii_digit).collect();
    !expected.is_empty() && expected == given
}

/// What the owner hears when verifying: the code, spoken as digits, twice.
pub fn twiml_capture(notice: &str, recording_secs_max: u32) -> String {
    assert!(recording_secs_max >= 1);
    format!(
        "<Response><Say>{}</Say><Record maxLength=\"{recording_secs_max}\" timeout=\"120\" playBeep=\"true\" trim=\"do-not-trim\"/></Response>",
        xml_escape(notice)
    )
}

pub fn twiml_verification(code: &str) -> String {
    let spoken = code
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "<Response><Say>Your inseam verification code is {spoken}. Again: {spoken}.</Say></Response>"
    )
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().take(NOTICE_CHARS_MAX) {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_starts_keep_the_last_hour_bounded() {
        let starts = recent_starts("10,20,5000,4000,garbage", 5000, 3);
        assert_eq!(starts, vec![4000, 5000]);
        let starts = recent_starts("1,2,3,4,5", 10, 3);
        assert_eq!(starts, vec![3, 4, 5]);
        assert!(recent_starts("", 10, 3).is_empty());
    }

    #[test]
    fn calls_are_refused_too_close_together_or_past_the_ceiling() {
        assert!(check_call_allowed(&[], 1000, 2).is_ok());
        assert!(check_call_allowed(&[990], 1000, 2).is_err());
        assert!(check_call_allowed(&[100], 1000, 2).is_ok());
        assert!(check_call_allowed(&[100, 200], 1000, 2).is_err());
    }

    #[test]
    fn verification_codes_are_six_digits_and_match_loosely() {
        let code = verification_code();
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
        assert!(codes_match("123456", "123 456"));
        assert!(!codes_match("123456", "123457"));
        assert!(!codes_match("", ""));
    }

    #[test]
    fn twiml_escapes_the_notice_and_bounds_the_recording() {
        let twiml = twiml_capture("This call <is> being recorded & kept", 7200);
        assert!(twiml.contains("This call &lt;is&gt; being recorded &amp; kept"));
        assert!(twiml.contains("maxLength=\"7200\""));
        let spoken = twiml_verification("123456");
        assert!(spoken.contains("1 2 3 4 5 6"));
    }
}
