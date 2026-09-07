//! Composition edits from inside the plugin tree (`design/composition.md`).
//!
//! The kernel reconciles the composition at boot and on every edit, and a
//! running node's composition is edited by an owner operation — install a
//! loaded plugin — that runs *inside* the tree, where nothing can hold the
//! kernel. So the plugin asks instead of acting: it submits a
//! [`CompositionEdit`] through the kernel-provided `composition` service,
//! and the distribution that owns the kernel and the composition file takes
//! the edit off [`CompositionEdits`], applies it with
//! [`Kernel::apply_composition_edit`], and replies with the fiber snapshot.
//! Applying is the boot path, not a second one: append to the overlay file,
//! reconcile the layered composition, and roll the file back if the new
//! entry fails to activate — so the file stays the truth a fresh boot would
//! converge to.

use std::path::Path;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::composition::{Composition, Entry};
use super::error::SubstrateError;
use super::fiber::{FiberState, FiberView};
use super::kernel::Kernel;
use super::service::ServiceKey;

/// The kernel-provided service a plugin injects to edit the composition.
pub const COMPOSITION: ServiceKey<CompositionEditor> = ServiceKey::new("composition");

/// Edits queued but not yet taken by the distribution. Edits are owner
/// actions, one at a time; a full queue means nobody is applying them.
const EDITS_QUEUED_MAX: usize = 8;

/// Longest a submitter waits for the distribution to apply an edit: long
/// enough for a first-sighting admission run, short enough that a
/// distribution not servicing edits fails loudly instead of hanging.
pub const EDIT_TIMEOUT: Duration = Duration::from_secs(60);

/// One change to the node's composition overlay.
#[derive(Debug, Clone, PartialEq)]
pub enum CompositionEdit {
    /// Append an entry and reconcile. An entry whose fiber fails to
    /// activate is rolled back, file and tree alike, and the failure is
    /// the reply.
    Mount(Entry),
    /// Patch entries by id — a settings write: each entry's `config`
    /// replaces its target's wholesale and `disabled` sets the toggle
    /// ([`Composition::configure_entry`]). The overlay is re-serialized,
    /// the tree reconciled, and if any patched entry lands failed or the
    /// composition refuses the result, file and tree go back.
    Configure(Vec<Entry>),
    /// No change: answer with the current snapshot.
    Inspect,
}

/// Entries one `Configure` may patch — every first-party entry with room
/// to spare; a larger patch is not a settings write.
pub const CONFIGURE_ENTRIES_MAX: usize = 64;

/// The reply to one edit: every fiber as it stands afterwards, and the
/// composition they were reconciled against — the distribution base with
/// the overlay layered over it — so a settings projection reads what runs.
#[derive(Debug, Clone)]
pub struct CompositionSnapshot {
    pub fibers: Vec<FiberView>,
    pub composition: Composition,
}

pub type EditOutcome = Result<CompositionSnapshot, SubstrateError>;

/// An edit a plugin submitted, waiting for the distribution to apply it.
pub struct PendingEdit {
    edit: CompositionEdit,
    reply: oneshot::Sender<EditOutcome>,
}

impl PendingEdit {
    pub fn edit(&self) -> &CompositionEdit {
        &self.edit
    }

    /// Hand the outcome back to the submitter. A submitter that gave up
    /// (timed out) is not an error here; the edit was applied regardless.
    pub fn reply(self, outcome: EditOutcome) {
        let _ = self.reply.send(outcome);
    }
}

/// The submitting end, bound by the kernel as the `composition` service.
pub struct CompositionEditor {
    sender: mpsc::Sender<PendingEdit>,
}

impl CompositionEditor {
    pub(crate) fn pair() -> (Self, CompositionEdits) {
        let (sender, receiver) = mpsc::channel(EDITS_QUEUED_MAX);
        (Self { sender }, CompositionEdits { receiver })
    }

    /// Submit an edit and wait for the distribution to apply it.
    pub async fn submit(&self, edit: CompositionEdit) -> EditOutcome {
        let (reply, outcome) = oneshot::channel();
        self.sender
            .try_send(PendingEdit { edit, reply })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => SubstrateError::EditQueueFull,
                mpsc::error::TrySendError::Closed(_) => SubstrateError::EditsUnserviced,
            })?;
        match tokio::time::timeout(EDIT_TIMEOUT, outcome).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_dropped)) => Err(SubstrateError::EditsUnserviced),
            Err(_elapsed) => Err(SubstrateError::EditsUnserviced),
        }
    }
}

/// The applying end: the distribution that owns the kernel takes this once
/// ([`Kernel::take_composition_edits`]) and services it beside its
/// transport.
pub struct CompositionEdits {
    receiver: mpsc::Receiver<PendingEdit>,
}

impl CompositionEdits {
    /// The next submitted edit; `None` once the kernel — which holds the
    /// submitting end — is gone.
    pub async fn next(&mut self) -> Option<PendingEdit> {
        self.receiver.recv().await
    }
}

