# Retrieval tuning, 13 September 2026

BEIR NFCorpus and EnterpriseRAG were evaluated on the current Finder and indexing implementation, with the directory transform explicitly enabled. Enterprise runs stop after retrieval. No GPT-5.4 agent or judge phase has run.

The source revision was `b414e6e7aa0f` with the changes committed alongside this report. Each manifest records its actual binary hash and dirty source state. Saved baseline binaries and the updated binary are distinguished by those hashes.

| Full EnterpriseRAG, 511,962 documents, 500 questions | Recall@8 | Hit@8 | MRR@8 | Folder slots / 4,000 |
| --- | ---: | ---: | ---: | ---: |
| [lexical weight 1](../runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/manifest.json) | 63.37% | 70.43% | 0.432677 | 1170 |
| [lexical weight 0.1](../runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/retrieval/20260913T184155Z-b414e6e7aa0f/manifest.json) | 66.42% | 72.55% | 0.596429 | 13 |
| [lexical weight 0.25](../runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/retrieval/20260913T184918Z-b414e6e7aa0f/manifest.json) | 66.42% | 72.55% | 0.596429 | 20 |

Recall, hit rate, and MRR average over the 470 questions with expected documents. The other 30 questions remain in the 500-query workload and observability data. Each full replay uses the baseline index without indexing or repair, preserving all folder summaries, names, keywords, child-address links, and graph structure.

| BEIR, same index, 323 queries | RRF k | Lexical weight | nDCG@10 | Recall@10 | MAP@10 |
| --- | ---: | ---: | ---: | ---: | ---: |
| [baseline](../runs/beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/manifest.json) | 60 | 1.0 | 0.36885 | 0.18205 | 0.14262 |
| [20260913T183030Z](../runs/beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/retrieval/20260913T183030Z-b414e6e7aa0f/manifest.json) | 10 | 1.0 | 0.37926 | 0.18708 | 0.14539 |
| [20260913T183327Z](../runs/beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/retrieval/20260913T183327Z-b414e6e7aa0f/manifest.json) | 5 | 1.0 | 0.37927 | 0.18748 | 0.14517 |
| [20260913T183719Z](../runs/beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/retrieval/20260913T183719Z-b414e6e7aa0f/manifest.json) | 10 | 0.1 | 0.37917 | 0.18720 | 0.14487 |
| [20260913T184015Z](../runs/beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/retrieval/20260913T184015Z-b414e6e7aa0f/manifest.json) | 5 | 1.0 | 0.37784 | 0.18626 | 0.14560 |
| [20260913T184212Z](../runs/beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/retrieval/20260913T184212Z-b414e6e7aa0f/manifest.json) | 10 | 1.0 | 0.38311 | 0.18818 | 0.14749 |
| [20260913T184445Z](../runs/beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/retrieval/20260913T184445Z-b414e6e7aa0f/manifest.json) | 60 | 1.0 | 0.36833 | 0.18176 | 0.14195 |

These BEIR runs retain 384-dimensional embeddings of whole abstracts, a 12,000-character summary target, and extractive folder summaries with a zero summary-call budget. Folder results occupied zero top-ten slots. The same-index plateau at RRF 5–10 is more useful than treating the fifth decimal as a stable optimum. Independent fresh zero-budget runs also improved nDCG@10 from 0.36885 at RRF 60 to 0.37943 at RRF 10.

