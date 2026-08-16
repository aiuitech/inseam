# Index Maintenance

Every `inseam index <dir>` run is a **reconciling sweep** ([design/index-maintenance.md](../../design/index-maintenance.md)): it converges the index on reality (what the connection enumerates) and on the composition (what the mounted transforms would build). There is no separate maintenance command and no plugin-lifecycle hook into the index — dirtiness is discovered by the sweep, never triggered.

## What one sweep does, in order

1. **Re-embed, if pending** — a changed embedding identity (declared by the mounted embedder) re-populates the Lance table from SQLite text. No transforms re-run, no LLM spend; search refuses with instructions until this completes. Interrupted migrations redo.
2. **Per-source reconciliation** — a source re-indexes (subtree deleted and rebuilt with the full mounted transform set) when its content changed (`modified` + byte size), its prior run was interrupted, or it is **shape-stale** (below). Catalog-only rows (over `max_sources`, or first seen past the cutoff) carry no stamp, so they are picked up automatically once budget or horizon allows.
3. **Vanished-source removal** — cataloged sources under the swept scope that enumeration no longer sees are deleted outright. No tombstones; a reappearing file is simply new.
4. **Entity GC** — entity fragments with no remaining relations are dropped.

## Claims-aware shape staleness

Each deep-indexed source stores two records ([storage.md](storage.md)):

- the **shape stamp** — a digest of the transform registrations that *participated* in its subtree (entry id + config fingerprint, plus artifact version/hash for sandboxed transforms, plus the sweep's own decomposition dials);
- the **mimetype inventory** — every mimetype in the subtree, root and emitted.

On a later sweep the expected stamp is recomputed from the *current* registrations intersected with the stored inventory. So: removing, reconfiguring, or upgrading a transform dirties exactly the sources it touched; mounting a new transform dirties only sources whose inventory intersects its claims — installing a video plugin never re-runs paid LLM summaries over markdown notes. Rebuilds run with the full mounted set, so chained transforms (one claiming what another emits) resolve in one pass.

## Composition tiers

Which entry's config changed decides the blast radius ([configuration.md](../configuration.md)):

- **Query-time** (`finder`, the llm entry's models) — never re-indexes.
- **Run-metering** (`sweep.max_sources`, per-transform `llm_call_budget`) — never re-indexes; bounds each run, so a big invalidation migrates across as many sweeps as budgets allow.
- **Shape** (transform entry configs, transform mounts/unmounts, `sweep`'s `max_depth`/`max_fragments_per_source`/`max_content_bytes`, the llm `transform_model` for LLM-hungry transforms) — stamps diverge; affected sources re-index on their next sweep.
- **Embedding** (the `embedder` entry) — the in-place re-embed only.

## Scope shrinkage never evicts

Tightening `sweep.modified_after` or `max_content_bytes` stops future work but deletes nothing already built — tighten-then-loosen is free. Out-of-cutoff sources are still cataloged (address + envelope only) when first seen.

## Change detection

The sweep's only external need is enumeration. Host-specific change feeds (FSEvents, Gmail history, …) arrive as connection-plugin capabilities that schedule targeted sweeps — events are hints, the sweep is the truth.
