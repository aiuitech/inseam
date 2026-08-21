//! The plugin-host bridge (`design/plugins.md`): mounts **loaded
//! plugins** — WASM components against the WIT projection of the service
//! seams — into the same plugin model linked plugins use. Tier is
//! provenance, not shape: to the `transforms` registry, a component-backed
//! transform is indistinguishable from a linked one.
//!
//! Security posture, in order:
//! - **Sandboxed by construction**: a component sees only the host imports
//!   the bridge implements, attenuated per its manifest. No sockets, no
//!   filesystem, no ambient anything.
//! - **Capability attenuation at the bridge**: the LLM handle a component
//!   calls through is the same metered grant linked transforms get; the
//!   manifest gates whether it exists at all.
//! - **Claims cannot widen silently**: effective claims are the manifest's
//!   declared claims intersected with what the component exports.
//! - **Release cooldown**: a newly observed artifact soaks before it may
//!   activate, on a locally unforgeable first-seen clock; capability
//!   widening between versions requires explicit owner approval regardless
//!   of soak (`design/plugins.md` — release cooldown).
//! - **Fuel limits**: every application runs with bounded fuel, so a
//!   spinning component times out instead of wedging the sweep.
//! - **Install-time admission**: the first time this node sees an artifact,
//!   the conformance harness ([`check_artifact`]) runs against it — a
//!   component that traps on hostile input or fails its own golden checks
//!   is refused with a reason instead of mounting and silently degrading.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

