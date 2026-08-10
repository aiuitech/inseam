//! Index profiles: everything about index quality is a per-node dial
//! (`design/indexing.md`). A phone runs extractive summaries over envelope
//! text with no entities; a big node runs every transform with LLM quality.
//! Same machinery, different dial positions.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::address::Timestamp;
use crate::dates::{self, DateError};
use crate::fragment::RelationKind;

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("could not read profile `{path}`: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("profile `{path}` is not valid TOML: {source}")]
    Toml {
        path: String,
        source: Box<toml::de::Error>,
    },
    #[error("profile date cutoff: {0}")]
    Cutoff(#[from] DateError),
}

/// A node's index configuration, loaded from TOML. Every field has a default
/// so a partial file (or none at all) is a valid profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexProfile {
    pub endpoint: EndpointConfig,
    pub embedding: EmbeddingConfig,
    pub llm: LlmConfig,
    pub summary: SummaryConfig,
    pub entities: EntityConfig,
    pub budget: BudgetConfig,
    pub cutoff: CutoffConfig,
    pub finder: FinderConfig,
}

impl IndexProfile {
    /// The **shape tier** of this profile, canonicalized: the fields that
    /// change what a source's fragment subtree looks like, and nothing else
    /// (`design/index-maintenance.md`). Stored per source at indexing; a
    /// mismatch on a later sweep makes the source dirty. Query-time and
    /// run-metering fields stay out so tuning them never re-indexes.
    pub fn shape_stamp(&self) -> String {
        format!(
            "v1|model={}|summary={}|entities={},{}|depth={}|fragments={}|content_bytes={}",
            self.llm.transform_model,
            self.summary.target_chars,
            self.entities.enabled,
            self.entities.max_per_source,
            self.budget.max_depth,
            self.budget.max_fragments_per_source,
            self.budget.max_content_bytes,
        )
    }

    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ProfileError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let profile: Self = toml::from_str(&raw).map_err(|source| ProfileError::Toml {
            path: path.display().to_string(),
            source: Box::new(source),
        })?;
        profile.cutoff.modified_after_epoch()?; // fail on a bad date now, not mid-index
        Ok(profile)
    }
}

