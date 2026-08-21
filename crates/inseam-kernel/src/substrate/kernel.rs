//! The kernel: runs plugins and owns the store (`design/kernel.md`). It
//! reconciles a declarative [composition](super::composition) against the
//! running fiber set — at boot and on every edit — with reactive activation,
//! effect-unwind teardown, and per-fiber failure containment. The design
//! target is confluence: the quiescent state after any history of loads,
//! unloads, and edits equals a fresh boot of the final composition.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use super::composition::{Composition, Entry};
use super::error::SubstrateError;
use super::events::EventBus;
use super::fiber::{EntryId, Fiber, FiberState, FiberView};
use super::plugin::{ApplyCx, EffectAction, PluginFactory, SchemeFactory};
use super::service::{Binding, Facts, Provider, ServiceKey};
use crate::state::StateStore;
use crate::store::IndexStore;

/// The kernel-provided services every plugin may consume.
pub const STORE: ServiceKey<IndexStore> = ServiceKey::new("store");
pub const STATE: ServiceKey<StateStore> = ServiceKey::new("state");

/// Restart rounds one fiber may consume within a single reconcile before the
/// kernel declares a dependency cycle instead of spinning.
const ACTIVATIONS_PER_RECONCILE_MAX: u32 = 8;

pub struct Kernel {
    factories: HashMap<String, Arc<dyn PluginFactory>>,
    scheme_factories: Vec<Arc<dyn SchemeFactory>>,
    /// Every mounted fiber, in mount order. One collection, not a map plus
    /// an order list: a node runs tens of fibers, so linear scans are free
    /// and there is no second structure to drift.
    fibers: Vec<Fiber>,
    bindings: HashMap<String, Binding>,
    bus: EventBus,
    store: Arc<IndexStore>,
}

impl Kernel {
    /// Open the store under `data_dir` and stand up an empty plugin tree.
    /// `factories` are the linked plugins this distribution ships; scheme
    /// factories resolve dynamic refs (the loaded tier).
    pub async fn boot(
        data_dir: &Path,
        factories: Vec<Arc<dyn PluginFactory>>,
        scheme_factories: Vec<Arc<dyn SchemeFactory>>,
    ) -> Result<Self, SubstrateError> {
        let store = Arc::new(IndexStore::open(data_dir).await?);
        let mut bindings = HashMap::new();
        bindings.insert(
            STORE.name().to_string(),
            Binding::new(Provider::Kernel, Arc::clone(&store), Facts::new()),
        );
        bindings.insert(
            STATE.name().to_string(),
            Binding::new(
                Provider::Kernel,
                Arc::new(StateStore::new(Arc::clone(&store))),
                Facts::new(),
            ),
        );
        Ok(Self {
            factories: factories
                .into_iter()
                .map(|f| (f.name().to_string(), f))
                .collect(),
            scheme_factories,
            fibers: Vec::new(),
            bindings,
            bus: EventBus::new(),
            store,
        })
    }

