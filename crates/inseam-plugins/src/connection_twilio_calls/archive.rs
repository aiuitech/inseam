//! The local archive of captured calls: one directory per recording under
//! the entry's own data directory, holding the audio, the transcript once
//! it exists, and a sidecar that records where the call came from and how
//! far ingestion got. The node stores these bytes — unlike every other
//! remote host, whose content the index only references — because the
//! recording is deleted from the provider the moment it is safely here:
//! the provider's account is the operator's, and a tenant's calls must not
//! rest where the operator can read them (`design/call-capture.md`).
//!
//! Everything in here is filesystem work with no network; the pull that
//! fills it lives in `connection.rs`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use inseam_seams::SeamError;

/// Recording directories one listing will read: ten thousand calls is
/// years of daily use, and the bound keeps a corrupted directory from
/// becoming an unbounded walk.
pub const RECORDINGS_LISTED_MAX: usize = 10_000;

pub const AUDIO_FILE: &str = "audio.mp3";
pub const TRANSCRIPT_FILE: &str = "transcript.txt";
pub const SIDECAR_FILE: &str = "call.json";

/// Where a recording's transcript stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TranscriptState {
    /// Transcription is not configured for this node.
    Disabled,
    /// Requested from the provider; polled on later pulls.
    Pending {
        transcript_sid: String,
    },
    /// Written to `transcript.txt`.
    Done,
    Failed {
        reason: String,
    },
}

impl TranscriptState {
    /// Whether ingestion is finished as far as transcription goes, so the
    /// provider's copy may be removed.
    pub fn settled(&self) -> bool {
        match self {
            Self::Disabled | Self::Done | Self::Failed { .. } => true,
            Self::Pending { .. } => false,
        }
    }
}

/// What the archive knows about one captured call. Written before the
/// audio so a crash mid-download leaves provenance beside a partial file,
/// and rewritten as transcription and deletion progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecar {
    pub recording_sid: String,
    pub call_sid: String,
    /// The provider number that placed or answered the capture leg.
    pub from: String,
    /// The owner's phone, as the provider saw it.
    pub to: String,
    pub started_epoch: i64,
    pub duration_secs: u64,
    pub channels: u32,
    pub audio_bytes: u64,
    pub transcript: TranscriptState,
    /// Whether the provider's copy has been removed.
    pub deleted_at_provider: bool,
}

pub struct Archive {
    root: PathBuf,
}

impl Archive {
    /// `root` is the entry's directory under the node's data dir; created
    /// on first use.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn recording_dir(&self, recording_sid: &str) -> PathBuf {
        self.root.join("recordings").join(recording_sid)
    }

    pub fn has(&self, recording_sid: &str) -> bool {
        self.recording_dir(recording_sid)
            .join(SIDECAR_FILE)
            .is_file()
    }

    /// Every sidecar in the archive, oldest call first, bounded.
    pub fn list(&self) -> Result<Vec<Sidecar>, SeamError> {
        let dir = self.root.join("recordings");
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let entries = std::fs::read_dir(&dir).map_err(|e| io_error(&dir, e))?;
        let mut sidecars = Vec::new();
        for entry in entries.take(RECORDINGS_LISTED_MAX) {
            let entry = entry.map_err(|e| io_error(&dir, e))?;
            let sidecar_path = entry.path().join(SIDECAR_FILE);
            if !sidecar_path.is_file() {
                continue;
            }
            sidecars.push(read_sidecar(&sidecar_path)?);
        }
        sidecars.sort_by(|a, b| {
            a.started_epoch
                .cmp(&b.started_epoch)
                .then_with(|| a.recording_sid.cmp(&b.recording_sid))
        });
        Ok(sidecars)
    }

    pub fn read_sidecar(&self, recording_sid: &str) -> Result<Sidecar, SeamError> {
        read_sidecar(&self.recording_dir(recording_sid).join(SIDECAR_FILE))
    }

    /// Write (or rewrite) a sidecar atomically: to a temporary name, then
    /// renamed over the old one, so a reader never sees a half file.
    pub fn write_sidecar(&self, sidecar: &Sidecar) -> Result<(), SeamError> {
        let dir = self.recording_dir(&sidecar.recording_sid);
        std::fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
        let bytes = serde_json::to_vec_pretty(sidecar)
            .map_err(|e| SeamError::failed(format!("serializing sidecar: {e}")))?;
        write_atomic(&dir.join(SIDECAR_FILE), &bytes)
    }

    pub fn write_audio(&self, recording_sid: &str, bytes: &[u8]) -> Result<(), SeamError> {
        write_atomic(&self.recording_dir(recording_sid).join(AUDIO_FILE), bytes)
    }

    pub fn write_transcript(&self, recording_sid: &str, text: &str) -> Result<(), SeamError> {
        write_atomic(
            &self.recording_dir(recording_sid).join(TRANSCRIPT_FILE),
            text.as_bytes(),
        )
    }

    pub fn read_file(&self, recording_sid: &str, file: &str) -> Result<Vec<u8>, SeamError> {
        let path = self.recording_dir(recording_sid).join(file);
        std::fs::read(&path).map_err(|e| io_error(&path, e))
    }

    pub fn file_size(&self, recording_sid: &str, file: &str) -> Option<u64> {
        std::fs::metadata(self.recording_dir(recording_sid).join(file))
            .ok()
            .map(|m| m.len())
    }
}

