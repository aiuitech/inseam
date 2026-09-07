//! The `connections` plugin: the seam's registry provider. Connection
//! plugins (`connection_fs`, and every service connection — linked or
//! loaded) register the hosts they steward into it as effects; the sweep
//! and operations resolve a connection by host. It is deliberately nothing
//! but the registry: enumeration, reads, and capabilities belong to the
//! connections themselves (`design/connections.md`). Every change to it —
//! a registration, a disposal — is announced as [`ConnectionsChanged`] on
//! the kernel bus, which is how the roster learns to publish and withdraw
//! stewardship records without polling.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use inseam_kernel::address::HostId;
use inseam_kernel::substrate::{
    ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError,
};
use inseam_seams::connection::{Connections, ConnectionsChanged, Registration, CONNECTIONS};
use inseam_seams::SeamError;

pub struct ConnectionsRegistry;

pub struct ConnectionsRegistryFactory;

impl inseam_kernel::substrate::PluginFactory for ConnectionsRegistryFactory {
    fn name(&self) -> &str {
        "connections"
    }

    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(ConnectionsRegistry))
    }
}

#[async_trait::async_trait]
impl Plugin for ConnectionsRegistry {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "connections",
            inject: INJECT,
            provides: &["connections"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        cx.provide(
            &CONNECTIONS,
            Arc::new(Registry::new(cx.bus().clone())) as Arc<dyn Connections>,
            Facts::new(),
        )?;
        Ok(())
    }
}

struct Registry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    entries: RwLock<Vec<(u64, Arc<Registration>)>>,
    next: AtomicU64,
    bus: EventBus,
}

impl Registry {
    fn new(bus: EventBus) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                entries: RwLock::new(Vec::new()),
                next: AtomicU64::new(0),
                bus,
            }),
        }
    }
}

impl RegistryInner {
    /// Announce a change. Called only once the entries lock is released:
    /// a listener re-reads the snapshot, and the lock is not reentrant.
    fn announce(&self) {
        self.bus.emit(&ConnectionsChanged);
    }
}

