# Index Profiles

Index quality is a per-node dial ([design/indexing.md](../../design/indexing.md)). The profile is TOML; every field defaults, unknown keys error. Resolution order in [configuration.md](../configuration.md).

```toml
[endpoint]                # the OpenAI-compatible server all LLM work goes through
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[embedding]
provider = "endpoint"     # "endpoint" | "hashed" | "none"
model = "openai/text-embedding-3-small"
dimensions = 1536

[llm]
transform_model = "google/gemini-2.5-flash-lite"  # summaries + entities
agent_model = "openai/gpt-5-mini"                 # `inseam agent`

[summary]
target_chars = 400        # length is configurable; existence is not
llm_call_budget = 500     # per run; extractive fallback beyond

[entities]
enabled = true
llm_call_budget = 500
max_per_source = 12

[budget]
max_sources = 0           # deep-index cap per run (0 = unlimited); rest is catalog-only
max_fragments_per_source = 400
max_depth = 6
max_content_bytes = 2000000  # bigger sources stay envelope-only

[cutoff]
modified_after = ""       # "YYYY-MM-DD": older sources catalog but don't index

[finder]
seed_k = 60               # per seed list (full-text, vector)
rrf_k = 60.0
damping = 0.5             # PPR: walk continues vs. restarts at seeds
iterations = 12
epsilon = 1e-6
max_hints = 3
max_vector_distance = 0.75  # cosine-distance floor: farther "neighbors" are noise, not seeds

[finder.weights]          # how strongly each relation kind conducts relevance
contains = 1.0
links_to = 0.4
derived_from = 0.9
mentions = 0.8
transcribes = 1.0
```

## Dial positions

- **Small device**: `provider = "hashed"` (or `"none"`), `entities.enabled = false`, `summary.llm_call_budget = 0`, tight `cutoff` — offline, free, still functional. This is exactly the profile the test suite runs under.
- **Big node**: the defaults above — remote embeddings, LLM summaries and entities, no cutoff.

## What changing a field costs

Fields partition into invalidation tiers ([maintenance.md](maintenance.md)): `[finder]`, `agent_model`, and `[endpoint]` are read at query/call time and never re-index; `max_sources` and the `llm_call_budget`s only meter runs; `transform_model`, `summary.target_chars`, the `entities` shape fields, and the `budget` shape caps re-index each source on its next sweep (via the per-source profile stamp); `embedding.provider`/`model`/`dimensions` trigger an in-place re-embed — vectors rebuild from stored text, no transforms re-run.
