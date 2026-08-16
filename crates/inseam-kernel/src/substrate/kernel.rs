//! The kernel: runs plugins and owns the store (`design/kernel.md`). It
//! reconciles a declarative [composition](super::composition) against the
//! running fiber set — at boot and on every edit — with reactive activation,
//! effect-unwind teardown, and per-fiber failure containment. The design
//! target is confluence: the quiescent state after any history of loads,
//! unloads, and edits equals a fresh boot of the final composition.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use super::composition::{Composition, Entry};
use super::error::SubstrateError;
use super::events::EventBus;
use super::fiber::{EntryId, Fiber, FiberState, FiberView};
use super::plugin::{ApplyCx, EffectAction, PluginFactory, SchemeFactory};
use super::service::{Binding, Facts, ServiceKey};
use crate::state::StateStore;
use crate::store::IndexStore;

/// The kernel-provided services every plugin may consume.
pub const STORE: ServiceKey<IndexStore> = ServiceKey::new("store");
pub const STATE: ServiceKey<StateStore> = ServiceKey::new("state");

/// Restart rounds one fiber may consume within a single reconcile before the
/// kernel declares a dependency cycle instead of spinning.
const MAX_ACTIVATIONS: usize = 8;

pub struct Kernel {
    factories: HashMap<String, Arc<dyn PluginFactory>>,
    scheme_factories: Vec<Arc<dyn SchemeFactory>>,
    fibers: HashMap<EntryId, Fiber>,
    order: Vec<EntryId>,
    bindings: HashMap<String, Binding>,
    bus: EventBus,
    store: Arc<IndexStore>,
}

