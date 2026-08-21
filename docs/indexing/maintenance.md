# Index Maintenance

Every `inseam index <dir>` run is a **reconciling sweep** ([design/index-maintenance.md](../../design/index-maintenance.md)): it brings the index in line with reality (what the connection currently lists) and with the configuration (what the mounted transforms would build). There is no separate maintenance command, and plugins never trigger index work directly — the sweep discovers what's out of date on its own.

## What one sweep does, in order

1. **Re-embed, if pending** — if the embedding model changed, vectors are rebuilt in the search tables from the text already in the catalog. No transforms re-run, no LLM spend; search refuses (with instructions) until this finishes. An interrupted re-embed restarts.
2. **Per-source reconciliation** — a source re-indexes (its index subtree deleted and rebuilt with the full mounted transform set) when its content changed (`modified` + byte size), its previous run was interrupted, or it is **shape-stale** (see below). Catalog-only rows (past `max_sources`, or first seen outside the date cutoff) carry no stamp, so they're picked up automatically once the budget or cutoff allows.
3. **Vanished-source removal** — cataloged sources under the swept scope that the listing no longer shows are deleted outright. No tombstones; a file that reappears is simply new. Sources the ignore rules now cover ([ignore.md](ignore.md)) leave the listing the same way and are removed by this step.
4. **Keyed-fragment cleanup** — keyed fragments (entities, for instance) left with no relations are dropped.

## How a run moves: plan in parallel, land in order

Deep indexing is a pipeline ([design/indexing.md](../../design/indexing.md)):

1. **Decide** — one pass over the listing marks each source unchanged, past the cutoff, catalog-only (past `max_sources`), or dirty. Catalog-only rows are written in batches.
2. **Plan** — dirty sources are planned `concurrency` at a time (default 8). Planning is where the time goes: the content is read and every transform claiming a fragment runs — all claimants of one fragment at once, so a summary and an entity extraction of the same section are in flight together — producing the source's whole subtree as data, with no store writes.
3. **Land** — each plan lands in one store transaction, in enumeration order regardless of which finished first, so the index a run builds (fragment ids included) does not depend on `concurrency`.
4. **Embed** — the landed plan's text rows stream to the embedding stage, which embeds batches of 128 concurrently and lands them in order together with the `indexed` marks of the sources they complete. A source is marked indexed only once every one of its rows is searchable, so a crash mid-run leaves it dirty, never half-searchable.

LLM budgets stay exact under concurrency: every application of a transform shares one per-run meter, and each call reserves against it atomically before it is made.

## Shape staleness: re-index only what a change touches

Each deep-indexed source stores two records ([storage.md](storage.md)):

- the **shape stamp** — a fingerprint of the transforms that *participated* in building its index (entry id + config fingerprint, plus the artifact version/hash for loaded transforms, plus the sweep's own size/depth settings);
- the **mimetype inventory** — every mimetype in its subtree, root and emitted.

On a later sweep, the expected stamp is recomputed from the *current* transforms intersected with the stored inventory. So: removing, reconfiguring, or upgrading a transform re-indexes exactly the sources it touched; mounting a new transform re-indexes only sources whose inventory matches its claims — installing a video plugin never re-runs paid LLM summaries over your markdown notes. Rebuilds run with the full mounted set, so chained transforms (one claiming what another emits) resolve in a single pass.

## What a config change costs

Which entry changed decides how much re-work happens ([configuration.md](../configuration.md)):

- **Query-time** (`finder`, the llm entry's models) — never re-indexes.
- **Run limits** (`sweep.max_sources`, `sweep.concurrency`, per-transform `llm_call_budget`) — never re-indexes; they just bound or pace each run, so a big rebuild spreads across as many sweeps as the budgets allow.
- **Shape** (transform configs, transform mounts/unmounts, the sweep's `max_depth`/`max_fragments_per_source`/`max_content_bytes`, the llm `transform_model` for transforms that use it) — stamps stop matching; affected sources re-index on their next sweep.
- **Embedding** (the `embedder` entry) — only the in-place re-embed.

## Shrinking scope never deletes — ignoring does

Tightening `sweep.modified_after` or `max_content_bytes` stops future work but removes nothing already built — you can tighten and loosen freely. Sources outside the cutoff are still cataloged (address + envelope only) when first seen.

Ignore rules are different on purpose: they say a source is not yours to index, so an ignored source is not cataloged at all, and one that was indexed earlier is removed on the next sweep ([ignore.md](ignore.md)). Lifting a rule readmits it as new.

## Change detection

The only thing the sweep needs from outside is a listing of what exists. Host-specific change feeds (FSEvents, Gmail history, …) arrive later as connection-plugin capabilities that schedule targeted sweeps — events are hints; the sweep is the truth.
