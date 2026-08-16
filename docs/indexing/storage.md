# Index Storage

Two layers under the node's data dir, owned entirely by the kernel ([../kernel.md](../architecture/kernel.md)) — no plugin ever issues DDL, and there are no data migrations anywhere: a schema-version bump drops and recreates tables, and the next sweep rebuilds them from enumeration.

## SQLite: source of truth (`catalog.sqlite3`)

- `sources` — the catalog: address (host + locator), envelope columns, `raw_bytes` for change detection, `root_fragment`, `indexed` flag, and the two claims-aware shape records ([maintenance.md](maintenance.md)): `shape_stamp` (digest of the participating transform registrations) and `mimetypes` (the subtree's mimetype inventory). Both NULL for catalog-only rows.
- `fragments` — the semantic graph's vertices: mimetype, text, extent. `source` is NULL only for entity fragments, which are deduplicated index-wide.
- `relations` — typed edges (`contains`, `links-to`, `derived-from`, `mentions`, `transcribes`), cascading on fragment deletion.
- `entities` — the dedup registry: `kind:normalized-name` → fragment id.
- `plugin_state` / `plugin_state_meta` — the kernel-provided `state` service: per-plugin namespaced key-value, declared with a version; version mismatch discards the namespace (the wasm host keeps release-cooldown first-seen clocks here).
- `meta` — schema version (3) plus the embedding identity the index was built with.

## LanceDB: derived search surfaces (`lance/`)

The search surface binds **lazily to the mounted embedder**: when the embedder plugin activates it declares its identity (`model`, `dimensions`) and the Lance table opens under it; no embedder mounted means searches refuse with instructions. A declared identity differing from the recorded one pends an **in-place re-embed** — vectors rebuilt from SQLite text, graph untouched.

One `fragments` table holds every text-bearing fragment: `id`, `source`, `text`, and a nullable `vector` column (omitted at 0 dimensions). Two search paths: full-text (tantivy-backed FTS, rebuilt after every index run) and vector nearest-k under cosine distance. Lance rows are strictly derived: deleting `lance/` and re-running `inseam index <dir> --rebuild` reproduces them.

## Idempotency and change detection

A source is skipped only when its `modified` timestamp and byte size match the catalog, its last run completed (`indexed = 1`, set only after the full subtree is stored), **and** its stored shape stamp equals the stamp the currently mounted transforms would produce for its stored inventory. Interrupted runs, config shape changes, and transform mounts/unmounts therefore re-index exactly the affected sources next sweep. Entity fragments survive source rebuilds; an entity left with no relations is garbage-collected at the end of the sweep.
