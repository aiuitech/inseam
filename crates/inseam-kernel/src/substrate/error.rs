//! Substrate errors. Failures are loud and name the entry or key involved:
//! a fiber failing lands that fiber alone, and a composition that cannot
//! settle says exactly which entries wait on which keys.

use thiserror::Error;

/// What a plugin's own code reports from `apply` or construction.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct PluginError(pub String);

impl PluginError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<String> for PluginError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for PluginError {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[derive(Debug, Error)]
pub enum SubstrateError {
    #[error("entry `{entry}` names unknown plugin `{plugin}`")]
    UnknownPlugin { entry: String, plugin: String },
    #[error("composition has two entries with id `{0}`")]
    DuplicateEntry(String),
    #[error("entry `{entry}` config is invalid: {message}")]
    ConfigInvalid { entry: String, message: String },
    #[error("service `{key}` is already provided by entry `{holder}`")]
    ProvideConflict { key: String, holder: String },
    #[error(
        "plugin `{plugin}` accessed service `{key}` without declaring it in its manifest; \
         inject declarations are the capability manifest and must be complete"
    )]
    UndeclaredInject { plugin: String, key: String },
    #[error("plugin `{plugin}` provided `{key}` without declaring it in its manifest")]
    UndeclaredProvide { plugin: String, key: String },
    #[error("service `{key}` is not provided")]
    MissingService { key: String },
    #[error("service `{key}` is bound to a different type than the consumer expects")]
    WrongServiceType { key: String },
    #[error(
        "composition cannot settle; entries are waiting on services nothing provides:\n{}",
        .waiting.iter().map(|(e, keys)| format!("  {e}: missing {}", keys.join(", ")))
            .collect::<Vec<_>>().join("\n")
    )]
    Unsettled { waiting: Vec<(String, Vec<String>)> },
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Composition(#[from] super::composition::CompositionError),
    // Composition edits at runtime (`edits.rs`).
    #[error("composition already has an entry `{0}`")]
    EntryExists(String),
    #[error("entry `{entry}` did not activate and was rolled back: {reason}")]
    MountFailed { entry: String, reason: String },
    /// A settings write left some configured entry failed, or the
    /// composition refused it; the overlay went back to its previous text.
    #[error("configuring {entries} was rolled back: {reason}")]
    ConfigureFailed { entries: String, reason: String },
    #[error("could not write composition overlay `{path}`: {source}")]
    OverlayIo {
        path: String,
        source: std::io::Error,
    },
    #[error(
        "this node's runtime did not apply the composition edit; only a running node          (`inseam serve`) applies edits — a one-shot command edits the file directly"
    )]
    EditsUnserviced,
    #[error("too many composition edits are waiting to be applied; retry shortly")]
    EditQueueFull,
}
