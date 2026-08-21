//! The loaded-plugin conformance harness (`design/registry.md`): every
//! gate a `.wasm` artifact passes through — the author's red/green loop
//! (`inseam plugin check`), the node's install-time admission, and the
//! registry's CI — runs this same code, so passing it once means passing it
//! everywhere.
//!
//! Four phases, in increasing depth:
//! - **static** — the manifest is well-formed and internally coherent;
//! - **mount** — the component compiles, instantiates against the real
//!   bridge linker, and exports claims that intersect the manifest's;
//! - **contract** — the hostile-input battery: applications with text
//!   withheld, capabilities refused, and garbage bytes must degrade, never
//!   trap (a guest panic *is* a trap, so "never panics" falls out here);
//! - **golden** — the plugin's own declarative checks
//!   (`<artifact>.checks.toml`): example inputs and expected output shapes,
//!   executed through the real bridge with a canned LLM. Tests as data, not
//!   code — nothing in them needs to be trusted, only run.
//!
//! The harness is hermetic by construction: capabilities are fakes, no
//! kernel boots, nothing persists, and per-call instantiation guarantees no
//! residue between cases.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

use inseam_conformance::{ChecksFile, Emitted, EmittedFragment, GoldenCheck};
use inseam_kernel::fragment::{Mimetype, RelationKind};
use inseam_seams::transforms::GrantedLlm;
use inseam_seams::SeamError;

use crate::exports::inseam::plugin::transform::{ClaimSpec, Envelope, Fragment};
use crate::{
    build_linker, new_engine, pattern_matches, patterns_overlap, ArtifactManifest, Invocation,
    TransformPlugin, WasmEntryConfig,
};

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Static,
    Mount,
    Contract,
    Golden,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Static => "static",
            Self::Mount => "mount",
            Self::Contract => "contract",
            Self::Golden => "golden",
        })
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Pass,
    Warn(String),
    Fail(String),
}

#[derive(Debug, Clone)]
pub struct CheckItem {
    pub phase: Phase,
    pub name: String,
    pub outcome: Outcome,
}

/// The harness verdict: an ordered list of checks with outcomes. Failures
/// never abort the run early unless later phases would be meaningless
/// (an unreadable manifest, a component that will not instantiate).
#[derive(Debug)]
pub struct CheckReport {
    pub artifact: PathBuf,
    pub items: Vec<CheckItem>,
    /// Declared ∩ exported claim patterns, once the mount phase reaches them.
    pub effective_claims: Vec<String>,
}

impl CheckReport {
    fn new(artifact: &Path) -> Self {
        Self {
            artifact: artifact.to_path_buf(),
            items: Vec::new(),
            effective_claims: Vec::new(),
        }
    }

    fn push(&mut self, phase: Phase, name: impl Into<String>, outcome: Outcome) {
        self.items.push(CheckItem {
            phase,
            name: name.into(),
            outcome,
        });
    }

    pub fn passed(&self) -> bool {
        !self
            .items
            .iter()
            .any(|i| matches!(i.outcome, Outcome::Fail(_)))
    }

