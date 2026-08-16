//! Services: typed interfaces bound to well-known keys (`design/kernel.md`).
//! At most one provider binds a key at a time; consumers receive a typed
//! `Arc` handle and branch on declared **capability facts**, never on
//! provider identity.

use std::any::Any;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;

use super::error::SubstrateError;
use super::fiber::EntryId;

/// A well-known service key, carrying the trait consumers see at that key.
/// Definitions live with the seam (`inseam-seams`), apart from providers and
/// consumers, so swapping a provider never touches either.
pub struct ServiceKey<T: ?Sized + 'static> {
    name: &'static str,
    _marker: PhantomData<fn(&T)>,
}

impl<T: ?Sized + 'static> ServiceKey<T> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _marker: PhantomData,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }
}

impl<T: ?Sized> std::fmt::Debug for ServiceKey<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ServiceKey({})", self.name)
    }
}

/// Declared properties of whatever provider is mounted at a key: "offers a
/// change feed", "works offline", "embeds 1536 dims". Consumers branch on
/// facts so provider swaps never require touching a consumer.
#[derive(Debug, Clone, Default)]
pub struct Facts(BTreeMap<&'static str, serde_json::Value>);

impl Facts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, key: &'static str, value: impl Into<serde_json::Value>) -> Self {
        self.0.insert(key, value.into());
        self
    }

    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.0.get(key)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| v.as_str())
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| v.as_bool())
    }

    pub fn u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.as_u64())
    }
}

/// One live binding: the providing entry, the erased `Arc<T>` handle, and the
/// provider's declared facts.
pub(crate) struct Binding {
    pub provider: EntryId,
    handle: Box<dyn Any + Send + Sync>,
    pub facts: Facts,
}

impl Binding {
    pub fn new<T: ?Sized + Send + Sync + 'static>(
        provider: EntryId,
        handle: Arc<T>,
        facts: Facts,
    ) -> Self {
        Self {
            provider,
            handle: Box::new(handle),
            facts,
        }
    }

    pub fn typed<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: &ServiceKey<T>,
    ) -> Result<Arc<T>, SubstrateError> {
        self.handle
            .downcast_ref::<Arc<T>>()
            .cloned()
            .ok_or_else(|| SubstrateError::WrongServiceType {
                key: key.name().to_string(),
            })
    }
}
