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
    /// No change: answer with the current fiber snapshot.
    Inspect,
}

/// The reply to one edit: every fiber as it stands afterwards.
pub type EditOutcome = Result<Vec<FiberView>, SubstrateError>;

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

impl Kernel {
    /// Apply one edit against the node's composition: `base` is the
    /// distribution's layer, `overlay_path` the node's file. The overlay is
    /// the only layer edited; the reply is the fiber snapshot afterwards.
    pub async fn apply_composition_edit(
        &mut self,
        base: &Composition,
        overlay_path: &Path,
        edit: &CompositionEdit,
    ) -> EditOutcome {
        match edit {
            CompositionEdit::Inspect => Ok(self.fibers()),
            CompositionEdit::Mount(entry) => self.mount_entry(base, overlay_path, entry).await,
        }
    }

    /// Append `entry` to the overlay and reconcile; roll the file back if
    /// the composition no longer parses, the reconcile refuses it, or the
    /// new fiber fails to activate. An entry left `Pending` stays: waiting
    /// on a service is a composition state, not a failure.
    async fn mount_entry(
        &mut self,
        base: &Composition,
        overlay_path: &Path,
        entry: &Entry,
    ) -> EditOutcome {
        let previous = read_overlay(overlay_path)?;
        let before = layer(base, previous.as_deref(), overlay_path)?;
        if before.resolved().iter().any(|e| e.id == entry.id) {
            return Err(SubstrateError::EntryExists(entry.id.clone()));
        }
        let appended = append_entry(previous.as_deref().unwrap_or_default(), entry);
        write_overlay(overlay_path, &appended)?;

        let attempt = match layer(base, Some(&appended), overlay_path) {
            Ok(composition) => self.reconcile_edit(&composition).await,
            Err(error) => Err(error),
        };
        let failure = match attempt {
            Ok(()) => self.fiber_failure(&entry.id),
            Err(error) => Some(error.to_string()),
        };
        match failure {
            None => Ok(self.fibers()),
            Some(reason) => {
                restore_overlay(overlay_path, previous.as_deref())?;
                // The tree goes back with the file; the previous composition
                // settled before, so its only possible error is `Unsettled`,
                // which `reconcile_edit` already treats as a state.
                self.reconcile_edit(&before).await?;
                assert!(
                    self.fiber_failure(&entry.id).is_none(),
                    "a rolled-back entry is no longer mounted"
                );
                Err(SubstrateError::MountFailed {
                    entry: entry.id.clone(),
                    reason,
                })
            }
        }
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

    /// The failure reason of the fiber running `id`, if it failed.
    fn fiber_failure(&self, id: &str) -> Option<String> {
        self.fibers()
            .into_iter()
            .find(|fiber| fiber.id == id)
            .and_then(|fiber| match fiber.state {
                FiberState::Failed(reason) => Some(reason),
                FiberState::Active | FiberState::Pending => None,
            })
    }
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
            pending.reply(Ok(Vec::new()));
        });
        let outcome = editor.submit(CompositionEdit::Inspect).await;
        assert!(matches!(outcome, Ok(fibers) if fibers.is_empty()));
        applier.await.expect("applier finishes");
    }
}
