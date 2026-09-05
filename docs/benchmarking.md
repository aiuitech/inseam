# Benchmarking

The repository's benchmark harness and commands live in [benchmarks/README.md](../benchmarks/README.md). Start there for either benchmark:

- **EnterpriseRAG-Bench** measures indexing at scale, retrieval, agent answers, and an LLM judge over half a million documents. Hours and dollars per run.
- **BEIR NFCorpus** measures retrieval alone with the standard nDCG family over 3,633 abstracts and 323 queries. Minutes and cents per run, and the one to reach for on every retrieval change.

Benchmark design decisions, including data pins, model policy, scoring, run evidence, and the performance estimate, live in [design/benchmarking.md](../design/benchmarking.md).
