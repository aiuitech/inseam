//! The plugin-host bridge (`design/plugins.md`): mounts **loaded
//! plugins** — WASM components against the WIT projection of the service
//! seams — into the same plugin model linked plugins use. Tier is
//! provenance, not shape: to the `transforms` registry, a component-backed
//! transform is indistinguishable from a linked one, and to the
//! `connections` registry a component-backed host is one more steward.
//!
//! Two seams cross the boundary today. A **transform** (`transform.rs`) is
//! instantiated per call, so nothing leaks between sources. A
//! **connection** (`connection.rs`) is long-running: one instance per
//! entry, configured once, kept for the life of the fiber, re-instantiated
//! only after a trap.
//!
//! Security posture, in order:
//! - **Sandboxed by construction**: a component sees only the host imports
//!   the bridge implements, attenuated per its manifest. No sockets, no
//!   filesystem, no ambient anything.
//! - **Capability attenuation at the bridge**: the LLM handle a component
//!   calls through is the same metered grant linked transforms get; the
//!   network is a described request the node performs under the manifest's
//!   host allow list and the node's SSRF guard; an OAuth grant is a bearer
//!   the node attaches, never a token the component sees.
//! - **Claims cannot widen silently**: effective claims are the manifest's
//!   declared claims intersected with what the component exports; a
//!   connection's effective capabilities are declared AND exported, and its
//!   host kind must be the one the manifest names.
//! - **Release cooldown**: a newly observed artifact soaks before it may
//!   activate, on a locally unforgeable first-seen clock; capability
//!   widening between versions — a new host in the allow list included —
//!   requires explicit owner approval regardless of soak.
//! - **Fuel limits**: every call runs with bounded fuel, so a spinning
//!   component times out instead of wedging the sweep.
//! - **Install-time admission**: the first time this node sees an artifact,
//!   the conformance harness ([`check_artifact`]) runs against it — a
//!   component that traps on hostile input or fails its own golden checks
//!   is refused with a reason instead of mounting and silently degrading.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use wasmtime::Engine;
use wasmtime::component::Linker;

use inseam_kernel::substrate::{
    ApplyCx, Plugin, PluginError, STATE, SchemeFactory, fnv1a, parse_config,
};
use inseam_seams::SeamError;
use inseam_seams::fetch::{FetchMethod, FetchRequest, FetchResponse, Fetcher, HostPattern};
use inseam_seams::oauth::{Grant, GrantId, OAUTH};
use inseam_seams::transforms::GrantedLlm;

wasmtime::component::bindgen!({
    world: "transform-plugin",
    imports: { default: async },
    exports: { default: async },
});

/// The connection world shares the `host` and `fetch` imports with the
/// transform world — one implementation of each, one linker.
mod connection_world {
    wasmtime::component::bindgen!({
        world: "connection-plugin",
        imports: { default: async },
        exports: { default: async },
        with: {
            "inseam:plugin/host": crate::inseam::plugin::host,
            "inseam:plugin/fetch": crate::inseam::plugin::fetch,
        },
    });
}

mod check;
mod check_connection;
mod connection;
mod transform;

pub use check::{
    CheckItem, CheckReport, Outcome, Phase, TriedFragment, TryInput, TryOutcome, check_artifact,
    fixture_files, try_artifact,
};
pub use check_connection::{TriedSource, TryCall, TryConnectionOutcome, try_connection};
pub use connection::WasmConnectionPlugin;
pub use transform::WasmTransformPlugin;

/// The whole WIT package, embedded so the CLI can hand it to an author
/// (`inseam seams --wit`) without a source checkout or a network. Both
/// worlds, one package, one file: redirect it to `wit/plugin.wit` and
/// generate bindings from it.
pub fn plugin_wit() -> String {
    const TRANSFORM: &str = include_str!("../wit/transform.wit");
    const CONNECTION: &str = include_str!("../wit/connection.wit");
    let package_line = "package inseam:plugin@0.1.0;\n";
    assert!(TRANSFORM.contains(package_line));
    assert!(CONNECTION.starts_with(package_line));
    format!("{TRANSFORM}\n{}", &CONNECTION[package_line.len()..])
}