    pub fn store(&self) -> &Arc<IndexStore> {
        &self.store
    }

    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    /// Typed access to a bound service from outside the plugin tree (the
    /// distribution's transport edge — the CLI reaching `operations`).
    pub fn service<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: &ServiceKey<T>,
    ) -> Result<Arc<T>, SubstrateError> {
        self.bindings
            .get(key.name())
            .ok_or_else(|| SubstrateError::MissingService {
                key: key.name().to_string(),
            })?
            .typed(key)
    }

    /// The mounted provider's facts for a key.
    pub fn facts<T: ?Sized + 'static>(&self, key: &ServiceKey<T>) -> Option<&Facts> {
        self.bindings.get(key.name()).map(|b| &b.facts)
    }

    /// Reconcile the running fiber tree against `composition`. Edits apply
    /// per entry: unchanged entries are untouched, changed ones dispose and
    /// remount, removed ones unwind. A composition that names an unknown
    /// plugin or an invalid config is refused before anything changes —
    /// the running tree stays as it was. Apply failures are per-entry and
    /// contained; the composition-level error after settling is reserved
    /// for unsatisfiable dependencies.
    pub async fn reconcile(&mut self, composition: &Composition) -> Result<(), SubstrateError> {
        let target = composition.resolved();
        let mut target_ids: HashSet<&str> = HashSet::with_capacity(target.len());
        for entry in &target {
            if !target_ids.insert(entry.id.as_str()) {
                return Err(SubstrateError::DuplicateEntry(entry.id.clone()));
            }
        }

        // Plan first, mutate second: every new or changed entry is built
        // before any fiber is touched, so a bad entry leaves the tree intact.
        let mut mounts: Vec<Fiber> = Vec::new();
        for entry in &target {
            let id = EntryId(entry.id.clone());
            let plugin_ref = entry.plugin.clone().unwrap_or_default();
            let unchanged = self.fiber(&id).is_some_and(|existing| {
                existing.plugin_ref == plugin_ref
                    && existing.config_digest == config_digest(&entry.config)
            });
            if !unchanged {
                mounts.push(self.instantiate(&id, &plugin_ref, entry)?);
            }
        }

        let gone: Vec<EntryId> = self
            .fibers
            .iter()
            .filter(|f| !target_ids.contains(f.id.as_str()))
            .map(|f| f.id.clone())
            .collect();
        for id in &gone {
            self.unload(id);
            self.fibers.retain(|f| f.id != *id);
        }
        for fiber in mounts {
            self.unload(&fiber.id);
            self.fibers.retain(|f| f.id != fiber.id);
            self.fibers.push(fiber);
        }

        self.settle().await
    }

    /// Unload every fiber — process shutdown is one more reconcile, against
    /// the empty composition.
    pub async fn shutdown(&mut self) {
        // The empty composition names no plugin, no config, and leaves no
        // fiber waiting on anything: none of `reconcile`'s errors can arise.
        self.reconcile(&Composition::default())
            .await
            .expect("the empty composition always settles");
        assert!(self.fibers.is_empty(), "shutdown leaves no fiber mounted");
    }

    fn instantiate(
        &self,
        id: &EntryId,
        plugin_ref: &str,
        entry: &Entry,
    ) -> Result<Fiber, SubstrateError> {
        let built = if let Some(factory) = self.factories.get(plugin_ref) {
            factory.build(&entry.config)
        } else if let Some((factory, artifact)) = self
            .scheme_factories
            .iter()
            .find_map(|f| plugin_ref.strip_prefix(f.scheme()).map(|artifact| (f, artifact)))
        {
            factory.build(artifact, &entry.config)
        } else {
            return Err(SubstrateError::UnknownPlugin {
                entry: id.as_str().to_string(),
                plugin: plugin_ref.to_string(),
            });
        };
        let plugin = built.map_err(|e| SubstrateError::ConfigInvalid {
            entry: id.as_str().to_string(),
            message: e.to_string(),
        })?;
        Ok(Fiber {
            id: id.clone(),
            plugin_ref: plugin_ref.to_string(),
            config_digest: config_digest(&entry.config),
            plugin,
            state: FiberState::Pending,
            effects: Vec::new(),
        })
    }

    /// Drive pending fibers to active until nothing more can move. Reactive,
    /// not ordered: a fiber activates when its required keys are all bound; a
    /// newly bound key restarts already-active fibers that inject it, so file
    /// order carries no semantics. Still-pending fibers at the end are a loud
    /// composition error naming their missing keys.
    async fn settle(&mut self) -> Result<(), SubstrateError> {
        let mut activations: HashMap<EntryId, u32> = HashMap::new();
        // Every round charges one fiber one activation, and a fiber is
        // failed once it exceeds its allowance, so rounds are bounded.
        let rounds_max = u32::try_from(self.fibers.len())
            .expect("fiber count fits u32")
            .saturating_mul(ACTIVATIONS_PER_RECONCILE_MAX + 1);
        let mut rounds: u32 = 0;
        while let Some(id) = self.next_activatable() {
            rounds += 1;
            assert!(rounds <= rounds_max, "settle rounds stay within the activation allowance");
            let count = activations.entry(id.clone()).or_insert(0);
            *count += 1;
            if *count > ACTIVATIONS_PER_RECONCILE_MAX {
                self.fiber_mut(&id).state = FiberState::Failed(
                    "dependency cycle: fiber kept restarting within one reconcile".to_string(),
                );
                continue;
            }
            let new_keys = self.activate(&id).await;
            self.restart_dependents(&new_keys, &id);
        }

        let waiting: Vec<(String, Vec<String>)> = self
            .fibers
            .iter()
            .filter(|f| f.state == FiberState::Pending)
            .map(|f| (f.id.as_str().to_string(), self.missing_keys(f)))
            .collect();
        if waiting.is_empty() {
            Ok(())
        } else {
            Err(SubstrateError::Unsettled { waiting })
        }
    }

    /// The first pending fiber, in mount order, whose required keys are all
    /// bound.
    fn next_activatable(&self) -> Option<EntryId> {
        self.fibers
            .iter()
            .find(|f| f.state == FiberState::Pending && self.missing_keys(f).is_empty())
            .map(|f| f.id.clone())
    }

    /// Required keys of `fiber` that nothing currently provides.
    fn missing_keys(&self, fiber: &Fiber) -> Vec<String> {
        fiber
            .required_keys()
            .filter(|k| !self.bindings.contains_key(*k))
            .map(str::to_string)
            .collect()
    }

    /// Run one pending fiber's `apply`. Returns the keys it newly bound;
    /// a failed apply lands the fiber `Failed`, unwinds whatever it had
    /// already registered (containment: no partial state survives), and
    /// returns nothing.
    async fn activate(&mut self, id: &EntryId) -> Vec<String> {
        let before: HashSet<String> = self.bindings.keys().cloned().collect();
        let index = self
            .fibers
            .iter()
            .position(|f| f.id == *id)
            .expect("activation candidates are mounted fibers");
        let fiber = &mut self.fibers[index];
        assert!(fiber.state == FiberState::Pending, "only pending fibers activate");
        assert!(fiber.effects.is_empty(), "a pending fiber owns no effects");
        let result = {
            let mut cx = ApplyCx {
                entry: &fiber.id,
                manifest: fiber.plugin.manifest(),
                bindings: &mut self.bindings,
                effects: &mut fiber.effects,
                bus: &self.bus,
            };
            fiber.plugin.apply(&mut cx).await
        };
        match result {
            Ok(()) => {
                fiber.state = FiberState::Active;
                self.bindings
                    .keys()
                    .filter(|k| !before.contains(*k))
                    .cloned()
                    .collect()
            }
            Err(e) => {
                tracing::error!(entry = %id, "plugin failed to apply: {e}");
                fiber.state = FiberState::Failed(e.to_string());
                self.unwind_effects(id);
                Vec::new()
            }
        }
    }

    /// Appearance-restarts: active fibers injecting a key that just became
    /// bound go back to pending and re-run against it — optional
    /// injections included, which is what keeps mount order meaningless.
    fn restart_dependents(&mut self, new_keys: &[String], except: &EntryId) {
        let dependents: Vec<EntryId> = self
            .fibers
            .iter()
            .filter(|f| f.id != *except)
            .filter(|f| f.state == FiberState::Active)
            .filter(|f| new_keys.iter().any(|k| f.injects(k)))
            .map(|f| f.id.clone())
            .collect();
        for dependent in &dependents {
            self.unload(dependent);
        }
    }

    /// Deactivate a fiber together with everything that transitively
    /// consumes what it provides. Consumers unwind before their providers,
    /// so a consumer can still use the very capability it is losing during
    /// its own teardown (its `Arc` stays alive through it). Iterative and
    /// bounded: a cycle of mutual consumers — the case the activation
    /// allowance exists for — unwinds in mount order instead of recursing.
    /// Every fiber touched lands back in `Pending`.
    fn unload(&mut self, id: &EntryId) {
        let mut remaining = self.dependent_closure(id);
        let members = remaining.len();
        let mut rounds: usize = 0;
        while !remaining.is_empty() {
            rounds += 1;
            assert!(rounds <= members, "each round unwinds exactly one member");
            let next = remaining
                .iter()
                .position(|candidate| !self.consumed_within(candidate, &remaining))
                .unwrap_or(0);
            let victim = remaining.remove(next);
            self.unwind_effects(&victim);
            self.fiber_mut(&victim).state = FiberState::Pending;
        }
    }

    /// `root` plus every active fiber that transitively injects a key some
    /// member provides, in discovery order. Empty when `root` is not active.
    fn dependent_closure(&self, root: &EntryId) -> Vec<EntryId> {
        let mut closure: Vec<EntryId> = Vec::new();
        if !self.fiber(root).is_some_and(|f| f.state == FiberState::Active) {
            return closure;
        }
        closure.push(root.clone());
        let mut cursor: usize = 0;
        while cursor < closure.len() {
            assert!(closure.len() <= self.fibers.len(), "closure members are distinct fibers");
            let provided = self.keys_provided_by(&closure[cursor]);
            let newly: Vec<EntryId> = self
                .fibers
                .iter()
                .filter(|f| f.state == FiberState::Active)
                .filter(|f| !closure.contains(&f.id))
                .filter(|f| provided.iter().any(|k| f.injects(k)))
                .map(|f| f.id.clone())
                .collect();
            closure.extend(newly);
            cursor += 1;
        }
        closure
    }

    /// Whether some other member of `set` injects a key `provider` provides.
    fn consumed_within(&self, provider: &EntryId, set: &[EntryId]) -> bool {
        let provided = self.keys_provided_by(provider);
        set.iter()
            .filter(|other| *other != provider)
            .filter_map(|other| self.fiber(other))
            .any(|f| provided.iter().any(|k| f.injects(k)))
    }

    fn keys_provided_by(&self, id: &EntryId) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|(_, b)| b.provider == Provider::Fiber(id.clone()))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Run a fiber's accumulated disposers in reverse. Removal is derived,
    /// not authored: this *is* the uninstall path, for every plugin.
    fn unwind_effects(&mut self, id: &EntryId) {
        let effects = std::mem::take(&mut self.fiber_mut(id).effects);
        for effect in effects.into_iter().rev() {
            match effect.action {
                EffectAction::Unbind(key) => {
                    self.bindings.remove(&key);
                }
                EffectAction::Custom(undo) => undo(),
            }
        }
    }

    fn fiber(&self, id: &EntryId) -> Option<&Fiber> {
        self.fibers.iter().find(|f| f.id == *id)
    }

    fn fiber_mut(&mut self, id: &EntryId) -> &mut Fiber {
        self.fibers
            .iter_mut()
            .find(|f| f.id == *id)
            .expect("callers name a mounted fiber")
    }

    /// Snapshot of every fiber for status surfaces.
    pub fn fibers(&self) -> Vec<FiberView> {
        self.fibers
            .iter()
            .map(|f| FiberView {
                id: f.id.as_str().to_string(),
                plugin: f.plugin_ref.clone(),
                state: f.state.clone(),
                effects: f.effects.iter().map(|e| e.label.clone()).collect(),
                missing: match f.state {
                    FiberState::Pending => self.missing_keys(f),
                    FiberState::Active | FiberState::Failed(_) => Vec::new(),
                },
                // The snapshot re-checks the environment the plugin's own
                // apply reads (pair assertions: the declaration and the
                // failure must agree). Active fibers resolved their
                // secrets, so only parked ones report them.
                missing_secrets: match f.state {
                    FiberState::Active => Vec::new(),
                    FiberState::Pending | FiberState::Failed(_) => f
                        .plugin
                        .secrets()
                        .into_iter()
                        .filter(|need| std::env::var(&need.env).is_err())
                        .collect(),
                },
            })
            .collect()
    }

    /// Which entry provides each currently bound key — introspection for
    /// tests and `inseam plugins`. The kernel's own bindings report `kernel`.
    pub fn providers(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .bindings
            .iter()
            .map(|(k, b)| (k.clone(), b.provider.to_string()))
            .collect();
        out.sort();
        out
    }
}

