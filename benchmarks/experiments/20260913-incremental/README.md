# Incremental indexing experiment, 13 September 2026

The same node indexed the pinned 25,000-document EnterpriseRAG slice and its 275 folders, then changed only folder-summary or Finder settings. Every command and its exact composition is recorded in [results.json](results.json). This is a reuse experiment, not a fresh-index benchmark comparison.

| Step | Sources rebuilt | Sources unchanged | Seconds |
| --- | ---: | ---: | ---: |
| initial | 25,275 | 0 | 48.511 |
| folder400 | 275 | 25,000 | 14.009 |
| folder200 | 275 | 25,000 | 8.537 |
| finder_only | 0 | 25,275 | 4.557 |
| restore_folder | 275 | 25,000 | 7.557 |

All steps used zero model calls and no embedder, matching the retrieval profile. The folder changes avoided rewriting 98.9% of sources. The no-change sweep still enumerates the corpus; `benchmarks/requery.py` avoids that sweep entirely for query-only tuning.

Wall times include contention from other benchmarks and compilation, so the source reuse counts are stronger evidence than a claimed speedup ratio. The SQLite file remained 288,403,521 bytes including the node identity file: deleting or replacing summary rows can free internal pages without shrinking the file. No compaction was forced.

A public API integration test separately verifies folder-only invalidation, no-change convergence, and restoration of the default target. Shape/cache tests verify that ordinary file identities stay unchanged and the assigned model remains an identity dependency.

To reproduce, use a new empty node directory and execute the commands in `results.json` in order, replacing only that node path. Reusing the experiment's existing node would make its initial step incremental too.

The final public [`inseam status` output](status.txt) confirms the restored index still contains 25,275 sources, 70,042 fragments, 44,767 relations, and 44,767 search rows.