/// The seams that accept loaded plugins, as the manifest names them.
pub const SEAMS: [&str; 2] = ["transform", "connection"];

use inseam::plugin::fetch::{Host as FetchImports, Request, Response};
use inseam::plugin::host::Host as HostImports;

/// The engine every bridge and harness instance shares the configuration
/// of: async execution with fuel metering.
fn new_engine() -> Engine {
    let mut config = wasmtime::Config::new();
    config.async_support(true);
    config.consume_fuel(true);
    Engine::new(&config).expect("static wasmtime config is valid")
}

/// The real bridge linker: our `host` and `fetch` interfaces plus core WASI
/// (satisfied only by the empty context). The harness links the same way,
/// so a component that mounts under `check` mounts under the kernel.
fn build_linker(engine: &Engine) -> Result<Linker<Invocation>, wasmtime::Error> {
    let mut linker: Linker<Invocation> = Linker::new(engine);
    inseam::plugin::host::add_to_linker::<Invocation, wasmtime::component::HasSelf<Invocation>>(
        &mut linker,
        |state| state,
    )?;
    inseam::plugin::fetch::add_to_linker::<Invocation, wasmtime::component::HasSelf<Invocation>>(
        &mut linker,
        |state| state,
    )?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    Ok(linker)
}

// ---------------------------------------------------------------------------
// Granted capabilities
// ---------------------------------------------------------------------------

/// The network as a component sees it: a described request the node
/// performs, or refuses. The real grant is the SSRF-guarded fetcher under
/// the manifest's allow list; the harness's is canned replies.
#[async_trait::async_trait]
pub trait GrantedFetch: Send + Sync {
    /// Perform `request`; with `authorize`, attach the granted credential
    /// first (refuse when there is none).
    async fn fetch(
        &self,
        request: FetchRequest,
        authorize: bool,
    ) -> Result<FetchResponse, SeamError>;
}

/// The bridge's real network grant: the guard, and the OAuth grant whose
/// bearer `authorize` attaches. The token is read here and only here —
/// it never crosses into the component.
pub struct GuardedFetch {
    fetcher: Fetcher,
    grant: Option<Arc<dyn Grant>>,
}

impl GuardedFetch {
    /// The network grant a manifest earns under an entry's dials: the
    /// guard over the manifest's allow list, plus the grant when there is
    /// one. `None` when the manifest names no host — then `fetch` refuses.
    pub(crate) fn for_manifest(
        manifest: &ArtifactManifest,
        config: &WasmEntryConfig,
        grant: Option<Arc<dyn Grant>>,
    ) -> Result<Option<Arc<dyn GrantedFetch>>, String> {
        let patterns = manifest.capabilities.host_patterns()?;
        if patterns.is_empty() {
            return Ok(None);
        }
        let fetcher = Fetcher::for_allowed_hosts_only(
            patterns,
            config.fetch_bytes_max.max(1),
            Duration::from_millis(config.fetch_timeout_ms.max(1)),
            config.fetch_redirects_max,
            format!(
                "inseam/{} plugin/{}",
                env!("CARGO_PKG_VERSION"),
                manifest.name
            ),
        );
        Ok(Some(Arc::new(Self { fetcher, grant })))
    }
}

#[async_trait::async_trait]
impl GrantedFetch for GuardedFetch {
    async fn fetch(
        &self,
        request: FetchRequest,
        authorize: bool,
    ) -> Result<FetchResponse, SeamError> {
        let mut extra: Vec<(String, String)> = Vec::new();
        if authorize {
            let Some(grant) = &self.grant else {
                return Err(SeamError::Unavailable(
                    "authorize requested, but this entry names no oauth grant (set `grant = \"<id>\"` on the entry)"
                        .to_string(),
                ));
            };
            let token = grant.access_token().await?;
            extra.push(("Authorization".to_string(), token.authorization_header()));
        }
        self.fetcher.send(&request, &extra).await
    }
}

/// Everything a component may be handed for one instantiation. Each
/// field is `None` unless the manifest requested it and the node can
/// grant it.
pub(crate) struct Grants {
    pub llm: Option<Arc<dyn GrantedLlm>>,
    pub bytes: Option<Vec<u8>>,
    pub fetch: Option<Arc<dyn GrantedFetch>>,
    /// Requests one instantiation (a transform) or one seam call (a
    /// connection) may perform.
    pub fetch_calls_max: u32,
}

