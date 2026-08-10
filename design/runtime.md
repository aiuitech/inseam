# Runtime

Technology decisions for the core.

## Rust, single binary

The core is written in **Rust** and ships as a single static binary. One artifact runs a node anywhere — a laptop, a phone-adjacent device, a beefy server, a container. A node is: the binary + a config file + a data directory. Rust also gives us first-class wasmtime embedding for [plugins](plugins.md) and the performance headroom the [index](discovery.md) needs.

## Index storage: LanceDB

The discovery index is built on **LanceDB** (embedded, via the native Rust crate):

- Hybrid **full-text + vector** search in one store — exactly the index shape discovery calls for.
- **Embedded**, no server process — matches "a node is one binary" and works on small devices.
- **Object-store backend** — the same tables run on local disk for devices and on **S3** for large hosted nodes.

The catalog itself (addresses, envelopes, properties — the source of truth) is not necessarily Lance; a small transactional store (SQLite is the working assumption) may hold it, with Lance strictly as the derived index. To be settled when the storage layer is designed.

### Known risks

- LanceDB's Rust API is the native layer but less documented than the Python surface; FTS feature coverage (phrase queries, tokenizers) needs verification against our needs early.
- S3-backed tables have optimistic-concurrency constraints; fine while each index has a single writing node, which matches the per-node index design.

## Paths not taken

- **Go / TypeScript core.** Weaker wasmtime story (Go) or heavy runtime + packaging pain (TS); Rust wins on embedding, binary distribution, and index performance.
- **SQLite + sqlite-vec for the index.** Simplest possible stack, viable fallback, but weaker at scale for hybrid search and no object-store story.
- **Server-based search engines (Qdrant, Meilisearch, Elasticsearch).** A separate server process per node contradicts the single-binary node and small-device targets.

## Open questions

- Embedding generation on small devices (bundled small model? remote node as embedding provider?).
- Catalog store decision (SQLite vs. Lance-for-everything vs. custom log).
- Minimum supported targets (is mobile a first-class node platform?).