/// Stable digest of an entry's config for change detection. FNV-1a over the
/// canonical TOML rendering (tables are sorted maps, so rendering is
/// deterministic).
pub fn config_digest(config: &toml::Table) -> u64 {
    // A `toml::Table` is by construction a valid TOML document.
    let rendered = toml::to_string(config).expect("a toml table renders as toml");
    fnv1a(rendered.as_bytes())
}

/// FNV-1a, inlined for cross-process determinism (std's hasher doesn't
/// guarantee it).
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_digest_is_stable_and_renders_every_config_shape() {
        // Values, nested tables, arrays of tables, and a datetime — the
        // shapes a TOML config can take — all render; key order in the
        // source text does not change the digest.
        let a: toml::Table = toml::from_str(
            "b = 1\na = \"x\"\nwhen = 2026-08-21T00:00:00Z\n[t]\nk = [1, 2]\n[[arr]]\nn = 1\n[[arr]]\nn = 2\n",
        )
        .expect("parses");
        let b: toml::Table = toml::from_str(
            "a = \"x\"\nwhen = 2026-08-21T00:00:00Z\nb = 1\n[[arr]]\nn = 1\n[[arr]]\nn = 2\n[t]\nk = [1, 2]\n",
        )
        .expect("parses");
        assert_eq!(config_digest(&a), config_digest(&b));
        let mut c = a.clone();
        c.insert("b".into(), toml::Value::Integer(2));
        assert_ne!(config_digest(&a), config_digest(&c));
    }
}
