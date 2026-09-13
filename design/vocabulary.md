# Vocabulary

**Status: proposal with validation gates, not adopted architecture.** Nothing here is implemented. The measurements in [indexing](indexing.md) (retrieval hints) and the last end-to-end run (`benchmarks/runs/enterprise-rag-bench/20260907T032046Z-6b040bbfd905/report.md`) are the evidence it answers to, and the gates at the end say what must be true before any of it lands.

The **vocabulary** is the corpus's own words — product names, codenames, region codes, ticket ids, people, customers, the jargon a team uses — as first-class rows in the index, grouped into **clusters** of words that belong together, so that a document and a question are both read in the corpus's terms instead of a general model's. It is the answer to two measured failures: the glossary terms a general model extracts are textbook words that join thousands of documents and flood the graph walk, and a quarter of the benchmark's questions paraphrase their document so that no word matches.

## The thesis: grounding is a corpus problem, not a document problem

The hints transform asked a model, one document at a time, for the document's "internal terms". It answered with the industry's words (SLO, KV cache, P95) because, reading one document with no view of the corpus, it cannot know which words are local. Locality is a property of the corpus: a word is local when it is rare in general language and recurs in a few of *these* documents. So the vocabulary is built **from the index's own statistics first, and the model is asked only what statistics cannot answer** — what a term means, which spellings are the same thing, and what a searcher would call it instead.

Three consequences shape everything below:

1. **Terms are mined and matched, not generated.** A term row exists because the text contains it, so its key is exact by construction and resolution ("Greg" vs "Greg Hunt") is a separate, later step over the term rows, not a property of extraction.
2. **The model reads a cluster, not a document, when it grounds.** Alias resolution, glosses, and search phrases are asked once per changed cluster (thousands at most), never once per document (hundreds of thousands). This is the LLM-in-the-loop clustering, at the granularity where it is affordable and where the model has the context it needs.
3. **A question is grounded the same way a document is.** The query is matched against the vocabulary exactly, and embedded against the clusters for what it paraphrases; the clusters' terms and aliases become lexical seeds. This is the bridge from "the new top end 80GB accelerator" to `H200` with no per-query model call.

The retrieval walk itself does not change. The [Finder](finder.md) already runs personalized PageRank over typed, weighted relations; what changes is what is in the graph, how seeds are made, and how hubs are bounded.

## Rows

All vocabulary rows are **keyed fragments** ([indexing](indexing.md)): they belong to no source, dedupe index-wide under a key, carry no address, conduct relevance, and never rank as results. They are lexical rows: full-text indexed in the lexical table, never given a vector.

- **Term** (`term:<normalized>`, `text/x-inseam-term`) — a word or phrase local to the corpus, with a plain gloss once a cluster pass has written one. Anchored `mentions` from every source-content fragment whose text contains it.
- **Identifier** (`identifier:<normalized>`, `text/x-inseam-identifier`) — a ticket, PR, version, metric, or config name, exactly as written. Near-unique by nature: the strongest conductor in the graph.
- **Entity** (`entity:<kind>:<normalized>`, `text/x-inseam-entity;kind=…`) — a person, customer, project, place. The entity extractor's vocabulary, unchanged.
- **Alias** (`alias:<normalized>`, `text/x-inseam-alias`) — what a searcher would say instead of a term: a paraphrase, a plain description, a spelling variant. Related `aliases` to exactly one term, identifier, or entity. Aliases are the vocabulary's outward face: they exist so a question's words land on a lexical row that leads to the corpus's word.

Every vocabulary row records its **document frequency** (the number of sources anchored to it), maintained by the cluster pass, because the hub bound below must read it without counting edges at query time.

Each term, identifier, and entity belongs to **at most one cluster**. One home per row keeps membership a column rather than a table and makes the invariant checkable; cross-topic reach is the graph's job through the documents, not the cluster's.

## Clusters

A **cluster** is a set of vocabulary rows that co-occur across sources, with one vector over its concatenated terms and glosses in a stable order. Clusters are stored in their own catalog table (`clusters`: id, label, text digest, vector, member count, document frequency, last changed sweep). They are **not fragments and not in the relation graph**: a cluster linked to sixty terms linked to thousands of documents would be the exact super-hub the hints measurement showed floods the walk. Clusters are read at two moments only, the cluster pass and query grounding.

Bounds: `clusters_max` (10,000 by default), `cluster_terms_max` (64). A vocabulary row that would push a cluster past its cap forms a sibling instead; a cluster pass never splits.

