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
| `connections` | `connections` | — (the host-connection registry) |
| `fs` | `connection-fs` | `machine_id` (unset: the platform's machine id, or one minted once and kept in the data directory — the host id is derived from it, never configured; [indexing/filesystem-host.md](indexing/filesystem-host.md#identity-and-addresses)), `roots` (the folders this host indexes, absolute paths; empty lets any folder be named per run — [indexing/filesystem-host.md](indexing/filesystem-host.md#configured-folders)), `skip_hidden` (true), `gitignore` (true), `ignore` (gitignore-syntax patterns; [indexing/ignore.md](indexing/ignore.md)) |
| `oauth` | `oauth` | `callback_port` (47781), `authorization_timeout_secs` (300), `credentials_dir` (the node's private `oauth/` directory), `grants` (generic grants: provider URLs, scopes, client environment variables, and authorization parameters; connections such as `google` register their own) |
| `google` | `connection-google` | `grant` (`google`), `client_id_env` (`GOOGLE_CLIENT_ID`), `client_secret_env` (`GOOGLE_CLIENT_SECRET`; `""` for none), `services` (all five), `sources_max` (5000) — one Google account as one host per service ([indexing/google-workspace.md](indexing/google-workspace.md)) |
| — | `connection-web` | `allow_hosts` (empty: any public host; `example.com`, `*.example.com`), `content_bytes_max` (16 MiB), `timeout_ms` (10000), `redirects_max` (3), `user_agent` — the public web as a fetch-only host; not in the base composition, mount it to let links be followed ([indexing/web-host.md](indexing/web-host.md)) |
| `llm` | `llm-endpoint` | `base_url` (OpenRouter), `api_key_env` (`OPENROUTER_API_KEY`; `""` for a keyless endpoint such as a local ollama), `transform_model`, `transform_reasoning_effort` (unset; `"none"` for thinking models), `agent_model`, `batches_url` (derived: OpenRouter's `/api/beta/batches`; other endpoints have no batch lane), `transform_batch_model` (derived: `<transform_model>:batch`), `batch_requests_max` (10000 per batch-API job) — the batch lane is in [indexing/embeddings.md](indexing/embeddings.md#the-batch-lane) |
| `embedder` | `embedder` | `provider` = `endpoint` \| `hashed` \| `none`, `model`, `dimensions` (unset = the model's native width; validated against the model), `vectors` = `all` \| `summaries` ([indexing/embeddings.md](indexing/embeddings.md)) |
| `transforms` | `transforms` | — (the registry) |
| `markdown` | `transform-markdown` | — |
| `directory` | `transform-directory` | — |
| `chunker` | `transform-chunker` | `target_chars` (1600) |
| `summarizer` | `transform-summarizer` | `target_chars` (400), `llm_call_budget` (500), `llm_lane` (`interactive`; `batch` parks summaries into batch-API jobs — `inseam index --batch` does it per run) |
| `entities` | `transform-entities` | `max_per_source` (12), `llm_call_budget` (500), `llm_lane` (`interactive`) |
| — | `transform-links` | `follow` (`["image/*"]`), `probe` (true) — links become typed content references; not in the base composition ([indexing/transforms.md](indexing/transforms.md)) |
| `finder` | `finder` | `seed_k`, `rrf_k`, `damping`, `iterations`, `epsilon`, `max_hints`, `max_vector_distance`, `graph_hops` (default 2, max 4), `graph_relation_limit` (default 20000, max 100000), `[weights]` (`default` + `by_kind` by relation kind name) |
| `sweep` | `sweep` | `max_sources` (0 = unlimited; `inseam index --catalog-only` / `--max-sources` override it per run), `concurrency` (8; sources planned at once — transform applications, LLM calls included, in flight together), `batch_concurrency` (4096; sources planned at once when LLM calls ride the batch lane — what one batch job gathers), `max_fragments_per_source` (400), `max_depth` (6), `max_content_bytes` (2 MB), `max_reference_hops` (1; the crawl depth — how many content references a chain may follow away from a source before the sweep stops applying transforms to them; [indexing/transforms.md](indexing/transforms.md#content-references)), `modified_after` (`YYYY-MM-DD`), `ignore` (rules over addresses and envelopes; [indexing/ignore.md](indexing/ignore.md)) |
| `node` | `node` | `display_name` (unset: the machine's hostname), `always_on` (false), `deep_index` (true), `relays` (true) — this node's identity and what it advertises; the key itself is minted, never configured ([network/identity.md](network/identity.md)) |
| `transport` | `transport-iroh` | `relay` (`n0`, iroh's public relays; `none` for direct only; or the network's own `https://` relay URL), `bind_port` (0: an ephemeral UDP port; a backbone names its forwarded one), `request_timeout_secs` (30), `idle_timeout_secs` (120; the keep-alive is a third of it, so at least 3) ([network/transport.md](network/transport.md)) |
| `roster` | `roster` | `endpoint_poll_secs` (30; how soon a changed endpoint is republished) ([network/roster.md](network/roster.md)) |
| `sync` | `sync` | `interval_secs` (60), `initial_delay_secs` (2), `peers_per_round_max` (32, the ceiling), `request_timeout_secs` (30) ([network/sync.md](network/sync.md)) |
| `routing` | `routing` | `request_timeout_secs` (30; one routed read end to end), `fan_out` (true), `fan_out_timeout_ms` (3000), `fan_out_nodes_max` (8, the ceiling) ([network/routing.md](network/routing.md)) |
| `operations` | `operations` | — |

Example `composition.toml` — an offline node with a loaded OCR plugin:

```toml
[[entry]]
id = "embedder"
[entry.config]
provider = "hashed"
model = "hashed"
dimensions = 256
vectors = "summaries"   # vectors for summaries only; full-text still covers every fragment

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

[[entry]]
id = "github"
plugin = "wasm:plugins/github/github.wasm"   # a loaded connection: a host, not a transform
[entry.config]
roots = [""]           # the scopes this host is indexed by
# grant = "github"     # the oauth grant whose bearer the plugin's authorized requests carry
[entry.config.plugin]  # the plugin's own config, handed to it as TOML
repository = "octo/hello"
```

Secrets never live in the composition or the store: the `llm` entry names an environment variable (`api_key_env`), and OAuth grants — the `google` entry's, or generic ones on `oauth` — name `client_id_env` and optional `client_secret_env` variables. The CLI reads those variables from your shell; the macOS app has no shell environment, so its Settings screen stores each value in your login Keychain and exports it into the process environment at node open ([architecture/macos-app.md](architecture/macos-app.md)). The app's visual Configuration tab reads and writes these same entries through native controls; its Advanced tab edits the same `composition.toml` directly. The web console's configuration panel edits the same document on a running node ([architecture/hosted-node.md](architecture/hosted-node.md#settings-from-the-console)): the node validates it, rewrites its `composition.toml`, and restarts only the entries that changed. CLI, app, and console therefore remain one node with one config. The node's key is not configuration either: it is minted on first boot into `<data-dir>/node/secret.key` and lives nowhere else ([network/identity.md](network/identity.md)).

## What a config change costs

Which entry you change decides how much work the next `inseam index` does ([indexing/maintenance.md](indexing/maintenance.md)):

- `finder` and the llm `agent_model` only affect query time — free.
- Budgets (`max_sources`, `llm_call_budget`) and the sweep's `concurrency` only shape each run — free.
- Transform configs and the sweep's size/depth dials change what the index would build, so affected sources re-index.
- The `embedder` entry (model, dimensions, `vectors`) re-computes vectors in place, without re-running transforms.
- Mounting or unmounting a transform plugin re-indexes only the sources that plugin applies to.
- Ignore rules (the `fs` entry's patterns, the `sweep` entry's rules) remove sources they newly cover and readmit sources they stop covering ([indexing/ignore.md](indexing/ignore.md)).
- The network entries (`node`, `transport`, `roster`, `sync`, `routing`) never touch the index — free. A changed `display_name` or capability republishes the node record; a changed `relay` or `bind_port` rebinds the transport, and the new endpoints are republished on the next roster pass.
