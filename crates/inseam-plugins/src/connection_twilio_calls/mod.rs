//! Call capture through Twilio (`design/call-capture.md`): one plugin that
//! is both a **connection** — the calls it captured, served from a local
//! archive as a host of kind `twilio-calls` — and the **`call-capture`
//! provider** that verifies the owner's phone and places the call the
//! owner merges into their conversation.
//!
//! Credentials are an API key scoped to one Twilio account, read from the
//! environment like every secret a node holds. On a hosted node that
//! account is a per-tenant subaccount the control plane provisioned; a
//! self-hoster's own account plays the same role. Either way the key
//! reaches exactly one account's numbers and recordings, and nothing else.

mod archive;
mod capture;
mod connection;
mod twilio;

use std::sync::Arc;
use std::time::Duration;

use url::Url;

use inseam_kernel::substrate::{
    ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, STATE, SecretNeed, parse_config,
};
use inseam_seams::call_capture::{CALL_CAPTURE, CallCapture, PhoneNumber};
use inseam_seams::connection::{
    Capabilities, HostDescription, HostKind, Registration, derive_host_id, register_as_effect,
};

use archive::Archive;
use capture::{CapturePolicy, CaptureService, NOTICE_CHARS_MAX, STATE_VERSION, SystemClock};
use connection::{CallsHost, PullPolicy};
use twilio::{Credentials, TwilioClient};

pub use capture::{twiml_capture, twiml_verification};
pub use connection::render_transcript;

/// Twilio's ceiling on `<Record maxLength>`: four hours, which also matches
/// the recording ceiling the iOS app keeps.
pub const RECORDING_SECS_CEILING: u32 = 14_400;
/// Twilio's ceiling on how long a call may ring before it is unanswered.
pub const RING_SECS_CEILING: u32 = 600;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TwilioCallsConfig {
    /// Environment variable holding the (sub)account SID the key is
    /// scoped to.
    pub account_sid_env: String,
    /// Environment variables holding the API key's SID and secret.
    pub api_key_sid_env: String,
    pub api_key_secret_env: String,
    /// Environment variable holding the E.164 number this account owns:
    /// what the node dials from, and what the owner can dial into.
    pub phone_number_env: String,
    /// Environment variable holding the Conversation Intelligence service
    /// SID to transcribe with. Unset or empty leaves recordings audio-only.
    pub intelligence_service_sid_env: String,
    /// What the answering party hears before recording starts.
    pub notice: String,
    /// Longest one capture recording may run, in seconds.
    pub recording_secs_max: u32,
    /// How long the owner's phone rings before the call is given up.
    pub ring_secs: u32,
    /// Capture and verification calls the node will place per hour.
    pub calls_per_hour_max: u32,
    /// How long a spoken verification code stays valid.
    pub verification_ttl_secs: i64,
    /// Recordings ingested per sweep, and pending transcripts polled.
    pub recordings_per_pull_max: u32,
    pub transcript_polls_per_pull_max: u32,
    /// Most bytes one recording download may be. MP3 at 32 kbit/s is
    /// about 14 MiB an hour, so the default covers the four-hour ceiling.
    pub recording_bytes_max: u64,
    pub timeout_ms: u64,
    /// Keep Twilio's copy of a recording after it is archived here.
    pub retain_at_provider: bool,
    /// Twilio's API origins; overridden only by tests.
    pub api_base: String,
    pub intelligence_base: String,
}

impl Default for TwilioCallsConfig {
    fn default() -> Self {
        Self {
            account_sid_env: "TWILIO_ACCOUNT_SID".to_string(),
            api_key_sid_env: "TWILIO_API_KEY_SID".to_string(),
            api_key_secret_env: "TWILIO_API_KEY_SECRET".to_string(),
            phone_number_env: "TWILIO_PHONE_NUMBER".to_string(),
            intelligence_service_sid_env: "TWILIO_INTELLIGENCE_SERVICE_SID".to_string(),
            notice: "This call is being recorded by inseam for the person who added this line."
                .to_string(),
            recording_secs_max: 7_200,
            ring_secs: 30,
            calls_per_hour_max: 6,
            verification_ttl_secs: 600,
            recordings_per_pull_max: 25,
            transcript_polls_per_pull_max: 25,
            recording_bytes_max: 64 * 1024 * 1024,
            timeout_ms: 60_000,
            retain_at_provider: false,
            api_base: "https://api.twilio.com".to_string(),
            intelligence_base: "https://intelligence.twilio.com".to_string(),
        }
    }
}

