# Index Storage

One libSQL database (`catalog.sqlite3`) under the node's data dir, owned entirely by the kernel ([../architecture/kernel.md](../architecture/kernel.md)) — no plugin ever changes the schema, and there are no data migrations anywhere: a schema-version bump drops and recreates the tables, and the next sweep rebuilds them from the source listing. Two groups of tables share the file: the catalog tables are the source of truth, the search tables are derived from them.

## Catalog tables: source of truth

- `sources` — the catalog: address (host + locator), envelope columns (including the optional content `digest`), `raw_bytes` for change detection, `root_fragment`, an `indexed` flag, and the two shape records ([maintenance.md](maintenance.md)): `shape_stamp` (fingerprint of the transforms that built it) and `mimetypes` (the subtree's mimetype inventory). Both NULL for catalog-only rows.
- `fragments` — the graph's vertices: mimetype, text, extent. `source` is NULL only for keyed fragments, which are shared across the whole index.
- `relations` — typed edges, stored input → output; the kind is an open name (`contains`, `derives`, `links-to`, `mentions`, `transcribes`, …), deleted along with their fragments.
- `keyed_fragments` — the dedup registry for index-wide fragments: plugin-namespaced key (`entity:person:greg`) → fragment id.
- `plugin_state` / `plugin_state_meta` — the kernel's `state` service: per-plugin namespaced key-value, declared with a version; a version mismatch discards the namespace (the wasm host keeps its release-cooldown first-seen clocks here).
- `meta` — schema version (5) plus the embedding identity the index was built with (model, dimensions, and which fragments carry vectors; [embeddings.md](embeddings.md)).

## Search tables: derived search surface

The search surface is tied to whichever embedder is mounted: when the embedder plugin starts, it declares its identity (`model`, `dimensions`, `vectors`) and the search tables open under it; with no embedder mounted, searches refuse with instructions. If the declared identity differs from the recorded one, an **in-place re-embed** is queued — the search tables are dropped and re-populated from the catalog's text, graph untouched.

The `search_rows` table holds every text-bearing fragment: `id`, `source`, `text`, and a compact candidate vector (`F8_BLOB`, omitted at 0 dimensions; NULL for rows outside the embedder's vector scope — under `vectors = "summaries"` only summary rows carry one). It is indexed by source for subtree purges and by libSQL DiskANN for approximate nearest-neighbor lookup; `search_fts` is an external-content FTS5 index over the same rows, kept in sync by triggers. Vector search asks DiskANN for four times the requested result count, then orders that bounded candidate set by cosine distance. Search rows are purely derived: `inseam index <dir> --rebuild` reconstructs them from the catalog at any time.

Nodes created before the DiskANN surface stored float32 vectors without an ANN index. `inseam repair` converts those resident vectors to the compact column, releases each redundant blob in the same bounded batch, checkpoints the WAL between batches, and then builds the index in place. Each batch commits independently, so an interrupted repair continues from the remaining rows without rerunning source indexing or retaining a second full corpus copy. It does not fetch documents, rerun transforms, call the embedding endpoint, change the embedding identity, or discard catalog/index completion state. Node startup and `inseam status` only inspect readiness; `inseam repair --rebuild` explicitly reconstructs an already-present DiskANN index. This derived-surface convergence is the narrow exception to the no-migration rule for catalog state: it exists so a storage optimization never turns into an expensive semantic rebuild.

Writes are transactional across both table groups: a source's whole subtree — catalog row, old subtree purge, fragments, relations, keyed fragments and their anchors — lands in one transaction, and dropping a source, its fragments, or a garbage-collected keyed fragment removes the matching search rows in the same transaction, so a crash can never leave search rows pointing at fragments the catalog no longer has. The other direction — fragments written but not yet searchable — is what the `indexed` mark guards: it is written in the same transaction as the last of the source's search rows, so an interrupted run leaves the source dirty and the next sweep rebuilds it.

The database runs in WAL mode with `synchronous = NORMAL`: commits append to the log without a per-commit fsync, which a power cut can lose but never corrupt — every table is rebuildable, and the sweep re-indexes whatever lost its mark. All writes go through one store-level write lock, so concurrent writers (the sweep's landing and embedding stages, a query-time operation) never interleave statements inside each other's transactions.

## Idempotency and change detection

A source is skipped only when its `modified` timestamp and byte size match the catalog, its last run completed (`indexed = 1`, set only after the full subtree is stored), **and** its stored shape stamp matches what the currently mounted transforms would produce for its stored inventory. So interrupted runs, config changes, and transform mounts/unmounts re-index exactly the affected sources on the next sweep. Keyed fragments (entities) survive source rebuilds; one left with no relations is cleaned up at the end of the sweep.
