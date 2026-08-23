# Benchmarks

The first benchmark is [EnterpriseRAG-Bench](https://github.com/onyx-dot-app/EnterpriseRAG-Bench): 500 questions over slightly more than 500,000 synthetic enterprise documents. The harness pins dataset release `v1.0.0`, verifies the published checksums, and runs the installed `inseam` CLI as an external process. This measures the same binary a user runs.

## Requirements

- Python 3.10 or newer
- `git`, `curl`, and `unzip` on `PATH`
- the `inseam` CLI on `PATH`
- `OPENROUTER_API_KEY`
- enough local disk for the 1.26 GB download, its extracted files, and a fresh Inseam index. Reserve at least 20 GB before a full run.

The harness sends embeddings to `openai/text-embedding-3-small`. It uses `stealth/ox-alpha` for summaries, entity extraction, answer generation, citation cleanup, correctness scoring, and fact scoring. The default run limits summaries and entity extraction to 500 LLM calls each. Inseam uses its deterministic fallback after a transform spends its budget. Raise `--llm-call-budget` only after estimating the cost and runtime.

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

The runner prints each active phase immediately. While an `inseam` command is still running, it prints an elapsed-time heartbeat every five seconds; query progress also includes the question number and ID. The current phase is mirrored in `manifest.json`, so a second terminal can distinguish indexing, querying, and evaluation without inspecting processes.

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

Every invocation creates a new ignored index under `benchmark/fixtures/enterprise-rag-bench/nodes/`. This prevents a warm index from being reported as a fresh indexing result. Failed and interrupted runs remain on disk with their partial logs and a non-completed manifest.

## Recorded runs

Commit completed runs under `benchmarks/runs/<UTC timestamp>-<inseam commit>/`. Each run contains:

- `manifest.json`: start and finish time, total duration, current phase, OS, CPU, RAM, disk, Inseam CLI version, binary hash, source revision and dirty state, dataset pins, model names, command timeouts, options, index duration, score summaries, and completion status.
- `composition.toml`: the exact Inseam composition used.
- `queries.jsonl`: per-question start and finish time, retrieval time, answer time, total time, answer, initial Finder document IDs, agent-cited document IDs, the combined evaluator document set, and every raw Finder result with its address and score.
- `answers.jsonl`: the candidate file sent to EnterpriseRAG-Bench.
- `enterprise-rag-bench-results.json`: the evaluator's per-question results and aggregate scores.
- `evaluator-dependencies.txt`: installed evaluator package versions.
- `logs/`: index, query, agent, and evaluator output.

The manifest's `scores.retrieval` block is computed from the initial ranked Finder results without an LLM. `scores.enterprise_rag_bench` evaluates the combined initial and agent-cited document set and records the upstream correctness, completeness, document recall, and invalid-extra-document aggregates. The raw result file remains authoritative.
