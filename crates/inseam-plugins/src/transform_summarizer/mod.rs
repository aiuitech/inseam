//! The `transform-summarizer` plugin: the one mandatory enrichment. The
//! finder serves summaries in every response so an AI client can decide
//! whether to keep digging, so every indexed source must have one
//! (`design/indexing.md`). Config sets the length, never the existence:
//! LLM when the granted handle allows it, extractive for text otherwise,
//! envelope-derived for everything else ([`summarize`]). Its golden checks
//! live beside it in `summarizer.checks.toml`.

mod summarize;

use std::sync::Arc;

use inseam_kernel::fragment::{Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::transforms::{
    register_as_effect, Registration, Transform, TransformCtx, TransformKind, TransformOutput,
};

use summarize::SummaryKind;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SummarizerConfig {
    /// Target summary length in characters. Config sets the length, not the
    /// existence: every indexed source gets a summary.
    pub target_chars: usize,
    /// LLM summaries per index run (run-metering tier — not in the shape
    /// stamp); beyond it the summarizer falls back to extractive summaries
    /// so the mandatory-summary invariant still holds.
    pub llm_call_budget: usize,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            target_chars: 400,
            llm_call_budget: 500,
        }
    }
}

pub struct SummarizerPlugin {
    config: SummarizerConfig,
}

pub struct SummarizerFactory;

impl PluginFactory for SummarizerFactory {
    fn name(&self) -> &str {
        "transform-summarizer"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(SummarizerPlugin {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for SummarizerPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("transforms")];
        Manifest {
            name: "transform-summarizer",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "summarizer".to_string(),
                transform: Arc::new(SummarizerTransform {
                    target_chars: self.config.target_chars,
                }),
                llm_call_budget: self.config.llm_call_budget,
                shape_fingerprint: format!(
                    "summarizer-v1|target_chars={}",
                    self.config.target_chars
                ),
            },
        )
    }
}

pub(crate) struct SummarizerTransform {
    pub(crate) target_chars: usize,
}

#[async_trait::async_trait]
impl Transform for SummarizerTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Enrichment
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && !mimetype.is_inseam_defined()
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let hint = ctx.envelope.hint.as_deref();
        let (text, kind) = match ctx.text.filter(|t| !t.trim().is_empty()) {
            Some(content) => {
                summarize::summarize_text(ctx.llm.as_deref(), hint, content, self.target_chars).await
            }
            None => (
                summarize::envelope_summary(ctx.envelope),
                SummaryKind::Envelope,
            ),
        };
        if text.is_empty() {
            return TransformOutput::default();
        }
        TransformOutput::sprouts(vec![Sprout::leaf(
            NewFragment {
                // The `via` param records provenance: llm, extractive, or
                // envelope. The index report counts by it.
                mimetype: Mimetype::summary().with_param("via", kind.as_str()),
                text: Some(text),
                extent: None,
            },
            RelationKind::derives(),
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{ContentLength, Envelope, Timestamp};

    fn envelope(content_type: &str, hint: &str) -> Envelope {
        Envelope {
            source_type: "file".into(),
            content_type: Mimetype::parse(content_type).expect("valid mimetype"),
            length: ContentLength::Bytes(100),
            created: None,
            modified: Some(Timestamp(1_700_000_000)),
            observed: Timestamp(1_700_000_100),
            properties: Vec::new(),
            hint: Some(hint.into()),
        }
    }

    fn ctx<'a>(
        envelope: &'a Envelope,
        mimetype: &'a Mimetype,
        text: Option<&'a str>,
    ) -> TransformCtx<'a> {
        TransformCtx {
            envelope,
            mimetype,
            is_root: true,
            text,
            bytes: None,
            llm: None,
        }
    }

    #[tokio::test]
    async fn without_content_derives_from_the_envelope() {
        let envelope = envelope("image/jpeg", "IMG_2019.jpeg");
        let m = envelope.content_type.clone();
        let out = SummarizerTransform { target_chars: 200 }
            .apply(ctx(&envelope, &m, None))
            .await;
        assert_eq!(out.sprouts.len(), 1);
        let sprout = &out.sprouts[0];
        assert_eq!(sprout.relation, RelationKind::derives());
        assert_eq!(sprout.fragment.mimetype.param("via"), Some("envelope"));
        assert!(sprout
            .fragment
            .text
            .as_deref()
            .expect("has text")
            .contains("IMG_2019.jpeg"));
    }

    #[tokio::test]
    async fn without_llm_capability_is_extractive() {
        let envelope = envelope("text/markdown", "note.md");
        let m = envelope.content_type.clone();
        let out = SummarizerTransform { target_chars: 200 }
            .apply(ctx(&envelope, &m, Some("# Reno\n\nBudget notes for the kitchen.")))
            .await;
        assert_eq!(out.sprouts[0].fragment.mimetype.param("via"), Some("extractive"));
    }
}
