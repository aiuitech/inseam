//! The `transforms` plugin: the seam's registry provider. Transform plugins
//! (`transform_markdown`, `transform_chunker`, … and every loaded transform
//! the wasm bridge mounts) register into it as effects, and the sweep reads
//! its snapshot. It is deliberately nothing but the registry: claims, apply,
//! and budgets belong to the transforms themselves (`design/plugins.md`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use inseam_kernel::substrate::{ApplyCx, Facts, Inject, Manifest, Plugin, PluginError};
use inseam_seams::transforms::{Registration, TRANSFORMS, Transforms};

pub struct TransformsRegistry;

pub struct TransformsRegistryFactory;

impl inseam_kernel::substrate::PluginFactory for TransformsRegistryFactory {
    fn name(&self) -> &str {
        "transforms"
    }

    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(TransformsRegistry))
    }
}

#[async_trait::async_trait]
impl Plugin for TransformsRegistry {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "transforms",
            inject: INJECT,
            provides: &["transforms"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        cx.provide(
            &TRANSFORMS,
            Arc::new(Registry::default()) as Arc<dyn Transforms>,
            Facts::new(),
        )?;
        Ok(())
    }
}

#[derive(Default)]
struct Registry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    entries: RwLock<Vec<(u64, Arc<Registration>)>>,
    next: AtomicU64,
}

impl Transforms for Registry {
    fn register(&self, registration: Registration) -> Box<dyn FnOnce() + Send> {
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        self.inner
            .entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, Arc::new(registration)));
        // The disposer holds the registry weakly: a transform being unwound
        // after the whole registry is gone (full teardown, reverse order)
        // must be a no-op, not a resurrection.
        let weak = Arc::downgrade(&self.inner);
        Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner
                    .entries
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(i, _)| *i != id);
            }
        })
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
        out.sort_by(|a, b| {
            (a.transform.kind(), a.entry_id.as_str())
                .cmp(&(b.transform.kind(), b.entry_id.as_str()))
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_seams::transforms::{Transform, TransformKind};

    /// A claims-nothing transform of the given kind, enough to exercise the
    /// registry's ordering and disposal.
    struct Stub(TransformKind);

    #[async_trait::async_trait]
    impl Transform for Stub {
        fn kind(&self) -> TransformKind {
            self.0
        }

        fn claims(&self, _mimetype: &inseam_kernel::fragment::Mimetype, _is_root: bool) -> bool {
            false
        }

        async fn apply(
            &self,
            _ctx: inseam_seams::transforms::TransformCtx<'_>,
        ) -> inseam_seams::transforms::TransformOutput {
            inseam_seams::transforms::TransformOutput::default()
        }
    }

    #[test]
    fn registry_snapshot_orders_structural_before_enrichment() {
        let registry = Registry::default();
        let reg = |entry: &str, kind: TransformKind| Registration {
            entry_id: entry.to_string(),
            name: entry.to_string(),
            transform: Arc::new(Stub(kind)),
            llm_call_budget: 0,
            llm_lane: inseam_seams::llm::LlmLane::Interactive,
            shape_fingerprint: "x".into(),
        };
        // Disposers are held, not run: dropping one must not unregister.
        let _keep_summarizer = registry.register(reg("summarizer", TransformKind::Enrichment));
        let dispose_md = registry.register(reg("markdown", TransformKind::Structural));
        let _keep_chunker = registry.register(reg("chunker", TransformKind::Structural));

        let order: Vec<String> = registry
            .snapshot()
            .iter()
            .map(|r| r.entry_id.clone())
            .collect();
        assert_eq!(order, vec!["chunker", "markdown", "summarizer"]);

        // The disposer is the whole uninstall path.
        dispose_md();
        let order: Vec<String> = registry
            .snapshot()
            .iter()
            .map(|r| r.entry_id.clone())
            .collect();
        assert_eq!(order, vec!["chunker", "summarizer"]);
    }
}
