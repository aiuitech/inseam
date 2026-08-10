# Index Storage

Two layers under the node's data dir, settling the catalog-store question from [design/runtime.md](../../design/runtime.md):

## SQLite: source of truth (`catalog.sqlite3`)

- `sources` — the catalog: address (host + locator), envelope columns, `raw_bytes` for change detection, `root_fragment`, `indexed` flag, and `profile_stamp` — the profile shape the subtree was built under ([maintenance.md](maintenance.md)); NULL for catalog-only rows.
- `fragments` — the semantic graph's vertices: mimetype, text, extent. `source` is NULL only for entity fragments, which are deduplicated index-wide.
- `relations` — typed edges (`contains`, `links-to`, `derived-from`, `mentions`, `transcribes`), cascading on fragment deletion.
- `entities` — the dedup registry: `kind:normalized-name` -> fragment id.
- `meta` — schema version (2) plus the embedding model + dimensions this index was built with. Opening with a different embedding config puts the store into a re-embed-pending state: search refuses with instructions, and the next index run re-populates Lance from SQLite text ([maintenance.md](maintenance.md)).

## LanceDB: derived search surfaces (`lance/`)

One `fragments` table holding every **text-bearing** fragment: `id`, `source`, `text`, and a nullable `vector` column sized to the profile's dimensions (omitted entirely when embeddings are off). Two search paths:

- full-text (tantivy-backed FTS index, rebuilt after every index run so appended rows are always covered)
- vector nearest-k under cosine distance

Lance rows are strictly derived: deleting the whole `lance/` directory and re-running `inseam index <dir> --rebuild` reproduces them from SQLite + sources.

## Idempotency and change detection

A source is `Unchanged` only when its `modified` timestamp and on-disk byte size match the catalog, its last index run completed (`indexed = 1`, set only after the full subtree is stored), **and** its `profile_stamp` matches the current profile's shape tier. Interrupted runs and profile shape changes therefore re-index the affected source next time. Changed sources have their fragment subtree deleted (relations cascade; Lance rows deleted by id) and rebuilt. Entity fragments survive source rebuilds; only their `mentions` edges into the rebuilt source are re-derived — an entity left with no relations at all is garbage-collected at the end of the sweep. The full sweep contract, including vanished-source removal, lives in [maintenance.md](maintenance.md).
