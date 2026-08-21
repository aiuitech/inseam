//! Plugins: the unit of everything (`design/plugins.md`). A plugin is five
//! declarations — name, config, inject, provide, apply — instantiated by the
//! kernel as a fiber. The `inject` list is the plugin's capability manifest:
//! the apply context refuses access to any key not declared there, so what a
//! plugin *can* touch is readable off its declaration.

use std::sync::Arc;

use serde::de::DeserializeOwned;

use super::error::{PluginError, SubstrateError};
use super::events::{EventBus, Subscription};
use super::fiber::EntryId;
use super::service::{Binding, Facts, Provider, ServiceKey};

/// One injected key. `required: false` marks a dependency the plugin can run
/// without (it branches at apply time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inject {
    pub key: &'static str,
    pub required: bool,
}

impl Inject {
    pub const fn required(key: &'static str) -> Self {
        Self {
            key,
            required: true,
        }
    }

    pub const fn optional(key: &'static str) -> Self {
        Self {
            key,
            required: false,
        }
    }
}

/// The static half of a plugin: identity plus its capability manifest.
#[derive(Debug, Clone, Copy)]
pub struct Manifest {
    pub name: &'static str,
    pub inject: &'static [Inject],
    pub provides: &'static [&'static str],
}

impl Manifest {
    pub(crate) fn injects(&self, key: &str) -> bool {
        self.inject.iter().any(|i| i.key == key)
    }

    pub(crate) fn declares_provide(&self, key: &str) -> bool {
        self.provides.contains(&key)
    }
}

/// A plugin instance, constructed from its entry's config. `apply` runs when
/// every required injection is satisfied; all its registrations are effects,
/// so there is no uninstall path to write — unload unwinds them in reverse.
#[async_trait::async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn manifest(&self) -> Manifest;
    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError>;

    /// Secrets this configured instance reads from the environment, each
    /// with the reason a UI shows when asking the owner to provide it.
    /// Declared per instance, not in the static manifest, because the
    /// variable name comes from config (`api_key_env`-style). Declaring is
    /// advisory — `apply` still fails loudly when the value is absent; the
    /// declaration is what lets status surfaces explain instead of just
    /// reporting the failure.
    fn secrets(&self) -> Vec<SecretNeed> {
        Vec::new()
    }
}

/// One environment-variable secret a configured plugin needs, and why —
/// `purpose` is owner-facing prose a settings UI can print verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretNeed {
    pub env: String,
    pub purpose: String,
}

/// Builds plugin instances for composition entries naming this plugin.
/// Construction is where config is parsed — parse, don't validate: a factory
/// returns a typed instance or a loud error, never a half-configured plugin.
pub trait PluginFactory: Send + Sync {
    fn name(&self) -> &str;
    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError>;
}

/// Resolves plugins the distribution did not statically link, keyed by a ref
/// scheme (`wasm:` today). This is how the loaded tier enters the same
/// plugin model: the plugin-host bridge registers a scheme factory.
pub trait SchemeFactory: Send + Sync {
    /// The scheme prefix, including the colon (e.g. `"wasm:"`).
    fn scheme(&self) -> &str;
    fn build(&self, artifact_ref: &str, config: &toml::Table)
    -> Result<Box<dyn Plugin>, PluginError>;
}

/// Parse an entry's config table into the plugin's typed config. Unknown
/// fields are rejected by the config types themselves (serde deny_unknown).
pub fn parse_config<T: DeserializeOwned>(config: &toml::Table) -> Result<T, PluginError> {
    T::deserialize(config.clone()).map_err(|e| PluginError(format!("config: {e}")))
}

/// One recorded effect: a change to shared state paired with its undo,
/// registered at the moment the change is made (`design/kernel.md`).
pub(crate) struct Effect {
    pub label: String,
    pub action: EffectAction,
}

pub(crate) enum EffectAction {
    /// Withdraw a service binding (kernel-interpreted so consumers can be
    /// deactivated first).
    Unbind(String),
    /// A self-contained undo: registry removals, subscriptions, spawned-task
    /// aborts.
    Custom(Box<dyn FnOnce() + Send>),
}

