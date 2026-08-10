# The Finder Algorithm

Implements [design/finder.md](../../design/finder.md): seed with hybrid search, then let the graph boost what search alone would underrank. All constants live in the profile's `[finder]` section ([../index/profiles.md](../index/profiles.md)).

## 1. Seed: hybrid search, fused by rank

The query runs against both Lance surfaces: full-text (BM25) and vector nearest-k under cosine distance — the latter only after dropping hits beyond `max_vector_distance`, because nearest-k always returns *something*, however unrelated. The two best-first lists fuse by **reciprocal rank fusion**:

```
seed(f) = Σ over lists  1 / (rrf_k + rank_f)
```

Rank-based fusion sidesteps BM25 scores and cosine distances living on incomparable scales, and will fuse the same way across nodes when fan-out arrives.

## 2. Boost: personalized PageRank over the relation graph

Seed scores, normalized to a distribution, become the restart vector for power-iterated PPR on the **undirected** relation graph, edge weights by relation kind (`[finder.weights]`), parallel edges summed:

```
p ← (1 − damping) · seed  +  damping · (Wᵀ p + dangling · seed)
```

Bounded propagation: `damping = 0.5` and ≤ `iterations` rounds (ε early-exit) keep the boost local rather than converging to global centrality. Entity fragments are the highways: an entity seeded by the query conducts relevance to every fragment that mentions it.

## 3. Score: boost, never gate

```
final(f) = seed(f) + p(f)
```

Addition guarantees a fragment with no useful relations keeps its seed standing (property-tested: every seed retains at least `(1 − damping)` of its normalized mass).

## 4. Rollup: fragments -> sources

Fragments group by source (entity fragments, having none, conduct but never rank). A source scores its best fragment plus a tapered corroboration bonus — max alone ignores independent hits, sum invites long-document bias:

```
source = f1 + 0.1·f2 + 0.05·f3
```

Results normalize to `score = 1.0` at the top and carry the envelope, the mandatory summary, and up to `max_hints` fragment hints (text preview + extent) so a client knows where to `scan` next.
