//! The transform-output cache's vocabulary (`design/indexing.md`): the key
//! that names one application of one transform to one input, the observer
//! that says whether an application actually spent the LLM, and the codec
//! for the output.
//!
//! An LLM transform's output is filed once per `(input content digest,
//! transform shape identity)`. The identity is the registration's entry id
//! and shape fingerprint — the same ingredients the shape stamp digests, with
//! the transform model folded in — so a change that would re-shape the
//! output stops matching on its own and no flush is ever needed. Only an
//! output the LLM produced is filed: a fallback (extractive summary, budget
//! spent, endpoint down) is what the run got, not what the transform means,
//! and filing it would make the fallback permanent.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use inseam_kernel::address::ContentDigest;
use inseam_kernel::fragment::Mimetype;
use inseam_seams::SeamError;
use inseam_seams::transforms::{GrantedLlm, Registration, TransformOutput};

/// Whether a registration's outputs are worth caching: only LLM-hungry
/// transforms are. Structural decomposition is cheap and its output is
/// regenerated on every rebuild by design.
pub(super) fn caches_output(registration: &Registration) -> bool {
    registration.llm_call_budget > 0
}

/// The cache key for applying `registration` to an input with this digest,
/// mimetype, and position. Everything that changes the output is in it:
/// the content, how the transform saw it, and the transform's identity.
pub(super) fn cache_key(
    digest: &ContentDigest,
    mimetype: &Mimetype,
    is_root: bool,
    registration: &Registration,
) -> String {
    let material = format!(
        "v1|{}|{}|{}|{}|{}",
        digest.to_hex(),
        mimetype.essence(),
        u8::from(is_root),
        registration.entry_id,
        registration.input_shape_fingerprint(mimetype, is_root)
    );
    ContentDigest::of_bytes(material.as_bytes()).to_hex()
}

pub(super) fn encode_output(output: &TransformOutput) -> Option<String> {
    serde_json::to_string(output).ok()
}

pub(super) fn decode_output(encoded: &str) -> Option<TransformOutput> {
    serde_json::from_str(encoded).ok()
}

/// A granted LLM handle that counts how its calls went, so the planner
/// knows whether the output it got back is the model's or a fallback's.
pub(super) struct ObservedLlm {
    inner: Arc<dyn GrantedLlm>,
    completed: AtomicUsize,
    failed: AtomicUsize,
}

impl ObservedLlm {
    pub(super) fn new(inner: Arc<dyn GrantedLlm>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            completed: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
        })
    }

    /// The output is the model's: at least one call answered and none
    /// failed (a failed call means the transform fell back somewhere).
    pub(super) fn output_is_the_models(&self) -> bool {
        let completed = self.completed.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        completed >= 1 && failed == 0
    }

    fn record(&self, result: &Result<String, SeamError>) {
        match result {
            Ok(_) => self.completed.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.failed.fetch_add(1, Ordering::Relaxed),
        };
    }
}

