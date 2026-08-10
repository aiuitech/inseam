# Crate Layout

The repo is a Cargo workspace, per the workspace decision in [design/runtime.md](../design/runtime.md):

- `crates/inseam` — the core library; all business logic lives here. All modules below are in this crate.
- `crates/inseam-cli` — the `inseam` binary (`cargo install --path crates/inseam-cli`): clap parsing, env/profile resolution, logging setup, and calls into the core's `ops`/`agent`. No business logic.
- `crates/inseam-ffi` — C ABI staticlib over the core for native app shells; header at `include/inseam_ffi.h`. See [macos-app.md](macos-app.md).
- `apps/macos` — the SwiftUI macOS app (SwiftPM, not a cargo workspace member). See [macos-app.md](macos-app.md).

Shared dependency versions live in `[workspace.dependencies]` in the root `Cargo.toml`; member crates reference them with `dep.workspace = true`.

| Module | What lives there |
| --- | --- |
| `address` | `HostId`, `Locator`, `Address` (`inseam://<host>/<locator>`), `Envelope`, `Timestamp`, `ContentLength`, trust `Property` |
| `fragment` | `FragmentId`, `Mimetype` (with params), `Extent`, `RelationKind`, `Relation`, `NewFragment`, `Sprout` |
| `profile` | `IndexProfile` and all its TOML-configurable sections |
| `host_fs` | The built-in filesystem connection: enumerate, resolve, read ([index/filesystem-host.md](index/filesystem-host.md)) |
| `store` | `IndexStore`: SQLite catalog + graph, Lance search surfaces ([index/storage.md](index/storage.md)) |
| `transform` | The `TransformRegistry` (the registration pathway core and plugins share) + `markdown`, `chunk`, `summarize`, `entities` submodules ([index/transforms.md](index/transforms.md)) |
| `indexer` | The pipeline: enumeration, registry-driven transforms with capability mediation, embeddings, store writes |
| `embed` | `Embedder`: the configured endpoint, deterministic hashed (offline/tests), or disabled |
| `llm` | The only networking module: an OpenAI-compatible client (embeddings, chat, model catalog); the endpoint is profile config |
| `finder` | Seed fusion (RRF), personalized PageRank, source rollup ([finder/algorithm.md](finder/algorithm.md)) |
| `ops` | Transport-neutral operation messages + the `Node` facade ([finder/operations.md](finder/operations.md)) |
| `agent` | The live-LLM demo loop driving the operations as tools |
| `dates` | Civil-date <-> epoch helpers (no calendar crate) |

Tests: unit tests inline per module; `crates/inseam/tests/` holds the end-to-end ladder walk (`index_and_find.rs`), the relational-relevance proof (`finder_graph.rs`), and property tests over the Finder's numeric core (`finder_props.rs`). All tests run offline on the hashed embedder.