/// What `apply` sees: typed access to injected services, the event bus, and
/// the effect ledger. Everything mutating goes through here so teardown can
/// be derived instead of authored.
pub struct ApplyCx<'a> {
    pub(crate) entry: &'a EntryId,
    pub(crate) manifest: Manifest,
    pub(crate) bindings: &'a mut std::collections::HashMap<String, Binding>,
    pub(crate) effects: &'a mut Vec<Effect>,
    pub(crate) bus: &'a EventBus,
}

impl ApplyCx<'_> {
    /// The id of the composition entry this fiber runs as.
    pub fn entry_id(&self) -> &str {
        self.entry.as_str()
    }

    /// A required injection. Undeclared access is refused loudly — the
    /// manifest is the capability boundary, not a suggestion.
    pub fn get<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: &ServiceKey<T>,
    ) -> Result<Arc<T>, PluginError> {
        self.checked(key)?
            .ok_or_else(|| PluginError(format!("service `{}` is not provided", key.name())))
    }

    /// An optional injection: `None` when nothing currently provides the key.
    pub fn try_get<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: &ServiceKey<T>,
    ) -> Result<Option<Arc<T>>, PluginError> {
        self.checked(key)
    }

    fn checked<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: &ServiceKey<T>,
    ) -> Result<Option<Arc<T>>, PluginError> {
        if !self.manifest.injects(key.name()) {
            return Err(PluginError(
                SubstrateError::UndeclaredInject {
                    plugin: self.manifest.name.to_string(),
                    key: key.name().to_string(),
                }
                .to_string(),
            ));
        }
        match self.bindings.get(key.name()) {
            Some(binding) => Ok(Some(binding.typed(key).map_err(|e| PluginError(e.to_string()))?)),
            None => Ok(None),
        }
    }

    /// The mounted provider's capability facts for a key, if any.
    pub fn facts(&self, key: &str) -> Option<&Facts> {
        if !self.manifest.injects(key) {
            return None;
        }
        self.bindings.get(key).map(|b| &b.facts)
    }

    /// Bind an implementation to a service key. The binding is an effect:
    /// unload withdraws it, deactivating consumers first.
    pub fn provide<T: ?Sized + Send + Sync + 'static>(
        &mut self,
        key: &ServiceKey<T>,
        implementation: Arc<T>,
        facts: Facts,
    ) -> Result<(), PluginError> {
        if !self.manifest.declares_provide(key.name()) {
            return Err(PluginError(
                SubstrateError::UndeclaredProvide {
                    plugin: self.manifest.name.to_string(),
                    key: key.name().to_string(),
                }
                .to_string(),
            ));
        }
        if let Some(existing) = self.bindings.get(key.name()) {
            return Err(PluginError(
                SubstrateError::ProvideConflict {
                    key: key.name().to_string(),
                    holder: existing.provider.to_string(),
                }
                .to_string(),
            ));
        }
        self.bindings.insert(
            key.name().to_string(),
            Binding::new(Provider::Fiber(self.entry.clone()), implementation, facts),
        );
        self.effects.push(Effect {
            label: format!("provide {}", key.name()),
            action: EffectAction::Unbind(key.name().to_string()),
        });
        Ok(())
    }

    /// Record a custom effect: any change to shared state, paired with its
    /// undo now rather than in a separate uninstall path.
    pub fn effect(&mut self, label: impl Into<String>, undo: impl FnOnce() + Send + 'static) {
        self.effects.push(Effect {
            label: label.into(),
            action: EffectAction::Custom(Box::new(undo)),
        });
    }

    /// The event bus. Subscriptions should be kept alive via
    /// [`ApplyCx::keep`] so they unwind with the fiber.
    pub fn bus(&self) -> &EventBus {
        self.bus
    }

    /// Tie a subscription's lifetime to this fiber.
    pub fn keep(&mut self, label: impl Into<String>, subscription: Subscription) {
        self.effects.push(Effect {
            label: label.into(),
            action: EffectAction::Custom(Box::new(move || drop(subscription))),
        });
    }
}