fn read_sidecar(path: &Path) -> Result<Sidecar, SeamError> {
    let bytes = std::fs::read(path).map_err(|e| io_error(path, e))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| SeamError::failed(format!("{}: sidecar is not readable: {e}", path.display())))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SeamError> {
    let parent = path
        .parent()
        .ok_or_else(|| SeamError::failed(format!("{} has no parent", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes).map_err(|e| io_error(&temporary, e))?;
    std::fs::rename(&temporary, path).map_err(|e| io_error(path, e))
}

fn io_error(path: &Path, error: std::io::Error) -> SeamError {
    SeamError::failed(format!("{}: {error}", path.display()))
}

/// A locator on this host: `<recording sid>/<file>`, where the file is one
/// of the three the archive keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingLocator {
    pub recording_sid: String,
    pub file: &'static str,
}

impl RecordingLocator {
    pub fn parse(locator: &str) -> Result<Self, SeamError> {
        let Some((sid, file)) = locator.split_once('/') else {
            return Err(SeamError::Invalid(format!(
                "locator `{locator}` is not `<recording>/<file>`"
            )));
        };
        let sid_ok = sid.starts_with("RE")
            && sid.len() == 34
            && sid.bytes().all(|b| b.is_ascii_alphanumeric());
        if !sid_ok {
            return Err(SeamError::Invalid(format!(
                "locator `{locator}` does not name a recording"
            )));
        }
        let file = match file {
            AUDIO_FILE => AUDIO_FILE,
            TRANSCRIPT_FILE => TRANSCRIPT_FILE,
            SIDECAR_FILE => SIDECAR_FILE,
            other => {
                return Err(SeamError::Invalid(format!(
                    "locator `{locator}`: `{other}` is not a file this host serves"
                )));
            }
        };
        Ok(Self {
            recording_sid: sid.to_string(),
            file,
        })
    }

    pub fn render(recording_sid: &str, file: &str) -> String {
        format!("{recording_sid}/{file}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidecar(sid: &str, started: i64) -> Sidecar {
        Sidecar {
            recording_sid: sid.to_string(),
            call_sid: "CA1".into(),
            from: "+15550000001".into(),
            to: "+14155550123".into(),
            started_epoch: started,
            duration_secs: 60,
            channels: 1,
            audio_bytes: 3,
            transcript: TranscriptState::Disabled,
            deleted_at_provider: false,
        }
    }

    #[test]
    fn sidecars_round_trip_and_list_oldest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = Archive::new(dir.path().to_path_buf());
        assert!(archive.list().expect("empty").is_empty());
        archive.write_sidecar(&sidecar("RE2", 200)).expect("writes");
        archive.write_sidecar(&sidecar("RE1", 100)).expect("writes");
        archive.write_audio("RE1", b"mp3").expect("writes");
        let listed = archive.list().expect("lists");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].recording_sid, "RE1");
        assert!(archive.has("RE1"));
        assert!(!archive.has("RE9"));
        assert_eq!(archive.read_file("RE1", AUDIO_FILE).expect("reads"), b"mp3");
        assert_eq!(archive.file_size("RE1", AUDIO_FILE), Some(3));
        assert_eq!(archive.file_size("RE1", TRANSCRIPT_FILE), None);
        assert!(!dir.path().join("recordings/RE1/call.tmp").exists());
    }

    #[test]
    fn locators_name_a_recording_and_one_of_its_files() {
        let sid = format!("RE{}", "a".repeat(32));
        let locator = RecordingLocator::parse(&format!("{sid}/audio.mp3")).expect("parses");
        assert_eq!(locator.recording_sid, sid);
        assert_eq!(locator.file, AUDIO_FILE);
        assert_eq!(
            RecordingLocator::render(&sid, TRANSCRIPT_FILE),
            format!("{sid}/transcript.txt")
        );
        for bad in ["RE1/audio.mp3", "nope", "../x/audio.mp3"] {
            assert!(
                RecordingLocator::parse(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(RecordingLocator::parse(&format!("{sid}/secret.txt")).is_err());
    }

    #[test]
    fn transcript_states_know_when_ingestion_is_settled() {
        assert!(TranscriptState::Disabled.settled());
        assert!(TranscriptState::Done.settled());
        assert!(TranscriptState::Failed { reason: "x".into() }.settled());
        assert!(
            !TranscriptState::Pending {
                transcript_sid: "GT1".into()
            }
            .settled()
        );
    }
}
