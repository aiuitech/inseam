//! The loaded **transform**: a component on the `transforms` seam,
//! instantiated per call (`design/plugins.md` — the bridge policy for this
//! seam), so a poisoned run cannot leak into the next source.

use std::sync::Arc;

use wasmtime::Store;
use wasmtime::component::{Component, Linker};

use inseam_kernel::fragment::{Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{ApplyCx, Inject, Manifest as PluginManifest, Plugin, PluginError};
use inseam_seams::llm::LlmLane;
use inseam_seams::transforms::{
    Registration, Transform, TransformCtx, TransformKind, TransformOutput, register_as_effect,
};

use crate::exports::inseam::plugin::transform::{ClaimSpec, Envelope, Fragment};
use crate::{
    GrantedFetch, Grants, Invocation, Mounted, TransformPlugin, build_linker, pattern_matches,
    patterns_overlap,
};

pub struct WasmTransformPlugin {
    mounted: Mounted,
}

impl WasmTransformPlugin {
    pub(crate) fn new(mounted: Mounted) -> Self {
        assert_eq!(mounted.manifest.seam, "transform");
        Self { mounted }
    }
}

#[async_trait::async_trait]
impl Plugin for WasmTransformPlugin {
    fn manifest(&self) -> PluginManifest {
        static INJECT: &[Inject] = &[
            Inject::required("transforms"),
            Inject::required("state"),
            Inject::optional("llm"),
            Inject::optional("oauth"),
        ];
        PluginManifest {
            name: "wasm-transform",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let mounted = &self.mounted;
        mounted.pass_gates(cx).await?;
        let manifest = &mounted.manifest;

        let component = Component::new(&mounted.engine, &mounted.artifact_bytes).map_err(|e| {
            PluginError(format!(
                "{}: not a valid component: {e}",
                mounted.artifact.display()
            ))
        })?;
        let linker =
            build_linker(&mounted.engine).map_err(|e| PluginError(format!("linker: {e}")))?;

        // Ask the component for its claims once, at mount: the effective
        // claim set is declared ∩ exported.
        let exported = self.call_claims(&component, &linker).await.map_err(|e| {
            PluginError(format!(
                "{}: claims() failed: {e}",
                mounted.artifact.display()
            ))
        })?;
        let effective: Vec<String> = manifest
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
                manifest.name, manifest.claims, exported.mimetypes
            )));
        }

        let kind = match manifest.kind.as_deref() {
            Some("structural") => TransformKind::Structural,
            _ => TransformKind::Enrichment,
        };
        let transform = WasmTransform {
            engine: mounted.engine.clone(),
            component,
            linker: Arc::new(linker),
            plugin_name: manifest.name.clone(),
            claims: effective,
            roots_only: manifest.roots_only || exported.roots_only,
            kind,
            wants_bytes: manifest.capabilities.source_bytes,
            grant_llm: manifest.capabilities.llm,
            fetch: mounted.granted_fetch(cx)?,
            fetch_calls_max: mounted.config.fetch_calls_max,
            fuel: mounted.config.fuel,
        };

        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: manifest.name.clone(),
                transform: Arc::new(transform),
                llm_call_budget: if manifest.capabilities.llm {
                    manifest.capabilities.llm_call_budget
                } else {
                    0
                },
                // Loaded transforms ride the interactive lane; a run-level
                // `--batch` still moves them, since the lane is the
                // grantor's choice, not the transform's.
                llm_lane: LlmLane::Interactive,
                // The artifact version and content hash are in the shape
                // fingerprint: an upgraded loaded transform dirties exactly
                // the sources it built (`design/index-maintenance.md`).
                shape_fingerprint: format!(
                    "wasm|{}|{}|{}",
                    manifest.name, manifest.version, mounted.artifact_hash
                ),
            },
        )
    }
}

