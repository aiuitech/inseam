# Benchmarking

For what we have tried and learned, see the
[indexing and Finder experiment history](../design/indexing-experiments.md).

The repository's benchmark harness and commands live in [benchmarks/README.md](../benchmarks/README.md). Start there for either benchmark:

- **EnterpriseRAG-Bench** measures indexing at scale, retrieval, agent answers, and an LLM judge over half a million documents. Hours and dollars per run.
- **BEIR NFCorpus** measures retrieval alone with the standard nDCG family over 3,633 abstracts and 323 queries. Minutes and cents per run, and the one to reach for on every retrieval change.

Benchmark design decisions, including data pins, model policy, scoring, run evidence, and the performance estimate, live in [design/benchmarking.md](../design/benchmarking.md).

## Attribution: what bought recall and what carried noise

An EnterpriseRAG-Bench run asks the Finder for 50 results per question (the CLI's clamp) and scores the first `--query-limit` of them, so recall and MRR mean what they always meant while every gold document reports where it actually landed. With `--explain` (the default; `--no-explain` skips it) each query also returns its score ledger, and the run records:

- `attribution.jsonl`, beside `answers.jsonl`: one row per gold document — `question_id`, `question_type`, `document_id`, `rank` (1-based within the 50, or `null`), and the result's `evidence` object with its ledger, or `null` when the document was not returned.
- `retrieval-attribution.json`: the tables the [vocabulary design](../design/vocabulary.md) asks for. Per channel (`prose`, `lexical`, `vector`, `exact`, `cluster`): how many gold documents it seeded, seeded alone, and was the largest ledger contributor for, and the same three counts over noise (documents in the scored top list that are neither gold nor in the question's `valid_doc_ids`). Per row kind: the documents whose walk mass arrived mostly through that kind. `rows_carried_gold` and `rows_carried_noise`: the 25 vocabulary rows that carried mass into the most gold and the most noise documents, each with its document frequency. `hubs_excluded`: the 25 highest-degree hubs any query left out. Everything is repeated under `by_question_type`.
- `scores.retrieval.attribution_ready` in the manifest says whether any result carried a ledger; `scores.retrieval_attribution` holds the channel and row-kind tables.

The runner prints the channel and row-kind tables when scoring finishes. `--finder-override KEY=VALUE` (repeatable) passes a query-time Finder override to every query as `inseam query --finder KEY=VALUE`, for example `--finder-override seed_lists.cluster.weight=0`; the list is recorded in the manifest's `options.finder_overrides`, so a matrix of settings over one index is a set of runs that differ by exactly that list. `--vocabulary-llm-budget N` (both runners; default 0) lets the vocabulary pass ground that many clusters with the model during indexing; at the default the pass mines, matches, and clusters without a model call, so an indexing comparison stays model-free unless grounding is what it measures. It is recorded as `options.vocabulary_llm_budget`.

[The September 2026 retrieval audit](../benchmarks/reports/20260913-retrieval-audit.md) compares folder-aware retrieval tuning, query-only replays, and incremental folder re-indexing before the GPT-5.4 agent phase.

For a short end-to-end iteration, use the [GPT-5.4 navigation check](../benchmarks/README.md#fast-gpt-54-navigation-check).
It judges the first and last 10 questions while retaining the full document
corpus and reusing its index. Keep the question IDs and models fixed when
comparing Finder changes.

[The first GPT-5.4 bookend baseline](../benchmarks/reports/20260913-gpt54-bookends.md)
scored 92.0 in 5 minutes 42 seconds and records the failures to track next.

[The navigation improvement diagnosis](../benchmarks/reports/20260913-navigation-improvement-plan.md)
compares those failures with public answers and proposes bounded query-time
experiments before changing the corpus index or increasing the sample.

The [Finder refinement implementation](finder/refinement.md) combines searches,
expansion, and scans with explicit candidate accumulation. Indexed excerpts
supplement the summary prefix even when the lean benchmark index provides no
fragment hints.

[The additive-navigation rerun](../benchmarks/reports/20260913-gpt54-refinement.md)
improved the same sample from 92 to 99, with 100% correctness. It took 7 minutes
25 seconds and $2.81 in answer cost, versus 5 minutes 42 seconds and $1.64.
The report records the remaining completeness gap and the increased search
cost on unanswerable questions before any larger evaluation.

[The expanded 50-question run](../benchmarks/reports/20260913-gpt54-50-bookends.md)
scored 87.33 with 90% correctness in 18 minutes 44 seconds, at $6.61 in answer
cost. The original 20 scored 100; the added 30 scored 78.89. High-level
company questions exposed three incorrect answers out of five. Use
`--bookend-count 25` to repeat this first-25/last-25 sample.

The [100-question GPT-5.4 review](../benchmarks/reports/20260913-gpt54-100-bookends.md)
scored 78.98 over the first fifty and last fifty questions on the full index.
It includes a paired comparison with the earlier fifty and records the pause
before any extension to 500.

The [vocabulary-enriched recall comparison](../benchmarks/reports/20260913-vocabulary-recall.md) tests fresh indexes against both historical scores and the current Finder on preserved old indexes. It reports initial recall separately from agent answer quality.