use inseam_kernel::fragment::{Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{
    fnv1a, parse_config, ApplyCx, Inject, Manifest as PluginManifest, Plugin, PluginError,
    SchemeFactory, STATE,
};
use inseam_seams::transforms::{
    register_as_effect, GrantedLlm, Registration, Transform, TransformCtx, TransformKind,
    TransformOutput,
};

wasmtime::component::bindgen!({
    world: "transform-plugin",
    imports: { default: async },
    exports: { default: async },
});

mod check;

pub use check::{
    check_artifact, fixture_files, try_artifact, CheckItem, CheckReport, Outcome, Phase,
    TriedFragment, TryInput, TryOutcome,
};

/// The transform seam's WIT world, embedded so the CLI can hand it to an
/// author (`inseam seams --wit`) without a source checkout or a network.
pub const TRANSFORM_WIT: &str = include_str!("../wit/transform.wit");

use inseam::plugin::host::Host as HostImports;

/// The engine every bridge and harness instance shares the configuration
/// of: async execution with fuel metering.
fn new_engine() -> Engine {
    let mut config = wasmtime::Config::new();
    config.async_support(true);
    config.consume_fuel(true);
    Engine::new(&config).expect("static wasmtime config is valid")
}

/// The real bridge linker: our `host` interface plus core WASI (satisfied
/// only by the empty context). The harness links the same way, so a
/// component that mounts under `check` mounts under the kernel.
fn build_linker(engine: &Engine) -> Result<Linker<Invocation>, wasmtime::Error> {
    let mut linker: Linker<Invocation> = Linker::new(engine);
    inseam::plugin::host::add_to_linker::<Invocation, wasmtime::component::HasSelf<Invocation>>(
        &mut linker,
        |state| state,
    )?;
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    Ok(linker)
}

/// One transform application's host-side state: the capabilities this
/// invocation was granted, and nothing else. The WASI context exists only
/// because the `wasm32-wasip2` std links core WASI interfaces; it is built
/// **empty** — no preopened directories, no environment, no args, no
/// network — so the component's real surface stays the `host` interface.
struct Invocation {
    plugin: String,
    llm: Option<Arc<dyn GrantedLlm>>,
    bytes: Option<Vec<u8>>,
    wasi: wasmtime_wasi::WasiCtx,
    table: wasmtime_wasi::ResourceTable,
}

impl Invocation {
    fn new(plugin: String, llm: Option<Arc<dyn GrantedLlm>>, bytes: Option<Vec<u8>>) -> Self {
        Self {
            plugin,
            llm,
            bytes,
            wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
            table: wasmtime_wasi::ResourceTable::new(),
        }
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
        match &self.llm {
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
        match &self.llm {
            Some(llm) => llm
                .describe_image(&prompt, &mimetype, &image)
                .await
                .map_err(|e| e.to_string()),
            None => Err("llm capability not granted".to_string()),
        }
    }

    async fn source_bytes(&mut self) -> Result<Vec<u8>, String> {
        self.bytes
            .clone()
            .ok_or_else(|| "source-bytes capability not granted".to_string())
    }
}

// ---------------------------------------------------------------------------
// Artifact manifest
// ---------------------------------------------------------------------------

/// The manifest that ships beside a `.wasm` artifact
/// (`<artifact>.manifest.toml`): identity, which seam the component
/// implements, its declared claims, and the capabilities it requests. This
/// is what an owner (or a registry scanner) reviews — the bridge enforces
/// that the component gets nothing beyond it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactManifest {
    pub name: String,
    pub version: String,
    /// The seam the component implements; `transform` is the first.
    pub seam: String,
    /// Declared claims: mimetype essences or `type/*` patterns.
    #[serde(default)]
    pub claims: Vec<String>,
    #[serde(default = "default_true")]
    pub roots_only: bool,
    /// `structural` or `enrichment` (default).
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

fn default_true() -> bool {
    true
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
}

impl Capabilities {
    fn summary(&self) -> String {
        format!(
            "llm={},source_bytes={},budget={}",
            self.llm, self.source_bytes, self.llm_call_budget
        )
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
/// artifact's own manifest).
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
    /// Fuel per application; a spinning component runs out instead of
    /// wedging the sweep.
    pub fuel: u64,
    /// Install-time admission: run the conformance harness the first time
    /// this artifact (+ manifest + checks) is seen, and refuse a failing
    /// plugin. Cached by content hash in the node's state.
    pub admission: AdmissionMode,
}

impl Default for WasmEntryConfig {
    fn default() -> Self {
        Self {
            cooldown_days: 0,
            allow_new: false,
            fuel: 2_000_000_000,
            admission: AdmissionMode::Enforce,
        }
    }
}

// ---------------------------------------------------------------------------
// The scheme factory and plugin
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
        let artifact = PathBuf::from(artifact_ref);
        let manifest_path = artifact.with_extension("manifest.toml");
        let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
            PluginError(format!(
                "cannot read plugin manifest {}: {e}",
                manifest_path.display()
            ))
        })?;
        let manifest: ArtifactManifest = toml::from_str(&raw)
            .map_err(|e| PluginError(format!("{}: {e}", manifest_path.display())))?;
        if manifest.seam != "transform" {
            return Err(PluginError(format!(
                "unsupported seam `{}`; this bridge mounts `transform` components",
                manifest.seam
            )));
        }
        let bytes = std::fs::read(&artifact).map_err(|e| {
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
        Ok(Box::new(WasmTransformPlugin {
            engine: self.engine.clone(),
            artifact,
            artifact_hash: format!("{:016x}", fnv1a(&bytes)),
            admission_hash,
            artifact_bytes: bytes,
            manifest,
            config: parse_config(config)?,
        }))
    }
}

pub struct WasmTransformPlugin {
    engine: Engine,
    artifact: PathBuf,
    artifact_hash: String,
    admission_hash: String,
    artifact_bytes: Vec<u8>,
    manifest: ArtifactManifest,
    config: WasmEntryConfig,
}

#[async_trait::async_trait]
impl Plugin for WasmTransformPlugin {
    fn manifest(&self) -> PluginManifest {
        static INJECT: &[Inject] = &[
            Inject::required("transforms"),
            Inject::required("state"),
            Inject::optional("llm"),
        ];
        PluginManifest {
            name: "wasm-transform",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        self.enforce_cooldown(cx).await?;
        self.admit(cx).await?;

        let component = Component::new(&self.engine, &self.artifact_bytes)
            .map_err(|e| PluginError(format!("{}: not a valid component: {e}", self.artifact.display())))?;
        let linker = build_linker(&self.engine).map_err(|e| PluginError(format!("linker: {e}")))?;

        // Ask the component for its claims once, at mount: the effective
        // claim set is declared ∩ exported.
        let exported = self
            .call_claims(&component, &linker)
            .await
            .map_err(|e| PluginError(format!("{}: claims() failed: {e}", self.artifact.display())))?;
        let effective: Vec<String> = self
            .manifest
            .claims
            .iter()
            .filter(|declared| {
                exported
                    .mimetypes
                    .iter()
                    .any(|e| patterns_overlap(declared, e))
            })
            .cloned()
            .collect();
        if effective.is_empty() {
            return Err(PluginError(format!(
                "{}: no effective claims (manifest declares {:?}, component exports {:?})",
                self.manifest.name, self.manifest.claims, exported.mimetypes
            )));
        }

        let kind = match self.manifest.kind.as_deref() {
            Some("structural") => TransformKind::Structural,
            _ => TransformKind::Enrichment,
        };
        let transform = WasmTransform {
            engine: self.engine.clone(),
            component,
            linker: Arc::new(linker),
            plugin_name: self.manifest.name.clone(),
            claims: effective,
            roots_only: self.manifest.roots_only || exported.roots_only,
            kind,
            wants_bytes: self.manifest.capabilities.source_bytes,
            grant_llm: self.manifest.capabilities.llm,
            fuel: self.config.fuel,
        };

        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: self.manifest.name.clone(),
                transform: Arc::new(transform),
                llm_call_budget: if self.manifest.capabilities.llm {
                    self.manifest.capabilities.llm_call_budget
                } else {
                    0
                },
                // The artifact version and content hash are in the shape
                // fingerprint: an upgraded loaded transform dirties exactly
                // the sources it built (`design/index-maintenance.md`).
                shape_fingerprint: format!(
                    "wasm|{}|{}|{}",
                    self.manifest.name, self.manifest.version, self.artifact_hash
                ),
            },
        )
    }
}