| Exploratory fresh run | Scope | Main controls | Primary score | Index MB after queries | Index seconds |
| --- | --- | --- | ---: | ---: | ---: |
| [20260913T180734Z](../runs/beir-nfcorpus/20260913T180734Z-b414e6e7aa0f/manifest.json) | BEIR | RRF 60, dims 384, budget 1 | 0.36918 | 40.358 | 52.800 |
| [20260913T180931Z](../runs/beir-nfcorpus/20260913T180931Z-b414e6e7aa0f/manifest.json) | BEIR | RRF 10, dims 384, budget 1 | 0.37826 | 40.362 | 26.225 |
| [20260913T181419Z](../runs/beir-nfcorpus/20260913T181419Z-b414e6e7aa0f/manifest.json) | BEIR | RRF 10, dims 192, budget 1 | 0.35222 | 29.413 | 24.048 |
| [20260913T181420Z](../runs/beir-nfcorpus/20260913T181420Z-b414e6e7aa0f/manifest.json) | BEIR | RRF 20, dims 384, budget 1 | 0.37463 | 40.366 | 33.494 |
| [20260913T182211Z](../runs/beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/manifest.json) | BEIR | RRF 60, dims 384, budget 0 | 0.36885 | 40.354 | 9.092 |
| [20260913T182212Z](../runs/beir-nfcorpus/20260913T182212Z-b414e6e7aa0f/manifest.json) | BEIR | RRF 10, dims 384, budget 0 | 0.37943 | 40.354 | 9.775 |
| [20260913T180627Z](../runs/enterprise-rag-bench/20260913T180627Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 60, seeds 60, damping 0.5, lexical 1, folders 24000, keywords 12 | 79.83000 | 288.391 | 17.854 |
| [20260913T180735Z](../runs/enterprise-rag-bench/20260913T180735Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 60, seeds 60, damping 0.5, lexical 1, folders 24000, keywords 0 | 79.78000 | 288.453 | 20.047 |
| [20260913T180810Z](../runs/enterprise-rag-bench/20260913T180810Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 60, seeds 60, damping 0.0, lexical 1, folders 24000, keywords 0 | 74.45000 | 288.461 | 17.440 |
| [20260913T181000Z](../runs/enterprise-rag-bench/20260913T181000Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 1, seeds 60, damping 0.5, lexical 1, folders 24000, keywords 12 | 78.47000 | 288.387 | 26.275 |
| [20260913T181001Z](../runs/enterprise-rag-bench/20260913T181001Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 10, seeds 20, damping 0.5, lexical 1, folders 24000, keywords 12 | 78.68000 | 288.391 | 25.232 |
| [20260913T181417Z](../runs/enterprise-rag-bench/20260913T181417Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 60, seeds 60, damping 0.5, lexical 1, folders 400, keywords 12 | 81.01000 | 278.811 | 18.681 |
| [20260913T181418Z](../runs/enterprise-rag-bench/20260913T181418Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 10, seeds 60, damping 0.5, lexical 1, folders 400, keywords 12 | 78.86000 | 278.819 | 18.030 |
| [20260913T181642Z](../runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/manifest.json) | full | RRF 60, seeds 60, damping 0.5, lexical 1, folders 24000, keywords 12 | 63.37000 | 5225.341 | 617.658 |
| [20260913T181901Z](../runs/enterprise-rag-bench/20260913T181901Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 60, seeds 60, damping 0.5, lexical 0.1, folders 400, keywords 12 | 84.17000 | 278.807 | 51.245 |
| [20260913T181902Z](../runs/enterprise-rag-bench/20260913T181902Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 60, seeds 60, damping 0.5, lexical 0.25, folders 400, keywords 12 | 84.28000 | 278.807 | 51.241 |
| [20260913T181947Z](../runs/enterprise-rag-bench/20260913T181947Z-b414e6e7aa0f/manifest.json) | 25k slice | RRF 60, seeds 60, damping 0.5, lexical 0.1, folders 24000, keywords 12 | 84.16000 | 288.387 | 29.636 |

Primary score is BEIR nDCG@10 or Enterprise mean document recall in percent. Slice and full-corpus scores are separate comparisons. The full indexing time is the resumed attempt, not a fresh uninterrupted measurement.

The selected full replay improved recall on 33 questions, regressed on 1, and tied on 436. It gained first hits on 11 questions and lost them on 1; 129 still returned no expected document. Per-query deltas are in [the audit data](20260913-retrieval-audit.json).

| Full-corpus question type | Questions with gold documents | Baseline recall | Selected recall |
| --- | ---: | ---: | ---: |
| basic | 175 | 76.57% | 77.14% |
| completeness | 20 | 36.89% | 46.97% |
| conflicting_info | 20 | 77.50% | 77.50% |
| constrained | 30 | 80.00% | 81.67% |
| intra_document_reasoning | 40 | 87.50% | 87.50% |
| miscellaneous | 20 | 85.00% | 85.00% |
| project_related | 40 | 49.90% | 59.46% |
| semantic | 125 | 36.00% | 41.60% |


Lexical weights 0.1 and 0.25 tied on all full-corpus retrieval metrics. Prefer 0.25 for the next navigation phase because it preserves a stronger lexical contribution at the same measured retrieval score and index size. The equal-weight baseline remains available for comparison. The selected composition is [the 0.25 replay configuration](../runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/retrieval/20260913T184918Z-b414e6e7aa0f/composition.toml).

The main retrieval change is `finder.lexical_weight`. It reduces the influence of lexical matches before graph propagation while retaining every lexical candidate, name, identifier, and directory entry. Its default remains 1.0. The selected benchmark compositions are retrieval candidates for the navigation phase; the agent evaluation should determine any product-default change.

On the 25k slice, a keyword cap of 12 produced 79.83% recall, versus 79.78% with a cap of zero. The cap does not add 12 keywords to every document: verbatim summaries emit no separate keywords, so this comparison affects extractively summarized sources, principally folders in this profile. The small difference alone is weak evidence for a ranking benefit. Removing graph propagation and reducing the seed pool to 20 also reduced recall. Expanding the pool to 120 with lexical weight 0.1 produced 83.92% recall, below the 84.16% result at 60, so the larger pool was rejected. The slice includes every expected document plus a seeded sample of distractors. Its scores do not estimate full-corpus scores.

Shortening folder summaries to 400 characters reduced the slice index from about 288.4 MB to 278.8 MB, a 3.3% saving. The best short-folder slice reached 84.28% recall, only 0.12 percentage points above the long-folder 0.1 profile. We retained long summaries for the full-corpus comparisons because they preserve more material for navigation. The folder-specific setting remains available and incremental. Reducing BEIR vectors to 192 dimensions saved about 27% of index bytes but lowered nDCG@10 to 0.35222, so 384 dimensions were retained.

The selected full Enterprise configuration uses the same 5,225,340,993-byte index as its baseline. The zero-summary-budget BEIR index occupies 40,353,857 bytes. Retrieval-only settings add no index rows. Footprints are measured after query processes close, because an outstanding SQLite WAL can make an intermediate measurement larger. In-place replacement can also free pages without shrinking the database file; the incremental experiment does not claim that unchanged file size means unchanged content.

The [incremental experiment](../experiments/20260913-incremental/README.md) rebuilt only 275 folders while retaining all 25,000 files. Under concurrent machine load, its fresh sweep took 48.51 seconds, the first folder-only sweep 14.01 seconds, a subsequent folder-target change 8.54 seconds, and a Finder-only no-change sweep 4.56 seconds. These are descriptive timings, not isolated speed ratios. Retrieval replay avoids the indexing sweep altogether. The source-level reuse counts are the stronger result. Public status after restoring the folder target confirms 25,275 sources, 70,042 fragments, and 44,767 relations remain.

An early full indexing attempt failed with a SQLite lock while the harness polled `inseam status`. That command also boots a store writer. The harness now uses elapsed-time heartbeats during indexing; the resumed attempt finished, retaining 182,438 already-indexed sources and indexing the remaining 329,823. Its 617.658-second duration excludes the earlier 192.032-second failed attempt and should not be compared with an uninterrupted fresh run. The model-free Enterprise profile made no summary, embedding, answer, or judge calls. BEIR used embedding API calls, and the early budget-one experiments also made a folder-summary call; embedding charges are not included in the index report's summary-spend counter.

The query metadata identifies full-corpus seed retrieval as the main query cost. At baseline, median seed retrieval was 650 ms, graph propagation 51 ms, and source rollup 137 ms. Candidate sources and relation counts remained unchanged in the 0.1 replay, which confirms that weighting changed ranking without deleting candidate evidence. Timing differences across these runs include machine load and cache effects and are not claimed as an algorithmic speedup.

Folder hits now occupy their actual ranks in both scorers. Previous Enterprise MRR calculations removed folders before locating the first expected document and could overstate the score; historical MRR using that calculation is not comparable. A folder whose name contains a document identifier is still unjudged. The current audit recomputed and verified scores from the retained raw rankings.

Repeated BEIR queries on one unchanged index showed ranking variation. RRF 10 produced nDCG@10 values of 0.37926 and 0.38311, versus baseline RRF 60 values of 0.36885 and 0.36833. RRF 5 produced 0.37927 and 0.37784. RRF 10 is the supported choice across fresh and repeated runs; the individual maximum is not a guaranteed score. Two identical RRF-5 replays changed 144 result rankings, with seed/candidate trace counts changing on 15 queries. A follow-up should isolate approximate vector candidate variation and stabilize tied ranking order. All tuning used the benchmark questions, so these gains need a separate workload or the next agent phase to establish generality.

Validation passed: 49 Python benchmark tests; five Finder integration tests; 12 maintenance integration tests; 13 summarizer tests; five seam shape tests; and four transform-cache tests. `cargo fmt`, documentation generation, and `cargo install --path crates/inseam-cli` completed. Clippy passed with warnings denied and `clippy::collapsible_if` explicitly allowed, matching the repository's instruction to split compound conditions; the unmodified strict command reports that lint in existing `inseam-seams/src/extract.rs` code.

The full Enterprise index and the chosen composition are retained for the GPT-5.4 phase. No agent answer generation or judge evaluation has started. That phase must assess whether iterative queries, folder navigation, and evidence reading recover the misses that a single retrieval list leaves behind.
