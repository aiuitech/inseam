# Index Maintenance

Every `inseam index <scope>` run is a **reconciling sweep** over one host ([connections.md](connections.md)) ([design/index-maintenance.md](../../design/index-maintenance.md)): it brings the index in line with reality (what the connection currently lists) and with the configuration (what the mounted transforms would build). There is no separate maintenance command, and plugins never trigger index work directly — the sweep discovers what's out of date on its own.

## What one sweep does, in order

1. **Re-embed, if pending** — if the embedding identity changed (model, dimensions, or which fragments get vectors — [embeddings.md](embeddings.md)), vectors are rebuilt in the search tables from the text already in the catalog. No transforms re-run, no LLM spend; search refuses (with instructions) until this finishes. An interrupted re-embed restarts.
2. **Per-source reconciliation** — a source re-indexes (its index subtree deleted and rebuilt with the full mounted transform set) when its content changed (`modified` + byte size), its previous run was interrupted, or it is **shape-stale** (see below). Catalog-only rows (past `max_sources`, or first seen outside the date cutoff) carry no stamp, so they're picked up automatically once the budget or cutoff allows.
3. **Vanished-source removal** — cataloged sources under the swept scope that the listing no longer shows are deleted outright. No tombstones; a file that reappears is simply new. Sources the ignore rules now cover ([ignore.md](ignore.md)) leave the listing the same way and are removed by this step.
4. **Keyed-fragment cleanup** — keyed fragments (entities, for instance) left with no relations are dropped.

## How a run moves: plan in parallel, land in order

Deep indexing is a pipeline ([design/indexing.md](../../design/indexing.md)):

