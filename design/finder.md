# Finder

The **Finder** is inseam's retrieval algorithm: how a query against the [semantic graph](indexing.md) becomes a ranked list of sources. Its shape: **seed with hybrid search, then let the graph boost what search alone would underrank.**

## Seed: hybrid search over all fragments

The query runs against both indexes every fragment participates in:

- full-text search over text fragments
- vector similarity over all embeddings

Each retrieved fragment gets one fused seed score. The working idea is normalizing each side's score to a common scale and combining; the strongest known alternative is **reciprocal rank fusion** (RRF), which fuses by *rank* instead of score and sidesteps the fact that BM25 scores and cosine similarities live on incomparable, corpus-dependent scales. RRF is the working assumption until real relevance data argues otherwise — it also fuses cleanly across *nodes* (see below), where raw scores are even less comparable.

## Boost: relevance flows along relations

Seed scores then propagate through the graph: a fragment related to other retrieved fragments is more relevant than its own score says. Conceptually: I scored 0.2, my neighbor scored 0.1, some fraction of the neighbor's relevance leaks to me and I finish at 0.25.

This is **spreading activation**, and its principled form is **Personalized PageRank** (PPR): random walks restarting at the seed fragments with probability proportional to their seed scores, converging to a score that blends "matched the query" with "well-connected to things that matched." This is the same mechanism HippoRAG uses (PPR seeded from query entities) — validation that graph-boosted retrieval outperforms flat similarity, and prior art to steal parameter choices from. GraphRAG solves a different problem (corpus-level summarization via community detection) and is not this.

Design choices within that frame:

- **Relation kinds carry weights.** A `mentions` edge into a shared entity should conduct relevance differently than a structural `contains` edge or a speculative `links-to`. Kinds are an open vocabulary ([indexing](indexing.md)), so the finder weights them by name with a default for kinds it has never seen — a new plugin's relations conduct on day one and are tunable in the finder's config. Entity fragments are the highways: two sources mentioning the same person become one hop apart.
- **Bounded propagation.** A damping factor and small iteration count (or hop limit) keep the boost local; unbounded spreading converges to global centrality — "important" fragments drowning out *relevant* ones.
- **Bounded materialization.** Propagation loads only the relation neighborhood around fused seeds: two hops and at most 20,000 relations by default, with hard maxima of four hops and 100,000 relations. Loading every edge made query latency grow with the entire catalog even though the relevance walk is intentionally local.
- **Boost, never gate.** A fragment with no useful relations keeps its seed score untouched.

## From fragments to sources

Ranked fragments roll up to their sources: a source's score aggregates its fragments' final scores (working assumption: max, with a small bonus for multiple independent hits — sum invites long-document bias). The Finder returns what [discovery](discovery.md) promises — ranked **addresses + envelopes** — and for every result: its score, its **summary** (the mandatory transform exists exactly for this), and fragment-level hints such as the matching fragment's text or a timestamp for a video hit.

## Incremental discovery

A Finder response is not an answer — it is a decision point. The first query does its best, but it presents *results*, each with enough context (score, summary, hints) for an AI client to choose a path and dig. Three follow-up moves, each an operation in the [node API](node-api.md):

- **Query again** — refine and re-search.
- **`expand`** — take one result and get back its fragments and relations: the source's subtree of the semantic graph, with relation kinds explicit, so the client navigates structure — this section, that linked page, these entities — instead of re-searching. Expansion is served from a node's own index, so its depth reflects that node's profile.
- **`scan`** — read a slice of the source itself. Every source and fragment records an extent ([indexing](indexing.md)), so a client facing a 10 MB file can request lines 5000–6000 through the normal fetch path instead of pulling the whole thing. Ranges are line-oriented for text (working assumption: byte ranges split UTF-8 mid-character and align with nothing semantic, while extents are already recorded in lines; structureless single-line files may force a byte fallback). Scan applies only to text mimetypes — scanning media makes the node look for descendant text fragments and scan those instead: scanning a video means reading lines of its transcript.

Because authorization is source-level only, this loop needs no per-fragment access decisions: a requester cleared to fetch a source is cleared to see its summary, expand its subtree, and scan any slice of it.

## Across the network

The Finder runs per node against that node's own index. Fan-out ([discovery](discovery.md)) means asking each stronger node to run *its* Finder and merging ranked lists — RRF again, since scores from differently-profiled indexes don't compare. Boundary queries run the same algorithm with the property filter applied to the result set ([access-control](access-control.md)); fragments inherit their source's trust properties precisely so this filter works.