impl TwilioCallsConfig {
    /// Every bound stated positively, checked once at build.
    fn validate(&self) -> Result<(), String> {
        if !(1..=RECORDING_SECS_CEILING).contains(&self.recording_secs_max) {
            return Err(format!(
                "recording_secs_max must be 1..={RECORDING_SECS_CEILING}"
            ));
        }
        if !(1..=RING_SECS_CEILING).contains(&self.ring_secs) {
            return Err(format!("ring_secs must be 1..={RING_SECS_CEILING}"));
        }
        if self.calls_per_hour_max == 0 {
            return Err("calls_per_hour_max must be at least 1".to_string());
        }
        if self.verification_ttl_secs <= 0 {
            return Err("verification_ttl_secs must be at least 1".to_string());
        }
        if self.recordings_per_pull_max == 0 {
            return Err("recordings_per_pull_max must be at least 1".to_string());
        }
        if self.recording_bytes_max == 0 || self.timeout_ms == 0 {
            return Err("recording_bytes_max and timeout_ms must be at least 1".to_string());
        }
        if self.notice.trim().is_empty() || self.notice.chars().count() > NOTICE_CHARS_MAX {
            return Err(format!("notice must be 1..={NOTICE_CHARS_MAX} characters"));
        }
        for (name, env) in [
            ("account_sid_env", &self.account_sid_env),
            ("api_key_sid_env", &self.api_key_sid_env),
            ("api_key_secret_env", &self.api_key_secret_env),
            ("phone_number_env", &self.phone_number_env),
        ] {
            if env.trim().is_empty() {
                return Err(format!("{name} must name an environment variable"));
            }
        }
        Ok(())
    }
}

pub fn twilio_calls_host_kind() -> HostKind {
    HostKind::new("twilio-calls").expect("literal host kind is valid")
}

pub struct TwilioCallsFactory;

impl inseam_kernel::substrate::PluginFactory for TwilioCallsFactory {
    fn name(&self) -> &str {
        "connection-twilio-calls"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        let config: TwilioCallsConfig = parse_config(config)?;
        config.validate().map_err(PluginError)?;
        Ok(Box::new(TwilioCallsPlugin { config }))
    }
}

pub struct TwilioCallsPlugin {
    config: TwilioCallsConfig,
}

/// What the environment supplied, parsed once.
struct Environment {
    credentials: Credentials,
    phone_number: PhoneNumber,
    intelligence_service_sid: Option<String>,
}

fn read_environment(config: &TwilioCallsConfig) -> Result<Environment, PluginError> {
    let required = |env: &str| -> Result<String, PluginError> {
        std::env::var(env)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| PluginError(format!("environment variable {env} is not set")))
    };
    let credentials = Credentials {
        account_sid: required(&config.account_sid_env)?,
        api_key_sid: required(&config.api_key_sid_env)?,
        api_key_secret: required(&config.api_key_secret_env)?,
    };
    let phone_number = PhoneNumber::parse(&required(&config.phone_number_env)?)
        .map_err(|e| PluginError(format!("{}: {e}", config.phone_number_env)))?;
    let intelligence_service_sid = std::env::var(&config.intelligence_service_sid_env)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    Ok(Environment {
        credentials,
        phone_number,
        intelligence_service_sid,
    })
}