/// One overlay change ready to apply: the file's previous text, the text
/// to write, and the entries whose fibers must not land failed.
struct OverlayChange {
    previous: Option<String>,
    next: String,
    watched: Vec<String>,
}

impl Kernel {
    /// Apply one edit against the node's composition: `base` is the
    /// distribution's layer, `overlay_path` the node's file. The overlay is
    /// the only layer edited; the reply is the snapshot afterwards.
    pub async fn apply_composition_edit(
        &mut self,
        base: &Composition,
        overlay_path: &Path,
        edit: &CompositionEdit,
    ) -> EditOutcome {
        let previous = read_overlay(overlay_path)?;
        let before = layer(base, previous.as_deref(), overlay_path)?;
        let change = match edit {
            CompositionEdit::Inspect => {
                return Ok(self.snapshot(before));
            }
            CompositionEdit::Mount(entry) => {
                if before.resolved().iter().any(|e| e.id == entry.id) {
                    return Err(SubstrateError::EntryExists(entry.id.clone()));
                }
                OverlayChange {
                    next: append_entry(previous.as_deref().unwrap_or_default(), entry),
                    previous,
                    watched: vec![entry.id.clone()],
                }
            }
            CompositionEdit::Configure(patches) => OverlayChange {
                next: configure_entries(previous.as_deref(), overlay_path, patches)?,
                previous,
                watched: patches.iter().map(|patch| patch.id.clone()).collect(),
            },
        };
        match self
            .apply_overlay_change(base, overlay_path, &before, change)
            .await
        {
            Ok(after) => Ok(self.snapshot(after)),
            Err((entries, reason)) => Err(match edit {
                CompositionEdit::Mount(entry) => SubstrateError::MountFailed {
                    entry: entry.id.clone(),
                    reason,
                },
                CompositionEdit::Configure(_) | CompositionEdit::Inspect => {
                    SubstrateError::ConfigureFailed { entries, reason }
                }
            }),
        }
    }

    fn snapshot(&self, composition: Composition) -> CompositionSnapshot {
        CompositionSnapshot {
            fibers: self.fibers(),
            composition,
        }
    }

    /// Write the overlay and reconcile; roll the file back if the
    /// composition no longer parses, the reconcile refuses it, or a watched
    /// fiber fails to activate. A watched entry left `Pending` stays:
    /// waiting on a service is a composition state, not a failure. The
    /// error names the watched entries and the reason.
    async fn apply_overlay_change(
        &mut self,
        base: &Composition,
        overlay_path: &Path,
        before: &Composition,
        change: OverlayChange,
    ) -> Result<Composition, (String, String)> {
        let watched_list = change.watched.join(", ");
        let io = |error: SubstrateError| (watched_list.clone(), error.to_string());
        write_overlay(overlay_path, &change.next).map_err(io)?;
        let attempt = match layer(base, Some(&change.next), overlay_path) {
            Ok(composition) => match self.reconcile_edit(&composition).await {
                Ok(()) => Ok(composition),
                Err(error) => Err(error.to_string()),
            },
            Err(error) => Err(error.to_string()),
        };
        let failure = match &attempt {
            Ok(_) => self.fiber_failure(&change.watched),
            Err(reason) => Some(reason.clone()),
        };
        let Some(reason) = failure else {
            return attempt.map_err(|reason| (watched_list, reason));
        };
        restore_overlay(overlay_path, change.previous.as_deref()).map_err(io)?;
        // The tree goes back with the file; the previous composition
        // settled before, so its only possible error is `Unsettled`,
        // which `reconcile_edit` already treats as a state.
        self.reconcile_edit(before).await.map_err(io)?;
        Err((watched_list, reason))
    }

    /// Reconcile for an edit: entries waiting on services are a state the
    /// snapshot reports, not a reason to refuse the edit (boot takes the
    /// same stance).
    async fn reconcile_edit(&mut self, composition: &Composition) -> Result<(), SubstrateError> {
        match self.reconcile(composition).await {
            Ok(()) | Err(SubstrateError::Unsettled { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// The first failure among the fibers running `ids`, as `id: reason`.
    fn fiber_failure(&self, ids: &[String]) -> Option<String> {
        self.fibers()
            .into_iter()
            .filter(|fiber| ids.contains(&fiber.id))
            .find_map(|fiber| match fiber.state {
                FiberState::Failed(reason) => Some(format!("{}: {reason}", fiber.id)),
                FiberState::Active | FiberState::Pending => None,
            })
    }
}

/// The overlay with `patches` applied and re-serialized. Serialization
/// normalizes the owner's comments away — the settings form is a typed
/// projection, not a lossless editor (`design/composition.md`).
fn configure_entries(
    previous: Option<&str>,
    overlay_path: &Path,
    patches: &[Entry],
) -> Result<String, SubstrateError> {
    if patches.is_empty() || patches.len() > CONFIGURE_ENTRIES_MAX {
        return Err(SubstrateError::ConfigureFailed {
            entries: format!("{} entries", patches.len()),
            reason: format!("a settings write patches 1 to {CONFIGURE_ENTRIES_MAX} entries"),
        });
    }
    let mut overlay = match previous {
        Some(text) => Composition::parse(text, &overlay_path.display().to_string())?,
        None => Composition::default(),
    };
    for patch in patches {
        overlay.configure_entry(patch)?;
    }
    Ok(overlay.to_toml())
}

/// The overlay's text, `None` when the node has no overlay file yet.
fn read_overlay(path: &Path) -> Result<Option<String>, SubstrateError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SubstrateError::OverlayIo {
            path: path.display().to_string(),
            source,
        }),
    }
}

fn write_overlay(path: &Path, text: &str) -> Result<(), SubstrateError> {
    let io = |source| SubstrateError::OverlayIo {
        path: path.display().to_string(),
        source,
    };
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).map_err(io)?;
    }
    std::fs::write(path, text).map_err(io)
}