impl Grants {
    pub(crate) fn none() -> Self {
        Self {
            llm: None,
            bytes: None,
            fetch: None,
            fetch_calls_max: 0,
        }
    }
}

/// One instantiation's host-side state: the capabilities it was granted,
/// and nothing else. The WASI context exists only because the
/// `wasm32-wasip2` std links core WASI interfaces; it is built **empty** —
/// no preopened directories, no environment, no args, no network — so the
/// component's real surface stays the `host` and `fetch` interfaces.
pub(crate) struct Invocation {
    plugin: String,
    grants: Grants,
    fetch_calls_left: u32,
    wasi: wasmtime_wasi::WasiCtx,
    table: wasmtime_wasi::ResourceTable,
}

impl Invocation {
    pub(crate) fn new(plugin: String, grants: Grants) -> Self {
        let fetch_calls_left = grants.fetch_calls_max;
        Self {
            plugin,
            grants,
            fetch_calls_left,
            wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
            table: wasmtime_wasi::ResourceTable::new(),
        }
    }

    /// Start a fresh call budget: what a long-running connection does
    /// before every seam call.
    pub(crate) fn renew_call_budget(&mut self) {
        self.fetch_calls_left = self.grants.fetch_calls_max;
    }
}

impl wasmtime_wasi::WasiView for Invocation {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl HostImports for Invocation {
    async fn log(&mut self, message: String) {
        tracing::info!(plugin = %self.plugin, "{message}");
    }

    async fn llm_complete(&mut self, system: String, user: String) -> Result<String, String> {
        match &self.grants.llm {
            Some(llm) => llm
                .complete(&system, &user)
                .await
                .map_err(|e| e.to_string()),
            None => Err("llm capability not granted".to_string()),
        }
    }

    async fn llm_describe_image(
        &mut self,
        prompt: String,
        mimetype: String,
        image: Vec<u8>,
    ) -> Result<String, String> {
        match &self.grants.llm {
            Some(llm) => llm
                .describe_image(&prompt, &mimetype, &image)
                .await
                .map_err(|e| e.to_string()),
            None => Err("llm capability not granted".to_string()),
        }
    }

    async fn source_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.grants
            .bytes
            .clone()
            .ok_or_else(|| "source-bytes capability not granted".to_string())
    }
}

impl FetchImports for Invocation {
    async fn fetch(&mut self, request: Request) -> Result<Response, String> {
        let Some(granted) = &self.grants.fetch else {
            return Err("fetch capability not granted: the manifest names no hosts".to_string());
        };
        if self.fetch_calls_left == 0 {
            return Err(format!(
                "fetch call budget spent ({} per call)",
                self.grants.fetch_calls_max
            ));
        }
        self.fetch_calls_left -= 1;
        let described = describe_request(&request).map_err(|e| e.to_string())?;
        let answer = granted
            .fetch(described, request.authorize)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Response {
            status: answer.status,
            headers: answer.headers,
            body: answer.body,
        })
    }
}

/// Parse a component's request at the boundary: a known method, a URL that
/// parses, hygienic headers. Refusals name what was wrong.
fn describe_request(request: &Request) -> Result<FetchRequest, SeamError> {
    let method = FetchMethod::try_from(request.method.as_str())?;
    let url = url::Url::parse(&request.url)
        .map_err(|e| SeamError::Refused(format!("url `{}`: {e}", request.url)))?;
    let described = FetchRequest {
        method,
        url,
        headers: request.headers.clone(),
        body: request.body.clone(),
    };
    described.validate()?;
    Ok(described)
}

// ---------------------------------------------------------------------------
// Artifact manifest
// ---------------------------------------------------------------------------

