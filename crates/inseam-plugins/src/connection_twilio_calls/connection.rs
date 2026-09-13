//! The host side of the connection: a pull that moves finished recordings
//! from Twilio into the local archive (audio, then transcript, then the
//! provider's copy removed), and the `Connection` that serves the archive
//! as sources. Every source of a call shares the recording's SID as its
//! locator stem — `<RE…>/audio.mp3`, `<RE…>/transcript.txt`,
//! `<RE…>/call.json` — the same shape the iOS call-recordings host uses,
//! so downstream tooling reads both alike.
//!
//! The pull runs at the head of every enumeration and is bounded per run:
//! so many recordings ingested, so many transcripts polled. What it cannot
//! finish this run it finishes next run; nothing waits or sleeps.

use std::sync::Arc;
use std::time::SystemTime;

use inseam_kernel::address::{
    Address, ContentLength, Envelope, HostId, Locator, Property, Timestamp, TrustLevel,
};
use inseam_kernel::fragment::Mimetype;
use inseam_seams::SeamError;
use inseam_seams::connection::{Connection, EnumeratedSource};
use inseam_seams::text::{check_line_range, count_lines, slice_lines};

use super::archive::{
    AUDIO_FILE, Archive, RecordingLocator, SIDECAR_FILE, Sidecar, TRANSCRIPT_FILE, TranscriptState,
};
use super::twilio::{RecordingRecord, Sentence, TranscriptStatus, TwilioClient};

pub const SOURCE_TYPE_AUDIO: &str = "call-audio";
pub const SOURCE_TYPE_TRANSCRIPT: &str = "call-transcript";
pub const SOURCE_TYPE_SIDECAR: &str = "call-metadata";

/// Bounds and switches the pull reads from config.
#[derive(Debug, Clone)]
pub struct PullPolicy {
    /// Recordings ingested per pull.
    pub recordings_per_pull_max: u32,
    /// Pending transcripts polled per pull.
    pub transcript_polls_per_pull_max: u32,
    /// The Conversation Intelligence service to transcribe with; none
    /// leaves every recording audio-only.
    pub intelligence_service_sid: Option<String>,
    /// Keep the provider's copy after ingestion. Off by default: the
    /// provider account belongs to the operator.
    pub retain_at_provider: bool,
}

/// What one pull did, for the log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PullReport {
    pub ingested: u32,
    pub transcripts_completed: u32,
    pub transcripts_failed: u32,
    pub deleted_at_provider: u32,
}

pub struct CallsHost {
    host_id: HostId,
    twilio: Arc<TwilioClient>,
    archive: Arc<Archive>,
    policy: PullPolicy,
}

impl CallsHost {
    pub fn new(
        host_id: HostId,
        twilio: Arc<TwilioClient>,
        archive: Arc<Archive>,
        policy: PullPolicy,
    ) -> Self {
        assert!(policy.recordings_per_pull_max >= 1);
        Self {
            host_id,
            twilio,
            archive,
            policy,
        }
    }

    /// Move what Twilio holds into the archive, bounded, and advance every
    /// recording whose ingestion is unfinished.
    pub async fn pull(&self) -> Result<PullReport, SeamError> {
        let mut report = PullReport::default();
        let listed = self
            .twilio
            .list_recordings(self.policy.recordings_per_pull_max)
            .await?;
        let fresh: Vec<&RecordingRecord> = listed
            .iter()
            .filter(|r| !self.archive.has(&r.sid))
            .collect();
        for record in fresh {
            self.ingest(record).await?;
            report.ingested += 1;
        }
        let mut polls: u32 = 0;
        for sidecar in self.archive.list()? {
            let mut sidecar = sidecar;
            if let TranscriptState::Pending { transcript_sid } = sidecar.transcript.clone()
                && polls < self.policy.transcript_polls_per_pull_max
            {
                polls += 1;
                self.poll_transcript(&mut sidecar, &transcript_sid, &mut report)
                    .await?;
            }
            let removable = sidecar.transcript.settled()
                && !sidecar.deleted_at_provider
                && !self.policy.retain_at_provider;
            if removable {
                self.twilio.delete_recording(&sidecar.recording_sid).await?;
                sidecar.deleted_at_provider = true;
                self.archive.write_sidecar(&sidecar)?;
                report.deleted_at_provider += 1;
            }
        }
        Ok(report)
    }

