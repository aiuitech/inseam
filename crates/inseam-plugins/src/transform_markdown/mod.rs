//! The `transform-markdown` plugin: the structural transform for
//! `text/markdown` roots. It decomposes a document by its own heading
//! structure ([`decompose`]) and registers that into the `transforms` seam
//! exactly the way a loaded transform does — the dog-food that proves the
//! contract (`design/plugins.md`). Its golden checks live beside it in
//! `markdown.checks.toml`.

pub mod decompose;

use std::sync::Arc;

use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::transforms::{
    register_as_effect, Registration, Transform, TransformCtx, TransformKind, TransformOutput,
};

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarkdownConfig {}

pub struct MarkdownPlugin {
    #[allow(dead_code)]
    config: MarkdownConfig,
}

pub struct MarkdownFactory;

impl PluginFactory for MarkdownFactory {
    fn name(&self) -> &str {
        "transform-markdown"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(MarkdownPlugin {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for MarkdownPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("transforms")];
        Manifest {
            name: "transform-markdown",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "markdown".to_string(),
                transform: Arc::new(MarkdownTransform),
                llm_call_budget: 0,
                shape_fingerprint: "markdown-v1".to_string(),
            },
        )
    }
}

pub(crate) struct MarkdownTransform;

#[async_trait::async_trait]
impl Transform for MarkdownTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Structural
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && !mimetype.is_inseam_defined() && mimetype.essence() == "text/markdown"
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        TransformOutput::sprouts(ctx.text.map(decompose::decompose).unwrap_or_default())
    }
}
