//! Per-plugin namespaced, versioned state — the kernel-provided `state`
//! service (`design/kernel.md`). A namespace is declared with a version; on
//! mismatch the namespace is discarded and rebuilt, never migrated. Plugin
//! state must therefore be derived or re-obtainable — credentials and
//! configuration live in the composition and credential files, never here.

use std::sync::Arc;

use crate::store::{IndexStore, StoreError};

pub struct StateStore {
    store: Arc<IndexStore>,
}

impl StateStore {
    pub fn new(store: Arc<IndexStore>) -> Self {
        Self { store }
    }

    /// Open a namespace at a declared version. If the stored version
    /// differs, every key in the namespace is discarded first.
    pub fn namespace(&self, ns: &str, version: &str) -> Result<StateNamespace, StoreError> {
        self.store.state_open_namespace(ns, version)?;
        Ok(StateNamespace {
            store: Arc::clone(&self.store),
            ns: ns.to_string(),
        })
    }
}

/// A handle scoped to one namespace; keys never collide across plugins.
pub struct StateNamespace {
    store: Arc<IndexStore>,
    ns: String,
}

impl StateNamespace {
    pub fn get(&self, key: &str) -> Result<Option<String>, StoreError> {
        self.store.state_get(&self.ns, key)
    }

    pub fn put(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.store.state_put(&self.ns, key, value)
    }

    pub fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.store.state_delete(&self.ns, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(IndexStore::open(dir.path()).await.expect("opens"));
        (dir, StateStore::new(store))
    }

    #[tokio::test]
    async fn namespaces_isolate_and_roundtrip() {
        let (_dir, state) = state().await;
        let a = state.namespace("plugin-a", "1").expect("opens");
        let b = state.namespace("plugin-b", "1").expect("opens");
        a.put("k", "va").expect("puts");
        b.put("k", "vb").expect("puts");
        assert_eq!(a.get("k").expect("gets").as_deref(), Some("va"));
        assert_eq!(b.get("k").expect("gets").as_deref(), Some("vb"));
        a.delete("k").expect("deletes");
        assert_eq!(a.get("k").expect("gets"), None);
        assert_eq!(b.get("k").expect("gets").as_deref(), Some("vb"));
    }

    #[tokio::test]
    async fn version_mismatch_discards_the_namespace() {
        let (_dir, state) = state().await;
        let v1 = state.namespace("plugin-a", "1").expect("opens");
        v1.put("k", "old").expect("puts");
        let v2 = state.namespace("plugin-a", "2").expect("reopens at v2");
        assert_eq!(v2.get("k").expect("gets"), None, "v2 starts empty");
        v2.put("k", "new").expect("puts");
        // Same version keeps state.
        let again = state.namespace("plugin-a", "2").expect("reopens");
        assert_eq!(again.get("k").expect("gets").as_deref(), Some("new"));
    }
}