impl Connections for Registry {
    /// One connection per host per node: a second registration for a host
    /// already stewarded here is refused with both entries named.
    fn register(
        &self,
        registration: Registration,
    ) -> Result<Box<dyn FnOnce() + Send>, SeamError> {
        let mut entries = self.inner.entries.write().unwrap_or_else(|e| e.into_inner());
        if let Some((_, holder)) = entries
            .iter()
            .find(|(_, r)| r.host.id == registration.host.id)
        {
            return Err(SeamError::Refused(format!(
                "host `{}` is already stewarded by entry `{}`; entry `{}` cannot register a second connection to it",
                registration.host.id, holder.entry_id, registration.entry_id
            )));
        }
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        entries.push((id, Arc::new(registration)));
        drop(entries);
        self.inner.announce();
        // The disposer holds the registry weakly: a connection being
        // unwound after the whole registry is gone (full teardown, reverse
        // order) must be a no-op, not a resurrection.
        let weak = Arc::downgrade(&self.inner);
        Ok(Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner
                    .entries
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(i, _)| *i != id);
                inner.announce();
            }
        }))
    }

    fn snapshot(&self) -> Vec<Arc<Registration>> {
        let mut out: Vec<Arc<Registration>> = self
            .inner
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, r)| Arc::clone(r))
            .collect();
        out.sort_by(|a, b| a.host.id.as_str().cmp(b.host.id.as_str()));
        out
    }

    fn resolve(&self, host: &HostId) -> Option<Arc<Registration>> {
        self.inner
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(_, r)| r.host.id == *host)
            .map(|(_, r)| Arc::clone(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::Address;
    use inseam_seams::connection::{
        Capabilities, Connection, EnumeratedSource, HostDescription, HostKind,
    };

    /// A connection that lists nothing; enough to exercise the registry.
    struct Stub;

    #[async_trait::async_trait]
    impl Connection for Stub {
        async fn enumerate(&self, _root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
            Ok(Vec::new())
        }
        fn locator_prefix(&self, _root: &str) -> Option<String> {
            None
        }
        async fn read_text(&self, _address: &Address) -> Result<String, SeamError> {
            Ok(String::new())
        }
        async fn read_lines(&self, _a: &Address, _s: u64, _e: u64) -> Result<String, SeamError> {
            Ok(String::new())
        }
        async fn read_bytes(&self, _address: &Address) -> Result<Vec<u8>, SeamError> {
            Ok(Vec::new())
        }
    }

    fn registration(entry: &str, host: &str) -> Registration {
        Registration {
            entry_id: entry.to_string(),
            host: HostDescription {
                id: HostId::new(host).expect("valid host id"),
                kind: HostKind::filesystem(),
                display_name: host.to_string(),
            },
            capabilities: Capabilities::READ_ONLY,
            roots: Vec::new(),
            connection: Arc::new(Stub),
        }
    }

    #[test]
    fn snapshot_orders_by_host_and_resolve_finds_the_steward() {
        let registry = Registry::new(EventBus::new());
        let _keep_b = registry.register(registration("b", "host-b")).expect("registers");
        let dispose_a = registry.register(registration("a", "host-a")).expect("registers");
        let ids: Vec<String> = registry
            .snapshot()
            .iter()
            .map(|r| r.host.id.to_string())
            .collect();
        assert_eq!(ids, vec!["host-a", "host-b"]);
        let a = HostId::new("host-a").expect("valid");
        assert_eq!(registry.resolve(&a).expect("resolves").entry_id, "a");

        // The disposer is the whole uninstall path.
        dispose_a();
        assert!(registry.resolve(&a).is_none());
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn refuses_a_second_connection_to_the_same_host() {
        let registry = Registry::new(EventBus::new());
        let _keep = registry.register(registration("fs", "same")).expect("registers");
        let again = registry.register(registration("fs-two", "same"));
        assert!(matches!(again, Err(SeamError::Refused(_))));
        assert_eq!(registry.snapshot().len(), 1, "the refused registration left no trace");
    }

    #[test]
    fn a_registration_and_its_disposal_each_announce_a_change() {
        let bus = EventBus::new();
        let registry = Registry::new(bus.clone());
        let changes = Arc::new(AtomicU64::new(0));
        let counting = Arc::clone(&changes);
        let _subscription = bus.on::<ConnectionsChanged>(move |_| {
            counting.fetch_add(1, Ordering::SeqCst);
        });

        let dispose = registry.register(registration("fs", "host-a")).expect("registers");
        assert_eq!(changes.load(Ordering::SeqCst), 1, "registering announces once");

        let refused = registry.register(registration("fs-two", "host-a"));
        assert!(matches!(refused, Err(SeamError::Refused(_))));
        assert_eq!(changes.load(Ordering::SeqCst), 1, "a refusal changes nothing");

        dispose();
        assert_eq!(changes.load(Ordering::SeqCst), 2, "disposing announces once");
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn a_listener_may_read_the_snapshot_from_inside_the_event() {
        // The lock is released before the announcement, so a listener that
        // re-reads the registry — the only sensible reaction — never
        // deadlocks against the registration that woke it.
        let bus = EventBus::new();
        let registry = Arc::new(Registry::new(bus.clone()));
        let seen = Arc::new(AtomicU64::new(u64::MAX));
        let reading = Arc::clone(&registry);
        let recording = Arc::clone(&seen);
        let _subscription = bus.on::<ConnectionsChanged>(move |_| {
            let count = u64::try_from(reading.snapshot().len()).expect("fits");
            recording.store(count, Ordering::SeqCst);
        });

        let dispose = registry.register(registration("fs", "host-a")).expect("registers");
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        dispose();
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }
}
