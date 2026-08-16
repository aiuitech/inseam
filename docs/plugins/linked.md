# Linked Plugins

The trusted tier ([design/plugins.md](../../design/plugins.md)), named for how it arrives: **linked** into the binary at build, where [loaded plugins](loaded.md) mount from artifacts at runtime. Rust, statically linked, activated by composition. `crates/inseam-plugins` ships the first-party set; a distribution registers the factories it links (`inseam_plugins::factories()`) with `Kernel::boot`, and a custom distribution compiles additional linked plugins in from source ([distributions.md](distributions.md)).

Every plugin is the same five declarations — name, config, inject, provide, apply ([kernel.md](../kernel.md)). What each first-party plugin does:

| Plugin | Provides | Injects | Role |
| --- | --- | --- | --- |
| `connection-fs` | `connection` | — | the local filesystem host ([index/filesystem-host.md](../index/filesystem-host.md)) |
| `llm-endpoint` | `llm` | — | OpenAI-compatible client; declares `transform_model`/`agent_model` as facts. Fails loudly (alone) when its key env is unset |
| `embedder` | `embedder` | store, llm? | endpoint / hashed / none; declares the embedding identity to the store, which binds the search surface |
| `transforms` | `transforms` | — | the registry seam transform plugins register into |
| `transform-markdown` | — | transforms | structural decomposition of markdown |
| `transform-chunker` | — | transforms | structural fallback for structureless text |
| `transform-summarizer` | — | transforms | the mandatory summary (LLM → extractive → envelope) |
| `transform-entities` | — | transforms | LLM entity extraction |
| `finder` | `finder` | store, embedder | RRF + personalized PageRank ranking ([finder/algorithm.md](../finder/algorithm.md)) |
| `sweep` | `sweep` | store, connection, transforms, embedder, llm? | the reconciling sweep ([index/maintenance.md](../index/maintenance.md)) |
| `operations` | `operations` | store, connection, finder, sweep | the transport-neutral node API ([finder/operations.md](../finder/operations.md)) |

Discipline notes:

- Trusted ≠ undisciplined: injections are the only reach a plugin has, so its manifest is an auditable capability list. Reaching for an undeclared key fails the fiber.
- Transforms never touch I/O or the `llm` seam directly. The sweep hands each application a **granted, metered LLM capability** (`GrantedLlm`); when the per-run budget is spent — or any `LlmCall` guard listener denies — the capability refuses, and the transform degrades.
- The provider/consumer split is a design rule: consumers bind to the seam definition (`inseam-seams`), so swapping providers restarts consumers without editing them.
