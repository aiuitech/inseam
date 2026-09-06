# The Finder Algorithm

Implements [design/finder.md](../../design/finder.md): start from hybrid search hits, then let the graph boost what search alone would underrank. All the constants live in the `finder` entry's config ([../configuration.md](../configuration.md)) — it's a query-time setting, so tuning them never re-indexes anything.

## 1. Seed: two searches, merged by rank

The query runs against both search tables: full-text (BM25) and vector nearest-k by cosine distance — the latter after dropping hits beyond `max_vector_distance`, because nearest-k always returns *something*, however unrelated. `seeds` can run one list alone (`full-text` or `vector`) to see which search the fusion is carrying, or because the node's embedder is not worth asking; it is query-time and never re-indexes. The two best-first lists merge by **reciprocal rank fusion**:

```
seed(f) = Σ over lists  1 / (rrf_k + rank_f)
```

Merging by rank instead of score sidesteps the fact that BM25 scores and cosine distances aren't comparable numbers — and it will merge the same way across nodes when multi-node search arrives.

## 2. Boost: spread relevance along the graph

The seed scores, normalized into a distribution, become the restart vector for personalized PageRank over a seed-local slice of the **undirected** relation graph, with edge weights by relation kind name (`[finder.weights]`: `by_kind` plus a `default` for kinds not listed) and parallel edges summed. The default slice is two hops and at most 20,000 relations (`graph_hops`, `graph_relation_limit`), so retrieval work does not grow with every unrelated edge in the catalog:

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

## 5. Merge: collapse by content digest

Results whose envelopes carry equal content digests ([design/addressing.md](../../design/addressing.md)) are the same bytes living in two places — a local file and its cloud twin. They collapse into one result: the best-scoring copy supplies the score, summary, and hints, and the other copies ride along as `replicas` so a caller can pick where to fetch. Results without a digest never collapse — best-effort dedup degrades to duplication, never a wrong merge.
