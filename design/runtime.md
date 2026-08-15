# Runtime

Technology decisions for the kernel and distributions.

## Rust, single binary

The kernel is written in **Rust** and every distribution ships as a single static binary. One artifact runs a node anywhere — a laptop, a phone-adjacent device, a beefy server, a container. A node is: the binary + a [composition](composition.md) + a data directory. Rust also gives us first-class wasmtime embedding for sandboxed [plugins](plugins.md) and the performance headroom the [index](discovery.md) needs.

## Workspace: kernel, plugins, distributions

The repo is a Cargo workspace shaped like the architecture: the **kernel crate** (substrate + store, [kernel](kernel.md)), **plugin crates** for the native plugins (grouped pragmatically, not one-crate-per-plugin from day one), and **distribution app crates** that link a plugin set and ship a base composition. The CLI (`inseam-cli`, installing the `inseam` binary) is the first distribution; the macOS GUI (SwiftUI in `apps/macos`, linking through the `inseam-ffi` C ABI staticlib) is the second. Non-Rust app shells live under `apps/`, outside the cargo workspace, in this same repo — the FFI header and app are generated against the source, and one repo keeps them in lockstep. Future apps — mobile targets, a daemon — follow the same pattern. (The current single `inseam` library crate is the pre-substrate layout; it splits along these lines as the kernel lands.)

The FFI boundary is deliberately thin: opaque node handle, blocking calls (the handle owns its tokio runtime), JSON responses reusing the exact serde views the operations layer already defines. No second protocol to design — the C ABI is just another transport adapter over `node-api.md` operations. "Single binary" remains true per artifact: each distribution is one self-contained binary embedding the whole node; nothing is a client to a separate core process. App crates hold only transport/UI concerns (argument parsing, env loading, logging setup, platform bindings) — anything two apps would both need belongs in a plugin or the kernel.

## Index storage: LanceDB

The discovery index is built on **LanceDB** (embedded, via the native Rust crate):

- Hybrid **full-text + vector** search in one store — exactly the index shape discovery calls for.
- **Embedded**, no server process — matches "a node is one binary" and works on small devices.
- **Object-store backend** — the same tables run on local disk for devices and on **S3** for large hosted nodes.

The catalog itself (addresses, envelopes, properties — the source of truth) is **SQLite**, settled when the storage layer was built: one transactional store holds the catalog *and* the semantic graph (fragments + relations + entity registry), with Lance strictly the derived search surface (vectors + FTS over text-bearing fragments). Lance data is rebuildable from SQLite + fetches at any time; the embedding model/dimensions an index was built with are recorded and guarded at open.

### Known risks

- LanceDB's Rust API is the native layer but less documented than the Python surface; FTS feature coverage (phrase queries, tokenizers) needs verification against our needs early.
- S3-backed tables have optimistic-concurrency constraints; fine while each index has a single writing node, which matches the per-node index design.

## Paths not taken

- **Go / TypeScript core.** Weaker wasmtime story (Go) or heavy runtime + packaging pain (TS); Rust wins on embedding, binary distribution, and index performance.
- **SQLite + sqlite-vec for the index.** Simplest possible stack, viable fallback, but weaker at scale for hybrid search and no object-store story.
- **Server-based search engines (Qdrant, Meilisearch, Elasticsearch).** A separate server process per node contradicts the single-binary node and small-device targets.

## Open questions

- Embedding generation on small devices (bundled small model? remote node as embedding provider?). A deterministic hashed bag-of-words embedder ships as the zero-cost offline fallback; it is a stopgap, not the answer.
- Minimum supported targets (is mobile a first-class node platform?).
