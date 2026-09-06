//! The plugin substrate: how plugins exist and compose (`design/kernel.md`).

mod composition;
mod edits;
mod error;
mod events;
mod fiber;
mod kernel;
mod plugin;
mod service;

pub use composition::{Composition, CompositionError, Entry, ENTRY_COUNT_MAX};
pub use edits::{
    CompositionEdit, CompositionEditor, CompositionEdits, CompositionSnapshot, EditOutcome,
    PendingEdit, COMPOSITION, EDIT_TIMEOUT,
};
pub use error::{PluginError, SubstrateError};
pub use events::{EventBus, Guard, Next, Notify, Subscription, Verdict, Waterfall};
pub use fiber::{EntryId, FiberState, FiberView};
pub use kernel::{config_digest, fnv1a, Kernel, STATE, STORE};
pub use plugin::{
    parse_config, ApplyCx, Inject, Manifest, Plugin, PluginFactory, SchemeFactory, SecretNeed,
};
pub use service::{Facts, ServiceKey};
