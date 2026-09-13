# Benchmarking

The repository's benchmark harness and commands live in [benchmarks/README.md](../benchmarks/README.md). Start there for either benchmark:

- **EnterpriseRAG-Bench** measures indexing at scale, retrieval, agent answers, and an LLM judge over half a million documents. Hours and dollars per run.
- **BEIR NFCorpus** measures retrieval alone with the standard nDCG family over 3,633 abstracts and 323 queries. Minutes and cents per run, and the one to reach for on every retrieval change.

Benchmark design decisions, including data pins, model policy, scoring, run evidence, and the performance estimate, live in [design/benchmarking.md](../design/benchmarking.md).

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