/// The manifest that ships beside a `.wasm` artifact
/// (`<artifact>.manifest.toml`): identity, which seam the component
/// implements, what it declares for that seam, and the capabilities it
/// requests. This is what an owner (or a registry scanner) reviews — the
/// bridge enforces that the component gets nothing beyond it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    pub name: String,
    pub version: String,
    /// The seam the component implements: `transform` or `connection`.
    pub seam: String,
    /// Transform seam: declared claims, mimetype essences or `type/*`.
    #[serde(default)]
    pub claims: Vec<String>,
    #[serde(default = "default_true")]
    pub roots_only: bool,
    /// Transform seam: `structural` or `enrichment` (default).
    #[serde(default)]
    pub kind: Option<String>,
    /// Connection seam: the host kind the component stewards (`github`).
    /// Identity, not a hint — the exported kind must equal it.
    #[serde(default)]
    pub host_kind: Option<String>,
    /// Connection seam: what the edge is declared to support; the
    /// effective capabilities are these AND what the component exports.
    #[serde(default)]
    pub connection: ConnectionDeclaration,
    #[serde(default)]
    pub capabilities: Capabilities,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectionDeclaration {
    pub enumerates: bool,
    pub change_feed: bool,
    pub writable: bool,
}

impl Default for ConnectionDeclaration {
    fn default() -> Self {
        Self {
            enumerates: true,
            change_feed: false,
            writable: false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    /// May call `llm-complete` / `llm-describe-image` (metered).
    pub llm: bool,
    /// May read the claimed source's raw bytes.
    pub source_bytes: bool,
    /// LLM calls per index run charged to this plugin.
    pub llm_call_budget: usize,
    /// The hosts `fetch` may contact — exact hosts or `*.domain`
    /// patterns. Empty means the component can reach nothing; adding one
    /// later is capability widening.
    pub hosts: Vec<String>,
    /// May ask `fetch` to attach the bearer token of the OAuth grant the
    /// entry names (`grant = "…"` in the entry config).
    pub grant: bool,
}

impl Capabilities {
    fn summary(&self) -> String {
        let mut hosts = self.hosts.clone();
        hosts.sort();
        format!(
            "llm={},source_bytes={},budget={},hosts=[{}],grant={}",
            self.llm,
            self.source_bytes,
            self.llm_call_budget,
            hosts.join(","),
            self.grant
        )
    }

    /// The allow list, parsed. Every pattern must be well-formed: a
    /// manifest naming a host the guard cannot read is a manifest that
    /// grants something nobody reviewed.
    pub fn host_patterns(&self) -> Result<Vec<HostPattern>, String> {
        self.hosts.iter().map(|h| HostPattern::parse(h)).collect()
    }
}

/// What a failed admission check does to the mount: refuse (the default),
/// mount with a logged warning, or skip admission entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdmissionMode {
    #[default]
    Enforce,
    Warn,
    Off,
}

/// Per-entry bridge config (the composition side, distinct from the
/// artifact's own manifest): the owner's dials.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WasmEntryConfig {
    /// Days a newly observed artifact must soak before activating
    /// (`design/plugins.md` — release cooldown). The clock counts from when
    /// *this node* first saw the artifact hash; nothing in the artifact can
    /// forge that.
    pub cooldown_days: u32,
    /// Explicit owner override: activate a version regardless of soak time
    /// or capability widening. A consent moment, not a default.
    pub allow_new: bool,
    /// Fuel per call; a spinning component runs out instead of wedging
    /// the sweep.
    pub fuel: u64,
    /// Install-time admission: run the conformance harness the first time
    /// this artifact (+ manifest + checks) is seen, and refuse a failing
    /// plugin. Cached by content hash in the node's state.
    pub admission: AdmissionMode,
    /// The plugin's own configuration, handed to a connection component
    /// through `configure` as TOML. Its schema is the plugin's.
    pub plugin: toml::Table,
    /// Connection seam: the scopes the owner configured this host to
    /// index, as the component interprets them (`Registration::roots`).
    pub roots: Vec<String>,
    /// The OAuth grant whose bearer `fetch` attaches on `authorize`
    /// (requires the manifest's `grant` capability).
    pub grant: Option<GrantId>,
    /// Most bytes one fetched body may be.
    pub fetch_bytes_max: u64,
    /// Whole-request timeout for one fetch.
    pub fetch_timeout_ms: u64,
    /// Redirect hops one fetch may follow, each re-guarded; at most ten.
    pub fetch_redirects_max: u32,
    /// Fetches per transform application, or per connection call.
    pub fetch_calls_max: u32,
}

