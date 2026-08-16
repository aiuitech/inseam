# Index Maintenance

How an index stays true to its sources and its profile after the first build. One mechanism does all of it: the **reconciling sweep**.

## The sweep is the only mechanism

An index run reconciles the catalog against two authorities: **reality** (what the connection enumerates) and **the composition** (what the index is supposed to look like). This is the same converge-never-migrate stance the [kernel](kernel.md) takes toward the plugin tree, applied to index data. Everything that maintains the index — first build, file edits, deletions, profile changes, interrupted runs — is the same sweep noticing a discrepancy and converging on it. There is no second maintenance system, no migration scripts, no event log to replay.

A source is **dirty** when any of these hold:

1. **Content changed** — `modified` timestamp or byte size differ from the catalog.
2. **Interrupted** — a prior run died before `indexed` was marked (set only after the full subtree is stored).
3. **Shape-stale** — the source's recorded **shape stamp** doesn't match the current composition's shape tier (below). Catalog-only rows carry no stamp, so they automatically qualify for deep indexing whenever budget or cutoff later allows.
4. **Vanished** — enumeration no longer sees it: the fragment subtree and catalog row are deleted. The index is derived and rebuildable, so there are no tombstones; a file that reappears is simply new.

After the per-source work, the sweep garbage-collects **entity fragments with no remaining relations** — the natural consequence of source deletions, rebuilds, and `entities.enabled = false`, all converging without special cases.

Dirtiness is discovered; nothing is triggered by the profile changing or a file being written. This is what makes maintenance idempotent, interrupt-safe, and meterable: the per-run budgets (`max_sources`, LLM call budgets) bound how much convergence any one sweep performs, and repeated sweeps finish the job.

## Change detection belongs to the connection

"How does the index update itself when files change" has no core answer, because change *detection* is host-specific: FSEvents on macOS, inotify on Linux, the Gmail history API, a Slack events socket. Per [connections](connections.md), those live in connection plugins — a **change feed** is a connection capability alongside enumeration.

The division of labor:

- The **core** owns the sweep. Its only external need is enumeration.
- A **connection with a change feed** turns full sweeps into targeted ones: an event marks paths dirty and schedules a sweep scoped to them. Events are **hints, never authoritative** — feeds drop events, watchers miss writes during downtime. A periodic full sweep heals everything a feed missed.
- A connection without a feed (plain filesystem today) just gets swept on demand or on a schedule.

Platform clients fit the same shape: a macOS app bundling a node is a UI over the [node API](node-api.md), and the FSEvents watching belongs to that node's filesystem connection — not to the UI layer. (Repo-wise this eventually means a Cargo workspace — core crate, CLI, platform clients — but the current lib + bin split already draws the boundary; the workspace conversion waits for the first real second crate.)

## Composition changes invalidate only what they touch

Config edits partition into four tiers by blast radius, and the tier is a property of *which entry's config changed* in the [composition](composition.md):

| Tier | Entries | Invalidates |
| --- | --- | --- |
| **Query-time** | finder config, agent model, endpoint | Nothing — read at query/call time |
| **Run-metering** | sweep budgets (`max_sources`, per-transform LLM call budgets) | Nothing — they bound how much work a *run* does, not what output looks like |
| **Shape** | transform entries: which are mounted and their configs (transform model, summary lengths, entity caps, decomposition budgets) | The fragment subtrees built under the old shape, source by source |
| **Embedding** | embedder entry (provider, model, dimensions) | Vectors only — the graph is untouched |

**Shape** is captured per source when its subtree lands, and it is **claims-aware** — a global "all mounted transforms" digest would make plugin churn ruinously expensive (mounting a video transform must not re-run paid LLM summaries over every markdown note). Two records per source:

- the **shape stamp**: a canonical digest of the transform entries that *participated* in the subtree — entry config, artifact version + content hash for sandboxed transforms, the transform model for LLM-hungry transforms, and the sweep's own decomposition dials (depth, fragment, and content-byte caps). Removing, reconfiguring, or upgrading a transform dirties exactly the sources it touched.
- the **mimetype inventory**: every mimetype present in the subtree, root and emitted. A newly mounted transform dirties only sources whose inventory intersects its claims.

A stamp or inventory hit makes the source dirty; the sweep rebuilds its subtree like any content change, with the full mounted transform set, so chains resolve in one rebuild (a transform claiming what another new transform emits is satisfied in the same pass; the rebuilt inventory catches anything discovered later on the next sweep). A composition edit is therefore never a big-bang re-index — it makes the affected sources *look* dirty, and the run budgets meter the convergence across as many sweeps as it takes. Plugin churn is absorbed through the same door, with no plugin-lifecycle hooks into the index: the reconciler restarts fibers, their registrations unwind or appear as effects, and the next sweep discovers the divergence — dirtiness stays discovered, never triggered. Granularity is deliberately the source subtree, not the individual transform: finer invalidation would save some LLM calls but means mixing fragments built under different shapes inside one source, which is where inconsistency bugs live.

**Embedding** changes trigger an **in-place re-embed**, not a rebuild: fragment text is all in SQLite, so the Lance table is recreated and re-populated by re-embedding stored text — zero LLM spend, no transforms re-run, no graph changes. Detected at open (the store records the model + dimensions it was built with), performed by the next index run, and search refuses with instructions until that run completes. Interrupted re-embeds redo: the stored embedding meta is updated only at the end.

## Scope shrinkage never evicts

Tightening `cutoff.modified_after` or `budget.max_content_bytes` stops *future* work; it never deletes what a looser profile already built. Derived fragments cost real LLM dollars, and turning a dial should not destroy paid-for understanding — tighten-then-loosen must be free. Storage reclamation is an explicit owner operation (a future `vacuum`), never a sweep side effect.

Out-of-cutoff sources are still *cataloged* (address + envelope, no fragments) when first seen, so the catalog stays a complete map of the host; already-indexed sources that fall behind a tightened cutoff are left entirely untouched.

## Paths not taken

- **A file watcher in the core.** Change detection is host-specific; a core watcher would be the filesystem special case smuggled into the one place that must stay host-agnostic. Watchers are connection capabilities.
- **Treating feed events as authoritative.** An event-driven index diverges the moment one event is dropped. Events only *schedule* sweeps; the sweep re-derives truth.
- **Per-transform invalidation.** Rejected for now: subtree granularity keeps the delete-cascade model and never mixes shapes within a source.
- **Eviction on scope shrinkage.** Rejected — see above; deletion of valid, paid-for index data must be explicit.
- **A composition-version counter instead of a stamp.** A counter invalidates on *any* composition edit, including query-time tiers; the stamp invalidates only on entries that change what a subtree looks like.

## Open questions

- Whether a sandboxed transform's version bump always invalidates (artifact version is in the stamp) or its manifest may declare a release shape-compatible ("output unchanged, bug fix only") to skip the re-spend — and whether trusting that claim is acceptable, since the cost of a lie is staleness, not compromise.

- Targeted sweeps: the scoping API a change feed uses to sweep a subset without paying full enumeration (v1 sweeps the directory it is given).
- `vacuum`: the explicit reclamation operation (drop out-of-scope subtrees, compact Lance).
- Sweep scheduling defaults for feedless connections (cron-style timer in `inseam serve`?).
