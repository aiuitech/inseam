# The Finder Algorithm

Implements [design/finder.md](../../design/finder.md): start from hybrid search hits, then let the graph boost what search alone would underrank. All the constants live in the `finder` entry's config ([../configuration.md](../configuration.md)) — it's a query-time setting, so tuning them never re-indexes anything.

## 1. Seed: five lists, merged by rank

The query runs against five seed lists. Three are the hybrid search: prose full-text and lexical full-text and the vector neighbours. Two ground the question in the vocabulary ([../indexing/vocabulary.md](../indexing/vocabulary.md)): **exact** grounding, every n-gram of the question (up to four tokens) that spells a vocabulary row, longest phrases first and rarer rows first; and **cluster** grounding, the rows and aliases of the clusters whose vector clears `cluster_query_cosine` with the query's vector, at most `clusters_per_query_max` of them — the offline bridge from a paraphrased question to the corpus's word. Every list has its own dials in `[finder.seed_lists.<prose|lexical|vector|exact|cluster>]`: `enabled`, `weight` (its vote in fusion), and `ranks_max` (a rank gate: only that many of its best enter fusion). The vocabulary lists default to the rank-gated, down-weighted shape measured best for cue vectors — exact at weight 1 over 20 ranks, cluster at weight 0.3 over 5 — so their job is to add candidates the text lists lack, never to reorder what full-text carries.

The text lists run as before: prose full-text and lexical full-text (each BM25 over the query's words joined by OR, function words dropped, over its own table so a one-line term and a whole document are never scored by shared length statistics — on a large index "the" matches nearly every row and the ranker would score them all for rows that rank last anyway; a query made only of function words is searched whole) and vector nearest-k by cosine distance — the latter after dropping hits beyond `max_vector_distance`, because nearest-k always returns *something*, however unrelated. `seeds` can run one list alone (`full-text` or `vector`) to see which search the fusion is carrying, or because the node's embedder is not worth asking; it is query-time and never re-indexes. The two best-first lists merge by **reciprocal rank fusion**:

```
seed(f) = Σ over lists  weight(list) / (rrf_k + rank_f)      for rank_f ≤ ranks_max(list)
```

Every request may override any finder dial for itself alone with `--finder key=value` (`seed_lists.cluster.weight=0`, `hub_degree_max=200`, `graph_hops=4`); an unknown key is an `Invalid` error, and nothing is stored.

The prose and vector lists have weight 1. The lexical list uses `lexical_weight`, default 1, bounded to (0, 1]. Lower values keep names searchable while reducing their influence when a corpus has many container entries. A name-only query still receives lexical candidates and graph propagation. This is a query-time setting; the index and navigation references remain intact.

Prose and vector lists have weight 1. The lexical list uses `lexical_weight`, default 1, in the range (0, 1]. Lowering it keeps names searchable while reducing their contribution beside document prose. It changes ranking only and preserves all indexed navigation evidence.

Merging by rank instead of score sidesteps the fact that BM25 scores and cosine distances aren't comparable numbers — and it will merge the same way across nodes when multi-node search arrives.

## 2. Boost: spread relevance along the graph

The seed scores, normalized into a distribution, become the restart vector for personalized PageRank over a seed-local slice of the relation graph. An edge conducts in both directions, and each direction is weighted by the relation kind (`[finder.weights.by_kind]` plus `default`) times the **row kind** of the fragment mass flows into (`[finder.weights.by_row_kind]`: identifier 1.0, entity 0.8, term 0.6, facet 0.5, alias 0.4, prose, summary and entry 1.0), because a shared ticket number says more than a shared jargon word; parallel edges sum. The default slice is two hops and at most 20,000 relations (`graph_hops`, `graph_relation_limit`), so retrieval work does not grow with every unrelated edge in the catalog. The slice is loaded under the **hub bound**: a vertex with more than `hub_degree_max` relations (500 by default; 0 bounds nothing) — a corpus-wide term, a folder of five thousand entries, the host facet — is never expanded and every edge touching it is dropped, so it neither floods the walk nor spends the relation limit. Hub protection is a bound, not a damping, because every damping of hub terms measured still lost twenty points of recall. The hubs kept out are listed in `meta.hubs_excluded`.

```
p ← (1 − damping) · seed  +  damping · (Wᵀ p + dangling · seed)
```

The spread is kept local: `damping = 0.5` and at most `iterations` rounds (with an ε early exit) mean the boost stays near the seeds instead of drifting toward globally central fragments. Entity fragments are the highways: an entity the query hits carries relevance to every fragment that mentions it.

## 3. Score: boost, never gate

```
final(f) = seed(f) + p(f)
```

Because the boost is *added*, a fragment with no useful relations keeps its search score (property-tested: every seed keeps at least `(1 − damping)` of its normalized mass).

## 4. Rollup: fragments → sources

Fragments group by source (keyed fragments such as entities, having none, carry relevance but never rank). A source scores its best fragment plus a tapering bonus for additional hits — using only the max would ignore independent hits; summing everything would favor long documents:

```
source = f1 + 0.1·f2 + 0.05·f3
```

Results are normalized so the top score is `1.0`, and each carries the envelope, the mandatory summary, and up to `max_hints` fragment hints (text preview + extent) so a client knows where to `scan` next.

## 5. Filter and explain

A request's filters — `host`, `source_type`, `facets` (every named facet or entity value must be anchored to the source's root), `modified_after`, `modified_before` — remove candidate sources before the rollup; `meta.filtered_sources` counts them.

Under `explain` every result's evidence carries a **ledger**, the exact decomposition of its raw score: fusion is a sum over channels and the walk is linear in its restart vector, so the walk runs with one restart column per channel and the columns sum to the walk a single fused restart would produce. The ledger lists, per channel, the seed mass and the walk mass that channel induced (they sum to `score_raw`); the walk mass that arrived through neighbours of each row kind on the walk's last iteration; and the neighbours that carried the most mass into the source's scoring fragments, named, with document frequency and cluster for vocabulary rows. `inseam query --explain` prints it under the results; the benchmark harness records it for every gold document ([../benchmarking.md](../benchmarking.md)).

## 6. Merge: collapse by content digest

Results whose envelopes carry equal content digests ([design/addressing.md](../../design/addressing.md)) are the same bytes living in two places — a local file and its cloud twin. They collapse into one result: the best-scoring copy supplies the score, summary, and hints, and the other copies ride along as `replicas` so a caller can pick where to fetch. Results without a digest never collapse — best-effort dedup degrades to duplication, never a wrong merge.