impl Default for WasmEntryConfig {
    fn default() -> Self {
        Self {
            cooldown_days: 0,
            allow_new: false,
            fuel: 2_000_000_000,
            admission: AdmissionMode::Enforce,
            plugin: toml::Table::new(),
            roots: Vec::new(),
            grant: None,
            fetch_bytes_max: 16 * 1024 * 1024,
            fetch_timeout_ms: 10_000,
            fetch_redirects_max: 3,
            fetch_calls_max: 64,
        }
    }
}

// ---------------------------------------------------------------------------
// The scheme factory
// ---------------------------------------------------------------------------

/// Resolves `wasm:<path-to-artifact>` plugin refs for the kernel. One shared
/// engine compiles every component.
pub struct WasmSchemeFactory {
    engine: Engine,
}

impl WasmSchemeFactory {
    pub fn new(_data_dir: &Path) -> Self {
        Self {
            engine: new_engine(),
        }
    }
}

impl SchemeFactory for WasmSchemeFactory {
    fn scheme(&self) -> &str {
        "wasm:"
    }

    fn build(
        &self,
        artifact_ref: &str,
        config: &toml::Table,
    ) -> Result<Box<dyn Plugin>, PluginError> {
        let mounted = Mounted::load(self.engine.clone(), Path::new(artifact_ref), config)?;
        match mounted.manifest.seam.as_str() {
            "transform" => Ok(Box::new(WasmTransformPlugin::new(mounted))),
            "connection" => Ok(Box::new(WasmConnectionPlugin::new(mounted))),
            other => Err(PluginError(format!(
                "unsupported seam `{other}`; this bridge mounts {} components",
                SEAMS.join(" and ")
            ))),
        }
    }
}

/// Everything both seams' plugins share: the loaded artifact, its
/// manifest and entry config, and the gates every mount passes.
pub(crate) struct Mounted {
    pub engine: Engine,
    pub artifact: PathBuf,
    pub artifact_hash: String,
    admission_hash: String,
    pub artifact_bytes: Vec<u8>,
    pub manifest: ArtifactManifest,
    pub config: WasmEntryConfig,
}