    /// One recording: provenance first, then the bytes, then the
    /// transcript request, so a crash leaves a sidecar that says how far
    /// this got.
    async fn ingest(&self, record: &RecordingRecord) -> Result<(), SeamError> {
        let call = self.twilio.fetch_call(&record.call_sid).await?;
        let mut sidecar = Sidecar {
            recording_sid: record.sid.clone(),
            call_sid: record.call_sid.clone(),
            from: call.from,
            to: call.to,
            started_epoch: call.started_epoch.unwrap_or(record.created_epoch),
            duration_secs: record.duration_secs,
            channels: record.channels,
            audio_bytes: 0,
            transcript: TranscriptState::Disabled,
            deleted_at_provider: false,
        };
        let audio = self.twilio.download_recording_mp3(&record.sid).await?;
        sidecar.audio_bytes = u64::try_from(audio.len()).unwrap_or(u64::MAX);
        self.archive.write_sidecar(&sidecar)?;
        self.archive.write_audio(&record.sid, &audio)?;
        if let Some(service) = &self.policy.intelligence_service_sid {
            sidecar.transcript = match self.twilio.create_transcript(service, &record.sid).await {
                Ok(transcript_sid) => TranscriptState::Pending { transcript_sid },
                Err(error) => TranscriptState::Failed {
                    reason: error.to_string(),
                },
            };
            self.archive.write_sidecar(&sidecar)?;
        }
        Ok(())
    }

    async fn poll_transcript(
        &self,
        sidecar: &mut Sidecar,
        transcript_sid: &str,
        report: &mut PullReport,
    ) -> Result<(), SeamError> {
        match self.twilio.fetch_transcript_status(transcript_sid).await? {
            TranscriptStatus::Queued | TranscriptStatus::InProgress => Ok(()),
            TranscriptStatus::Completed => {
                let sentences = self.twilio.fetch_sentences(transcript_sid).await?;
                self.archive
                    .write_transcript(&sidecar.recording_sid, &render_transcript(&sentences))?;
                sidecar.transcript = TranscriptState::Done;
                self.archive.write_sidecar(sidecar)?;
                report.transcripts_completed += 1;
                Ok(())
            }
            TranscriptStatus::Failed => {
                sidecar.transcript = TranscriptState::Failed {
                    reason: "the provider reported the transcription failed".to_string(),
                };
                self.archive.write_sidecar(sidecar)?;
                report.transcripts_failed += 1;
                Ok(())
            }
        }
    }

    fn address(&self, recording_sid: &str, file: &str) -> Result<Address, SeamError> {
        let locator = Locator::new(RecordingLocator::render(recording_sid, file))
            .map_err(|e| SeamError::failed(e.to_string()))?;
        Ok(Address::new(self.host_id.clone(), locator))
    }

    /// The sources one archived call contributes: audio and metadata
    /// always, the transcript once it exists.
    fn sources_of(
        &self,
        sidecar: &Sidecar,
        observed: Timestamp,
    ) -> Result<Vec<EnumeratedSource>, SeamError> {
        let mut sources = Vec::with_capacity(3);
        let audio_bytes = self
            .archive
            .file_size(&sidecar.recording_sid, AUDIO_FILE)
            .unwrap_or(sidecar.audio_bytes);
        sources.push(EnumeratedSource {
            address: self.address(&sidecar.recording_sid, AUDIO_FILE)?,
            envelope: envelope(
                sidecar,
                SOURCE_TYPE_AUDIO,
                "audio/mpeg",
                ContentLength::Bytes(audio_bytes),
                observed,
            ),
            raw_bytes: audio_bytes,
        });
        if let Some(size) = self
            .archive
            .file_size(&sidecar.recording_sid, TRANSCRIPT_FILE)
        {
            let text = String::from_utf8_lossy(
                &self
                    .archive
                    .read_file(&sidecar.recording_sid, TRANSCRIPT_FILE)?,
            )
            .into_owned();
            sources.push(EnumeratedSource {
                address: self.address(&sidecar.recording_sid, TRANSCRIPT_FILE)?,
                envelope: envelope(
                    sidecar,
                    SOURCE_TYPE_TRANSCRIPT,
                    "text/plain",
                    ContentLength::Lines(count_lines(&text)),
                    observed,
                ),
                raw_bytes: size,
            });
        }
        let sidecar_bytes = self
            .archive
            .file_size(&sidecar.recording_sid, SIDECAR_FILE)
            .unwrap_or(0);
        sources.push(EnumeratedSource {
            address: self.address(&sidecar.recording_sid, SIDECAR_FILE)?,
            envelope: envelope(
                sidecar,
                SOURCE_TYPE_SIDECAR,
                "application/json",
                ContentLength::Bytes(sidecar_bytes),
                observed,
            ),
            raw_bytes: sidecar_bytes,
        });
        Ok(sources)
    }

    fn locator_of(&self, address: &Address) -> Result<RecordingLocator, SeamError> {
        if address.host != self.host_id {
            return Err(SeamError::failed(format!(
                "address {address} names host `{}`, not this call-capture host",
                address.host
            )));
        }
        RecordingLocator::parse(address.locator.as_str())
    }
}