impl WasmTransformPlugin {
    async fn call_claims(
        &self,
        component: &Component,
        linker: &Linker<Invocation>,
    ) -> Result<ClaimSpec, wasmtime::Error> {
        let mut store = Store::new(
            &self.mounted.engine,
            Invocation::new(self.mounted.manifest.name.clone(), Grants::none()),
        );
        store.set_fuel(self.mounted.config.fuel)?;
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
    engine: wasmtime::Engine,
    component: Component,
    linker: Arc<Linker<Invocation>>,
    plugin_name: String,
    claims: Vec<String>,
    roots_only: bool,
    kind: TransformKind,
    wants_bytes: bool,
    grant_llm: bool,
    fetch: Option<Arc<dyn GrantedFetch>>,
    fetch_calls_max: u32,
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
        let grants = Grants {
            llm: if self.grant_llm {
                ctx.llm.clone()
            } else {
                None
            },
            bytes: ctx.bytes.map(<[u8]>::to_vec),
            fetch: self.fetch.clone(),
            fetch_calls_max: self.fetch_calls_max,
        };
        let mut store = Store::new(
            &self.engine,
            Invocation::new(self.plugin_name.clone(), grants),
        );
        if store.set_fuel(self.fuel).is_err() {
            return TransformOutput::default();
        }
        let envelope = Envelope {
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
pub(crate) fn sprout_forest(plugin: &str, fragments: Vec<Fragment>) -> TransformOutput {
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
        let relation = RelationKind::new(f.relation.as_str()).unwrap_or_else(|e| {
            tracing::warn!(plugin, relation = %f.relation, "malformed relation kind ({e}); using contains");
            RelationKind::contains()
        });
        nodes.push(Some(Sprout::leaf(
            NewFragment {
                mimetype,
                text: f.text.clone(),
                extent: None,
                content_address: None,
            },
            relation,
        )));
        match f.parent {
            Some(p) if (p as usize) < i => children_of[p as usize].push(i),
            Some(_) => {
                tracing::warn!(
                    plugin,
                    "fragment parent must index an earlier fragment; treating as root"
                );
                roots.push(i);
            }
            None => roots.push(i),
        }
    }

    // Children index later fragments only, so building in reverse index
    // order sees every child complete before its parent: a bounded loop,
    // no recursion.
    let mut built: Vec<Option<Sprout>> = nodes;
    for i in (0..built.len()).rev() {
        let Some(mut sprout) = built[i].take() else {
            continue;
        };
        for &c in &children_of[i] {
            if let Some(child) = built[c].take() {
                sprout.children.push(child);
            }
        }
        built[i] = Some(sprout);
    }
    let sprouts = roots.into_iter().filter_map(|i| built[i].take()).collect();
    TransformOutput::sprouts(sprouts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprout_forest_rebuilds_trees_and_rejects_bad_parents() {
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
        assert_eq!(
            out.sprouts.len(),
            2,
            "root + orphan-as-root; forged summary dropped"
        );
        assert_eq!(out.sprouts[0].relation.as_str(), "transcribes");
        assert_eq!(out.sprouts[0].children.len(), 1);
        assert_eq!(
            out.sprouts[0].children[0].fragment.text.as_deref(),
            Some("child")
        );
    }

    #[test]
    fn sprout_forest_nests_grandchildren_in_order() {
        let leaf = |parent: Option<u32>, text: &str| Fragment {
            parent,
            mimetype: "text/plain".into(),
            relation: "contains".into(),
            text: Some(text.into()),
        };
        let out = sprout_forest(
            "test",
            vec![
                leaf(None, "a"),
                leaf(Some(0), "b"),
                leaf(Some(1), "c"),
                leaf(Some(0), "d"),
            ],
        );
        assert_eq!(out.sprouts.len(), 1);
        let a = &out.sprouts[0];
        assert_eq!(a.children.len(), 2);
        assert_eq!(a.children[0].fragment.text.as_deref(), Some("b"));
        assert_eq!(
            a.children[0].children[0].fragment.text.as_deref(),
            Some("c")
        );
        assert_eq!(a.children[1].fragment.text.as_deref(), Some("d"));
    }
}
