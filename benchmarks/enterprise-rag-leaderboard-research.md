# EnterpriseRAG-Bench leaderboard research

Research snapshot: 2026-09-06. This is a point-in-time research note, not a description of current Inseam behavior or a settled design decision.

## Short answer

Inseam's direction is sound. LLM preprocessing and an incremental discovery agent are both represented in the strongest public approaches. The important change is to preprocess into several searchable views instead of one lossy summary, and to make the answer loop track evidence coverage, conflicts, and stopping explicitly.

The highest-value next experiment is an ingest-time `retrieval card` with separate typed fragments for:

- a synopsis;
- likely questions and future-use cues;
- aliases, codenames, identifiers, people, projects, and dates;
- atomic claims with qualifiers and exact source extents;
- facts that distinguish the source from its nearest semantic neighbors.

Keep the original text in lexical search. Retrieve broadly and cheaply from these independent views, fuse by rank, rerank a bounded candidate set, and let the agent inspect the source before using a claim. Do not concatenate all generated text into one field because the ranking system needs to know why a source matched.

## What the leaderboard actually says

The [current leaderboard CSV](https://huggingface.co/spaces/onyx-dot-app/EnterpriseRAG-Bench-Leaderboard/resolve/main/data/final_display_data/leaderboard.csv) contains 25 systems. The top eight are:

| Rank | System | Overall | Correct | Complete | Recall | Invalid extra docs |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | metor.com | 80.34 | 82.0 | 86.22 | 85.53 | 4.96 |
| 2 | CDL | 78.95 | 82.0 | 85.16 | 80.54 | 14.08 |
| 3 | Troml | 76.79 | 83.8 | 81.84 | 86.55 | 12.65 |
| 4 | Skyller | 71.93 | 77.0 | 79.14 | 81.60 | 8.86 |
| 5 | OpenClaw | 68.22 | 81.6 | 72.86 | 79.02 | 0.47 |
| 6 | SovraRAG.ch | 65.61 | 74.6 | 72.37 | 78.80 | 8.87 |
| 7 | fgroo | 63.27 | 71.0 | 71.03 | 72.50 | 0.63 |
| 8 | OpenAI File Search | 61.03 | 69.8 | 67.87 | 71.65 | 15.70 |

The leaderboard publishes answers and per-question scores, but it does not publish retrieval traces or a configuration for most leading systems. Claims about a winner's internals should therefore be treated cautiously.

There are still useful signals in the public artifacts:

- Metor says its result used one agent and one attempt per question. It attributes quality to counting, disambiguation, selective semantic matching, and requiring the agent to read before it claims. Its public description does not reveal the retrieval implementation.
- CDL's public Reasonara description uses a typed graph of decisions, constraints, events, and goals. Each memory has a short label, full detail, and generated "cognitive cues" that describe future situations where it should appear. A learned policy chooses whether to refine, expand, or stop. The public leaderboard does not prove that every described component was used in CDL's submission.
- Troml disclosed a multi-step GPT-5.4 system, but no retrieval architecture.
- SovraRAG describes query rewriting, hybrid search, a knowledge graph, and bounded agentic synthesis. This is a product description, not a reproducible benchmark configuration.

The answer files also show a real precision and coverage tradeoff. Calculated over 500 public answers, metor submitted 6.55 document IDs per question on average, varying from 1 to 10. CDL submitted 15.70, Troml 14.02, Skyller exactly 10, OpenClaw 1.64, and fgroo 1.63. Broad evidence sets help recall and completeness, but CDL and Troml pay 14.08 and 12.65 invalid extra documents. The narrow OpenClaw and fgroo sets keep invalid documents below one but lose completeness. Fixed top-k is the wrong policy.

The category scores make the case for routing even clearer. Metor leads intra-document reasoning, conflicting information, completeness, miscellaneous, and information-not-found. CDL leads semantic, project-related, constrained, and high-level questions. Troml has the best aggregate correctness and document recall. No one retrieval behavior wins every information need.

Sources for these observations are the public [leaderboard repository](https://huggingface.co/spaces/onyx-dot-app/EnterpriseRAG-Bench-Leaderboard/tree/main/data/raw_data), [metor report](https://www.metor.com/blog/metor-memory-leads-enterpriserag-bench/), [CDL architecture description](https://causaldynamics.com/research/ai-context-window-problem-reasonara), [Troml submission](https://github.com/onyx-dot-app/EnterpriseRAG-Bench/issues/13), and [SovraRAG description](https://sovrarag.ch/sovra-rag).

## What the benchmark rewards

The [benchmark paper](https://arxiv.org/abs/2605.05253) matters more than any leaderboard marketing claim:

- The corpus has 511,962 documents from nine enterprise sources. Its nearest semantic neighbors have average cosine similarity 0.83, the same as the sampled real Onyx corpus. Dense retrieval has many plausible distractors.
- It deliberately includes misplaced files, near-duplicates with changed facts, contradictions, and internal terminology.
- The 500 questions require point lookup, low-overlap semantic matching, distant sections of one document, project synthesis, constraint handling, conflict resolution, exhaustive collection, odd corners of the corpus, high-level synthesis, and justified abstention.
- Confluence and Jira are only about 2.2% of documents but appear in 24% and 21% of grounded questions. Slack is 56% of documents but appears in 17%. Source type and authority matter.
- Single-vector retrieval is weak here. The published vector baseline scores 37.72 overall versus 50.60 for BM25. A later [controlled scaling study](https://arxiv.org/abs/2607.26497) found that BM25 remained on the low-cost frontier at every corpus size and led from mid-scale onward.
- The same scaling study found that a raw-file agent fell to 36.9 at full scale. Replacing its discovery mechanism with BM25 produced 69.4 on the matched 150-question evaluation. Agency did not repair weak global candidate discovery.

The benchmark score is per-question correctness multiplied by completeness. Retrieval recall helps but is not the objective by itself. A system needs enough evidence to cover the requested facts, must reject plausible distractors, and must know when no evidence exists.

## Implications for the current Finder

Current Inseam has several good pieces already:

- raw text participates in FTS;
- summaries are separate fragments;
- lexical and dense rankings use reciprocal-rank fusion;
- typed relations feed a bounded personalized PageRank walk;
- results expose summaries and fragment extents for `query -> expand/scan -> fetch`;
- artifact caches make changed-source enrichment much cheaper than a corpus rebuild.

The benchmark composition, however, disables entities, markdown structure, and chunking, then embeds only one 200-character summary per source. That shape is inexpensive but removes nearly every cross-source graph path and asks one short synopsis to carry semantic recall for a whole document. The current FTS path also indexes one unfielded text value and turns every query token into an OR term. It has no title, path, author, source-type, phrase, proximity, identifier, or date channel. PageRank cannot restore a candidate that never became a seed.

Inseam's existing BEIR NFCorpus result reinforces the point. The recorded full-summary run reaches nDCG@10 0.28789 and recall@10 0.14515. This is a useful smoke benchmark, but it leaves substantial headroom before the more difficult enterprise run.

## Recommended design

### 1. Compile several retrieval views at ingest time

Replace "summary plus keywords" with typed, independently ranked fragments. A first version should produce one bounded retrieval card per source:

```text
synopsis        What this source is about.
answer_cue[]    Questions or future situations for which it may matter.
alias[]         Names, codenames, acronyms, IDs, and spelling variants.
claim[]         One atomic fact plus subject, qualifiers, time, and extent.
relation[]      Typed links to people, projects, customers, tickets, and sources.
```

Generated likely questions are the indexing-side version of [docT5query](https://cs.uwaterloo.ca/~jimmylin/publications/Nogueira_Lin_2019_docTTTTTquery-v2.pdf). Cognitive cues are a broader version: "restaurant recommendation" can retrieve an allergy without sharing its words. Claims preserve details that summaries discard. A recent [ingest-time semantic compilation paper](https://arxiv.org/abs/2608.20845) reports 85.2% answer correctness with roughly 2.2K reader tokens for compiled claims, versus 72.5% with 16.3K tokens for its best chunk configuration. It is a short position paper, so treat the result as a hypothesis worth reproducing, not settled fact.

Every generated item must retain provenance to a source digest and exact extent. The agent should retrieve the generated view for discovery, then inspect the source text before answering. This makes generated text a route to evidence, never evidence by itself.

Change vector scope from the current `all | summaries` switch to an explicit bounded set of fragment kinds. A practical enterprise profile could embed synopses, cues, and claims while retaining all raw text in FTS.

### 2. Add a contrastive second indexing pass

The unusual idea I would test first is neighbor-aware enrichment. After the cheap text and vector index exists:

1. Find each source's nearest semantic siblings inside its project, entity neighborhood, and global ANN neighborhood.
2. Ask a cheap LLM to emit only the facts that distinguish the source from those siblings: date, region, version, status, customer, owner, numeric value, and explicit negations.
3. Store those facts as `distinguishes` fragments and add `conflicts-with`, `supersedes`, or `same-event-as` edges when supported by both sources.

Normal summaries emphasize common topic words, exactly what the benchmark's dense neighborhoods already share. Contrastive cards emphasize the small details that select the right near-duplicate. Bound the pass to high-confusion neighborhoods and changed documents so it does not require a corpus-wide LLM call on every sweep.

### 3. Strengthen cheap candidate discovery

Keep BM25 as a first-class channel. Improve it before replacing it:

- field title, path, source type, people, identifiers, and body separately;
- give exact IDs, quoted phrases, and rare terms dedicated channels;
- support phrase and proximity matching rather than only an OR over cleaned tokens;
- generate lexical rewrites that remove conversational filler and preserve qualifiers;
- add learned sparse expansion such as [SPLADE](https://arxiv.org/abs/2109.10086) only if it beats generated cues on the benchmark.

For dense recall, compare single-vector embeddings with a bounded late-interaction reranker. [ColBERTv2](https://arxiv.org/abs/2112.01488) retains token-level matching and reports a 6 to 10 times reduction in late-interaction storage. It is a better fit for distinguishing shared topics than one vector per source, but its index cost must be measured against Inseam's compact local-node goal.

Use separate result lists for raw FTS, fields, cues, claims, dense summaries, and graph-expanded sources. Fuse by rank, record channel provenance, and learn or configure channel weights per query plan. A candidate that matches an exact identifier should not be treated the same as one returned only by a loose vector neighbor.

### 4. Route by information need

Before retrieval, derive a small, typed query plan:

```text
intent          point | semantic | constrained | exhaustive | conflict | synthesis | absence
entities        normalized names and aliases
filters         source, project, author, date interval, status, region
subquestions    bounded list of independently answerable slots
stop_rule       one fact, every slot, conflict resolved, or search exhausted
```

This must describe generic information needs, not memorize benchmark category labels. [RAGRouter-Bench](https://arxiv.org/abs/2602.00296) finds no fixed RAG method dominates across query and corpus combinations. Query routing can stay above the phone-safe core Finder: a small node runs deterministic plans, while a larger node may use an LLM planner and reranker.

Useful routes include:

- Point lookup: exact identifiers, fielded BM25, narrow rerank, early stop.
- Low-overlap semantic: cues, generated likely questions, dense retrieval, then lexical feedback from promising sources.
- Constrained: parse qualifiers into hard filters or post-retrieval checks before ranking.
- Exhaustive: decompose into coverage slots and continue until no query adds a new supported fact.
- Conflict: retrieve the best candidate, then issue a deliberate counter-search for the same subject and predicate with different dates or values.
- Absence: search several independent channels and aliases, require low calibrated evidence across all of them, then abstain.

Query-side [HyDE](https://arxiv.org/abs/2212.10496) is worth testing only on low-overlap semantic plans. The benchmark's plain vector baseline is too weak to justify paying for HyDE on every query.

### 5. Retrieve, rerank, then diversify

Finder should optimize first-stage recall over a bounded union, perhaps 50 to 200 sources depending on the route. A second stage should rerank 20 to 50 candidates against the full query plan. Then diversify by source, project, time, and claim coverage instead of returning the first fixed k.

The reranker can be a local cross-encoder, a late-interaction scorer, or an optional LLM on a large node. It should score:

- direct relevance;
- satisfaction of every qualifier;
- source authority and freshness;
- support for an uncovered subquestion;
- contradiction with already selected evidence;
- near-duplicate redundancy.

For exhaustive questions, choose the next document by marginal supported-fact coverage, a bounded set-cover heuristic. For ordinary questions, stop when a calibrated score gap and completed evidence slots say another document is unlikely to change the answer. This replaces fixed top-k with an explicit precision and coverage policy.

### 6. Make evidence state a tool, not prompt text

The upstream agent baseline already preserves selected document IDs outside the conversation and survives context compaction. Inseam should go one step further with an evidence ledger that is durable for the query:

```text
add_evidence(address, extent, claim, stance, observed_time)
list_gaps()
list_conflicts()
remove_evidence(...)
```

Each answer claim must point to source text in the ledger. A generated summary, cue, or claim can suggest the source but cannot satisfy the ledger until `scan` or `fetch` confirms it.

The current discovery ladder is a good base. The next useful operations are likely:

- `query` with typed filters, result-channel reasons, and a continuation cursor;
- `search_within` a folder, source, project, or current result set;
- `compare` two or more sources and return differing claims with extents;
- directional, relation-kind-filtered expansion;
- query-local evidence operations.

Do not expose a giant generic search DSL first. The benchmark's own agent implementation succeeds by giving small tools good continuation hints, bounded output, preserved selection state, and warnings after repeated dead ends.

### 7. Make the graph temporal and query-conditioned

The current undirected PPR boost is a sound default for weakly typed relations. Enterprise conflicts need richer semantics:

- normalize people, projects, customers, ticket IDs, dates, and versions into keyed fragments;
- add `belongs-to`, `authored-by`, `reply-to`, `references`, `same-event-as`, `confirms`, `contradicts`, and `supersedes` relations;
- retain direction for asymmetric edges such as `supersedes`;
- select relation weights from the query plan instead of one global table;
- spread only after strong lexical or semantic seeds exist.

[HippoRAG 2](https://arxiv.org/abs/2502.14802) supports deeper passage integration with personalized PageRank, but graph construction must remain selective. The EnterpriseRAG scaling study found that expensive graph builders failed to cover even 2% of the full corpus. Extract structure deterministically where possible, use LLMs for high-value ambiguous relations, cache by digest, and measure generative tokens per indexed source.

### 8. Let answer generation trigger retrieval

The agent should draft an answer plan, not a prose answer, then retrieve for each missing slot. During composition it should pause when a sentence lacks evidence and issue a targeted query. This resembles [FLARE](https://arxiv.org/abs/2305.06983), which retrieves using a prediction of upcoming content when confidence is low, and [Self-RAG](https://arxiv.org/abs/2310.11511), which adapts retrieval and critiques evidence.

For Inseam, explicit state is preferable to hidden self-reflection:

1. Decompose the requested answer into claims or list slots.
2. Retrieve and confirm evidence per slot.
3. Search for counterevidence on high-impact claims.
4. Compose only from confirmed extents.
5. State uncertainty or absence when the bounded search policy is exhausted.

This should improve correctness multiplied by completeness without flooding the final context.

## Suggested experiment order

1. Run the current full EnterpriseRAG benchmark. Keep initial Finder recall separate from agent-cited recall, answer scores, cost, index size, latency, and turns. Without this baseline the leaderboard research cannot guide a design choice.
2. Improve fielded lexical search, exact identifiers, phrases, and query rewrites. This is the cheapest likely gain and protects scale.
3. Add retrieval-card cues and likely questions while retaining the current summaries. Ablate each generated view separately.
4. Add a bounded reranker and adaptive document selection. Measure invalid documents as closely as recall.
5. Add the evidence ledger, coverage stopping, and deliberate counter-search. Expect the largest gains on completeness, constrained, conflicting, and not-found categories.
6. Add entities and deterministic enterprise relations. Measure graph construction cost and per-category gain before adding LLM relation extraction.
7. Test neighbor-aware discriminators and temporal contradiction edges on only the hardest semantic neighborhoods.

Each experiment should report:

- overall correctness times completeness;
- correctness, completeness, document recall, and invalid extras by question type;
- initial recall at 10, 25, and 50 before the agent;
- answer evidence coverage and unsupported-claim count;
- query p50 and p95 latency, agent turns, and model tokens;
- indexing wall time, LLM calls and tokens, index bytes, and bytes per source;
- performance on the existing BEIR NFCorpus run so benchmark-specific gains are visible.

## Recommendation

Build the retrieval card, fielded lexical channels, and evidence ledger before investing in a larger embedding model or a corpus-wide LLM knowledge graph. Those three changes fit Inseam's architecture, preserve local and incremental operation, and target the leaderboard's clearest failure modes: vocabulary mismatch, near-duplicate confusion, incomplete answers, and unjustified stopping.

The distinctive bet is neighbor-aware indexing. Summaries tell search what a document shares with its topic. In a dense enterprise corpus, the useful index often needs to say how the document differs.