1. **Decide** — one pass over the listing marks each source unchanged, past the cutoff, catalog-only (past the run's deep budget), or dirty. Catalog-only rows are written in batches.
2. **Plan** — dirty sources are planned `concurrency` at a time (default 8; `batch_concurrency`, 4,096, when LLM calls ride the batch lane — [embeddings.md](embeddings.md#the-batch-lane)). Planning is where the time goes: the content is read and every transform claiming a fragment runs — all claimants of one fragment at once, so a summary and an entity extraction of the same section are in flight together — producing the source's whole subtree as data, with no store writes. `source_reads_in_flight_max` independently caps content reads at 128, so thousands of planners can park on batch calls without opening thousands of files together.
3. **Land** — each plan lands in one store transaction, in enumeration order regardless of which finished first, so the index a run builds (fragment ids included) does not depend on `concurrency`.
4. **Embed** — the landed plan's text rows stream to the embedding stage in 256-row batches. The endpoint client packs the vector-covered rows into requests of up to 128 inputs; uncovered rows land text-only, and four sweep batches embed concurrently. Each source's completion remembers its final-row boundary, so its `indexed` mark lands with the first ordered batch that crosses that boundary rather than waiting for later sources. A source is marked indexed only once every one of its rows is searchable, so a crash mid-run leaves it dirty, never half-searchable.
5. **Folders, after files** — vanished sources are removed, then folders run through steps 2–4 one depth level at a time, deepest first ([transforms.md](transforms.md#folders)). Each folder's content is composed from the catalog its children just landed in, and it is dirty when that listing's digest differs from the one its last run recorded — a directory's timestamp is not its content's recency — so a folder re-indexes when a child appears, vanishes, or re-summarizes. Folders spend the same deep budget, after the files; a folder none of whose children is deep-indexed yet stays catalog-only, which is how the cutoff and the budget reach it. Folders have no cutoff of their own.
6. **Finish** — orphaned keyed fragments collected, the full-text index compacted, and the DiskANN vector index built if it is missing. A run whose dirty set is at least half the size of what was already indexed drops the vector index before step 3 and rebuilds it here, because inserting rows through the index is far slower than building it once over the finished table ([storage.md](storage.md)).

LLM budgets stay exact under concurrency: every application of a transform shares one per-run meter, and each call reserves against it atomically before it is made.

## What a rebuild costs

Rebuilding a source re-runs decomposition, which is cheap, and re-pays only the expensive artifacts whose inputs actually changed. Before the planner applies an LLM transform (the summarizer, the entity extractor) it asks the catalog's `transform_cache` for an output filed under the input's content digest and the transform's shape identity — entry, config fingerprint, model ([storage.md](storage.md)). A hit is used as-is and counted under `reused: … transform outputs` in the report; a cached LLM summary still counts as an LLM summary, and costs no budget. Only outputs the model produced are filed: an extractive fallback (budget spent, endpoint down) is never cached, so the next run with budget asks the model. Vectors are reused the same way ([embeddings.md](embeddings.md#reuse-across-rebuilds)). Together this is what makes `--rebuild`, an interrupted run's second attempt, and a shape change in one transform affordable: changing the summarizer's `target_chars` re-summarizes everything, but mounting the entity extractor re-summarizes nothing.

## Ingest first, index later: the deep budget

How many sources one run deep-indexes is the run's **deep budget**. The composition's `sweep.max_sources` is the steady state (`0` = unlimited); `inseam index` overrides it for one run with `--catalog-only` (deep-index nothing) or `--max-sources N`. The override never outlives the run — the next unqualified run is back on the composition's dial.

`inseam index <scope> --catalog-only` is the ingest run: every enumerated source lands in the catalog as address + envelope (and so enters address sync), no transform runs, no LLM is spent, and every row is left catalog-only — no shape stamp — so the next run with budget deep-indexes it. There is no separate ingest command because there is no separate mechanism: cataloging is the first half of every sweep; the budget only decides whether the second half happens now.

`inseam catalog` lists what the catalog holds — `--pending` for sources that are cataloged but not yet deep-indexed (catalog-only, past the cutoff, or interrupted), `--indexed` for the ones with a landed subtree — with counts over the whole selection whatever `--limit` shows.

## Shape staleness: re-index only what a change touches

Each deep-indexed source stores two records ([storage.md](storage.md)):

- the **shape stamp** — a fingerprint of the transforms that *participated* in building its index (entry id + config fingerprint, plus the artifact version/hash for loaded transforms, plus the sweep's own size/depth settings);
- the **mimetype inventory** — every mimetype in its subtree, root and emitted.

On a later sweep, the expected stamp is recomputed from the *current* transforms intersected with the stored inventory. So: removing, reconfiguring, or upgrading a transform re-indexes exactly the sources it touched; mounting a new transform re-indexes only sources whose inventory matches its claims — installing a video plugin never re-runs paid LLM summaries over your markdown notes. Rebuilds run with the full mounted set, so chained transforms (one claiming what another emits) resolve in a single pass.

## What a config change costs

Which entry changed decides how much re-work happens ([configuration.md](../configuration.md)):

- **Query-time** (`finder`, the llm entry's models) — never re-indexes.
- **Run limits** (`sweep.max_sources` and the `--catalog-only` / `--max-sources` overrides, `sweep.concurrency`, per-transform `llm_call_budget`) — never re-indexes; they just bound or pace each run, so a big rebuild spreads across as many sweeps as the budgets allow.
- **Shape** (transform configs, transform mounts/unmounts, the sweep's `max_depth`/`max_fragments_per_source`/`max_content_bytes`/`max_reference_hops`, the llm `transform_model` for transforms that use it) — stamps stop matching; affected sources re-index on their next sweep, paying again only for the transforms whose own identity changed ([what a rebuild costs](#what-a-rebuild-costs)).
- **Embedding** (the `embedder` entry: provider, model, dimensions, `vectors`) — only the in-place re-embed.

## Shrinking scope never deletes — ignoring does

Tightening `sweep.modified_after` or `max_content_bytes` stops future work but removes nothing already built — you can tighten and loosen freely. Sources outside the cutoff are still cataloged (address + envelope only) when first seen.

Ignore rules are different on purpose: they say a source is not yours to index, so an ignored source is not cataloged at all, and one that was indexed earlier is removed on the next sweep ([ignore.md](ignore.md)). Lifting a rule readmits it as new.

## Change detection

The only thing the sweep needs from outside is a listing of what exists — it resolves the host's connection from the registry at the start of every run, so mounting or unmounting a connection never restarts the sweep. Host-specific change feeds (FSEvents, Gmail history, …) are a declared connection capability (`change_feed`); the scheduling hook that turns their hints into targeted sweeps is not built yet — events are hints; the sweep is the truth.