impl Mounted {
    fn load(engine: Engine, artifact: &Path, config: &toml::Table) -> Result<Self, PluginError> {
        let manifest_path = artifact.with_extension("manifest.toml");
        let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
            PluginError(format!(
                "cannot read plugin manifest {}: {e}",
                manifest_path.display()
            ))
        })?;
        let manifest: ArtifactManifest = toml::from_str(&raw)
            .map_err(|e| PluginError(format!("{}: {e}", manifest_path.display())))?;
        let config: WasmEntryConfig = parse_config(config)?;
        manifest.capabilities.host_patterns().map_err(|e| {
            PluginError(format!(
                "{}: [capabilities] hosts: {e}",
                manifest_path.display()
            ))
        })?;
        if config.grant.is_some() && !manifest.capabilities.grant {
            return Err(PluginError(format!(
                "entry names grant `{}`, but the manifest does not request the `grant` capability",
                config
                    .grant
                    .as_ref()
                    .map(|g| g.as_str())
                    .unwrap_or_default()
            )));
        }
        let bytes = std::fs::read(artifact).map_err(|e| {
            PluginError(format!("cannot read artifact {}: {e}", artifact.display()))
        })?;
        // Admission is keyed over everything that decides its verdict, so
        // editing the manifest or the golden checks re-runs it even when the
        // component itself is unchanged.
        let checks = std::fs::read(artifact.with_extension("checks.toml")).unwrap_or_default();
        let admission_hash = format!(
            "{:016x}",
            fnv1a(&[bytes.as_slice(), raw.as_bytes(), checks.as_slice()].concat())
        );
        Ok(Self {
            engine,
            artifact: artifact.to_path_buf(),
            artifact_hash: format!("{:016x}", fnv1a(&bytes)),
            admission_hash,
            artifact_bytes: bytes,
            manifest,
            config,
        })
    }

    /// The gates every mount passes, in order: cooldown, then admission.
    pub(crate) async fn pass_gates(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        self.enforce_cooldown(cx).await?;
        self.admit(cx).await
    }

    /// The network grant this entry gets: the guard under the manifest's
    /// allow list, plus the entry's OAuth grant when the manifest may use
    /// one. `None` when the manifest names no host — then `fetch` refuses.
    pub(crate) fn granted_fetch(
        &self,
        cx: &ApplyCx<'_>,
    ) -> Result<Option<Arc<dyn GrantedFetch>>, PluginError> {
        let grant = match (&self.config.grant, self.manifest.capabilities.grant) {
            (Some(id), true) => {
                let oauth = cx.get(&OAUTH)?;
                Some(oauth.grant(id).ok_or_else(|| {
                    PluginError(format!(
                        "entry names grant `{id}`, which no oauth provider holds (docs/plugins/oauth.md)"
                    ))
                })?)
            }
            _ => None,
        };
        GuardedFetch::for_manifest(&self.manifest, &self.config, grant).map_err(PluginError)
    }

    /// The release-cooldown gate. First-seen timestamps live in the
    /// kernel-provided state service under this bridge's namespace; the
    /// capability summary of the last approved version is stored beside
    /// them so widening is its own gate, regardless of soak time.
    async fn enforce_cooldown(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let state = cx.get(&STATE)?;
        let ns = state
            .namespace("wasm-host", "1")
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let seen_key = format!("first-seen:{}", self.artifact_hash);
        let first_seen: u64 = match ns
            .get(&seen_key)
            .await
            .map_err(|e| PluginError(e.to_string()))?
        {
            Some(ts) => ts.parse().unwrap_or(now),
            None => {
                ns.put(&seen_key, &now.to_string())
                    .await
                    .map_err(|e| PluginError(e.to_string()))?;
                now
            }
        };

        // Capability widening is its own gate: the diff, not the clock, is
        // the question.
        let caps_key = format!("capabilities:{}", self.manifest.name);
        let approved = ns
            .get(&caps_key)
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        let requested = self.manifest.capabilities.summary();
        match approved {
            Some(prior) if prior != requested && !self.config.allow_new => {
                return Err(PluginError(format!(
                    "plugin `{}` requests different capabilities than the approved version \
                     (approved: {prior}; requested: {requested}); review the manifest and set \
                     `allow_new = true` on its entry to approve",
                    self.manifest.name
                )));
            }
            _ => {
                ns.put(&caps_key, &requested)
                    .await
                    .map_err(|e| PluginError(e.to_string()))?;
            }
        }

        let cooldown_secs = u64::from(self.config.cooldown_days) * 86_400;
        let age = now.saturating_sub(first_seen);
        if age < cooldown_secs && !self.config.allow_new {
            let remaining_days = (cooldown_secs - age).div_ceil(86_400);
            return Err(PluginError(format!(
                "plugin `{}` version {} was first observed by this node {} day(s) ago and is in \
                 release cooldown for {} more day(s); set `allow_new = true` on its entry to \
                 activate it now (explicit consent for a version this young)",
                self.manifest.name,
                self.manifest.version,
                age / 86_400,
                remaining_days
            )));
        }
        Ok(())
    }

    /// Install-time admission (`design/registry.md`): run the conformance
    /// harness the first time this exact artifact + manifest + checks
    /// combination is seen, and cache the verdict in the node's state. A
    /// plugin that traps on hostile input or fails its own golden checks is
    /// refused with the report's first failure instead of mounting and
    /// silently degrading forever.
    async fn admit(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        if self.config.admission == AdmissionMode::Off {
            return Ok(());
        }
        let state = cx.get(&STATE)?;
        let ns = state
            .namespace("wasm-host", "1")
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        let key = format!("admission:{}", self.admission_hash);
        let verdict = match ns.get(&key).await.map_err(|e| PluginError(e.to_string()))? {
            Some(cached) => cached,
            None => {
                tracing::info!(
                    plugin = %self.manifest.name,
                    "first sighting of this artifact; running admission checks"
                );
                let report = check::check_artifact(&self.artifact).await;
                let verdict = match report.first_failure() {
                    None => "pass".to_string(),
                    Some(failure) => format!("fail:{failure}"),
                };
                ns.put(&key, &verdict)
                    .await
                    .map_err(|e| PluginError(e.to_string()))?;
                verdict
            }
        };
        match (verdict.strip_prefix("fail:"), self.config.admission) {
            (None, _) => Ok(()),
            (Some(reason), AdmissionMode::Warn) => {
                tracing::warn!(
                    plugin = %self.manifest.name,
                    "failed admission but admission = \"warn\": {reason}"
                );
                Ok(())
            }
            (Some(reason), _) => Err(PluginError(format!(
                "plugin `{}` failed admission: {reason} — run `inseam plugin check {}` for the \
                 full report; set `admission = \"warn\"` or `\"off\"` on its entry to override",
                self.manifest.name,
                self.artifact.display()
            ))),
        }
    }
}

