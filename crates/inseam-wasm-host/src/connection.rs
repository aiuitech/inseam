//! The loaded **connection**: a component on the `connections` seam
//! (`design/connections.md`), mounted as one more steward of one host. The
//! bridge policy for this seam is **long-running**: one instance per entry,
//! configured once at mount and kept for the life of the fiber, so the
//! component can hold pagination cursors and small caches between calls.
//! Every call still gets its own fuel and fetch budget, and an instance
//! that traps is discarded and rebuilt on the next call — a fault costs one
//! call, never the host.
//!
//! What the bridge decides, not the component: the host id (derived from
//! the kind and principal the component names, so it cannot forge another
//! host's identity), the effective capabilities (declared AND exported),
//! the validity of every locator and envelope that comes back, and the
//! `observed` stamp on each envelope.

use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::{Mutex, MutexGuard};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Locator, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::{ApplyCx, Inject, Manifest as PluginManifest, Plugin, PluginError};
use inseam_seams::SeamError;
use inseam_seams::connection::{
    Capabilities, Connection, EnumeratedSource, HostDescription, HostKind, Registration,
    derive_host_id, register_as_effect,
};
use inseam_seams::text::{check_line_range, slice_lines};

use crate::connection_world::ConnectionPlugin;
use crate::connection_world::exports::inseam::plugin::connection::{
    ContentLength as WitLength, EdgeCapabilities as WitCapabilities, Envelope as WitEnvelope,
    HostDescription as WitHost, Source as WitSource,
};
use crate::{GrantedFetch, Grants, Invocation, Mounted, build_linker};

/// Most sources one enumeration may return; a component answering more is
/// truncated with a warning rather than allowed to fill memory.
pub const SOURCES_PER_ENUMERATION_MAX: usize = 100_000;

pub struct WasmConnectionPlugin {
    mounted: Mounted,
}

impl WasmConnectionPlugin {
    pub(crate) fn new(mounted: Mounted) -> Self {
        assert_eq!(mounted.manifest.seam, "connection");
        Self { mounted }
    }
}

#[async_trait::async_trait]
impl Plugin for WasmConnectionPlugin {
    fn manifest(&self) -> PluginManifest {
        static INJECT: &[Inject] = &[
            Inject::required("connections"),
            Inject::required("state"),
            Inject::optional("oauth"),
        ];
        PluginManifest {
            name: "wasm-connection",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let mounted = &self.mounted;
        mounted.pass_gates(cx).await?;
        let manifest = &mounted.manifest;
        let declared_kind = manifest.host_kind.as_deref().ok_or_else(|| {
            PluginError(format!(
                "{}: a connection manifest must name its `host_kind`",
                manifest.name
            ))
        })?;

        let component = Component::new(&mounted.engine, &mounted.artifact_bytes).map_err(|e| {
            PluginError(format!(
                "{}: not a valid component: {e}",
                mounted.artifact.display()
            ))
        })?;
        let linker = Arc::new(
            build_linker(&mounted.engine).map_err(|e| PluginError(format!("linker: {e}")))?,
        );
        let config_toml = toml::to_string(&mounted.config.plugin)
            .map_err(|e| PluginError(format!("[entry.config.plugin]: {e}")))?;
        let connection = WasmConnection {
            engine: mounted.engine.clone(),
            component,
            linker,
            plugin_name: manifest.name.clone(),
            config_toml,
            fetch: mounted.granted_fetch(cx)?,
            fetch_calls_max: mounted.config.fetch_calls_max,
            fuel: mounted.config.fuel,
            host: None,
            instance: Mutex::new(None),
            prefixes: std::sync::Mutex::new(Vec::new()),
        };

        // The one instance, started now: configure, then ask who it is.
        let mut instance = connection.start().await.map_err(PluginError)?;
        let described = instance
            .describe_host()
            .await
            .map_err(|e| PluginError(format!("describe-host trapped: {e}")))?
            .map_err(|e| PluginError(format!("describe-host: {e}")))?;
        let exported = instance
            .capabilities()
            .await
            .map_err(|e| PluginError(format!("capabilities trapped: {e}")))?;
        let host = host_of(&manifest.name, declared_kind, &described).map_err(PluginError)?;
        let capabilities = Capabilities {
            enumerates: manifest.connection.enumerates && exported.enumerates,
            change_feed: manifest.connection.change_feed && exported.change_feed,
            writable: manifest.connection.writable && exported.writable,
        };
        let connection = WasmConnection {
            host: Some(host.id.clone()),
            instance: Mutex::new(Some(instance)),
            prefixes: std::sync::Mutex::new(Vec::new()),
            ..connection
        };
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                host,
                capabilities,
                roots: mounted.config.roots.clone(),
                connection: Arc::new(connection),
            },
        )
    }
}