/// The OpenAI-compatible endpoint this node's LLM work goes through —
/// embeddings, transform calls, and the agent demo alike. OpenRouter is the
/// default; any compatible server (OpenAI, Ollama, vLLM, ...) is
/// configuration, not code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointConfig {
    pub base_url: String,
    /// Name of the environment variable holding the API key.
    pub api_key_env: String,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingProvider {
    /// Remote embeddings through the configured `[endpoint]`.
    /// (`"openrouter"` is accepted as a legacy spelling.)
    #[serde(alias = "openrouter")]
    Endpoint,
    /// Deterministic local bag-of-words hashing: weak but offline and free.
    Hashed,
    /// No vectors at all; discovery degrades to full-text seeding only.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingConfig {
    pub provider: EmbeddingProvider,
    pub model: String,
    pub dimensions: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::Endpoint,
            model: "openai/text-embedding-3-small".to_string(),
            dimensions: 1536,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// Model for summary and entity transforms; cheap and fast wins here.
    pub transform_model: String,
    /// Model for the `inseam agent` demo loop; needs solid tool calling.
    pub agent_model: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            transform_model: "google/gemini-2.5-flash-lite".to_string(),
            agent_model: "openai/gpt-5-mini".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SummaryConfig {
    /// Target summary length in characters. Profiles configure the length,
    /// not the existence: every indexed source gets a summary.
    pub target_chars: usize,
    /// LLM summaries per index run; beyond this the summarizer falls back to
    /// extractive summaries so the mandatory-summary invariant still holds.
    pub llm_call_budget: usize,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            target_chars: 400,
            llm_call_budget: 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EntityConfig {
    pub enabled: bool,
    /// Entity-extraction LLM calls per index run.
    pub llm_call_budget: usize,
    /// Cap on entities taken from a single source.
    pub max_per_source: usize,
}

impl Default for EntityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            llm_call_budget: 500,
            max_per_source: 12,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// Sources to deep-index per run; the rest still enter the catalog.
    /// 0 means unlimited.
    pub max_sources: usize,
    /// Fragment cap per source; decomposition is truncated beyond it.
    pub max_fragments_per_source: usize,
    /// Decomposition depth cap.
    pub max_depth: usize,
    /// Sources larger than this are cataloged but not content-indexed.
    pub max_content_bytes: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_sources: 0,
            max_fragments_per_source: 400,
            max_depth: 6,
            max_content_bytes: 2_000_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CutoffConfig {
    /// `YYYY-MM-DD`; sources last modified before this date are cataloged but
    /// not indexed. Empty means no horizon.
    pub modified_after: Option<String>,
}

impl CutoffConfig {
    pub fn modified_after_epoch(&self) -> Result<Option<Timestamp>, DateError> {
        self.modified_after
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| dates::parse_ymd_epoch(s).map(Timestamp))
            .transpose()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FinderConfig {
    /// Fragments retrieved from each seed list (full-text and vector).
    pub seed_k: usize,
    /// The `k` constant in reciprocal rank fusion.
    pub rrf_k: f64,
    /// Personalized PageRank damping: probability a walk continues instead of
    /// restarting at the seeds. Keeps the boost local.
    pub damping: f64,
    pub iterations: usize,
    pub epsilon: f64,
    /// Fragment hints attached to each result.
    pub max_hints: usize,
    /// Vector hits farther than this cosine distance are noise, not seeds:
    /// nearest-k always returns something, even when nothing is close.
    pub max_vector_distance: f64,
    pub weights: RelationWeights,
}

impl Default for FinderConfig {
    fn default() -> Self {
        Self {
            seed_k: 60,
            rrf_k: 60.0,
            damping: 0.5,
            iterations: 12,
            epsilon: 1e-6,
            max_hints: 3,
            max_vector_distance: 0.75,
            weights: RelationWeights::default(),
        }
    }
}

/// How strongly each relation kind conducts relevance during propagation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelationWeights {
    pub contains: f64,
    pub links_to: f64,
    pub derived_from: f64,
    pub mentions: f64,
    pub transcribes: f64,
}

impl Default for RelationWeights {
    fn default() -> Self {
        Self {
            contains: 1.0,
            links_to: 0.4,
            derived_from: 0.9,
            mentions: 0.8,
            transcribes: 1.0,
        }
    }
}

impl RelationWeights {
    pub fn weight(&self, kind: RelationKind) -> f64 {
        match kind {
            RelationKind::Contains => self.contains,
            RelationKind::LinksTo => self.links_to,
            RelationKind::DerivedFrom => self.derived_from,
            RelationKind::Mentions => self.mentions,
            RelationKind::Transcribes => self.transcribes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_complete() {
        let p = IndexProfile::default();
        assert_eq!(p.embedding.dimensions, 1536);
        assert!(p.entities.enabled);
        assert_eq!(p.finder.weights.weight(RelationKind::Contains), 1.0);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let p: IndexProfile = toml::from_str(
            r#"
            [embedding]
            provider = "hashed"
            dimensions = 64

            [entities]
            enabled = false
            "#,
        )
        .expect("partial profile parses");
        assert_eq!(p.embedding.provider, EmbeddingProvider::Hashed);
        assert_eq!(p.embedding.dimensions, 64);
        assert!(!p.entities.enabled);
        assert_eq!(p.summary.target_chars, 400);
    }

    #[test]
    fn endpoint_defaults_to_openrouter_and_accepts_legacy_provider_name() {
        let p = IndexProfile::default();
        assert_eq!(p.endpoint.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(p.endpoint.api_key_env, "OPENROUTER_API_KEY");

        let legacy: IndexProfile = toml::from_str("[embedding]\nprovider = \"openrouter\"")
            .expect("legacy spelling parses");
        assert_eq!(legacy.embedding.provider, EmbeddingProvider::Endpoint);

        let custom: IndexProfile = toml::from_str(
            "[endpoint]\nbase_url = \"http://localhost:11434/v1\"\napi_key_env = \"OLLAMA_KEY\"",
        )
        .expect("custom endpoint parses");
        assert_eq!(custom.endpoint.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn rejects_unknown_fields() {
        let r: Result<IndexProfile, _> = toml::from_str("[embedding]\nmodle = \"typo\"");
        assert!(r.is_err());
    }

    #[test]
    fn cutoff_parses_to_epoch() {
        let c = CutoffConfig {
            modified_after: Some("2015-01-01".to_string()),
        };
        assert_eq!(
            c.modified_after_epoch().expect("valid date"),
            Some(Timestamp(1_420_070_400))
        );
        let empty = CutoffConfig {
            modified_after: Some(String::new()),
        };
        assert_eq!(empty.modified_after_epoch().expect("empty ok"), None);
    }

    #[test]
    fn cutoff_rejects_bad_dates() {
        let c = CutoffConfig {
            modified_after: Some("soon".to_string()),
        };
        assert!(c.modified_after_epoch().is_err());
    }
}