#[async_trait::async_trait]
impl Connection for CallsHost {
    /// The whole host is the one scope. The pull runs first; if the
    /// provider is unreachable the archive is still listed, so an outage
    /// never makes archived calls vanish from the catalog.
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        if !root.is_empty() {
            return Err(SeamError::Invalid(format!(
                "the call-capture host has one scope, the whole host (\"\"); `{root}` names nothing"
            )));
        }
        match self.pull().await {
            Ok(report) => tracing::info!(?report, "call capture pull"),
            Err(error) => {
                tracing::warn!(%error, "call capture pull failed; listing the archive as it stands")
            }
        }
        let observed = Timestamp::from(SystemTime::now());
        let mut sources = Vec::new();
        for sidecar in self.archive.list()? {
            sources.extend(self.sources_of(&sidecar, observed)?);
        }
        Ok(sources)
    }

    fn locator_prefix(&self, root: &str) -> Option<String> {
        root.is_empty().then(String::new)
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let bytes = self.read_bytes(address).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        check_line_range(start, end)?;
        let text = self.read_text(address).await?;
        slice_lines(&text, start, end)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let locator = self.locator_of(address)?;
        if !self.archive.has(&locator.recording_sid) {
            return Err(SeamError::UnknownSource(address.clone()));
        }
        self.archive.read_file(&locator.recording_sid, locator.file)
    }

    async fn describe(&self, address: &Address) -> Result<Envelope, SeamError> {
        let locator = self.locator_of(address)?;
        let sidecar = self.archive.read_sidecar(&locator.recording_sid)?;
        let observed = Timestamp::from(SystemTime::now());
        self.sources_of(&sidecar, observed)?
            .into_iter()
            .find(|s| s.address == *address)
            .map(|s| s.envelope)
            .ok_or_else(|| SeamError::UnknownSource(address.clone()))
    }
}

fn envelope(
    sidecar: &Sidecar,
    source_type: &str,
    mimetype: &str,
    length: ContentLength,
    observed: Timestamp,
) -> Envelope {
    let started = Timestamp(sidecar.started_epoch);
    Envelope {
        source_type: source_type.to_string(),
        content_type: Mimetype::parse(mimetype).expect("literal mimetype is valid"),
        length,
        created: Some(started),
        modified: Some(started),
        observed,
        properties: vec![
            claimed("call.from", &sidecar.from),
            claimed("call.to", &sidecar.to),
            claimed("call.duration_secs", &sidecar.duration_secs.to_string()),
        ],
        hint: Some(hint(sidecar)),
        content_digest: None,
        facets: Vec::new(),
    }
}

/// The provider's word about the call: who it dialed and for how long.
/// Claimed, never verified — the owner may have merged anyone in.
fn claimed(key: &str, value: &str) -> Property {
    Property {
        key: key.to_string(),
        value: value.to_string(),
        trust: TrustLevel::Claimed,
    }
}

fn hint(sidecar: &Sidecar) -> String {
    let minutes = sidecar.duration_secs.div_ceil(60);
    format!(
        "Call capture {} ({} min)",
        inseam_seams::dates::ymd(Timestamp(sidecar.started_epoch)),
        minutes
    )
}

/// One line per sentence: `[mm:ss] channel N: words`. The channel is the
/// most speaker attribution a merged call can honestly carry — everyone on
/// the owner's side of the merge arrives mixed on one channel.
pub fn render_transcript(sentences: &[Sentence]) -> String {
    let mut out = String::new();
    for sentence in sentences {
        let total = sentence.start_secs.max(0.0) as u64;
        out.push_str(&format!(
            "[{:02}:{:02}] channel {}: {}\n",
            total / 60,
            total % 60,
            sentence.channel,
            sentence.text
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts_render_one_timestamped_line_per_sentence() {
        let text = render_transcript(&[
            Sentence {
                channel: 1,
                start_secs: 0.4,
                text: "Hello there.".into(),
            },
            Sentence {
                channel: 2,
                start_secs: 61.0,
                text: "Hi.".into(),
            },
        ]);
        assert_eq!(
            text,
            "[00:00] channel 1: Hello there.\n[01:01] channel 2: Hi.\n"
        );
        assert_eq!(render_transcript(&[]), "");
    }

    #[test]
    fn hints_name_the_day_and_rounded_minutes() {
        let sidecar = Sidecar {
            recording_sid: "RE1".into(),
            call_sid: "CA1".into(),
            from: "+15550000001".into(),
            to: "+14155550123".into(),
            started_epoch: 1_789_236_323,
            duration_secs: 61,
            channels: 1,
            audio_bytes: 0,
            transcript: TranscriptState::Disabled,
            deleted_at_provider: false,
        };
        assert_eq!(hint(&sidecar), "Call capture 2026-09-12 (2 min)");
        let env = envelope(
            &sidecar,
            SOURCE_TYPE_AUDIO,
            "audio/mpeg",
            ContentLength::Bytes(1),
            Timestamp(0),
        );
        assert_eq!(env.properties.len(), 3);
        assert_eq!(env.properties[0].trust, TrustLevel::Claimed);
    }
}