/// Put the overlay back exactly as it was: its previous text, or absent.
fn restore_overlay(path: &Path, previous: Option<&str>) -> Result<(), SubstrateError> {
    match previous {
        Some(text) => write_overlay(path, text),
        None => std::fs::remove_file(path).map_err(|source| SubstrateError::OverlayIo {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// The node's composition: the distribution base with the overlay layered
/// over it, as boot computes it.
fn layer(
    base: &Composition,
    overlay: Option<&str>,
    overlay_path: &Path,
) -> Result<Composition, SubstrateError> {
    match overlay {
        None => Ok(base.clone()),
        Some(text) => {
            let overlay = Composition::parse(text, &overlay_path.display().to_string())?;
            Ok(base.clone().layered(overlay)?)
        }
    }
}

/// The overlay text with `entry` appended as one more `[[entry]]` table.
/// Appending text, rather than re-serializing, keeps the owner's file
/// exactly as they wrote it.
fn append_entry(existing: &str, entry: &Entry) -> String {
    let one = Composition {
        entries: vec![entry.clone()],
    };
    let mut appended = existing.to_string();
    if !appended.is_empty() && !appended.ends_with("\n\n") {
        if !appended.ends_with('\n') {
            appended.push('\n');
        }
        appended.push('\n');
    }
    appended.push_str(&one.to_toml());
    appended
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_keeps_existing_text_and_adds_one_entry_table() {
        let entry = Entry::new("ocr", "wasm:/plugins/ocr/ocr.wasm");
        let appended = append_entry("[[entry]]\nid = \"a\"\nplugin = \"a\"", &entry);
        assert!(appended.starts_with("[[entry]]\nid = \"a\"\nplugin = \"a\"\n\n"));
        let parsed = Composition::parse(&appended, "test").expect("parses");
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[1], entry);
        assert_eq!(
            append_entry("", &entry),
            Composition {
                entries: vec![entry.clone()]
            }
            .to_toml()
        );
    }

    #[test]
    fn configuring_rewrites_the_overlay_with_the_patches_applied() {
        let previous = "[[entry]]\nid = \"ocr\"\nplugin = \"wasm:ocr.wasm\"\n";
        let mut config = toml::Table::new();
        config.insert("target_chars".to_string(), toml::Value::Integer(9));
        let patches = vec![
            Entry {
                id: "chunker".to_string(),
                config,
                disabled: Some(false),
                ..Entry::default()
            },
            Entry {
                id: "ocr".to_string(),
                disabled: Some(true),
                ..Entry::default()
            },
        ];
        let text = configure_entries(Some(previous), Path::new("test"), &patches).unwrap();
        let parsed = Composition::parse(&text, "test").unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].id, "ocr");
        assert_eq!(parsed.entries[0].plugin.as_deref(), Some("wasm:ocr.wasm"));
        assert_eq!(parsed.entries[0].disabled, Some(true));
        assert_eq!(
            parsed.entries[1].config["target_chars"].as_integer(),
            Some(9)
        );
        assert!(configure_entries(None, Path::new("test"), &[]).is_err());
    }

    #[tokio::test]
    async fn an_unserviced_editor_fails_instead_of_hanging() {
        let (editor, edits) = CompositionEditor::pair();
        drop(edits);
        let outcome = editor.submit(CompositionEdit::Inspect).await;
        assert!(matches!(outcome, Err(SubstrateError::EditsUnserviced)));
    }

    #[tokio::test]
    async fn a_submitted_edit_reaches_the_applier_and_its_reply_comes_back() {
        let (editor, mut edits) = CompositionEditor::pair();
        let applier = tokio::spawn(async move {
            let pending = edits.next().await.expect("an edit arrives");
            assert_eq!(*pending.edit(), CompositionEdit::Inspect);
            pending.reply(Ok(CompositionSnapshot {
                fibers: Vec::new(),
                composition: Composition::default(),
            }));
        });
        let outcome = editor.submit(CompositionEdit::Inspect).await;
        assert!(matches!(outcome, Ok(snapshot) if snapshot.fibers.is_empty()));
        applier.await.expect("applier finishes");
    }
}
