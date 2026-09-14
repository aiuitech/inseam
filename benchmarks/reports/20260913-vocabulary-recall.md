# Vocabulary-enriched recall, 13 September 2026

## Question and method

Does the current vocabulary-enriched index and Finder improve initial document recall over the earlier tuned retrieval runs? This experiment runs retrieval only, without agent answers or an LLM judge. Cluster grounding during indexing still uses the configured model.

Source revision: `a6565cec91e7`. The installed binary was rebuilt with `cargo install --path crates/inseam-cli` before either run. Existing unrelated edits to `AGENTS.md` and `_ignore_SCRATCH.md` make the source tree dirty; neither changes the executable. Both benchmark manifests record the binary hash.

BEIR means NFCorpus here, not the entire BEIR suite. It includes all 3,633 abstracts and 323 test queries, with Recall@10. EnterpriseRAG includes all 511,962 documents and all 500 questions, with Recall@8 averaged over the 470 questions with expected documents. Folder hits retain their rank slots.

Both indexes are fresh. The vocabulary pass uses the implementation defaults at the recorded source revision, including a 500-call cluster grounding budget. Summary calls are disabled. BEIR retains whole-abstract embeddings at 384 dimensions, RRF 10, and lexical weight 1. EnterpriseRAG retains full-text-only retrieval, RRF 60, and lexical weight 0.1. Its existing no-embedder profile cannot exercise vector-based cluster grounding; lexical aliases, exact grounding, and vocabulary edges remain available.

The comparison measures the combined algorithm and index changes. It does not isolate the vocabulary contribution from other Finder changes, and model-generated aliases plus approximate vector retrieval can introduce run variation.

## Commands

```sh
python3 benchmarks/beir.py run --limit 323 --query-limit 10 --llm-call-budget 0 --summary-target-chars 12000 --summarization-lane interactive --keywords-max 0 --finder-rrf-k 10 --finder-lexical-weight 1
python3 benchmarks/enterprise_rag_bench.py run --skip-agent --limit 500 --query-limit 8 --llm-call-budget 0 --summary-target-chars 24000 --finder-lexical-weight 0.1
```

## Evidence

- BEIR baseline: `beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/retrieval/20260913T183030Z-b414e6e7aa0f`, Recall@10 18.708%.
- EnterpriseRAG baseline: `enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/retrieval/20260913T184155Z-b414e6e7aa0f`, Recall@8 66.42%.
- New BEIR run: `beir-nfcorpus/20260913T235438Z-a6565cec91e7`.
- New EnterpriseRAG run: `enterprise-rag-bench/20260913T235446Z-a6565cec91e7`.

BEIR enrichment is complete and shows no meaningful recall improvement. The owner abandoned EnterpriseRAG enrichment after eight hours because indexing was taking too long. The index process was terminated and the harness exited; no enriched EnterpriseRAG score exists. No restart is scheduled.

## Current Finder on the old indexes

These additional retrieval-only controls reuse the preserved baseline indexes without indexing or repair. They help separate query-algorithm changes from vocabulary-index changes. Concurrent indexing means their timings do not isolate performance.

The completed BEIR control `beir-nfcorpus/20260913T182211Z-b414e6e7aa0f/retrieval/20260914T014552Z-a6565cec91e7` scored 18.767% Recall@10 over all 323 queries, versus 18.708% in the selected historical replay. The 0.059 percentage-point increase is within the earlier same-index variation, which included 18.818% at RRF 10. This control does not establish a recall improvement from the current Finder alone.

```sh
python3 benchmarks/requery.py beir 20260913T182211Z-b414e6e7aa0f --finder-rrf-k 10 --finder-lexical-weight 1
python3 benchmarks/requery.py enterprise 20260913T181642Z-b414e6e7aa0f --finder-lexical-weight 0.1
```

The completed EnterpriseRAG control `enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/retrieval/20260914T014553Z-a6565cec91e7` scored 66.42% Recall@8 over the 470 questions with expected documents, exactly matching the selected historical replay. All 500 questions were queried. All 470 per-question recall values are identical to the historical control. No recall improvement is demonstrated on the old full index.

## Indexing observations

The full EnterpriseRAG document load finished in about six minutes. Vocabulary anchoring then traversed the content fragments, with sampled frontiers around 67% at 57 minutes, 80% at 67 minutes, 88% at 75 minutes, and over 99% at 86 minutes. These are approximate frontiers from the highest source-fragment anchor for the sampled vocabulary spelling `occurs`, not exact phase timings. These sampled observations do not provide exact phase timings because the run was interrupted.

After term anchoring, the pass spent over two hours attaching the host facet. A later read-only count found 445,000 host-facet anchors out of roughly 512,000 sources. The earlier inference that it had reached document-frequency recounting was incorrect. A one-second macOS process sample at about 137 minutes showed the active database stack dominated by SQLite page reads through `pread`. The catalog was about 7 GiB on a 16 GiB machine. The observation identifies a database I/O bottleneck during facet attachment; it does not benchmark an alternative implementation. The subsequent frequency recount uses a correlated distinct-source count, but the sample does not attribute the observed delay to that query.

BEIR grounding is sequential, with the implementation's 500-cluster call budget. At 140 minutes, 401 distinct clusters had rows with glosses. A glossed-cluster count is a progress proxy, not the exact count of successful calls. The earlier few-second baseline indexing times did not include this vocabulary pass.