#[async_trait::async_trait]
impl GrantedLlm for ObservedLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String, SeamError> {
        let result = self.inner.complete(system, user).await;
        self.record(&result);
        result
    }

    async fn describe_image(
        &self,
        prompt: &str,
        mimetype: &str,
        image: &[u8],
    ) -> Result<String, SeamError> {
        let result = self.inner.describe_image(prompt, mimetype, image).await;
        self.record(&result);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::fragment::{NewFragment, RelationKind, Sprout};
    use inseam_seams::llm::LlmLane;
    use inseam_seams::transforms::{Transform, TransformCtx, TransformKind};

    struct Never;

    #[async_trait::async_trait]
    impl Transform for Never {
        fn kind(&self) -> TransformKind {
            TransformKind::Enrichment
        }
        fn claims(&self, _: &Mimetype, _: bool) -> bool {
            false
        }
        async fn apply(&self, _: TransformCtx<'_>) -> TransformOutput {
            TransformOutput::default()
        }
    }

    fn registration(entry: &str, fingerprint: &str, budget: usize) -> Registration {
        Registration {
            entry_id: entry.into(),
            name: entry.into(),
            transform: Arc::new(Never),
            llm_call_budget: budget,
            llm_lane: LlmLane::Interactive,
            shape_fingerprint: fingerprint.into(),
        }
    }

    #[test]
    fn only_llm_hungry_transforms_cache() {
        assert!(caches_output(&registration("summarizer", "s", 500)));
        assert!(!caches_output(&registration("markdown", "m", 0)));
    }

    #[test]
    fn the_key_changes_with_every_ingredient_and_nothing_else() {
        let digest = ContentDigest::of_bytes(b"content");
        let other_digest = ContentDigest::of_bytes(b"other");
        let markdown = Mimetype::parse("text/markdown").expect("mimetype");
        let plain = Mimetype::text_plain();
        let base = registration("summarizer", "summarizer-v1|target_chars=400|model=m", 500);
        let key = cache_key(&digest, &markdown, true, &base);

        assert_eq!(key, cache_key(&digest, &markdown, true, &base));
        assert_eq!(key.len(), 64);
        assert_ne!(key, cache_key(&other_digest, &markdown, true, &base));
        assert_ne!(key, cache_key(&digest, &plain, true, &base));
        assert_ne!(key, cache_key(&digest, &markdown, false, &base));
        assert_ne!(
            key,
            cache_key(
                &digest,
                &markdown,
                true,
                &registration("other", "summarizer-v1|target_chars=400|model=m", 500)
            )
        );
        assert_ne!(
            key,
            cache_key(
                &digest,
                &markdown,
                true,
                &registration("summarizer", "summarizer-v1|target_chars=200|model=m", 500)
            )
        );
        // Run-metering dials are not ingredients.
        assert_eq!(
            key,
            cache_key(
                &digest,
                &markdown,
                true,
                &registration("summarizer", "summarizer-v1|target_chars=400|model=m", 1)
            )
        );
    }

    #[test]
    fn outputs_roundtrip_through_the_codec() {
        let output = TransformOutput::sprouts(vec![Sprout::leaf(
            NewFragment {
                mimetype: Mimetype::summary().with_param("via", "llm"),
                text: Some("a summary".into()),
                extent: None,
                content_address: None,
            },
            RelationKind::derives(),
        )]);
        let encoded = encode_output(&output).expect("encodes");
        assert_eq!(decode_output(&encoded), Some(output));
        assert_eq!(decode_output("not json"), None);
    }

    struct Scripted(Vec<Result<String, SeamError>>, std::sync::Mutex<usize>);

    #[async_trait::async_trait]
    impl GrantedLlm for Scripted {
        async fn complete(&self, _: &str, _: &str) -> Result<String, SeamError> {
            let mut next = self.1.lock().expect("lock");
            let result = match &self.0[*next] {
                Ok(text) => Ok(text.clone()),
                Err(_) => Err(SeamError::failed("scripted failure")),
            };
            *next += 1;
            result
        }
        async fn describe_image(&self, _: &str, _: &str, _: &[u8]) -> Result<String, SeamError> {
            self.complete("", "").await
        }
    }

    #[tokio::test]
    async fn an_output_is_the_models_only_after_answered_calls_with_no_failure() {
        let untouched = ObservedLlm::new(Arc::new(Scripted(vec![], std::sync::Mutex::new(0))));
        assert!(!untouched.output_is_the_models());

        let answered = ObservedLlm::new(Arc::new(Scripted(
            vec![Ok("summary".into())],
            std::sync::Mutex::new(0),
        )));
        answered.complete("s", "u").await.expect("answers");
        assert!(answered.output_is_the_models());

        let failed = ObservedLlm::new(Arc::new(Scripted(
            vec![Ok("summary".into()), Err(SeamError::failed("down"))],
            std::sync::Mutex::new(0),
        )));
        failed.complete("s", "u").await.expect("answers");
        assert!(failed.describe_image("p", "image/png", b"").await.is_err());
        assert!(!failed.output_is_the_models());
    }
}
