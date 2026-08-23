# Benchmarks

The first benchmark is [EnterpriseRAG-Bench](https://github.com/onyx-dot-app/EnterpriseRAG-Bench): 500 questions over slightly more than 500,000 synthetic enterprise documents. The harness pins dataset release `v1.0.0`, verifies the published checksums, and runs the installed `inseam` CLI as an external process. This measures the same binary a user runs.

## Requirements

- Python 3.10 or newer
- `git`, `curl`, and `unzip` on `PATH`
- the `inseam` CLI on `PATH`
- `OPENROUTER_API_KEY`
- enough local disk for the 1.26 GB download, its extracted files, and a fresh Inseam index. Reserve at least 20 GB before a full run.

The harness uses `google/gemini-2.5-flash-lite:batch` through OpenRouter for summaries and `stealth/ox-alpha` for answers, citation cleanup, correctness scoring, and fact scoring. Summary calls request low reasoning effort and exclude the reasoning trace from the response. Entity extraction, markdown splitting, and chunking are disabled. `openai/text-embedding-3-small` embeds one 200-character summary per source at 384 dimensions; source text still enters full-text search.

The default run limits summaries to 500 LLM calls. Inseam uses its deterministic fallback after the summarizer spends that budget. Raise `--llm-call-budget` only after estimating the cost and runtime. Concurrent `:batch` summary calls are collected for 50 ms and submitted through OpenRouter's Batch API in jobs of up to 128 requests, then polled every five seconds. Embeddings use base64 responses and pack up to 128 inputs per request, with four batches in flight. The lean source-plus-summary shape fills an embedding request from 256 search rows.

## Set up the fixture

Run this once from the repository root:

```sh
python3 benchmarks/enterprise_rag_bench.py setup
```

Setup downloads and verifies `all_documents.zip` and `questions.jsonl`, extracts the documents, checks out only the evaluator code at the pinned upstream revision, and creates its Python virtual environment. Everything lands under `benchmark/fixtures/enterprise-rag-bench/`. Git ignores that directory's contents because the fixture is large and upstream asks that the dataset not enter training corpora.

The download resumes if interrupted. Setup refuses a checksum mismatch and refuses to overwrite local changes in the ignored evaluator checkout.

## Run

```sh
export OPENROUTER_API_KEY=...
python3 benchmarks/enterprise_rag_bench.py run
```

A full run creates a fresh index and evaluates all 500 questions. It can take hours and makes many embedding and LLM requests. `--limit` limits questions and evaluator work, but indexing still covers the full corpus so retrieval scores remain meaningful.

The runner prints each active phase immediately. During indexing it polls `inseam status` every 30 seconds and prints elapsed time, fully indexed sources against the fixture total, cataloged sources, and search rows. A failed status probe reports `status unavailable` but does not fail the index. Other long commands retain their five-second elapsed-time heartbeat.

Before questions the runner runs a timed `inseam repair` step named “Preparing libSQL vector search index”; this is normally instant, but on a node created before vector indexing it performs the one-time in-place conversion and DiskANN build. The repair reuses the resident vectors and does not rerun source indexing or embedding. Query progress includes the question number and ID. The current phase is mirrored in `manifest.json`, so a second terminal can distinguish indexing, querying, and evaluation without inspecting processes.

For a one-question harness check after setup:

```sh
python3 benchmarks/enterprise_rag_bench.py run --limit 1 --skip-evaluation
```

Useful bounded controls:

```sh
python3 benchmarks/enterprise_rag_bench.py run \
  --limit 500 \
  --query-limit 8 \
  --turns 12 \
  --index-concurrency 8 \
  --llm-call-budget 500 \
  --evaluation-parallelism 4
```

Every `run` invocation creates a new ignored index under `benchmark/fixtures/enterprise-rag-bench/nodes/`. This prevents a warm index from being reported as a fresh indexing result. Failed and interrupted runs remain on disk with their partial logs and a non-completed manifest.

## Resume a run

If indexing completed but a later query, answer, or evaluation failed, resume the same run:

```sh
python3 benchmarks/enterprise_rag_bench.py resume <run-id>
```

Use the directory name under `benchmarks/runs/` as `<run-id>`. Resume loads the original options and composition, verifies the dataset and model pins, checks that the index command completed successfully, and reuses that run's ignored data directory. It refuses a missing or incomplete index. It starts with the first question that has no durable record, or goes directly to evaluation when every question is complete.

Each invocation is recorded in `manifest.json` under `attempts`, including its start and finish time, duration, Inseam binary identity, search-index preparation timing and log, starting and ending question counts, status, error, and log directory. The top-level duration is the sum of attempt durations. The index record keeps its original duration and structured completion counts for sources, fragments, relations, transforms, embeddings, and spend.

## Recorded runs

Commit completed runs under `benchmarks/runs/<UTC timestamp>-<inseam commit>/`. Each run contains:

- `manifest.json`: start and finish time, attempt history, total active duration, current phase, OS, CPU, RAM, disk, Inseam CLI version, binary hash, source revision and dirty state, dataset pins, model names, command timeouts, options, index duration and completion counts, score summaries, and completion status.
- `composition.toml`: the exact Inseam composition used.
- `queries.jsonl`: per-question start and finish time, retrieval time, answer time, total time, answer, initial Finder document IDs, agent-cited document IDs, the combined evaluator document set, and every raw Finder result with its address and score.
- `answers.jsonl`: the candidate file sent to EnterpriseRAG-Bench.
- `enterprise-rag-bench-results.json`: the evaluator's per-question results and aggregate scores.
- `evaluator-dependencies.txt`: installed evaluator package versions.
- `logs/`: the one-time index log plus attempt-specific query, agent, and evaluator output.

The manifest's `scores.retrieval` block is computed from the initial ranked Finder results without an LLM. `scores.enterprise_rag_bench` evaluates the combined initial and agent-cited document set and records the upstream correctness, completeness, document recall, and invalid-extra-document aggregates. The raw result file remains authoritative.
