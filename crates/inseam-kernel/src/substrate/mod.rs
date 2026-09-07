//! The plugin substrate: how plugins exist and compose (`design/kernel.md`).

mod composition;
mod edits;
mod error;
mod events;
mod fiber;
mod kernel;
mod plugin;
mod service;

pub use composition::{Composition, CompositionError, ENTRY_COUNT_MAX, Entry};
pub use edits::{
    COMPOSITION, CompositionEdit, CompositionEditor, CompositionEdits, CompositionSnapshot,
    EDIT_TIMEOUT, EditOutcome, PendingEdit,
};
pub use error::{PluginError, SubstrateError};
pub use events::{EventBus, Guard, Next, Notify, Subscription, Verdict, Waterfall};
pub use fiber::{EntryId, FiberState, FiberView};
pub use kernel::{Kernel, STATE, STORE, config_digest, fnv1a};
pub use plugin::{
    ApplyCx, Inject, Manifest, Plugin, PluginFactory, SchemeFactory, SecretNeed, parse_config,
};
pub use service::{Facts, ServiceKey};
