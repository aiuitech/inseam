# Configuration

A node's configuration is its **composition**: which plugins run with what config ([design/composition.md](../design/composition.md)). There is no other config file.

## Layers

1. **Distribution base** — the CLI (and the FFI library) ship a base composition mounting the standard entries below.
2. **Node composition** — `<data-dir>/composition.toml` (or `--composition` / `INSEAM_COMPOSITION`), patching base entries **by id** and adding new ones.
3. `inseam config` prints the composition; `inseam config --resolved` prints the layered result the node boots — what prints is what runs.

Patch semantics: a patch entry's `config` **replaces** the target's config wholesale (no field merge); `plugin` and `disabled` override when present; unknown ids append as new entries. Entries may nest (`[[entry.entries]]`) into groups; disabling a group prunes its subtree.

## The base entries and their configs

| id | plugin | config (defaults) |
| --- | --- | --- |
| `fs` | `connection-fs` | `host_id` (default `fs-<hostname>`) |
| `llm` | `llm-endpoint` | `base_url` (OpenRouter), `api_key_env` (`OPENROUTER_API_KEY`), `transform_model`, `agent_model` |
| `embedder` | `embedder` | `provider` = `endpoint` \| `hashed` \| `none`, `model`, `dimensions` |
| `transforms` | `transforms` | — (the registry) |
| `markdown` | `transform-markdown` | — |
| `chunker` | `transform-chunker` | `target_chars` (1600) |
| `summarizer` | `transform-summarizer` | `target_chars` (400), `llm_call_budget` (500) |
| `entities` | `transform-entities` | `max_per_source` (12), `llm_call_budget` (500) |
| `finder` | `finder` | `seed_k`, `rrf_k`, `damping`, `iterations`, `epsilon`, `max_hints`, `max_vector_distance`, `[weights]` |
| `sweep` | `sweep` | `max_sources` (0 = unlimited), `max_fragments_per_source` (400), `max_depth` (6), `max_content_bytes` (2 MB), `modified_after` (`YYYY-MM-DD`) |
| `operations` | `operations` | — |

Example `composition.toml` — offline node with a sandboxed OCR plugin:

```toml
[[entry]]
id = "embedder"
[entry.config]
provider = "hashed"
model = "hashed"
dimensions = 256

[[entry]]
id = "entities"
disabled = true

[[entry]]
id = "ocr"
plugin = "wasm:plugins/ocr/ocr.wasm"
[entry.config]
cooldown_days = 7      # release cooldown for newly observed artifact versions
# allow_new = true     # explicit consent to activate a version inside its cooldown
```

Secrets never live in the composition or the store: the `llm` entry names an environment variable (`api_key_env`), nothing more.

## Invalidation tiers

Which entry's config changed decides the blast radius ([index/maintenance.md](index/maintenance.md)): `finder` and the llm `agent_model` are query-time (free); budgets (`max_sources`, `llm_call_budget`) are run-metering (free); transform configs and the sweep's decomposition dials are shape (affected sources re-index); the `embedder` entry re-embeds in place. Mounting/unmounting a transform plugin dirties exactly the sources its claims touch.