Disk headroom was limited at launch. Removing only the regenerable `target/debug/incremental` compiler cache eventually restored about 5.4 GiB. No fixture, baseline index, or benchmark evidence was removed. Concurrent indexing and replay, operating-system caching, and shared machine load prevent isolated runtime comparisons.

## Completed vocabulary-enriched BEIR result

Recall@10 is 18.764% over all 323 queries. Against the historical 18.708% control, recall improved on seven queries, regressed on eight, and tied on 308. The mean gain is 0.056 percentage points. Against the current Finder on the old index, 18.767%, recall improved on five queries, regressed on nine, and tied on 309, a 0.003-point decrease. Neither comparison demonstrates a meaningful recall improvement; the differences are within previously observed approximate-retrieval variation.

Indexing took 9,118.744 seconds, with 9,060.191 seconds in grounding and subsequent cluster embedding. The pass made all 500 grounding calls, wrote 11,995 aliases and 4,075 glosses, and merged 266 rows. Only 454 distinct clusters retained rows with glosses, which confirms that the interim glossed-cluster count was not a call counter. The pass mined 30,855 candidates, kept 13,508, and added 158,753 term anchors plus 3,633 host-facet anchors. It founded 4,792 clusters and merged twenty. The log reports 5,266 cluster embeddings, including repeat embeddings after changes.

The settled index is 63,832,129 bytes, versus 40,353,857 bytes in the old index, a 58.2% increase. Reported indexing spend is $0.9327; this counter is not a verified total including embedding charges. The structured extraction is saved as `vocabulary-summary.json` alongside the completed run. The source log remains local under the benchmark artifact policy.

The hub report includes `OF`, `THE`, `IN`, `TO`, and `FOR` with thousands of source anchors. These rows are evidence that the vocabulary contains common language as well as useful corpus terms. The hub bound excludes their edges from the walk. This run does not isolate whether mining, grounding, seeding weights, or hub filtering explains the neutral result, and it does not justify increasing the grounding budget.

The [paired comparison data](20260913-vocabulary-recall.json) recomputes recall from retained rankings and document-ID sets, checks it against completed manifests, and records every per-query delta.

### Host-facet paging diagnosis

A read-only query-plan probe using the local Python SQLite library found that `rooted_sources_of_host` selects through `sqlite_autoindex_sources_1` by host and uses a temporary B-tree for `ORDER BY id`. The SQL asks for `host = ? AND id > ? AND root_fragment IS NOT NULL ORDER BY id LIMIT 1000`; the chosen index does not supply the requested ID order. This is consistent with repeated host scans and sorts across the roughly 512 pages, and the sampled runtime stack's page reads. The process continued attaching facets, reaching 460,000 sources later. No index or query implementation was changed during this benchmark. A future performance experiment should test an ordered host/ID access path and verify identical source coverage before comparing timings.

Host-facet attachment finished at roughly four hours elapsed, with 511,963 anchors. The first cluster appeared at roughly six hours twenty-two minutes, after the subsequent frequency work. These observations separate the long host-facet paging stage from the later statistics stage; the interrupted run has no final aggregate phase timings.

### EnterpriseRAG vocabulary quality before grounding

After frequencies were recounted, the highest-frequency term rows were `for` in 510,284 sources, `to` in 510,080, `in` in 496,375, and `with` in 480,589. The source count was 511,963. The first cluster label was `FOR`. The top twenty rows remain locally as `vocabulary-top-before-grounding.json` in the uncommitted interrupted run directory. This is direct evidence that the planted vocabulary and clustering input contain near-universal function words, despite the intended low-frequency corpus-term band. The hub bound may prevent their graph edges from affecting recall, but it does not avoid their indexing, anchoring, or clustering work. A later experiment should test the relationship between mining-time shape filters, normalization, matching, and the post-anchor document-frequency band. No mining or ranking change was made during this run.

A later drop below 5 GiB free triggered partial `cargo clean --profile dev` cleanup. It was interrupted after free space recovered to about 6 GiB, to reduce disk contention with clustering. Only regenerable debug build artifacts were removed; release artifacts, the installed binary, fixtures, and benchmark indexes were retained. Clustering advanced from 473 to 510 clusters over the following observation interval. This is an operational observation under changing load, not an isolated performance benchmark.

A read-only clustering-frontier probe around eight hours elapsed found 1,473 vocabulary rows at or before the least-frequent persisted cluster member in the pass's frequency-descending order. The pass is capped at 50,000 rows. Founded clusters persist immediately, whereas joins are buffered in batches of 2,000, so persisted membership is only a lower bound on progress. The frontier confirms substantial remaining clustering work; it is not a runtime forecast.

## Abandonment, 14 September 2026

At the owner's request, the EnterpriseRAG index process was terminated after approximately eight hours. The harness recorded failure with exit code -15 at 07:55:29 UTC and exited. The last progress observation recorded 1,682 clusters, 183,044 vocabulary rows, zero glosses, and 511,963 host-facet anchors. Grounding and recall evaluation had not completed. The local interrupted index is preserved, but its run directory is not committed under the completed-run artifact policy. No continuation is scheduled.

The captured log also reports 9,204,536 dropped candidates when the candidate table reached its cap. This is an additional coverage uncertainty, not evidence of a measured recall regression. The completed experiments show no meaningful BEIR gain and no change in EnterpriseRAG recall when the current Finder reuses the old index. Full enriched EnterpriseRAG recall remains unknown. The observed facet paging, frequency work, common-word vocabulary, and clustering cost need investigation before another full indexing attempt. No algorithm fix was attempted during this measurement.
