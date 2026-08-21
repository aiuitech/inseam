# Runtime

Technology decisions for the kernel and distributions.

## Rust, single binary

The kernel is written in **Rust** and every distribution ships as a single static binary. One artifact runs a node anywhere — a laptop, a phone-adjacent device, a beefy server, a container. A node is: the binary + a [composition](composition.md) + a data directory. Rust also gives us first-class wasmtime embedding for loaded [plugins](plugins.md) and the performance headroom the [index](discovery.md) needs.

## Workspace: kernel, plugins, distributions

The repo is a Cargo workspace shaped like the architecture: the **kernel crate** (substrate + store, [kernel](kernel.md)), **plugin crates** for the linked plugins (grouped pragmatically, not one-crate-per-plugin from day one), and **distribution app crates** that link a plugin set and ship a base composition. The CLI (`inseam-cli`, installing the `inseam` binary) is the first distribution; the macOS GUI (SwiftUI in `apps/macos`, linking through the `inseam-ffi` C ABI staticlib) is the second. Non-Rust app shells live under `apps/`, outside the cargo workspace, in this same repo — the FFI header and app are generated against the source, and one repo keeps them in lockstep. Future apps — mobile targets, a daemon — follow the same pattern. As built: `inseam-kernel` (substrate + store), `inseam-seams` (the seam definitions, apart from providers and consumers), `inseam-plugins` (first-party linked plugins), `inseam-wasm-host` (the loaded-tier bridge), and the `inseam-cli`/`inseam-ffi` distributions; loaded plugin projects live under `plugins/`, outside the workspace.

The FFI boundary is deliberately thin: opaque node handle, blocking calls (the handle owns its tokio runtime), JSON responses reusing the exact serde views the operations layer already defines. No second protocol to design — the C ABI is just another transport adapter over `node-api.md` operations. "Single binary" remains true per artifact: each distribution is one self-contained binary embedding the whole node; nothing is a client to a separate core process. App crates hold only transport/UI concerns (argument parsing, env loading, logging setup, platform bindings) — anything two apps would both need belongs in a plugin or the kernel.

## Index storage: libSQL

The derived search surface is **libSQL** (Turso's production SQLite fork, embedded via the native Rust crate with local-only features):

- Hybrid **full-text + vector** search in one embedded store: FTS5 for keyword seeds, native vector columns (`F32_BLOB`) with cosine distance for semantic seeds — exactly the index shape discovery calls for. DiskANN indexes (`libsql_vector_idx` / `vector_top_k`) are available in the same engine the day flat scans stop being fast enough; today's scale doesn't need them (the LanceDB era never built an ANN index either — every query was a flat scan).
- **Embedded**, no server process — matches "a node is one binary" and works on small devices.
- **Tiny dependency surface** — one bundled C library, replacing LanceDB's arrow/datafusion tree (~520 crates that dominated build times, `target/` size, and binary footprint while we used a fraction of them).

Everything lives in **one libSQL database file** (`catalog.sqlite3`): the catalog (addresses, envelopes, properties — the source of truth), the semantic graph (fragments + relations + keyed-fragment registry), plugin state, and the derived search tables (`search_rows` + its FTS5 index). The source-of-truth/derived boundary is drawn at the **table** level, not the file level: the search tables are strictly derived — dropped and rebuilt wholesale by re-embeds and schema bumps, rebuildable from the catalog tables at any time, with the embedding model/dimensions an index was built with recorded and guarded at open. One file means one engine, one transaction domain, and one thing to back up or delete; if catalog replication ever lands ([address-sync](address-sync.md)), the derived tables are excluded by name (they are per-node, [indexing](indexing.md)) — a filter, not a file split.

One engine is not just tidiness — it is forced. The catalog was rusqlite (vanilla SQLite, bundled) when the search surface moved to libSQL, and the two cannot coexist in one binary: both bundle a C library exporting the same `sqlite3_*` symbols, the linker keeps one copy, and whichever library initializes second trips over the other's global state at runtime. The catalog therefore moved onto libSQL in the same change (its API went async with it — the kernel is async throughout, so the cascade stopped at a handful of call sites), and the briefly separate `search.sqlite3` file was folded into the catalog database right after.

The database runs WAL with `synchronous = NORMAL`, and every write goes through one store-level write lock. NORMAL drops the per-commit fsync (a power cut can lose the last commits, never corrupt the file) — acceptable because every table is derived or re-derivable and the sweep's `indexed` mark is written only when a source's rows are all in place, so lost commits are re-indexed, not silently missing. The write lock is what makes one connection safe under the sweep's concurrent stages: a libSQL connection carries one open transaction, and two tasks writing through it would interleave statements into each other's transactions.

### Known risks

- FTS5's BM25 replaces tantivy's; ranking differs in the tail. The finder only consumes rank order (RRF fusion), so this is contained by design.
- Vector search is an exact scan (parity with the LanceDB usage, which never built an ANN index). The day it shows up in a profile, DiskANN (`libsql_vector_idx`) is one `CREATE INDEX` away in the same engine.

## Paths not taken

- **Go / TypeScript core.** Weaker wasmtime story (Go) or heavy runtime + packaging pain (TS); Rust wins on embedding, binary distribution, and index performance.
- **LanceDB** (the original choice, replaced August 2026). Chosen for hybrid search and an object-store backend; in practice we used a flat cosine scan plus FTS — no ANN index, no S3 — while paying for the full arrow/datafusion stack in compile time and binary size. The object-store story lost its pull once [indexing](indexing.md) settled on query fan-out over remote indexes.
- **Turso Database** (the from-scratch Rust rewrite of SQLite, successor to libSQL). Still beta as of August 2026 — beta storage engines fail the safety-first goal. It is the natural upgrade path from libSQL; revisit at 1.0-stable.
- **SQLite + sqlite-vec for the index.** Viable fallback, but brute-force-only vectors behind a loadable extension; libSQL ships vectors natively with a DiskANN growth path.
- **Server-based search engines (Qdrant, Meilisearch, Elasticsearch).** A separate server process per node contradicts the single-binary node and small-device targets.
- **Turso sync for the store.** Embedded-replica sync needs a sync server (Turso Cloud or self-hosted) — a standing external dependency against the single-binary node — and replicating the search tables is index shipping, which [discovery](discovery.md) already rejected: indexes are per-node, and reach comes from query fan-out. libSQL keeps the mechanism available if a plugin-level index-shipping experiment ever earns its place; it is not a core concept.

## Open questions

- Embedding generation on small devices (bundled small model? remote node as embedding provider?). A deterministic hashed bag-of-words embedder ships as the zero-cost offline fallback; it is a stopgap, not the answer.
- Minimum supported targets (is mobile a first-class node platform?).