/// The host a component described, checked against the manifest and
/// derived into an id. The kind is identity: a component exporting a kind
/// other than the one its manifest names is refused, not adapted.
pub(crate) fn host_of(
    plugin: &str,
    declared_kind: &str,
    described: &WitHost,
) -> Result<HostDescription, String> {
    let kind = HostKind::new(described.kind.as_str()).map_err(|e| format!("{plugin}: {e}"))?;
    if kind.as_str() != declared_kind {
        return Err(format!(
            "{plugin}: the component stewards kind `{kind}` but the manifest declares `{declared_kind}`"
        ));
    }
    if described.principal.trim().is_empty() {
        return Err(format!("{plugin}: the component named an empty principal"));
    }
    let id = derive_host_id(&kind, &described.principal);
    let display_name = if described.display_name.trim().is_empty() {
        described.principal.trim().to_string()
    } else {
        described.display_name.clone()
    };
    Ok(HostDescription {
        id,
        kind,
        display_name,
    })
}

// ---------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------

/// One live instantiation of a connection component: the store carrying
/// its grants, and the bindings to call it through. Every method hands
/// back the raw shape — `Err` is a trap, `Ok(Err)` the plugin's own error —
/// so callers (the bridge, the harness) can tell the two apart.
pub(crate) struct Instance {
    store: Store<Invocation>,
    bindings: ConnectionPlugin,
    fuel: u64,
}

impl Instance {
    /// Instantiate and configure. A configuration the component refuses is
    /// an error naming the plugin's reason.
    pub(crate) async fn start(
        engine: &Engine,
        component: &Component,
        linker: &Linker<Invocation>,
        plugin: &str,
        grants: Grants,
        fuel: u64,
        config_toml: &str,
    ) -> Result<Self, String> {
        let mut store = Store::new(engine, Invocation::new(plugin.to_string(), grants));
        store.set_fuel(fuel).map_err(|e| e.to_string())?;
        let bindings = ConnectionPlugin::instantiate_async(&mut store, component, linker)
            .await
            .map_err(|e| format!("{plugin}: does not instantiate: {e}"))?;
        let mut instance = Self {
            store,
            bindings,
            fuel,
        };
        match instance.configure(config_toml).await {
            Ok(Ok(())) => Ok(instance),
            Ok(Err(reason)) => Err(format!(
                "{plugin}: configure refused the plugin config: {reason}"
            )),
            Err(trap) => Err(format!("{plugin}: configure trapped: {trap}")),
        }
    }

    /// Every call starts with a full tank and a fresh fetch budget.
    fn renew(&mut self) -> Result<(), wasmtime::Error> {
        self.store.set_fuel(self.fuel)?;
        self.store.data_mut().renew_call_budget();
        Ok(())
    }

    pub(crate) async fn configure(
        &mut self,
        config: &str,
    ) -> Result<Result<(), String>, wasmtime::Error> {
        self.renew()?;
        self.bindings
            .inseam_plugin_connection()
            .call_configure(&mut self.store, config)
            .await
    }

    pub(crate) async fn describe_host(
        &mut self,
    ) -> Result<Result<WitHost, String>, wasmtime::Error> {
        self.renew()?;
        self.bindings
            .inseam_plugin_connection()
            .call_describe_host(&mut self.store)
            .await
    }