    pub fn warnings(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.outcome, Outcome::Warn(_)))
            .count()
    }

    /// The first failure as `"<check>: <reason>"` — what admission stores
    /// and surfaces as the fiber's failure reason.
    pub fn first_failure(&self) -> Option<String> {
        self.items.iter().find_map(|i| match &i.outcome {
            Outcome::Fail(reason) => Some(format!("{}: {reason}", i.name)),
            _ => None,
        })
    }

    pub fn render(&self) -> String {
        let mut out = format!("checking {}\n\n", self.artifact.display());
        for item in &self.items {
            let mark = match &item.outcome {
                Outcome::Pass => "ok",
                Outcome::Warn(_) => "warn",
                Outcome::Fail(_) => "FAIL",
            };
            out.push_str(&format!("  {:<9} {:<48} {mark}\n", item.phase.to_string(), item.name));
            match &item.outcome {
                Outcome::Warn(detail) | Outcome::Fail(detail) => {
                    out.push_str(&format!("            {detail}\n"));
                }
                Outcome::Pass => {}
            }
        }
        if !self.effective_claims.is_empty() {
            out.push_str(&format!(
                "\neffective claims: {}\n",
                self.effective_claims.join(", ")
            ));
        }
        let warnings = self.warnings();
        if self.passed() {
            out.push_str(&format!(
                "\nPASS ({} checks{})\n",
                self.items.len(),
                if warnings > 0 {
                    format!(", {warnings} warning(s)")
                } else {
                    String::new()
                }
            ));
        } else {
            out.push_str(&format!(
                "\nFAIL — {}\n",
                self.first_failure().unwrap_or_default()
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Golden checks: `<artifact>.checks.toml`
// ---------------------------------------------------------------------------
//
// The schema, the expectation matcher, and the mandatory-coverage gate are
// shared with the linked tier (`inseam_conformance::golden`) — one
// definition of "what counts as a test" across both tiers.

/// The fixture paths a checks file references (relative to itself) — what an
/// installer must fetch alongside the checks file.
pub fn fixture_files(checks_toml: &str) -> Vec<PathBuf> {
    ChecksFile::parse(checks_toml)
        .map(|f| f.fixture_files())
        .unwrap_or_default()
}

/// The wasm seam's output in the tier-neutral shape the golden matcher
/// judges. The transform WIT seam emits fragments only, never entities.
fn emitted_from_fragments(fragments: &[Fragment]) -> Emitted {
    Emitted {
        fragments: fragments
            .iter()
            .map(|f| EmittedFragment {
                mimetype: f.mimetype.clone(),
                relation: f.relation.clone(),
                text: f.text.clone(),
            })
            .collect(),
        entities: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Fake capabilities
// ---------------------------------------------------------------------------

/// The harness's granted LLM: a canned reply, or a refusal — the two shapes
/// a real grant has (the metered handle refuses once budget is spent).
struct CannedLlm(Option<String>);

#[async_trait::async_trait]
impl GrantedLlm for CannedLlm {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String, SeamError> {
        self.0
            .clone()
            .ok_or_else(|| SeamError::Unavailable("conformance check: llm refused".into()))
    }

    async fn describe_image(
        &self,
        _prompt: &str,
        _mimetype: &str,
        _image: &[u8],
    ) -> Result<String, SeamError> {
        self.0
            .clone()
            .ok_or_else(|| SeamError::Unavailable("conformance check: llm refused".into()))
    }
}

/// A tiny valid PNG (1x1 pixel), for byte-wanting image plugins.
const PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
    0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
    0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
    0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

const SENTINEL: &str = "INSEAM CONFORMANCE SENTINEL";

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// Run every phase against an artifact on disk (manifest and mandatory
/// checks file resolved beside it). Infallible by design: anything wrong
/// lands as a `Fail` item in the report, not an error.
pub async fn check_artifact(artifact: &Path) -> CheckReport {
    let mut report = CheckReport::new(artifact);
    let fuel = WasmEntryConfig::default().fuel;

    // ---- static ----------------------------------------------------------
    let manifest_path = artifact.with_extension("manifest.toml");
    let manifest: ArtifactManifest = match std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))
        .and_then(|raw| {
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", manifest_path.display()))
        }) {
        Ok(m) => {
            report.push(Phase::Static, "manifest parses", Outcome::Pass);
            m
        }
        Err(e) => {
            report.push(Phase::Static, "manifest parses", Outcome::Fail(e));
            return report;
        }
    };
    if manifest.seam != "transform" {
        report.push(
            Phase::Static,
            "seam is supported",
            Outcome::Fail(format!("unsupported seam `{}`", manifest.seam)),
        );
        return report;
    }
    report.push(
        Phase::Static,
        "claims are well-formed",
        if manifest.claims.is_empty() {
            Outcome::Fail("manifest declares no claims; the plugin would never run".into())
        } else if let Some(bad) = manifest.claims.iter().find(|c| !claim_pattern_valid(c)) {
            Outcome::Fail(format!("`{bad}` is neither a mimetype essence nor a `type/*` pattern"))
        } else {
            Outcome::Pass
        },
    );
    report.push(
        Phase::Static,
        "kind is known",
        match manifest.kind.as_deref() {
            None | Some("structural") | Some("enrichment") => Outcome::Pass,
            Some(other) => Outcome::Warn(format!(
                "unknown kind `{other}`; the bridge will treat it as enrichment"
            )),
        },
    );
    report.push(
        Phase::Static,
        "capabilities are coherent",
        if !manifest.capabilities.llm && manifest.capabilities.llm_call_budget > 0 {
            Outcome::Warn("llm_call_budget without `llm = true` is inert".into())
        } else if manifest.capabilities.llm && manifest.capabilities.llm_call_budget == 0 {
            Outcome::Warn("`llm = true` with a zero budget: every call will be refused".into())
        } else {
            Outcome::Pass
        },
    );

    // ---- mount ------------------------------------------------------------
    let engine = new_engine();
    let component = match std::fs::read(artifact)
        .map_err(|e| format!("cannot read artifact: {e}"))
        .and_then(|bytes| {
            Component::new(&engine, &bytes).map_err(|e| format!("not a valid component: {e}"))
        }) {
        Ok(c) => {
            report.push(Phase::Mount, "component compiles", Outcome::Pass);
            c
        }
        Err(e) => {
            report.push(Phase::Mount, "component compiles", Outcome::Fail(e));
            return report;
        }
    };
    let linker = match build_linker(&engine) {
        Ok(l) => l,
        Err(e) => {
            report.push(Phase::Mount, "bridge linker", Outcome::Fail(e.to_string()));
            return report;
        }
    };
    let exported = match call_claims(&engine, &component, &linker, &manifest.name, fuel).await {
        Ok(spec) => {
            report.push(
                Phase::Mount,
                "instantiates and exports claims()",
                Outcome::Pass,
            );
            spec
        }
        Err(e) => {
            report.push(
                Phase::Mount,
                "instantiates and exports claims()",
                Outcome::Fail(e.to_string()),
            );
            return report;
        }
    };
    match call_claims(&engine, &component, &linker, &manifest.name, fuel).await {
        Ok(again) if again.mimetypes == exported.mimetypes && again.roots_only == exported.roots_only => {
            report.push(Phase::Mount, "claims are deterministic", Outcome::Pass);
        }
        Ok(_) => report.push(
            Phase::Mount,
            "claims are deterministic",
            Outcome::Fail("two claims() calls returned different answers".into()),
        ),
        Err(e) => report.push(
            Phase::Mount,
            "claims are deterministic",
            Outcome::Fail(e.to_string()),
        ),
    }
    report.effective_claims = manifest
        .claims
        .iter()
        .filter(|declared| exported.mimetypes.iter().any(|e| patterns_overlap(declared, e)))
        .cloned()
        .collect();
    report.push(
        Phase::Mount,
        "declared and exported claims intersect",
        if report.effective_claims.is_empty() {
            Outcome::Fail(format!(
                "no overlap (manifest declares {:?}, component exports {:?})",
                manifest.claims, exported.mimetypes
            ))
        } else {
            Outcome::Pass
        },
    );
    let Some(battery_mimetype) = report.effective_claims.first().map(|p| concrete_essence(p))
    else {
        return report;
    };

    // ---- contract ----------------------------------------------------------
    let grant = |llm: Option<Arc<dyn GrantedLlm>>, bytes: Option<Vec<u8>>| {
        Invocation::new(
            manifest.name.clone(),
            if manifest.capabilities.llm { llm } else { None },
            if manifest.capabilities.source_bytes { bytes } else { None },
        )
    };
    let battery_bytes: Vec<u8> = if battery_mimetype.starts_with("image/") {
        PIXEL_PNG.to_vec()
    } else {
        b"inseam conformance sample bytes".to_vec()
    };
    let envelope = synthetic_envelope(&battery_mimetype);

    // Bare input: no text, no capabilities. The degrade path in its purest
    // form — a plugin that traps here would trap on every offline node.
    let bare = raw_apply(
        &engine, &component, &linker, grant(None, None), fuel,
        &envelope, &battery_mimetype, true, None,
    )
    .await;
    report.push(Phase::Contract, "degrades without text or capabilities", verdict_item(&bare));

    // Capabilities present but refusing — the shape of a spent budget or a
    // guard denial mid-run.
    let refused = raw_apply(
        &engine, &component, &linker,
        grant(Some(Arc::new(CannedLlm(None))), Some(battery_bytes.clone())), fuel,
        &envelope, &battery_mimetype, true, Some("conformance sample text"),
    )
    .await;
    report.push(Phase::Contract, "degrades when the llm refuses", verdict_item(&refused));

    // Garbage bytes: enumeration lies sometimes; a mislabeled or truncated
    // file must not take the plugin down.
    if manifest.capabilities.source_bytes {
        let garbage = raw_apply(
            &engine, &component, &linker,
            grant(Some(Arc::new(CannedLlm(Some(SENTINEL.into())))), Some(vec![0x00, 0xFF, 0x13, 0x37])),
            fuel, &envelope, &battery_mimetype, true, None,
        )
        .await;
        report.push(Phase::Contract, "survives garbage bytes", verdict_item(&garbage));
    }

    // The granted run: everything a well-behaved application gets. Its
    // output feeds the hygiene and determinism checks.
    let granted = |text: Option<&'static str>| {
        raw_apply(
            &engine, &component, &linker,
            grant(Some(Arc::new(CannedLlm(Some(SENTINEL.into())))), Some(battery_bytes.clone())),
            fuel, &envelope, &battery_mimetype, true, text,
        )
    };
    let first = granted(Some("inseam conformance sample text")).await;
    report.push(Phase::Contract, "applies with full capabilities", verdict_item(&first));
    if let ApplyVerdict::Output(fragments) = &first {
        report.push(Phase::Contract, "output is hygienic", hygiene(fragments));
        let second = granted(Some("inseam conformance sample text")).await;
        report.push(
            Phase::Contract,
            "output is deterministic",
            match &second {
                ApplyVerdict::Output(again) if fragments_equal(fragments, again) => Outcome::Pass,
                ApplyVerdict::Output(_) => Outcome::Warn(
                    "identical input produced different output; sources will churn on re-index".into(),
                ),
                other => Outcome::Fail(format!("second identical application failed: {}", verdict_text(other))),
            },
        );
    }

    // ---- golden -------------------------------------------------------------
    // Golden checks are mandatory: a plugin that ships none, or ships ones
    // that prove nothing, fails here — the same verdict admission enforces.
    let checks_path = artifact.with_extension("checks.toml");
    let Ok(raw_checks) = std::fs::read_to_string(&checks_path) else {
        report.push(
            Phase::Golden,
            "checks file",
            Outcome::Fail(format!(
                "no {} — golden checks are mandatory; write them first \
                 (docs/plugins/validation.md)",
                checks_path.display()
            )),
        );
        return report;
    };
    let checks = match ChecksFile::parse(&raw_checks) {
        Ok(c) => c,
        Err(e) => {
            report.push(
                Phase::Golden,
                "checks file",
                Outcome::Fail(format!("{}: {e}", checks_path.display())),
            );
            return report;
        }
    };
    match checks.required_coverage() {
        Ok(()) => report.push(Phase::Golden, "mandatory coverage", Outcome::Pass),
        Err(unmet) => {
            for reason in unmet {
                report.push(Phase::Golden, "mandatory coverage", Outcome::Fail(reason));
            }
        }
    }
    for check in &checks.check {
        let outcome = run_golden(
            &engine, &component, &linker, &manifest, fuel, &checks_path, check,
            &report.effective_claims,
        )
        .await;
        report.push(Phase::Golden, check.name.clone(), outcome);
    }
    report
}

async fn run_golden(
    engine: &Engine,
    component: &Component,
    linker: &Linker<Invocation>,
    manifest: &ArtifactManifest,
    fuel: u64,
    checks_path: &Path,
    check: &GoldenCheck,
    effective_claims: &[String],
) -> Outcome {
    if check.llm_returns.is_some() && !manifest.capabilities.llm {
        return Outcome::Warn("llm_returns is inert: the manifest does not request `llm`".into());
    }
    if check.expect.entity.is_some() {
        return Outcome::Fail(
            "`entity` is not expressible on the transform WIT seam (it emits fragments only); \
             assert the shape with `fragment_contains`/`relation`/`mimetype`"
                .into(),
        );
    }
    let essence = check.mimetype.split(';').next().unwrap_or_default();
    let claimed = effective_claims
        .iter()
        .any(|pattern| pattern_matches(pattern, essence));
    if !claimed {
        return Outcome::Fail(format!(
            "`{}` is outside the effective claims {:?}; this check would never run in production",
            check.mimetype, effective_claims
        ));
    }
    let bytes = match check.fixture_path(checks_path) {
        Err(reason) => return Outcome::Fail(reason),
        Ok(None) => None,
        Ok(Some(path)) => match std::fs::read(&path) {
            Ok(b) if manifest.capabilities.source_bytes => Some(b),
            Ok(_) => {
                return Outcome::Warn(
                    "bytes_file is inert: the manifest does not request `source_bytes`".into(),
                )
            }
            Err(e) => return Outcome::Fail(format!("fixture {}: {e}", path.display())),
        },
    };
    let llm: Option<Arc<dyn GrantedLlm>> = manifest
        .capabilities
        .llm
        .then(|| Arc::new(CannedLlm(check.llm_returns.clone())) as Arc<dyn GrantedLlm>);
    let verdict = raw_apply(
        engine, component, linker,
        Invocation::new(manifest.name.clone(), llm, bytes),
        fuel,
        &synthetic_envelope(&check.mimetype),
        &check.mimetype,
        check.is_root,
        check.text.as_deref(),
    )
    .await;
    let fragments = match verdict {
        ApplyVerdict::Output(f) => f,
        ApplyVerdict::PluginErr(_) => Vec::new(),
        ApplyVerdict::Trap(t) => return Outcome::Fail(format!("trapped: {t}")),
    };
    match check.verdict(&emitted_from_fragments(&fragments)) {
        Ok(()) => Outcome::Pass,
        Err(reason) => Outcome::Fail(reason),
    }
}

// ---------------------------------------------------------------------------
// Raw invocation
// ---------------------------------------------------------------------------

enum ApplyVerdict {
    Output(Vec<Fragment>),
    PluginErr(String),
    Trap(String),
}

fn verdict_text(v: &ApplyVerdict) -> String {
    match v {
        ApplyVerdict::Output(f) => format!("{} fragment(s)", f.len()),
        ApplyVerdict::PluginErr(e) => format!("plugin error: {e}"),
        ApplyVerdict::Trap(t) => format!("trap: {t}"),
    }
}

/// The contract's core distinction: `Ok(empty)` is the promised degrade
/// path, a returned `Err` is tolerated but discouraged, a trap (which is
/// what a guest panic becomes) is a hard failure.
fn verdict_item(v: &ApplyVerdict) -> Outcome {
    match v {
        ApplyVerdict::Output(_) => Outcome::Pass,
        ApplyVerdict::PluginErr(e) => Outcome::Warn(format!(
            "returned Err({e:?}); prefer Ok with an empty fragment list — errors are logged noise"
        )),
        ApplyVerdict::Trap(t) => Outcome::Fail(format!("trapped: {t}")),
    }
}

async fn raw_apply(
    engine: &Engine,
    component: &Component,
    linker: &Linker<Invocation>,
    invocation: Invocation,
    fuel: u64,
    envelope: &Envelope,
    mimetype: &str,
    is_root: bool,
    text: Option<&str>,
) -> ApplyVerdict {
    let mut store = Store::new(engine, invocation);
    if let Err(e) = store.set_fuel(fuel) {
        return ApplyVerdict::Trap(e.to_string());
    }
    let result = async {
        let plugin = TransformPlugin::instantiate_async(&mut store, component, linker).await?;
        plugin
            .inseam_plugin_transform()
            .call_apply(&mut store, envelope, mimetype, is_root, text)
            .await
    }
    .await;
    match result {
        Ok(Ok(output)) => ApplyVerdict::Output(output.fragments),
        Ok(Err(e)) => ApplyVerdict::PluginErr(e),
        Err(trap) => ApplyVerdict::Trap(trap.to_string()),
    }
}

async fn call_claims(
    engine: &Engine,
    component: &Component,
    linker: &Linker<Invocation>,
    name: &str,
    fuel: u64,
) -> Result<ClaimSpec, wasmtime::Error> {
    let mut store = Store::new(engine, Invocation::new(name.to_string(), None, None));
    store.set_fuel(fuel)?;
    let plugin = TransformPlugin::instantiate_async(&mut store, component, linker).await?;
    plugin.inseam_plugin_transform().call_claims(&mut store).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn synthetic_envelope(mimetype: &str) -> Envelope {
    Envelope {
        source_type: "conformance-check".into(),
        content_type: mimetype.to_string(),
        hint: Some("inseam conformance check".into()),
        modified: Some(0),
        raw_bytes: 64,
    }
}

fn claim_pattern_valid(pattern: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => !prefix.is_empty() && !prefix.contains('/'),
        None => Mimetype::parse(pattern).is_ok(),
    }
}

/// A concrete essence a pattern matches, for driving `apply`.
fn concrete_essence(pattern: &str) -> String {
    match pattern.strip_suffix("/*") {
        Some(prefix) => format!("{prefix}/x-conformance"),
        None => pattern.to_string(),
    }
}

fn hygiene(fragments: &[Fragment]) -> Outcome {
    for (i, f) in fragments.iter().enumerate() {
        match Mimetype::parse(&f.mimetype) {
            Ok(m) if m.is_inseam_defined() => {
                return Outcome::Fail(format!(
                    "fragment {i} emits inseam-defined mimetype `{}`; the bridge drops these",
                    f.mimetype
                ))
            }
            Ok(_) => {}
            Err(e) => return Outcome::Fail(format!("fragment {i} mimetype: {e}")),
        }
        if f.relation.parse::<RelationKind>().is_err() {
            return Outcome::Warn(format!(
                "fragment {i} relation `{}` is unknown; the bridge will coerce it to `contains`",
                f.relation
            ));
        }
        if let Some(parent) = f.parent
            && parent as usize >= i
        {
            return Outcome::Fail(format!(
                "fragment {i} parent {parent} does not index an earlier fragment"
            ));
        }
    }
    Outcome::Pass
}

fn fragments_equal(a: &[Fragment], b: &[Fragment]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.parent == y.parent
                && x.mimetype == y.mimetype
                && x.relation == y.relation
                && x.text == y.text
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(mimetype: &str, relation: &str, text: Option<&str>, parent: Option<u32>) -> Fragment {
        Fragment {
            parent,
            mimetype: mimetype.into(),
            relation: relation.into(),
            text: text.map(Into::into),
        }
    }

    #[test]
    fn hygiene_rejects_forged_summaries_and_forward_parents() {
        let forged = [fragment("text/x-inseam-summary", "derived-from", Some("x"), None)];
        assert!(matches!(hygiene(&forged), Outcome::Fail(_)));
        let forward = [
            fragment("text/plain", "contains", Some("a"), Some(1)),
            fragment("text/plain", "contains", Some("b"), None),
        ];
        assert!(matches!(hygiene(&forward), Outcome::Fail(_)));
        let fine = [
            fragment("text/plain", "transcribes", Some("a"), None),
            fragment("text/plain", "contains", Some("b"), Some(0)),
        ];
        assert!(matches!(hygiene(&fine), Outcome::Pass));
    }

    #[test]
    fn claim_patterns_validate_essences_and_wildcards() {
        assert!(claim_pattern_valid("image/png"));
        assert!(claim_pattern_valid("image/*"));
        assert!(!claim_pattern_valid("/*"));
        assert!(!claim_pattern_valid("not a mimetype"));
    }
}
