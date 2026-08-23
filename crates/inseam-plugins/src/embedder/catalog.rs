//! What is known about embedding models before any network: the width each
//! produces and whether a smaller width is meaningful. The table is the
//! first answer to "what does `model` produce"; the endpoint's own discovery
//! (a local ollama introspects its models) is the second; explicit config is
//! the last. Every entry is a documented fact about the model — a wrong
//! width here would refuse a correct config, so unknown beats guessed.
//!
//! Names are matched after normalizing the ways one model is spelled across
//! endpoints: OpenRouter's `provider/` prefix is dropped, and an ollama
//! `:tag` is tried first with the tag (sizes differ by tag for some
//! families) and then without.

use inseam_seams::llm::EmbeddingModel;

/// One known model: its canonical name (lowercase, no provider prefix) and
/// what it produces.
struct KnownModel {
    name: &'static str,
    dimensions: usize,
    /// Matryoshka-trained: a requested smaller `dimensions` keeps meaning.
    reducible: bool,
}

const fn fixed(name: &'static str, dimensions: usize) -> KnownModel {
    KnownModel {
        name,
        dimensions,
        reducible: false,
    }
}

const fn reducible(name: &'static str, dimensions: usize) -> KnownModel {
    KnownModel {
        name,
        dimensions,
        reducible: true,
    }
}

/// Tagged names precede their untagged family so the first match wins.
const KNOWN: &[KnownModel] = &[
    // OpenAI
    reducible("text-embedding-3-small", 1536),
    reducible("text-embedding-3-large", 3072),
    fixed("text-embedding-ada-002", 1536),
    // Google
    reducible("gemini-embedding-001", 3072),
    // Mistral
    fixed("mistral-embed", 1024),
    // Sentence-transformers family, as ollama and Hugging Face name it
    fixed("all-minilm:l6-v2", 384),
    fixed("all-minilm:l12-v2", 384),
    fixed("all-minilm", 384),
    fixed("all-minilm-l6-v2", 384),
    fixed("all-minilm-l12-v2", 384),
    // Nomic: v1.5 is Matryoshka-trained, and it is the one ollama ships.
    reducible("nomic-embed-text:v1.5", 768),
    fixed("nomic-embed-text:v1", 768),
    reducible("nomic-embed-text", 768),
    reducible("nomic-embed-text-v1.5", 768),
    fixed("nomic-embed-text-v1", 768),
    // mixedbread
    fixed("mxbai-embed-large", 1024),
    // BAAI
    fixed("bge-m3", 1024),
    fixed("bge-large", 1024),
    fixed("bge-large-en-v1.5", 1024),
    fixed("bge-base", 768),
    fixed("bge-base-en-v1.5", 768),
    fixed("bge-small", 384),
    fixed("bge-small-en-v1.5", 384),
    // Snowflake: width follows the size tag.
    fixed("snowflake-arctic-embed:22m", 384),
    fixed("snowflake-arctic-embed:33m", 384),
    fixed("snowflake-arctic-embed:110m", 768),
    fixed("snowflake-arctic-embed:137m", 768),
    fixed("snowflake-arctic-embed:335m", 1024),
    fixed("snowflake-arctic-embed", 1024),
    fixed("snowflake-arctic-embed2", 1024),
    // Qwen3 embeddings are Matryoshka-trained; width follows the size tag.
    reducible("qwen3-embedding:0.6b", 1024),
    reducible("qwen3-embedding:4b", 2560),
    reducible("qwen3-embedding:8b", 4096),
    // Google's EmbeddingGemma is Matryoshka-trained (768/512/256/128).
    reducible("embeddinggemma", 768),
    // IBM Granite
    fixed("granite-embedding:30m", 384),
    fixed("granite-embedding:278m", 768),
    fixed("granite-embedding", 384),
];

/// What the catalog knows about `model`, if anything.
pub(super) fn lookup(model: &str) -> Option<EmbeddingModel> {
    let (name, tag) = normalize(model);
    let with_tag = tag.map(|tag| format!("{name}:{tag}"));
    KNOWN
        .iter()
        .find(|known| with_tag.as_deref() == Some(known.name))
        .or_else(|| KNOWN.iter().find(|known| known.name == name))
        .map(|known| EmbeddingModel {
            dimensions: known.dimensions,
            reducible: known.reducible,
        })
}

/// `openai/text-embedding-3-small` → (`text-embedding-3-small`, None);
/// `all-minilm:l6-v2` → (`all-minilm`, Some(`l6-v2`)); `:latest` is the
/// untagged model.
fn normalize(model: &str) -> (String, Option<String>) {
    let lowered = model.trim().to_ascii_lowercase();
    let without_provider = lowered.rsplit('/').next().unwrap_or(&lowered);
    match without_provider.split_once(':') {
        Some((name, "latest")) => (name.to_string(), None),
        Some((name, tag)) => (name.to_string(), Some(tag.to_string())),
        None => (without_provider.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_and_direct_spellings_agree() {
        let a = lookup("openai/text-embedding-3-small").expect("known");
        let b = lookup("text-embedding-3-small").expect("known");
        assert_eq!(a, b);
        assert_eq!(a.dimensions, 1536);
        assert!(a.reducible);
    }

    #[test]
    fn ollama_tags_resolve_with_and_without_the_tag() {
        assert_eq!(lookup("all-minilm:l6-v2").unwrap().dimensions, 384);
        assert_eq!(lookup("all-minilm:latest").unwrap().dimensions, 384);
        assert_eq!(lookup("all-minilm").unwrap().dimensions, 384);
        assert!(!lookup("all-minilm:l6-v2").unwrap().reducible);
    }

    #[test]
    fn tagged_sizes_win_over_the_family_default() {
        assert_eq!(
            lookup("snowflake-arctic-embed:22m").unwrap().dimensions,
            384
        );
        assert_eq!(lookup("snowflake-arctic-embed").unwrap().dimensions, 1024);
        assert_eq!(lookup("qwen3-embedding:4b").unwrap().dimensions, 2560);
    }

    #[test]
    fn unknown_models_are_unknown_not_guessed() {
        assert_eq!(lookup("qwen3.5:9b"), None);
        assert_eq!(lookup("acme/embedder-9000"), None);
    }
}
