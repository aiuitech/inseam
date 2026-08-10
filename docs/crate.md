# Crate Layout

`inseam` is one Rust crate: a library plus the `inseam` CLI binary (`src/main.rs`), per the single-binary node decision in [design/runtime.md](../design/runtime.md).

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

Tests: unit tests inline per module; `tests/` holds the end-to-end ladder walk (`index_and_find.rs`), the relational-relevance proof (`finder_graph.rs`), and property tests over the Finder's numeric core (`finder_props.rs`). All tests run offline on the hashed embedder.