    pub(crate) async fn capabilities(&mut self) -> Result<WitCapabilities, wasmtime::Error> {
        self.renew()?;
        self.bindings
            .inseam_plugin_connection()
            .call_capabilities(&mut self.store)
            .await
    }

    pub(crate) async fn enumerate(
        &mut self,
        root: &str,
    ) -> Result<Result<Vec<WitSource>, String>, wasmtime::Error> {
        self.renew()?;
        self.bindings
            .inseam_plugin_connection()
            .call_enumerate(&mut self.store, root)
            .await
    }

    pub(crate) async fn locator_prefix(
        &mut self,
        root: &str,
    ) -> Result<Option<String>, wasmtime::Error> {
        self.renew()?;
        self.bindings
            .inseam_plugin_connection()
            .call_locator_prefix(&mut self.store, root)
            .await
    }

    pub(crate) async fn read_bytes(
        &mut self,
        locator: &str,
    ) -> Result<Result<Vec<u8>, String>, wasmtime::Error> {
        self.renew()?;
        self.bindings
            .inseam_plugin_connection()
            .call_read_bytes(&mut self.store, locator)
            .await
    }

    pub(crate) async fn describe(
        &mut self,
        locator: &str,
    ) -> Result<Result<WitEnvelope, String>, wasmtime::Error> {
        self.renew()?;
        self.bindings
            .inseam_plugin_connection()
            .call_describe(&mut self.store, locator)
            .await
    }
}

// ---------------------------------------------------------------------------
// The bridged connection
// ---------------------------------------------------------------------------

/// The connection the registry holds: the component's instance behind a
/// lock (a component is single-threaded; calls are serialized), rebuilt
/// after a trap.
struct WasmConnection {
    engine: Engine,
    component: Component,
    linker: Arc<Linker<Invocation>>,
    plugin_name: String,
    config_toml: String,
    fetch: Option<Arc<dyn GrantedFetch>>,
    fetch_calls_max: u32,
    fuel: u64,
    /// Set once the host is known; every address is checked against it.
    host: Option<HostId>,
    /// `None` between a trap and the next call.
    instance: Mutex<Option<Instance>>,
    /// The locator prefix recorded per enumerated scope, newest last and
    /// bounded, for the seam's synchronous `locator_prefix`.
    prefixes: std::sync::Mutex<Vec<(String, Option<String>)>>,
}

/// Scopes whose locator prefix is remembered; a node sweeps a handful.
const PREFIXES_REMEMBERED_MAX: usize = 256;

impl WasmConnection {
    async fn start(&self) -> Result<Instance, String> {
        Instance::start(
            &self.engine,
            &self.component,
            &self.linker,
            &self.plugin_name,
            Grants {
                llm: None,
                bytes: None,
                fetch: self.fetch.clone(),
                fetch_calls_max: self.fetch_calls_max,
            },
            self.fuel,
            &self.config_toml,
        )
        .await
    }