## Paths not taken

- **Flat top-k similarity (standard RAG).** Rejected: it is exactly what the graph exists to beat — chunk similarity misses results whose relevance is relational (the meeting note that never mentions the topic but is three `mentions` edges from everything that does).
- **Ad-hoc additive boosts as the spec.** The 0.2 → 0.25 arithmetic is the intuition, not the algorithm; hand-tuned addition invites feedback loops and order dependence. PPR/spreading activation is the same idea with convergence behavior you can reason about.
- **LLM re-ranking in the core loop.** Not rejected forever, but not the core algorithm: the Finder must run on a phone, offline, in milliseconds. A re-ranking pass could be a big-node option later.

## Settled since

- **Weights and PPR parameters are configurable (finder entry config) with fixed defaults** (`[finder]`: damping 0.5, ≤12 iterations with ε early-exit, RRF k=60, `graph_hops=2`, `graph_relation_limit=20000`; weights by kind name — contains 1.0, transcribes 1.0, derives 0.9, mentions 0.8, links-to 0.4 — with `default` 0.5 for any other kind; configured entries layer over the table rather than replacing it). Learned weights remain future work.
- **Boost, never gate — the formula**: final = seed + PPR score. Addition keeps every seed's standing (each retains ≥ (1−damping) of its normalized mass; property-tested).
- **Rollup**: source = f1 + 0.1·f2 + 0.05·f3 over its fragments' final scores, best-first.
- **Full-text seeds drop function words.** The seed query ORs the question's words for recall, and on 512,000 rows the function words alone matched nearly every row, so BM25 scored the whole table for a 2.5 s median query (EnterpriseRAG-Bench). A row matched only by function words ranks last regardless, so dropping them before the MATCH changes nothing that reaches the top and brings the median to 0.45 s; a query made only of function words is searched whole rather than not at all. *Rejected:* an FTS5 tokenizer stoplist (it would also strip the words from the index, and phrase searches that contain them).
- **Vector seeds carry a distance floor** (`max_vector_distance`, cosine): nearest-k always returns something, and beyond the floor a "neighbor" is noise, not a seed. Discovered the day flat similarity happily returned an orchid note for a fern query.
- **Scan mechanics**: 1-based inclusive line ranges; the end is clamped to the last line and to a fixed window (`SCAN_LINES_MAX`, 2000 lines) so one scan is never a fetch in disguise; a bad range is a typed client error, checked before anything is read. Scan reads text sources only — the line is the unit extents and scan share, and it is defined for text — where "text" is the one list the index, `fetch`, and the chunker share (`text/*` plus the structured application types: JSON, YAML, XML and kin), so the three never disagree about what has lines. Non-text sources serve their largest text descendant that is source content (a transcript), never derived understanding (a summary). The response carries the window actually served and the scanned text's line count, and query hints carry structured extents and their own scores, because the client's next move is to widen a scan around the hint that hit — the response should hand it the numbers, not a string to parse. Hosts read no further than the last requested line: buffered line reads on disk, a streamed body dropped mid-transfer on the web. *Rejected:* HTTP range requests for web scans (`Range:` addresses bytes; the index knows extents in lines, and a line's byte offset is only found by reading the bytes before it — a sequential read that stops early is the whole win, and works on servers that ignore `Range`). Byte fallback and time-range addressing remain open below.
- **Merge collapses by content digest.** After ranking (and after fan-out merging), results whose envelopes carry equal content digests ([addressing](addressing.md)) collapse into one logical result listing every address — the local file and its Google Drive twin rank once, not twice, and the caller picks its replica at fetch time. The best-scoring copy supplies the score, summary, and hints. Boundary filtering runs per address before the collapse, so an external requester's merged result lists only the copies they could fetch. Results without a digest never collapse — best-effort dedup degrades to duplication, never to a wrong merge.

## Open questions

- Learning relation weights from fetch-through feedback (a fetch after a query is a relevance signal).
- Merging fan-out results when nodes' indexes overlap (same source indexed by two nodes) — dedupe by address (cross-host copies dedupe by digest, settled above), but whose hint wins? Same question for `expand`: which node's index serves the subtree when several have one.
- Scan ranges for structureless single-line text (byte fallback) and media time ranges mapped through transcript extents.
