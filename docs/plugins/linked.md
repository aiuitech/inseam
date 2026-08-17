# Linked Plugins

The trusted tier ([design/plugins.md](../../design/plugins.md)), named for how the code arrives: **linked** into the binary at build time, where [loaded plugins](loaded.md) mount from artifacts at runtime. Rust, statically compiled, turned on by the composition. `crates/inseam-plugins` ships the first-party set; a distribution registers the factories it links (`inseam_plugins::factories()`) with `Kernel::boot`, and a custom distribution compiles additional linked plugins in from source ([distributions.md](distributions.md)).

Every plugin is the same five declarations — name, config, inject, provide, apply ([../architecture/kernel.md](../architecture/kernel.md)). What each first-party plugin does:

| Plugin | Provides | Injects | Role |
| --- | --- | --- | --- |
| `connection-fs` | `connection` | — | the local filesystem host ([indexing/filesystem-host.md](../indexing/filesystem-host.md)) |
| `llm-endpoint` | `llm` | — | OpenAI-compatible client; declares `transform_model`/`agent_model` as facts. Fails loudly (and alone) when its key env var is unset |
| `embedder` | `embedder` | store, llm? | endpoint / hashed / none; declares the embedding identity to the store, which opens the search surface under it |
| `transforms` | `transforms` | — | the registry transform plugins sign up with |
| `transform-markdown` | — | transforms | splits markdown by its heading structure |
| `transform-chunker` | — | transforms | fallback chunking for text with no structure |
| `transform-summarizer` | — | transforms | the mandatory summary (LLM → extractive → envelope) |
| `transform-entities` | — | transforms | LLM entity extraction |
| `finder` | `finder` | store, embedder | the ranking algorithm ([finder/algorithm.md](../finder/algorithm.md)) |
| `sweep` | `sweep` | store, connection, transforms, embedder, llm? | the reconciling sweep ([indexing/maintenance.md](../indexing/maintenance.md)) |
| `operations` | `operations` | store, connection, finder, sweep | the transport-neutral node API ([finder/operations.md](../finder/operations.md)) |

The agent skill at `skills/inseam-linked-plugin/SKILL.md` is the authoring guide for this tier ([../skills/inseam-linked-plugin.md](../skills/inseam-linked-plugin.md)); it assumes a source checkout, since a linked plugin is a build input.

Discipline notes:

- Trusted doesn't mean undisciplined: the injections are the only reach a plugin has, so its manifest is an auditable list of what it can touch. Reaching for an undeclared service fails the fiber.
- Transforms never touch I/O or the `llm` seam directly. The sweep hands each run a **granted, metered LLM handle** (`GrantedLlm`); when the per-run budget is spent — or any `LlmCall` guard listener denies — the handle refuses, and the transform falls back gracefully.
- Providers and consumers stay separate on purpose: consumers depend on the seam definition (`inseam-seams`), so swapping a provider restarts consumers without editing them.