    /// The live instance, started afresh if the last call trapped.
    async fn ready<'g>(
        &self,
        guard: &'g mut MutexGuard<'_, Option<Instance>>,
    ) -> Result<&'g mut Instance, SeamError> {
        if guard.is_none() {
            tracing::info!(plugin = %self.plugin_name, "re-instantiating the connection after a trap");
            **guard = Some(self.start().await.map_err(SeamError::failed)?);
        }
        guard
            .as_mut()
            .ok_or_else(|| SeamError::failed("instance vanished"))
    }

    /// Turn a call's raw shape into the seam's: a trap discards the
    /// instance and is an error; the plugin's own error passes through.
    fn settle<T>(
        &self,
        guard: &mut MutexGuard<'_, Option<Instance>>,
        call: &str,
        result: Result<Result<T, String>, wasmtime::Error>,
    ) -> Result<T, SeamError> {
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(reason)) => Err(SeamError::failed(format!(
                "{}: {call}: {reason}",
                self.plugin_name
            ))),
            Err(trap) => {
                tracing::warn!(plugin = %self.plugin_name, "{call} trapped; the instance is discarded: {trap}");
                **guard = None;
                Err(SeamError::failed(format!(
                    "{}: {call} trapped: {trap}",
                    self.plugin_name
                )))
            }
        }
    }

    fn record_prefix(&self, root: &str, prefix: Option<String>) {
        let mut prefixes = self.prefixes.lock().unwrap_or_else(|e| e.into_inner());
        prefixes.retain(|(scope, _)| scope != root);
        if prefixes.len() >= PREFIXES_REMEMBERED_MAX {
            prefixes.remove(0);
        }
        prefixes.push((root.to_string(), prefix));
        assert!(prefixes.len() <= PREFIXES_REMEMBERED_MAX);
    }

    fn own_locator<'a>(&self, address: &'a Address) -> Result<&'a str, SeamError> {
        match &self.host {
            Some(host) if *host == address.host => Ok(address.locator.as_str()),
            Some(host) => Err(SeamError::failed(format!(
                "address {address} names host `{}`, not `{host}`",
                address.host
            ))),
            None => Err(SeamError::failed("connection has no host yet")),
        }
    }
}

#[async_trait::async_trait]
impl Connection for WasmConnection {
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let host = self
            .host
            .clone()
            .ok_or_else(|| SeamError::failed("connection has no host yet"))?;
        let mut guard = self.instance.lock().await;
        let result = self.ready(&mut guard).await?.enumerate(root).await;
        let sources = self.settle(&mut guard, "enumerate", result)?;
        let prefix = self.ready(&mut guard).await?.locator_prefix(root).await;
        match prefix {
            Ok(prefix) => self.record_prefix(root, prefix),
            Err(trap) => {
                tracing::warn!(plugin = %self.plugin_name, "locator-prefix trapped; the instance is discarded: {trap}");
                *guard = None;
            }
        }
        Ok(admit_sources(&self.plugin_name, &host, sources))
    }

    /// The seam's method is synchronous and the instance sits behind an
    /// async lock, so the answer is the one recorded when the scope was
    /// last enumerated (`enumerate` asks the component right after). A
    /// scope never enumerated has no recorded prefix — reconciliation is
    /// skipped rather than guessed, as the seam documents for `None`.
    fn locator_prefix(&self, root: &str) -> Option<String> {
        self.prefixes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(scope, _)| scope == root)
            .and_then(|(_, prefix)| prefix.clone())
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let bytes = self.read_bytes(address).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        check_line_range(start, end)?;
        let text = self.read_text(address).await?;
        slice_lines(&text, start, end)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let locator = self.own_locator(address)?.to_string();
        let mut guard = self.instance.lock().await;
        let result = self.ready(&mut guard).await?.read_bytes(&locator).await;
        self.settle(&mut guard, "read-bytes", result)
    }

    async fn describe(&self, address: &Address) -> Result<Envelope, SeamError> {
        let locator = self.own_locator(address)?.to_string();
        let mut guard = self.instance.lock().await;
        let result = self.ready(&mut guard).await?.describe(&locator).await;
        let envelope = self.settle(&mut guard, "describe", result)?;
        Ok(envelope_of(&self.plugin_name, envelope))
    }
}

// ---------------------------------------------------------------------------
// Output hygiene
// ---------------------------------------------------------------------------

/// The sources a component enumerated, each checked: a locator that does
/// not parse is dropped with a warning, and the list is bounded.
pub(crate) fn admit_sources(
    plugin: &str,
    host: &HostId,
    sources: Vec<WitSource>,
) -> Vec<EnumeratedSource> {
    if sources.len() > SOURCES_PER_ENUMERATION_MAX {
        tracing::warn!(
            plugin,
            "enumeration returned {} sources; keeping the first {SOURCES_PER_ENUMERATION_MAX}",
            sources.len()
        );
    }
    sources
        .into_iter()
        .take(SOURCES_PER_ENUMERATION_MAX)
        .filter_map(|source| match Locator::new(source.locator.as_str()) {
            Ok(locator) => Some(EnumeratedSource {
                address: Address::new(host.clone(), locator),
                envelope: envelope_of(plugin, source.envelope),
                raw_bytes: source.raw_bytes,
            }),
            Err(e) => {
                tracing::warn!(plugin, locator = %source.locator, "dropping source with a malformed locator: {e}");
                None
            }
        })
        .collect()
}

