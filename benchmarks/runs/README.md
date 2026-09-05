# Benchmark runs

Each completed benchmark gets its own immutable directory here, grouped by benchmark: `enterprise-rag-bench/<run-id>/` and `beir-nfcorpus/<run-id>/`. See [the benchmark guide](../README.md) for the commands and recorded artifact schema.

Do not put an Inseam data directory or downloaded fixture here. Those belong under the ignored `benchmark/fixtures/` tree. Before committing a run, check that `manifest.json` says `"status": "completed"` and that its model entries match the assignments in the benchmark guide.
