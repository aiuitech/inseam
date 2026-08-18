//! Fibers: one running instance of a plugin, owned by the kernel, with a
//! reactive lifecycle (`design/kernel.md`). There is no boot order — a fiber
//! activates when everything it requires is provided, unloads when something
//! it requires withdraws, and failure lands the fiber alone.

use super::plugin::{Effect, Plugin, SecretNeed};

/// Stable identity of a composition entry; the reconciler's diffing key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub(crate) String);

impl EntryId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FiberState {
    /// Waiting for required injections; the payload names what's missing.
    Pending,
    Active,
    /// `apply` or construction failed; contained to this fiber.
    Failed(String),
}

pub(crate) struct Fiber {
    pub id: EntryId,
    pub plugin_ref: String,
    pub config_digest: u64,
    pub plugin: Box<dyn Plugin>,
    pub state: FiberState,
    pub effects: Vec<Effect>,
}

impl Fiber {
    /// Keys this fiber's manifest requires.
    pub fn required_keys(&self) -> impl Iterator<Item = &'static str> {
        self.plugin
            .manifest()
            .inject
            .iter()
            .filter(|i| i.required)
            .map(|i| i.key)
    }

    /// Whether this fiber's manifest injects `key` at all (required or not).
    pub fn injects(&self, key: &str) -> bool {
        self.plugin.manifest().injects(key)
    }
}

/// A public snapshot of one fiber for status surfaces (`inseam plugins`).
#[derive(Debug, Clone)]
pub struct FiberView {
    pub id: String,
    pub plugin: String,
    pub state: FiberState,
    /// Labels of the fiber's live effects — "what does this plugin own right
    /// now" as a query, not archaeology.
    pub effects: Vec<String>,
    pub missing: Vec<String>,
    /// Declared secrets currently absent from the environment — for a
    /// non-active fiber, the "enter this to enable" story a UI renders.
    pub missing_secrets: Vec<SecretNeed>,
}
