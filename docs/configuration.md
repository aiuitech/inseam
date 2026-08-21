# Configuration

A node's configuration is its **composition**: which plugins run, with what config ([design/composition.md](../design/composition.md)). There is no other config file.

## Layers

1. **Distribution base** — the CLI (and the FFI library) ship a built-in base composition with the standard entries below.
2. **Node composition** — `<data-dir>/composition.toml` (or `--composition` / `INSEAM_COMPOSITION`), which changes base entries **by id** and adds new ones.
3. `inseam config` prints the composition; `inseam config --resolved` prints the merged result the node actually boots with — what prints is what runs.

How merging works: when your entry matches a base entry's id, your `config` **replaces** the base config entirely (fields are not merged one by one); `plugin` and `disabled` override when present; ids the base doesn't know become new entries. Entries can nest (`[[entry.entries]]`) into groups; disabling a group disables everything inside it.

## The base entries and their configs

| id | plugin | config (defaults) |
| --- | --- | --- |
| `fs` | `connection-fs` | `host_id` (default `fs-<hostname>`), `skip_hidden` (true), `gitignore` (true), `ignore` (gitignore-syntax patterns; [indexing/ignore.md](indexing/ignore.md)) |
| `llm` | `llm-endpoint` | `base_url` (OpenRouter), `api_key_env` (`OPENROUTER_API_KEY`), `transform_model`, `agent_model` |
| `embedder` | `embedder` | `provider` = `endpoint` \| `hashed` \| `none`, `model`, `dimensions` |
| `transforms` | `transforms` | — (the registry) |
| `markdown` | `transform-markdown` | — |
| `chunker` | `transform-chunker` | `target_chars` (1600) |
| `summarizer` | `transform-summarizer` | `target_chars` (400), `llm_call_budget` (500) |
| `entities` | `transform-entities` | `max_per_source` (12), `llm_call_budget` (500) |
| `finder` | `finder` | `seed_k`, `rrf_k`, `damping`, `iterations`, `epsilon`, `max_hints`, `max_vector_distance`, `[weights]` (`default` + `by_kind` by relation kind name) |
| `sweep` | `sweep` | `max_sources` (0 = unlimited), `concurrency` (8; sources planned at once — transform applications, LLM calls included, in flight together), `max_fragments_per_source` (400), `max_depth` (6), `max_content_bytes` (2 MB), `modified_after` (`YYYY-MM-DD`), `ignore` (rules over addresses and envelopes; [indexing/ignore.md](indexing/ignore.md)) |
| `operations` | `operations` | — |

Example `composition.toml` — an offline node with a loaded OCR plugin:

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
cooldown_days = 7      # wait period before a newly seen plugin version activates
# allow_new = true     # explicit consent to activate a version still inside its wait period
# admission = "enforce"  # validation on first sighting: enforce | warn | off
```

Secrets never live in the composition or the store: the `llm` entry names an environment variable (`api_key_env`), nothing more. The CLI reads that variable from your shell; the macOS app has no shell environment, so its Settings screen stores the value in your login Keychain and exports it into the process environment at node open ([architecture/macos-app.md](architecture/macos-app.md)). The app's Settings also edits this same `composition.toml`, so CLI and app stay one node with one config.

## What a config change costs

Which entry you change decides how much work the next `inseam index` does ([indexing/maintenance.md](indexing/maintenance.md)):

- `finder` and the llm `agent_model` only affect query time — free.
- Budgets (`max_sources`, `llm_call_budget`) and the sweep's `concurrency` only shape each run — free.
- Transform configs and the sweep's size/depth dials change what the index would build, so affected sources re-index.
- The `embedder` entry re-computes vectors in place, without re-running transforms.
- Mounting or unmounting a transform plugin re-indexes only the sources that plugin applies to.
- Ignore rules (the `fs` entry's patterns, the `sweep` entry's rules) remove sources they newly cover and readmit sources they stop covering ([indexing/ignore.md](indexing/ignore.md)).