impl WasmTransformPlugin {
    /// The release-cooldown gate. First-seen timestamps live in the
    /// kernel-provided state service under this bridge's namespace; the
    /// capability summary of the last approved version is stored beside
    /// them so widening is its own gate, regardless of soak time.
    async fn enforce_cooldown(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let state = cx.get(&STATE)?;
        let ns = state
            .namespace("wasm-host", "1").await
            .map_err(|e| PluginError(e.to_string()))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let seen_key = format!("first-seen:{}", self.artifact_hash);
        let first_seen: u64 = match ns.get(&seen_key).await.map_err(|e| PluginError(e.to_string()))? {
            Some(ts) => ts.parse().unwrap_or(now),
            None => {
                ns.put(&seen_key, &now.to_string()).await
                    .map_err(|e| PluginError(e.to_string()))?;
                now
            }
        };

        // Capability widening is its own gate: the diff, not the clock, is
        // the question.
        let caps_key = format!("capabilities:{}", self.manifest.name);
        let approved = ns.get(&caps_key).await.map_err(|e| PluginError(e.to_string()))?;
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
                ns.put(&caps_key, &requested).await
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
            .namespace("wasm-host", "1").await
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
                ns.put(&key, &verdict).await
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

    async fn call_claims(
        &self,
        component: &Component,
        linker: &Linker<Invocation>,
    ) -> Result<exports::inseam::plugin::transform::ClaimSpec, wasmtime::Error> {
        let mut store = Store::new(
            &self.engine,
            Invocation::new(self.manifest.name.clone(), None, None),
        );
        store.set_fuel(self.config.fuel)?;
        let plugin = TransformPlugin::instantiate_async(&mut store, component, linker).await?;
        plugin
            .inseam_plugin_transform()
            .call_claims(&mut store)
            .await
    }
}

// ---------------------------------------------------------------------------
// The bridged transform
// ---------------------------------------------------------------------------

struct WasmTransform {
    engine: Engine,
    component: Component,
    linker: Arc<Linker<Invocation>>,
    plugin_name: String,
    claims: Vec<String>,
    roots_only: bool,
    kind: TransformKind,
    wants_bytes: bool,
    grant_llm: bool,
    fuel: u64,
}

#[async_trait::async_trait]
impl Transform for WasmTransform {
    fn kind(&self) -> TransformKind {
        self.kind
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        if mimetype.is_inseam_defined() || (self.roots_only && !is_root) {
            return false;
        }
        self.claims
            .iter()
            .any(|pattern| pattern_matches(pattern, mimetype.essence()))
    }

    fn wants_bytes(&self) -> bool {
        self.wants_bytes
    }

    /// Transforms are per-call instantiations (`design/plugins.md`): fresh
    /// component state every application, so a poisoned run cannot leak
    /// into the next source.
    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let mut store = Store::new(
            &self.engine,
            Invocation::new(
                self.plugin_name.clone(),
                if self.grant_llm { ctx.llm.clone() } else { None },
                ctx.bytes.map(<[u8]>::to_vec),
            ),
        );
        if store.set_fuel(self.fuel).is_err() {
            return TransformOutput::default();
        }
        let envelope = exports::inseam::plugin::transform::Envelope {
            source_type: ctx.envelope.source_type.clone(),
            content_type: ctx.envelope.content_type.to_string(),
            hint: ctx.envelope.hint.clone(),
            modified: ctx.envelope.modified.map(|t| t.0),
            raw_bytes: ctx.envelope.length.value(),
        };
        let result = async {
            let plugin =
                TransformPlugin::instantiate_async(&mut store, &self.component, &self.linker)
                    .await?;
            plugin
                .inseam_plugin_transform()
                .call_apply(
                    &mut store,
                    &envelope,
                    &ctx.mimetype.to_string(),
                    ctx.is_root,
                    ctx.text,
                )
                .await
        }
        .await;
        match result {
            Ok(Ok(output)) => sprout_forest(&self.plugin_name, output.fragments),
            Ok(Err(plugin_error)) => {
                tracing::warn!(
                    plugin = %self.plugin_name,
                    "loaded transform reported an error, emitting nothing: {plugin_error}"
                );
                TransformOutput::default()
            }
            Err(trap) => {
                tracing::warn!(
                    plugin = %self.plugin_name,
                    "loaded transform trapped (fuel exhausted or fault), emitting nothing: {trap}"
                );
                TransformOutput::default()
            }
        }
    }
}

