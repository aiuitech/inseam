//! Embedders: every fragment gets an embedding from whatever model the
//! profile configures (`design/indexing.md`). Enum dispatch keeps the
//! concrete cases obvious: the configured OpenAI-compatible endpoint,
//! deterministic local hashing
//! (offline/small-device fallback and the test embedder), or none.

use std::sync::Arc;

use thiserror::Error;

use crate::llm::{LlmClient, LlmError};
use crate::profile::{EmbeddingConfig, EmbeddingProvider};

/// Characters of input text an embedding sees; more adds cost, not recall.
const EMBED_INPUT_CHARS: usize = 6_000;

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error(
        "embedding provider is `endpoint` but no endpoint client is available; \
         set the API key named by [endpoint].api_key_env, or switch the provider"
    )]
    MissingClient,
    #[error("embedding call failed: {0}")]
    Call(#[from] LlmError),
    #[error("model returned {got}-dimension vectors, profile says {want}; fix the profile or re-index")]
    DimensionMismatch { want: usize, got: usize },
    #[error("embeddings are disabled in this profile")]
    Disabled,
}

#[derive(Debug)]
pub enum Embedder {
    Endpoint {
        client: Arc<LlmClient>,
        model: String,
        dimensions: usize,
    },
    Hashed {
        dimensions: usize,
    },
    Disabled,
}

impl Embedder {
    pub fn from_profile(
        config: &EmbeddingConfig,
        client: Option<Arc<LlmClient>>,
    ) -> Result<Self, EmbedError> {
        match config.provider {
            EmbeddingProvider::Endpoint => {
                let Some(client) = client else {
                    return Err(EmbedError::MissingClient);
                };
                Ok(Self::Endpoint {
                    client,
                    model: config.model.clone(),
                    dimensions: config.dimensions,
                })
            }
            EmbeddingProvider::Hashed => Ok(Self::Hashed {
                dimensions: config.dimensions,
            }),
            EmbeddingProvider::None => Ok(Self::Disabled),
        }
    }

    /// Vector width, or `None` when this node keeps no vectors.
    pub fn dimensions(&self) -> Option<usize> {
        match self {
            Self::Endpoint { dimensions, .. } | Self::Hashed { dimensions } => Some(*dimensions),
            Self::Disabled => None,
        }
    }

    /// Embed texts in order. Long inputs are truncated to a bounded prefix.
    pub async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        match self {
            Self::Endpoint {
                client,
                model,
                dimensions,
            } => {
                let bounded: Vec<String> = texts
                    .iter()
                    .map(|t| t.chars().take(EMBED_INPUT_CHARS).collect())
                    .collect();
                let refs: Vec<&str> = bounded.iter().map(String::as_str).collect();
                let vectors = client.embed(model, &refs).await?;
                if let Some(v) = vectors.first()
                    && v.len() != *dimensions {
                        return Err(EmbedError::DimensionMismatch {
                            want: *dimensions,
                            got: v.len(),
                        });
                    }
                Ok(vectors)
            }
            Self::Hashed { dimensions } => Ok(texts
                .iter()
                .map(|t| hashed_embedding(t, *dimensions))
                .collect()),
            Self::Disabled => Err(EmbedError::Disabled),
        }
    }
}

/// Deterministic bag-of-words embedding: each token hashes to a dimension and
/// a sign, counts accumulate, the vector is L2-normalized. Shared vocabulary
/// yields real cosine similarity — weak semantics, strong determinism.
fn hashed_embedding(text: &str, dimensions: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dimensions.max(1)];
    for token in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let h = fnv1a(&token.to_lowercase());
        let dim = (h % dimensions.max(1) as u64) as usize;
        let sign = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
        v[dim] += sign;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// FNV-1a, inlined for cross-process determinism (std's hasher doesn't
/// guarantee it).
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashed() -> Embedder {
        Embedder::Hashed { dimensions: 64 }
    }

    #[tokio::test]
    async fn hashed_embeddings_are_deterministic_and_normalized() {
        let e = hashed();
        let a = e.embed(&["kitchen renovation notes"]).await.expect("embeds");
        let b = e.embed(&["kitchen renovation notes"]).await.expect("embeds");
        assert_eq!(a, b);
        let norm: f32 = a[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn hashed_embeddings_rank_shared_vocabulary_higher() {
        let e = hashed();
        let vs = e
            .embed(&[
                "kitchen renovation budget",
                "renovation budget for the kitchen remodel",
                "quarterly tax filing checklist",
            ])
            .await
            .expect("embeds");
        let cos = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        assert!(cos(&vs[0], &vs[1]) > cos(&vs[0], &vs[2]));
    }

    #[tokio::test]
    async fn disabled_embedder_refuses() {
        assert!(matches!(
            Embedder::Disabled.embed(&["x"]).await,
            Err(EmbedError::Disabled)
        ));
        assert_eq!(Embedder::Disabled.dimensions(), None);
    }
}
