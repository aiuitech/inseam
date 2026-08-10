# Index Maintenance

Every `inseam index <dir>` run is a **reconciling sweep** ([design/index-maintenance.md](../../design/index-maintenance.md)): it converges the index on reality (what enumeration sees) and on the profile (what the index should look like). There is no separate maintenance command.

## What one sweep does, in order

1. **Re-embed, if pending** — an embedding model/dimensions change detected at open re-populates the Lance table from SQLite text under the new config. No transforms re-run, no LLM spend; search refuses with instructions until this completes. Interrupted migrations redo (the stored embedding meta updates only at the end).
2. **Per-source reconciliation** — a source is re-indexed (subtree deleted and rebuilt) when its content changed (`modified` + byte size), its prior run was interrupted (`indexed = 0`), or its **profile stamp** doesn't match the current profile's shape tier. Catalog-only rows (over `max_sources` budget, or first seen past the cutoff) carry no stamp, so they are picked up automatically once budget or horizon allows.
3. **Vanished-source removal** — cataloged sources under the swept directory that enumeration no longer sees are deleted outright (fragments, relations, search rows, catalog row). No tombstones; a reappearing file is simply new.
4. **Entity GC** — entity fragments with no remaining relations are dropped (registry rows cascade). This is how deletions, rebuilds, and `entities.enabled = false` all converge.

The report prints a `maintenance:` line (sources removed, entities collected, rows re-embedded) whenever any of it happened.

## Profile tiers

Profile fields invalidate only what they touch ([profiles.md](profiles.md)):

- **Query-time** (`[finder]`, `agent_model`, `[endpoint]`) — never re-indexes.
- **Run-metering** (`max_sources`, `llm_call_budget`s) — never re-indexes; bounds each run, so a big invalidation migrates across as many sweeps as the budgets allow.
- **Shape** (`transform_model`, `summary.target_chars`, `entities.enabled`/`max_per_source`, `max_depth`, `max_fragments_per_source`, `max_content_bytes`) — stamps subtrees; a change re-indexes each source on its next sweep.
- **Embedding** (`provider`/`model`/`dimensions`) — triggers the in-place re-embed only.

## Scope shrinkage never evicts

Tightening `cutoff.modified_after` or `max_content_bytes` stops future work but deletes nothing already built — tighten-then-loosen is free. Out-of-cutoff sources are still cataloged (address + envelope only) when first seen.

## Change detection

The sweep's only external need is enumeration; anything that wants the index fresher just runs sweeps more often or more narrowly. Host-specific change feeds (FSEvents, Gmail history, …) arrive as connection-plugin capabilities that schedule targeted sweeps — events are hints, the sweep is the truth ([design/index-maintenance.md](../../design/index-maintenance.md)).