/// `type/*` and exact-essence matching for claim patterns.
pub(crate) fn pattern_matches(pattern: &str, essence: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => essence
            .split_once('/')
            .is_some_and(|(t, _)| t.eq_ignore_ascii_case(prefix)),
        None => pattern.eq_ignore_ascii_case(essence),
    }
}

/// Whether two claim patterns can match a common essence (used for the
/// declared ∩ exported intersection).
pub(crate) fn patterns_overlap(a: &str, b: &str) -> bool {
    match (a.strip_suffix("/*"), b.strip_suffix("/*")) {
        (Some(pa), Some(pb)) => pa.eq_ignore_ascii_case(pb),
        (Some(_), None) => pattern_matches(a, b),
        (None, Some(_)) => pattern_matches(b, a),
        (None, None) => a.eq_ignore_ascii_case(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_patterns_match_exact_and_wildcard() {
        assert!(pattern_matches("image/png", "image/png"));
        assert!(pattern_matches("image/*", "image/webp"));
        assert!(!pattern_matches("image/*", "text/plain"));
        assert!(!pattern_matches("image/png", "image/jpeg"));
    }

    #[test]
    fn pattern_overlap_covers_wildcards_both_ways() {
        assert!(patterns_overlap("image/*", "image/png"));
        assert!(patterns_overlap("image/png", "image/*"));
        assert!(patterns_overlap("image/*", "image/*"));
        assert!(!patterns_overlap("image/*", "text/*"));
        assert!(!patterns_overlap("image/png", "image/jpeg"));
    }

    #[test]
    fn the_capability_summary_orders_hosts_so_only_real_widening_gates() {
        let a = Capabilities {
            hosts: vec!["b.example".into(), "a.example".into()],
            ..Capabilities::default()
        };
        let b = Capabilities {
            hosts: vec!["a.example".into(), "b.example".into()],
            ..Capabilities::default()
        };
        assert_eq!(a.summary(), b.summary());
        let wider = Capabilities {
            hosts: vec!["a.example".into(), "b.example".into(), "c.example".into()],
            ..Capabilities::default()
        };
        assert_ne!(a.summary(), wider.summary());
        let granted = Capabilities {
            grant: true,
            ..a.clone()
        };
        assert_ne!(a.summary(), granted.summary());
    }

    #[test]
    fn manifests_refuse_malformed_host_patterns() {
        let manifest: ArtifactManifest = toml::from_str(
            "name = \"x\"\nversion = \"0.1.0\"\nseam = \"connection\"\nhost_kind = \"x\"\n[capabilities]\nhosts = [\"*\"]\n",
        )
        .expect("parses");
        assert!(manifest.capabilities.host_patterns().is_err());
    }

    #[test]
    fn described_requests_are_parsed_at_the_boundary() {
        let fine = Request {
            method: "get".into(),
            url: "https://api.example.com/x".into(),
            headers: vec![("Accept".into(), "application/json".into())],
            body: None,
            authorize: false,
        };
        assert!(describe_request(&fine).is_ok());
        let bad_method = Request {
            method: "TRACE".into(),
            ..fine.clone()
        };
        assert!(describe_request(&bad_method).is_err());
        let bad_url = Request {
            url: "not a url".into(),
            ..fine.clone()
        };
        assert!(describe_request(&bad_url).is_err());
        let bad_header = Request {
            headers: vec![("Host".into(), "evil".into())],
            ..fine
        };
        assert!(describe_request(&bad_header).is_err());
    }
}