/// The kernel's envelope from the component's: content type parsed (an
/// unparseable one becomes `application/octet-stream`), `observed`
/// stamped now, and the trust and digest fields — which are the node's to
/// fill, never a plugin's to claim — left empty.
pub(crate) fn envelope_of(plugin: &str, envelope: WitEnvelope) -> Envelope {
    let content_type = Mimetype::parse(&envelope.content_type).unwrap_or_else(|e| {
        tracing::warn!(plugin, content_type = %envelope.content_type, "unparseable content type ({e}); using application/octet-stream");
        Mimetype::parse("application/octet-stream").expect("literal mimetype is valid")
    });
    Envelope {
        source_type: envelope.source_type,
        content_type,
        length: match envelope.length {
            WitLength::Lines(n) => ContentLength::Lines(n),
            WitLength::Bytes(n) => ContentLength::Bytes(n),
        },
        created: envelope.created.map(Timestamp),
        modified: envelope.modified.map(Timestamp),
        observed: Timestamp::from(SystemTime::now()),
        properties: Vec::new(),
        hint: envelope.hint.filter(|h| !h.trim().is_empty()),
        content_digest: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wit_envelope(content_type: &str) -> WitEnvelope {
        WitEnvelope {
            source_type: "file".into(),
            content_type: content_type.into(),
            length: WitLength::Bytes(3),
            created: None,
            modified: Some(1_700_000_000),
            hint: Some("  ".into()),
        }
    }

    #[test]
    fn hosts_are_derived_from_kind_and_principal_and_the_kind_is_identity() {
        let described = WitHost {
            kind: "github".into(),
            principal: "Octo/Hello".into(),
            display_name: String::new(),
        };
        let host = host_of("x", "github", &described).expect("valid");
        assert_eq!(
            host.id,
            derive_host_id(&HostKind::new("github").expect("kind"), "octo/hello")
        );
        assert_eq!(
            host.display_name, "Octo/Hello",
            "an empty display name falls back to the principal"
        );
        assert!(
            host_of("x", "gitlab", &described).is_err(),
            "kind mismatch is refused"
        );
        let empty = WitHost {
            principal: " ".into(),
            ..described.clone()
        };
        assert!(host_of("x", "github", &empty).is_err());
        let bad_kind = WitHost {
            kind: "Git Hub".into(),
            ..described
        };
        assert!(host_of("x", "github", &bad_kind).is_err());
    }

    #[test]
    fn envelopes_are_parsed_and_stamped_by_the_bridge() {
        let envelope = envelope_of("x", wit_envelope("text/markdown"));
        assert_eq!(envelope.content_type.essence(), "text/markdown");
        assert_eq!(envelope.modified, Some(Timestamp(1_700_000_000)));
        assert_eq!(envelope.hint, None, "a blank hint is no hint");
        assert!(envelope.properties.is_empty());
        assert!(envelope.content_digest.is_none());
        let fallback = envelope_of("x", wit_envelope("not a mimetype"));
        assert_eq!(fallback.content_type.essence(), "application/octet-stream");
    }

    #[test]
    fn malformed_locators_are_dropped_and_the_rest_addressed_on_the_host() {
        let host = HostId::new("github-0123456789abcdef").expect("host id");
        let source = |locator: &str| WitSource {
            locator: locator.into(),
            envelope: wit_envelope("text/plain"),
            raw_bytes: 3,
        };
        let admitted = admit_sources(
            "x",
            &host,
            vec![source("README.md"), source(""), source("src/lib.rs")],
        );
        assert_eq!(admitted.len(), 2);
        assert_eq!(admitted[0].address.host, host);
        assert_eq!(admitted[1].address.locator.as_str(), "src/lib.rs");
    }
}