#[async_trait::async_trait]
impl Plugin for TwilioCallsPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("connections"), Inject::required("state")];
        Manifest {
            name: "connection-twilio-calls",
            inject: INJECT,
            provides: &["call-capture"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let environment = read_environment(&self.config)?;
        let api_base = Url::parse(&self.config.api_base).map_err(|e| PluginError(e.to_string()))?;
        let intelligence_base =
            Url::parse(&self.config.intelligence_base).map_err(|e| PluginError(e.to_string()))?;
        let twilio = Arc::new(
            TwilioClient::new(
                environment.credentials,
                api_base,
                intelligence_base,
                self.config.recording_bytes_max,
                Duration::from_millis(self.config.timeout_ms),
            )
            .map_err(|e| PluginError(e.to_string()))?,
        );
        let archive = Arc::new(Archive::new(cx.data_dir().join(cx.entry_id())));
        let host_id = derive_host_id(&twilio_calls_host_kind(), twilio.account_sid());

        let host = CallsHost::new(
            host_id.clone(),
            Arc::clone(&twilio),
            Arc::clone(&archive),
            PullPolicy {
                recordings_per_pull_max: self.config.recordings_per_pull_max,
                transcript_polls_per_pull_max: self.config.transcript_polls_per_pull_max,
                intelligence_service_sid: environment.intelligence_service_sid,
                retain_at_provider: self.config.retain_at_provider,
            },
        );
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                host: HostDescription {
                    id: host_id,
                    kind: twilio_calls_host_kind(),
                    display_name: format!("Call capture {}", environment.phone_number),
                },
                capabilities: Capabilities::READ_ONLY,
                roots: Vec::new(),
                connection: Arc::new(host),
            },
        )?;

        let state = cx
            .get(&STATE)?
            .namespace(&format!("twilio-calls:{}", cx.entry_id()), STATE_VERSION)
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        let capture = CaptureService::new(
            twilio,
            state,
            archive,
            CapturePolicy {
                capture_number: environment.phone_number,
                notice: self.config.notice.clone(),
                recording_secs_max: self.config.recording_secs_max,
                ring_secs: self.config.ring_secs,
                calls_per_hour_max: self.config.calls_per_hour_max,
                verification_ttl_secs: self.config.verification_ttl_secs,
            },
            Arc::new(SystemClock),
        );
        cx.provide(
            &CALL_CAPTURE,
            Arc::new(capture) as Arc<dyn CallCapture>,
            Facts::new(),
        )?;
        Ok(())
    }

    fn secrets(&self) -> Vec<SecretNeed> {
        let need = |env: &str, purpose: &str| SecretNeed {
            env: env.to_string(),
            purpose: purpose.to_string(),
        };
        vec![
            need(
                &self.config.account_sid_env,
                "The Twilio account that owns the call-capture number; the node reaches this account only.",
            ),
            need(
                &self.config.api_key_sid_env,
                "The SID of a Twilio API key scoped to that account.",
            ),
            need(
                &self.config.api_key_secret_env,
                "The secret of that API key — how the node places calls and pulls recordings.",
            ),
            need(
                &self.config.phone_number_env,
                "The account's phone number in E.164 form: what the node dials from and what you merge into a call.",
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::substrate::PluginFactory as _;

    #[test]
    fn the_default_config_is_valid_and_bounds_hold() {
        assert!(TwilioCallsConfig::default().validate().is_ok());
        let mut config = TwilioCallsConfig::default();
        config.recording_secs_max = RECORDING_SECS_CEILING + 1;
        assert!(config.validate().is_err());
        config = TwilioCallsConfig {
            calls_per_hour_max: 0,
            ..TwilioCallsConfig::default()
        };
        assert!(config.validate().is_err());
        config = TwilioCallsConfig {
            notice: String::new(),
            ..TwilioCallsConfig::default()
        };
        assert!(config.validate().is_err());
        config = TwilioCallsConfig {
            phone_number_env: " ".to_string(),
            ..TwilioCallsConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn the_factory_refuses_unknown_keys_and_declares_four_secrets() {
        let mut table = toml::Table::new();
        table.insert("nope".into(), toml::Value::Boolean(true));
        assert!(TwilioCallsFactory.build(&table).is_err());
        let plugin = TwilioCallsFactory
            .build(&toml::Table::new())
            .expect("defaults build");
        let secrets = plugin.secrets();
        assert_eq!(secrets.len(), 4);
        assert_eq!(secrets[0].env, "TWILIO_ACCOUNT_SID");
        assert_eq!(secrets[3].env, "TWILIO_PHONE_NUMBER");
    }

    #[test]
    fn the_host_id_derives_from_the_account() {
        let a = derive_host_id(&twilio_calls_host_kind(), "AC123");
        let b = derive_host_id(&twilio_calls_host_kind(), "ac123");
        assert_eq!(a, b);
        assert!(a.as_str().starts_with("twilio-calls-"));
    }
}