impl Kernel {
    /// Open the store under `data_dir` and stand up an empty plugin tree.
    /// `factories` are the native plugins this distribution links; scheme
    /// factories resolve dynamic refs (the sandboxed tier).
    pub async fn boot(
        data_dir: &Path,
        factories: Vec<Arc<dyn PluginFactory>>,
        scheme_factories: Vec<Arc<dyn SchemeFactory>>,
    ) -> Result<Self, SubstrateError> {
        let store = Arc::new(IndexStore::open(data_dir).await?);
        let kernel_id = EntryId("kernel".to_string());
        let mut bindings = HashMap::new();
        bindings.insert(
            STORE.name().to_string(),
            Binding::new(kernel_id.clone(), Arc::clone(&store), Facts::new()),
        );
        bindings.insert(
            STATE.name().to_string(),
            Binding::new(
                kernel_id,
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
            fibers: HashMap::new(),
            order: Vec::new(),
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
    pub fn facts(&self, key: &str) -> Option<&Facts> {
        self.bindings.get(key).map(|b| &b.facts)
    }

    /// Reconcile the running fiber tree against `composition`. Edits apply
    /// per entry: unchanged entries are untouched, changed ones dispose and
    /// remount, removed ones unwind. Errors are per-entry and contained; the
    /// composition-level error is reserved for unsatisfiable dependencies.
    pub async fn reconcile(&mut self, composition: &Composition) -> Result<(), SubstrateError> {
        let target = composition.resolved();
        let mut seen = std::collections::HashSet::new();
        for entry in &target {
            if !seen.insert(entry.id.clone()) {
                return Err(SubstrateError::DuplicateEntry(entry.id.clone()));
            }
        }

        // Remove fibers whose entries left the composition.
        let target_ids: std::collections::HashSet<&str> =
            target.iter().map(|e| e.id.as_str()).collect();
        let gone: Vec<EntryId> = self
            .order
            .iter()
            .filter(|id| !target_ids.contains(id.as_str()))
            .cloned()
            .collect();
        for id in gone {
            self.unload(&id);
            self.fibers.remove(&id);
            self.order.retain(|o| o != &id);
        }

        // Mount new entries; remount changed ones.
        for entry in &target {
            let id = EntryId(entry.id.clone());
            let digest = config_digest(&entry.config);
            let plugin_ref = entry.plugin.clone().unwrap_or_default();
            if let Some(existing) = self.fibers.get(&id) {
                if existing.plugin_ref == plugin_ref && existing.config_digest == digest {
                    continue;
                }
                self.unload(&id);
                self.fibers.remove(&id);
                self.order.retain(|o| o != &id);
            }
            let fiber = self.instantiate(&id, &plugin_ref, entry)?;
            self.order.push(id.clone());
            self.fibers.insert(id, fiber);
        }

        self.settle().await
    }

    /// Unload every fiber (reverse mount order) — process shutdown as one
    /// more reconcile, against the empty composition.
    pub async fn shutdown(&mut self) {
        let ids: Vec<EntryId> = self.order.iter().rev().cloned().collect();
        for id in ids {
            self.unload(&id);
            self.fibers.remove(&id);
        }
        self.order.clear();
    }

    fn instantiate(
        &self,
        id: &EntryId,
        plugin_ref: &str,
        entry: &Entry,
    ) -> Result<Fiber, SubstrateError> {
        let built = if let Some(factory) = self.factories.get(plugin_ref) {
            factory.build(&entry.config)
        } else if let Some(factory) = self
            .scheme_factories
            .iter()
            .find(|f| plugin_ref.starts_with(f.scheme()))
        {
            let artifact = &plugin_ref[f_scheme_len(factory.scheme())..];
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
        let mut activations: HashMap<EntryId, usize> = HashMap::new();
        loop {
            let candidate = self.order.iter().find(|id| {
                self.fibers
                    .get(id)
                    .is_some_and(|f| f.state == FiberState::Pending)
                    && self
                        .fibers
                        .get(id)
                        .expect("checked above")
                        .required_keys()
                        .all(|k| self.bindings.contains_key(k))
            });
            let Some(id) = candidate.cloned() else { break };

            let count = activations.entry(id.clone()).or_insert(0);
            *count += 1;
            if *count > MAX_ACTIVATIONS {
                if let Some(fiber) = self.fibers.get_mut(&id) {
                    fiber.state = FiberState::Failed(
                        "dependency cycle: fiber kept restarting within one reconcile".to_string(),
                    );
                }
                continue;
            }

            // Take the fiber out so the apply context can borrow the kernel's
            // bindings mutably while the plugin runs.
            let mut fiber = self.fibers.remove(&id).expect("candidate exists");
            let mut effects = Vec::new();
            let before: std::collections::HashSet<String> =
                self.bindings.keys().cloned().collect();
            let result = {
                let mut cx = ApplyCx {
                    entry: &id,
                    manifest: fiber.plugin.manifest(),
                    bindings: &mut self.bindings,
                    effects: &mut effects,
                    bus: &self.bus,
                };
                fiber.plugin.apply(&mut cx).await
            };
            fiber.effects = effects;
            match result {
                Ok(()) => fiber.state = FiberState::Active,
                Err(e) => {
                    tracing::error!(entry = %id, "plugin failed to apply: {e}");
                    fiber.state = FiberState::Failed(e.to_string());
                }
            }
            let failed = matches!(fiber.state, FiberState::Failed(_));
            self.fibers.insert(id.clone(), fiber);
            if failed {
                // Containment: unwind whatever the half-applied fiber already
                // registered, so failure leaves no partial state behind.
                self.unwind_effects(&id);
                continue;
            }

            // Appearance-restarts: active fibers injecting a key this apply
            // just bound go back to pending and re-run against it.
            let new_keys: Vec<String> = self
                .bindings
                .keys()
                .filter(|k| !before.contains(*k))
                .cloned()
                .collect();
            for key in &new_keys {
                let dependents: Vec<EntryId> = self
                    .order
                    .iter()
                    .filter(|fid| **fid != id)
                    .filter(|fid| {
                        self.fibers.get(fid).is_some_and(|f| {
                            f.state == FiberState::Active && f.injects(key)
                        })
                    })
                    .cloned()
                    .collect();
                for dependent in dependents {
                    self.unload(&dependent);
                }
            }
        }

        let waiting: Vec<(String, Vec<String>)> = self
            .order
            .iter()
            .filter_map(|id| {
                let fiber = self.fibers.get(id)?;
                if fiber.state != FiberState::Pending {
                    return None;
                }
                let missing: Vec<String> = fiber
                    .required_keys()
                    .filter(|k| !self.bindings.contains_key(*k))
                    .map(str::to_string)
                    .collect();
                Some((id.as_str().to_string(), missing))
            })
            .collect();
        if waiting.is_empty() {
            Ok(())
        } else {
            Err(SubstrateError::Unsettled { waiting })
        }
    }

    /// Deactivate a fiber: consumers of anything it provides deactivate
    /// first (they may still use the withdrawn handle during their own
    /// teardown — their `Arc` stays alive through it), then the fiber's
    /// effects unwind in reverse. The fiber lands back in `Pending`.
    fn unload(&mut self, id: &EntryId) {
        let Some(state) = self.fibers.get(id).map(|f| f.state.clone()) else {
            return;
        };
        if state == FiberState::Pending {
            return;
        }
        let provided: Vec<String> = self
            .bindings
            .iter()
            .filter(|(_, b)| &b.provider == id)
            .map(|(k, _)| k.clone())
            .collect();
        for key in &provided {
            let consumers: Vec<EntryId> = self
                .order
                .iter()
                .filter(|fid| *fid != id)
                .filter(|fid| {
                    self.fibers
                        .get(fid)
                        .is_some_and(|f| f.state == FiberState::Active && f.injects(key))
                })
                .cloned()
                .collect();
            for consumer in consumers {
                self.unload(&consumer);
            }
        }
        self.unwind_effects(id);
        if let Some(fiber) = self.fibers.get_mut(id) {
            fiber.state = FiberState::Pending;
        }
    }

    /// Run a fiber's accumulated disposers in reverse. Removal is derived,
    /// not authored: this *is* the uninstall path, for every plugin.
    fn unwind_effects(&mut self, id: &EntryId) {
        let effects = match self.fibers.get_mut(id) {
            Some(fiber) => std::mem::take(&mut fiber.effects),
            None => return,
        };
        for effect in effects.into_iter().rev() {
            match effect.action {
                EffectAction::Unbind(key) => {
                    self.bindings.remove(&key);
                }
                EffectAction::Custom(undo) => undo(),
            }
        }
    }

    /// Snapshot of every fiber for status surfaces.
    pub fn fibers(&self) -> Vec<FiberView> {
        self.order
            .iter()
            .filter_map(|id| self.fibers.get(id))
            .map(|f| FiberView {
                id: f.id.as_str().to_string(),
                plugin: f.plugin_ref.clone(),
                state: f.state.clone(),
                effects: f.effects.iter().map(|e| e.label.clone()).collect(),
                missing: match f.state {
                    FiberState::Pending => f
                        .required_keys()
                        .filter(|k| !self.bindings.contains_key(*k))
                        .map(str::to_string)
                        .collect(),
                    _ => Vec::new(),
                },
            })
            .collect()
    }

    /// Which entry provides each currently bound key — introspection for
    /// tests and `inseam plugins`.
    pub fn providers(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .bindings
            .iter()
            .map(|(k, b)| (k.clone(), b.provider.as_str().to_string()))
            .collect();
        out.sort();
        out
    }
}

fn f_scheme_len(scheme: &str) -> usize {
    scheme.len()
}

/// Stable digest of an entry's config for change detection. FNV-1a over the
/// canonical TOML rendering (tables are sorted maps, so rendering is
/// deterministic).
pub fn config_digest(config: &toml::Table) -> u64 {
    let rendered = toml::to_string(config).unwrap_or_default();
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
