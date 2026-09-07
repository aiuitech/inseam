# Linked Plugins

The trusted tier ([design/plugins.md](../../design/plugins.md)), named for how the code arrives: **linked** into the binary at build time, where [loaded plugins](loaded.md) mount from artifacts at runtime. Rust, statically compiled, turned on by the composition. `crates/inseam-plugins` ships the first-party set; a distribution registers the factories it links (`inseam_plugins::factories()`) with `Kernel::boot`, and a custom distribution compiles additional linked plugins in from source ([distributions.md](distributions.md)).

Every plugin is the same five declarations — name, config, inject, provide, apply ([../architecture/kernel.md](../architecture/kernel.md)) — plus an optional sixth, `secrets()`: a plugin whose config names a secret environment variable declares it with an owner-facing reason, so a settings UI can ask for the key in plain language instead of surfacing the apply failure. What each first-party plugin does:

| Plugin | Provides | Injects | Role |
| --- | --- | --- | --- |
| `connections` | `connections` | — | the registry connection plugins sign up with ([indexing/connections.md](../indexing/connections.md)) |
| `connection-fs` | — | connections | the local filesystem host ([indexing/filesystem-host.md](../indexing/filesystem-host.md)) |
| `connection-google` | — | connections, oauth | one Google account as a host per service (Gmail, Drive, Calendar, Contacts, Tasks); registers its own grant and follows it ([indexing/google-workspace.md](../indexing/google-workspace.md)) |
| `connection-web` | — | connections | the public web as a fetch-only host behind an allow list and a public-address guard; not in the base composition ([indexing/web-host.md](../indexing/web-host.md)) |
| `oauth` | `oauth` | — | the node's OAuth grants — configured here or registered by connections: authorize from any client, store, refresh; host connections consume them by id ([oauth.md](oauth.md)) |
| `llm-endpoint` | `llm` | — | OpenAI-compatible client; declares `transform_model`/`agent_model` as facts. Fails loudly (and alone) when its key env var is unset, and declares that need via `Plugin::secrets()` so status surfaces can say why the key matters |
| `embedder` | `embedder` | store, llm? | endpoint / hashed / none; declares the embedding identity to the store, which opens the search surface under it |
| `transforms` | `transforms` | — | the registry transform plugins sign up with |
| `transform-markdown` | — | transforms | splits markdown by its heading structure |
| `transform-directory` | — | transforms | folder sources: the composed listing becomes one entry fragment per child, referencing the child ([indexing/transforms.md](../indexing/transforms.md#folders)) |
| `transform-chunker` | — | transforms | fallback chunking for text with no structure |
| `transform-summarizer` | — | transforms | the mandatory summary (verbatim → LLM → extractive → envelope) and the keywords beside it |
| `transform-entities` | — | transforms | LLM entity extraction |
| `transform-hints` | — | transforms | LLM retrieval hints: cues, synopsis, discriminators, glossary terms, identifiers, entities |
| `transform-links` | — | transforms, connections | the link follower: links to images become typed fragments referencing their web address; not in the base composition ([indexing/transforms.md](../indexing/transforms.md)) |
| `finder` | `finder` | store, embedder | the ranking algorithm ([finder/algorithm.md](../finder/algorithm.md)) |
| `sweep` | `sweep` | store, connections, transforms, embedder, llm? | the reconciling sweep ([indexing/maintenance.md](../indexing/maintenance.md)) |
| `operations` | `operations` | store, connections, finder, sweep, oauth?, node?, roster?, sync?, routing? | the transport-neutral node API, grant and network operations included; every local operation works without the network entries ([finder/operations.md](../finder/operations.md)) |
| `node` | `node` | — | this node's identity: the keypair minted once into the data directory, its display name, and the capabilities it advertises ([network/identity.md](../network/identity.md)) |
| `transport-iroh` | `transport` | node | node↔node connections: QUIC dialed by node id, relays, the admission handshake, sessions; handlers register per protocol name ([network/transport.md](../network/transport.md)) |
| `roster` | `roster` | store, node, transport, connections | publishes this node's node, host, and stewardship records; the transport's admission policy; invitations and expulsions ([network/roster.md](../network/roster.md)) |
| `sync` | `sync` | store, node, transport, roster | replicates the log with every dialable node on a timer; serves `inseam/sync/1` ([network/sync.md](../network/sync.md)) |
| `routing` | `routing` | store, node, transport, roster, connections, finder | reads a source through its steward, directly or through a relay, and fans queries out; serves `inseam/route/1` ([network/routing.md](../network/routing.md)) |

Each plugin is one directory under `crates/inseam-plugins/src/`, named for its composition name with underscores (`transform-markdown` → `transform_markdown/`): its `mod.rs` holds the config, factory, manifest, and apply; pure helpers sit beside it as sibling files; and every linked transform ships its own golden checks there too (`src/<plugin>/<registration>.checks.toml`) — the same schema and coverage rule as a loaded plugin's, run by the conformance suite through the seam ([validation.md](validation.md)). Registering a transform is one seam call, `inseam_seams::transforms::register_as_effect`, which the wasm bridge uses too; registering a connection is its twin, `inseam_seams::connection::register_as_effect` ([indexing/connections.md](../indexing/connections.md)); registering a handler for a wire protocol is `inseam_seams::transport::register_as_effect` ([network/transport.md](../network/transport.md)).

The agent skill at `skills/inseam-linked-plugin/SKILL.md` is the authoring guide for this tier ([../skills/inseam-linked-plugin.md](../skills/inseam-linked-plugin.md)); it assumes a source checkout, since a linked plugin is a build input.

Discipline notes:

- Trusted doesn't mean undisciplined: the injections are the only reach a plugin has, so its manifest is an auditable list of what it can touch. Reaching for an undeclared service fails the fiber.
- Transforms never touch I/O or the `llm` seam directly. The sweep hands each run a **granted, metered LLM handle** (`GrantedLlm`); when the per-run budget is spent — or any `LlmCall` guard listener denies — the handle refuses, and the transform falls back gracefully.
- Providers and consumers stay separate on purpose: consumers depend on the seam definition (`inseam-seams`), so swapping a provider restarts consumers without editing them.