/// Rebuild the sprout tree from the WIT-flattened fragment list. A parent
/// must index an earlier fragment; violations are logged and treated as
/// roots rather than trusted.
fn sprout_forest(
    plugin: &str,
    fragments: Vec<exports::inseam::plugin::transform::Fragment>,
) -> TransformOutput {
    let mut nodes: Vec<Option<Sprout>> = Vec::with_capacity(fragments.len());
    let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); fragments.len()];
    let mut roots: Vec<usize> = Vec::new();

    for (i, f) in fragments.iter().enumerate() {
        let mimetype = match Mimetype::parse(&f.mimetype) {
            Ok(m) if !m.is_inseam_defined() => m,
            Ok(_) => {
                tracing::warn!(plugin, mimetype = %f.mimetype, "loaded transform may not emit inseam-defined mimetypes; dropped");
                nodes.push(None);
                continue;
            }
            Err(e) => {
                tracing::warn!(plugin, "dropping fragment with bad mimetype: {e}");
                nodes.push(None);
                continue;
            }
        };
        let relation = f.relation.parse::<RelationKind>().unwrap_or_else(|_| {
            tracing::warn!(plugin, relation = %f.relation, "unknown relation; using contains");
            RelationKind::Contains
        });
        nodes.push(Some(Sprout::leaf(
            NewFragment {
                mimetype,
                text: f.text.clone(),
                extent: None,
            },
            relation,
        )));
        match f.parent {
            Some(p) if (p as usize) < i => children_of[p as usize].push(i),
            Some(_) => {
                tracing::warn!(plugin, "fragment parent must index an earlier fragment; treating as root");
                roots.push(i);
            }
            None => roots.push(i),
        }
    }

    fn build(i: usize, nodes: &mut [Option<Sprout>], children_of: &[Vec<usize>]) -> Option<Sprout> {
        let mut sprout = nodes[i].take()?;
        for &c in &children_of[i] {
            if let Some(child) = build(c, nodes, children_of) {
                sprout.children.push(child);
            }
        }
        Some(sprout)
    }

    let sprouts = roots
        .into_iter()
        .filter_map(|i| build(i, &mut nodes, &children_of))
        .collect();
    TransformOutput::sprouts(sprouts)
}

/// `type/*` and exact-essence matching for claim patterns.
fn pattern_matches(pattern: &str, essence: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => essence
            .split_once('/')
            .is_some_and(|(t, _)| t.eq_ignore_ascii_case(prefix)),
        None => pattern.eq_ignore_ascii_case(essence),
    }
}

/// Whether two claim patterns can match a common essence (used for the
/// declared ∩ exported intersection).
fn patterns_overlap(a: &str, b: &str) -> bool {
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
    fn sprout_forest_rebuilds_trees_and_rejects_bad_parents() {
        use exports::inseam::plugin::transform::Fragment;
        let out = sprout_forest(
            "test",
            vec![
                Fragment {
                    parent: None,
                    mimetype: "text/plain".into(),
                    relation: "transcribes".into(),
                    text: Some("root".into()),
                },
                Fragment {
                    parent: Some(0),
                    mimetype: "text/plain".into(),
                    relation: "contains".into(),
                    text: Some("child".into()),
                },
                Fragment {
                    parent: Some(9),
                    mimetype: "text/plain".into(),
                    relation: "contains".into(),
                    text: Some("orphan".into()),
                },
                Fragment {
                    parent: None,
                    mimetype: "text/x-inseam-summary".into(),
                    relation: "derived-from".into(),
                    text: Some("forged summary".into()),
                },
            ],
        );
        assert_eq!(out.sprouts.len(), 2, "root + orphan-as-root; forged summary dropped");
        assert_eq!(out.sprouts[0].relation, RelationKind::Transcribes);
        assert_eq!(out.sprouts[0].children.len(), 1);
        assert_eq!(
            out.sprouts[0].children[0].fragment.text.as_deref(),
            Some("child")
        );
    }
}
