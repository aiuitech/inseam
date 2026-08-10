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

- **Relation kinds carry weights.** A `mentions` edge into a shared entity should conduct relevance differently than a structural `contains` edge or a speculative `links-to`. Entity fragments are the highways: two sources mentioning the same person become one hop apart.
- **Bounded propagation.** A damping factor and small iteration count (or hop limit) keep the boost local; unbounded spreading converges to global centrality — "important" fragments drowning out *relevant* ones.
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

## Open questions

- Per-relation-kind weights: fixed defaults, profile-configurable, or eventually learned from fetch-through feedback (a fetch after a query is a relevance signal).
- PPR parameters (restart probability, iteration budget) and whether small devices run one-hop spreading as the cheap approximation.
- Merging fan-out results when nodes' indexes overlap (same source indexed by two nodes) — dedupe by address, but whose hint wins? Same question for `expand`: which node's index serves the subtree when several have one.
- Scan range mechanics: the line/byte unit fallback for structureless text, and whether media scans can address a time range directly (mapped through transcript extents).
