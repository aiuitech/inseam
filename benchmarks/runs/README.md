# Benchmark runs

Each completed benchmark gets its own immutable directory here. See [the benchmark guide](../README.md) for the commands and recorded artifact schema.

Do not put an Inseam data directory or downloaded fixture here. Those belong under the ignored `benchmark/fixtures/enterprise-rag-bench/` tree. Before committing a run, check that `manifest.json` says `"status": "completed"` and that its model entries all name `stealth/ox-alpha` except the embedding model.
