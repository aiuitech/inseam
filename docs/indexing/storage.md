# Index Storage

Two databases under the node's data dir, both libSQL, owned entirely by the kernel ([../architecture/kernel.md](../architecture/kernel.md)) — no plugin ever changes the schema, and there are no data migrations anywhere: a schema-version bump drops and recreates the tables, and the next sweep rebuilds them from the source listing.

## Catalog: source of truth (`catalog.sqlite3`)

- `sources` — the catalog: address (host + locator), envelope columns, `raw_bytes` for change detection, `root_fragment`, an `indexed` flag, and the two shape records ([maintenance.md](maintenance.md)): `shape_stamp` (fingerprint of the transforms that built it) and `mimetypes` (the subtree's mimetype inventory). Both NULL for catalog-only rows.
- `fragments` — the graph's vertices: mimetype, text, extent. `source` is NULL only for entity fragments, which are shared across the whole index.
- `relations` — typed edges (`contains`, `links-to`, `derived-from`, `mentions`, `transcribes`), deleted along with their fragments.
- `entities` — the dedup registry: `kind:normalized-name` → fragment id.
- `plugin_state` / `plugin_state_meta` — the kernel's `state` service: per-plugin namespaced key-value, declared with a version; a version mismatch discards the namespace (the wasm host keeps its release-cooldown first-seen clocks here).
- `meta` — schema version (3) plus the embedding identity the index was built with.

## Search: derived search surface (`search.sqlite3`)

The search surface is tied to whichever embedder is mounted: when the embedder plugin starts, it declares its identity (`model`, `dimensions`) and the search database opens under it; with no embedder mounted, searches refuse with instructions. If the declared identity differs from the recorded one, an **in-place re-embed** is queued — vectors rebuilt from the catalog's text, graph untouched.

One `search_rows` table holds every text-bearing fragment: `id`, `source`, `text`, and a vector column (`F32_BLOB`, omitted at 0 dimensions). Two search paths: full-text (FTS5, kept in sync by triggers and ranked by BM25) and vector nearest-k by cosine distance (`vector_distance_cos`, an exact scan). Search rows are purely derived: delete `search.sqlite3`, run `inseam index <dir> --rebuild`, and they come back.

## Idempotency and change detection

A source is skipped only when its `modified` timestamp and byte size match the catalog, its last run completed (`indexed = 1`, set only after the full subtree is stored), **and** its stored shape stamp matches what the currently mounted transforms would produce for its stored inventory. So interrupted runs, config changes, and transform mounts/unmounts re-index exactly the affected sources on the next sweep. Entity fragments survive source rebuilds; an entity left with no relations is cleaned up at the end of the sweep.