**Formation is by co-occurrence, not by embedding.** A new vocabulary row is assigned to the cluster whose members share the most sources with it (Jaccard over the `mentions` anchors), joining when the overlap clears `cluster_join_min` and founding a new cluster otherwise. This is the discrete form of "embed the source and find the nearest cluster", and it is exact, free, and available for a row that appears in one document (its co-occurrence is that document's other rows). Two clusters whose vectors exceed `cluster_merge_cosine` after re-embedding merge, smaller into larger, bounded to the clusters that changed in this pass against their nearest neighbours. The vector is for retrieval by paraphrase, where a discrete match has nothing to match on; it is not needed to decide membership.

## Derived fragments: candidates, never anchors

The graph as it stands is untouched: `contains` and `derives` edges, summaries, hints, cues, and keywords all stay, conduct at their weights, and rank as they do today. Vocabulary adds rows and `mentions` edges beside them; it removes nothing. Two rules say how derived understanding meets the vocabulary:

- **Derived fragments are never anchors.** A term anchors only to source-content fragments (sections, transcript lines, entries), as keyed sprouts already do ([transforms](../docs/indexing/transforms.md)). A summary repeats what its content says, so anchoring it too would count every mention twice, and a model-written summary can name a thing the content never does, which would be an edge the source cannot justify. A summary that a vector hit still receives walk mass from the vocabulary through `derives` from the root and `contains` to the content, so nothing is lost by the rule.
- **Derived fragments are the phrase candidates.** Token mining cannot see multi-word names; the summarizer's keywords and the extractor's cues and entity names can, and they are already paid for. Every keyword phrase and extracted name in the index is a candidate: one that the automaton then matches in `term_df_min` or more sources' *content* becomes a term, anchored to that content. The model proposes, the text confirms, and a phrase the model invented that appears nowhere plants nothing. This closes the multi-word question without a bigram count.

## The vocabulary pass

Vocabulary is a **sweep phase**, run after every file has landed and before folders, exactly as folders are composed from landed state ([indexing](indexing.md)). It reads the store and never a host, so it costs no I/O that indexing did not already pay, and it keeps the sweep's invariant that planning touches no store: per-source planners still emit keyed sprouts (entities, identifiers, cues) as pure functions of their text; the pass does what needs the whole corpus in view.

1. **Mine candidates.** Read the lexical statistics the full-text index already holds (FTS5's vocabulary table gives every token's document frequency for free). A candidate is a token or code-shaped string with document frequency in the local band — `term_df_min` (2) to `term_df_max` (a fraction of the corpus, 2% by default) — that carries the marks of internal naming: capitals inside a word, digits, hyphens, dots, underscores, or absence from a small general-English list. Multi-word phrases come from the model's per-source extraction and from the cluster pass, not from mining; token-level mining is what is free, and the identifier-shaped strings it catches are the ones that matter most.
2. **Match and anchor.** One bounded automaton over every candidate and every existing vocabulary row's spellings, run once over each source-content fragment landed or re-landed this sweep. A hit plants the row if new and anchors it `mentions`. This replaces the per-source "needle" anchor rule for vocabulary, and it is why a document indexed before a term was known still gets its edge: the pass anchors against *all* text it walks, and a full sweep walks everything.
3. **Update frequencies and cluster.** Recount document frequency for every row touched; assign new rows to clusters by co-occurrence; re-embed changed clusters (one embedding per changed cluster, cached by text digest like every vector); merge over-threshold pairs.
4. **Ground changed clusters with the model.** For each cluster that gained members this sweep, one call with the cluster's current rows, their glosses, and up to `cluster_context_sources` (3) short excerpts where its new rows appear. The model returns: which rows are the same thing (merges, applied by re-keying anchors to the survivor and dropping the loser), a gloss for each ungloss'd row, and up to `aliases_per_row_max` (4) search phrases per row in a searcher's plain words. Aliases land as rows related `aliases`. Cost: one call per changed cluster, bounded by `cluster_llm_budget`; with no budget or no model the pass still mines, matches, anchors, and clusters, and only glosses and aliases wait.

The pass is idempotent in the sweep's sense: a second run over unchanged text plants nothing, anchors nothing new, and changes no cluster. What it produced rides the shape stamp only through its config dials, so changing `term_df_max` re-runs the pass, never the per-source transforms.

**Why not ground per source, as first proposed.** The natural shape — embed the source, retrieve its cluster, hand the model the cluster as context, let it add terms — was considered and rejected on three grounds. It is order-dependent: whether source B joins the cluster source A founded depends on which planned first, and the sweep's built index is deliberately independent of concurrency. It reads and writes the store from planners, which the plan-then-land pipeline forbids so that landing stays one writer in enumeration order. And it puts the grounding context into every per-source cache key, so a cluster changing would either silently leave stale extractions or re-spend every LLM call in the corpus. Grounding per cluster gives the model more context (every document that uses the word, not one) at a thousandth of the calls, and leaves per-source extraction a pure, cacheable function of the text.

## The model's per-source job shrinks

The hints and entity transforms fold into one **extractor** transform that emits, per source: entities with kinds, identifiers, and cues (the plain-word questions the document answers). It no longer asks for glossary terms — mining finds the local ones and the model's were the industry's — and no longer writes glosses, which the cluster pass writes with the whole corpus in view. Discriminators and the synopsis stay as hint rows under the source. The entity kinds and the `mentions` edge are unchanged, so nothing in the graph reads differently.

## Facets: what the host says about a source

A source's envelope says things no text does and no model needs to guess: which **host** it came from (Slack, Gmail, Drive, GitHub), which **container** holds it (a channel, a thread, a label, a repository, a folder), who its **author** is, and when it was **modified**. Today the envelope carries the source type, the content type, timestamps, a title, and trust properties the hosts leave empty; channel and author exist only as boilerplate in the text, and the benchmark's weakest sources (Slack, HubSpot, transcripts) are the ones whose text opens with exactly that boilerplate.

The envelope gains **facets**: `Vec<Facet { key, value }>` in the connection's own vocabulary (`channel`, `thread`, `label`, `repository`, `author`, …), filled at enumeration, synced with the envelope like the rest of it. They are a separate field from `properties`, which carry trust levels and feed boundary filtering; a channel name must never be read as an access decision. The vocabulary pass turns them into rows, anchored from the **root**, not from text:

- `facet:host:<host kind>` and `facet:container:<host>:<value>` (`text/x-inseam-facet`), one row per distinct value, row kind **facet**.
- An author lands on the **entity row** `entity:person:<normalized>`, related `authored` from the root, so "wrote it" and "is mentioned in it" meet on one row and a person's name in a question reaches both.
- Modified time is not a row. A time is a range, and a range is a filter.

A host facet has a document frequency in the thousands by construction, so the hub bound keeps it out of the walk; a channel with forty messages conducts, and that is the right line. Facets are in the ledger and the harness as their own row kind, which turns the report's per-source table from a folder-name convention into a measured channel.

Their second use is as **constraints**. A query may carry filters — host, container, author, modified range — applied to the candidate set before rollup, the way boundary properties are. The exact grounding step recognises facet values in the question ("in Slack", "the #incidents thread") as seeds; a big node's grounded rewrite may promote them to filters; the agent gets them as query options so a question that names a system searches that system. The constrained question type already scores 95, so filters are for the agent's precision and for the leaf node whose whole index is one host, not for recall.

For the benchmark corpus, which is files under folders, the filesystem host has no facets to give; the harness's fixture would carry a sidecar mapping folder to host, or the filesystem host could take a configured rule that reads facets from path components. Undecided, and listed below.

## Retrieval

Seeding gains a **grounding** step before the existing hybrid seed; the walk and the rollup are the Finder's as they stand.

1. **Exact grounding.** The query runs through the same automaton the pass used. A vocabulary row the query names outright becomes a seed with the strength of a full-text hit.
2. **Paraphrase grounding.** The query vector (already computed for the vector seed list) is scored against every cluster vector — ten thousand dot products, well under a millisecond — and clusters above `cluster_query_cosine` contribute their rows and aliases as a fourth seed list, entering fusion at `cluster_seed_weight` (0.3) and only from their top `cluster_seed_ranks` (5), the same rank-gated, down-weighted shape the cue vectors measured best at. Their job is to add candidates the text lists lack, never to reorder what full-text already carries.
3. **Hybrid seeds and fusion**, as today: prose full-text, lexical full-text, vectors, reciprocal rank fusion.
4. **The walk**, as today, with two changes to what conducts. Edge weight is the relation kind's weight times a weight for the far end's row kind (`[finder.weights.by_row_kind]`: identifier 1.0, entity 0.8, term 0.6, facet 0.5, alias 0.4, prose 1.0), because a shared ticket number says more than a shared jargon word. And a vocabulary row whose document frequency exceeds `hub_df_max` keeps its lexical row and its seeds but contributes **no edges** to the walk's slice: hub protection is a bound, not a damping, because the measurement showed every damping of hub terms still lost twenty points.
5. **Rollup and merge**, as today.

A big node may add a **grounded rewrite**: one cheap model call that rewrites the question in the corpus's words given the clusters step 2 matched. It is the benchmark report's second lever with the cluster as context, and it is an option outside the core loop, which must still answer on a phone, offline, in milliseconds.

## Authority, as an experiment

"Traditional" PageRank — global, unpersonalized — was rejected for the walk because it converges to importance rather than relevance. It may still earn a place as a **prior**: the cluster pass could compute one bounded global PageRank over the whole graph per sweep and store an authority score per source, and the rollup could break ties among near-duplicate siblings toward the more-referenced one. The last run's largest failure is the agent answering from a sibling draft; whether the referenced copy is the authoritative one is an empirical question. Gate: on the slice, does the prior move the gold sibling above its near-duplicates more often than not? Unmeasured, so not designed further.

## Costs

Per sweep, on a 25,000-document corpus of 500 MB of text:

- Mining: one scan of the FTS vocabulary table; seconds.
- Matching: one automaton pass over the landed text; tens of seconds, no I/O beyond the catalog.
- Cluster vectors: at most `clusters_max` embeddings, only for changed clusters; a few dollars at most on the first sweep, cents after.
- Cluster grounding: one model call per changed cluster; the first sweep pays for every cluster (thousands of short calls, under the per-source extractor's cost), later sweeps for the few that changed.
- Storage: vocabulary rows are one-line lexical rows and `mentions` edges; the hints measurement already carried 300,000 of them without harm once the lexical table was split.

Per query: one automaton pass over the query and one dot product per cluster, both under a millisecond; the rest is the Finder as it stands.

## Observability: every point of score has a ledger entry

The proposal adds four ways for a document to reach the ranking (exact grounding, cluster grounding, vocabulary edges in the walk, an authority prior) beside the three that exist, and the only honest way to tune seven channels is to see, per result and per benchmark, what each one did. Today's `QueryTrace` counts hits per seed list and times the phases; it says nothing about *why a given source ranked*. The design adds a per-result **ledger**, makes every channel a query-time dial the benchmark can sweep without re-indexing, and has the harness read both against gold.

### The ledger

Under `explain` (a query option; the CLI's `--explain`, off by default because it costs a few extra walk iterations), every ranked source carries the exact decomposition of its score:

- **By seed channel.** Reciprocal-rank fusion is a sum over lists and the walk is linear in its restart vector, so the final score decomposes exactly: the walk runs once per channel on the same graph slice (a matrix of restart columns, same iteration count) and each source's score is reported as `prose + lexical + vector + exact + cluster` seed mass plus the walk mass each channel induced. No counterfactual runs are needed to say "this document is here because of cluster grounding".
- **By row kind.** The walk's last iteration is repeated with edges grouped by the far end's row kind, so each source also reports how much walk mass arrived through identifiers, entities, terms, facets, aliases, and prose. This is the cut that answers "are mined terms pulling most of the weight".
- **By row.** The vocabulary rows and clusters that carried mass into the source, best first, with each row's document frequency: the top three edges for a result are usually the whole story, and a row with a frequency in the thousands sitting at the top of many results is the hub the bound missed.
- **The prior**, when the authority experiment is on, as its own line.

The ledger rides `QueryTrace` beside the counts it already has, serializes with `--json`, and is printed under each result by the CLI as one line per non-zero channel. It is also served through the node API so the macOS app and the agent can show it; the agent is not shown it by default, because it is diagnosis, not evidence.

### Dials

Every channel and weight in this design is **query-time** ([index-maintenance](index-maintenance.md) tiers), so a sweep of settings costs queries, never an index:

- per seed list, `[finder.seed_lists.<prose|lexical|vector|exact|cluster>]`: `enabled`, `weight` (the vote in fusion; 1.0 is an equal vote), `ranks_max` (a rank gate: only this many of the list's best enter fusion). The existing `seeds = full-text | vector | both` becomes a shorthand over these.
- per row kind, `[finder.weights.by_row_kind]`, and per relation kind as today.
- `hub_df_max`, `cluster_query_cosine`, the walk's damping and iterations, and the authority prior's weight (0 is off).

A query request may carry **overrides** for any of these keys, restricted to the query-time tier (`inseam query --finder cluster.weight=0`): the composition stays the node's only configuration, and an override is a request parameter like `--limit`, never stored. The benchmark harness uses overrides to run a **matrix** of settings over one index: each cell is the full question set at one setting, recorded in the manifest as the override set, so two cells of a run differ by exactly what they say they differ by. The index-side dials (`term_df_min`, `term_df_max`, the shape rule, cluster thresholds) are shape and re-run the vocabulary pass, which is the cheap phase; the harness records them from the composition as it records everything else.

### The pass reports

The vocabulary pass reports into the sweep's `IndexReport` and `inseam status`: candidates mined, rows planted and anchored, rows above the hub bound (with the twenty highest by document frequency, named), clusters formed, joined, merged, and re-grounded, model calls and their cost, and time per step. `inseam vocabulary` lists rows and clusters with document frequency and cluster membership; `inseam vocabulary show <row>` prints one row's gloss, aliases, cluster, and the sources anchored to it. The first thing to do after a pass is read the top of that list: if it is full of industry words, the shape rule is wrong before any query is run.

### What the harness reads

For every question with gold documents, the harness records each gold document's rank and its ledger, whether or not it made the top ten, in `attribution.jsonl` beside `answers.jsonl` ([benchmarking](benchmarking.md), evidence in a run). From that it derives, and prints in the report:

- **per channel:** how many gold documents it seeded at all, how many it alone seeded (present in this list and no other), and how many it was the largest contributor for; the same for documents in the top ten that are neither gold nor valid, so a channel's noise stands beside its recall;
- **per row kind:** the same three counts over walk mass;
- **per vocabulary row:** the rows that most often carried gold, and the rows that most often carried non-gold into the top ten, each with its document frequency. The second list is the flood detector: a row that appears in many wrong top tens and few right ones is either a hub the bound should catch or a term mining should not have kept;
- **per question type**, all of the above, because the semantic quarter is where the vocabulary must pay and the basic three quarters are where it must not cost.

A run's report then says in one table what each channel bought and what it cost, and two runs differ in that table before they differ in the headline score. The matrix over query-time dials answers "what if this channel were weaker" directly; the ledger answers "what did it actually do" without running anything twice.

## Paths not taken

- **Vectors on vocabulary rows** (already rejected in [indexing](indexing.md)). Still rejected: the cluster carries the one vector the paraphrase bridge needs; a vector per term buys fuzzy name matching that aliases give exactly.
- **Clusters as fragments in the graph.** A cluster is a super-hub by construction; measured hub flooding says no. Clusters are read at grounding time only.
- **A topic model or community detection for clusters.** The vocabulary is words, not themes; co-occurrence over anchors is the signal at hand and needs no second algorithm.
- **The model choosing which words are local.** Measured: it chooses the industry's. Statistics choose; the model glosses.
- **Grounding per source at plan time.** Order-dependent, store-touching in planners, and it poisons the cache keys; see the vocabulary pass.
- **A per-query model rewrite in the core loop.** Kept as a big-node option; the cluster bridge is the offline, millisecond answer.

## Validation gates, in order

Each is a run on the 25,000-document slice with the per-role side-table harness ([benchmarking](benchmarking.md)), before any code beyond what the gate needs. The ledger, the query-time overrides, and the harness's attribution output land before gate 2, because they are what the gates read.

1. **Ceiling.** Mine candidates from the slice's FTS vocabulary with the band and shape rules above, offline. Of the 159 gold documents that finished outside the top 25, how many share a mined term or identifier with their question? That number bounds what exact grounding can recover. Below 30, stop here.
2. **Exact grounding and the hub bound.** Plant the mined rows, anchor, seed from exact matches, exclude hubs by `hub_df_max`. Recall per question type against 84.3. Must not lose on basic while gaining on semantic.
3. **Clusters and aliases.** Form clusters by co-occurrence, ground with the model, add the paraphrase seed list. Semantic recall against 56.0 end to end; the fusion weight and rank gate swept.
4. **Extractor fold.** Replace hints and entities with the single extractor; confirm nothing regresses, and that project-related recall rises with entities on.
5. **Authority prior.** Sibling tie-breaks on the near-duplicate questions, measured separately.

## Open questions

- Multi-word local phrases beyond what keywords and cues propose: whether a bounded bigram count over the landed text finds names the model never wrote, or whether the derived candidates cover it.
- Facets for a file corpus: a fixture sidecar, or a filesystem-host rule that reads facets from path components. The benchmark needs one of them before facets can be measured.
- Modified time as a sibling tie-break: among near-duplicate drafts the later one is more often the authoritative one; measured beside the authority prior, not assumed.
- The general-English list for the shape rule: size, source, and whether a corpus in another language needs its own.
- Re-anchoring after a merge in the cluster pass touches every edge of the losing row; whether that lands in the same transaction as the pass's other writes or in bounded batches.
- Whether `hub_df_max` is a count or a fraction of the corpus; a fraction scales, a count is legible.
- A phone's composition: the pass without a model still mines, matches, and clusters; whether a leaf node should run it at all or receive vocabulary from a larger node through the network is undecided ([discovery](discovery.md)).
