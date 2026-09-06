# Benchmarks

Two benchmarks run the installed `inseam` CLI as an external process, so every result measures the same binary a user runs:

- [EnterpriseRAG-Bench](#enterpriserag-bench): 500 questions over slightly more than 500,000 synthetic enterprise documents, answered by the agent and judged by an LLM. Hours and dollars per run.
- [BEIR NFCorpus](#beir-nfcorpus): the smallest dataset in the BEIR retrieval suite, 3,633 biomedical abstracts and 323 queries with graded relevance judgments, scored with the standard nDCG family. Minutes and cents per run.

Both runners share `harness.py`: bounded external commands with heartbeats, verified downloads, run identity and machine specifications, the `inseam index` and `inseam repair` steps, and the attempt ledger that makes a crashed or interrupted run resumable. Each runner owns its dataset pins, composition, query loop, and scoring.

## Requirements

- Python 3.10 or newer
- `git`, `curl`, and `unzip` on `PATH`
- the `inseam` CLI on `PATH`
- `OPENROUTER_API_KEY`

<<<<<<< HEAD
Both runners use `google/gemini-2.5-flash-lite` through OpenRouter's batch lane for summaries, with low reasoning effort and the reasoning trace excluded, and `openai/text-embedding-3-small` at 384 dimensions for embeddings. Entity extraction, markdown splitting, and chunking are disabled. The default run limits summaries to 500 LLM calls; Inseam uses its deterministic fallback after the summarizer spends that budget. Raise `--llm-call-budget` only after estimating the cost and runtime. Summaries ride the batch lane (`summarizer.llm_lane = "batch"`): the sweep parks up to 4,096 planners on their summary calls and OpenRouter's Batch API takes them as one job of up to 10,000 requests. Embeddings use base64 responses and pack up to 128 inputs per request, with four batches in flight.
=======
The harness uses `google/gemini-2.5-flash-lite` through OpenRouter's batch lane for summaries and `stealth/ox-alpha` for answers, citation cleanup, correctness scoring, and fact scoring. Summary calls disable reasoning and exclude the reasoning trace from the response. Entity extraction, markdown splitting, and chunking are disabled. `openai/text-embedding-3-small` embeds one 200-character summary per source at 384 dimensions; source text still enters full-text search.
>>>>>>> ba12a580 (fix(benchmarks): disable summary reasoning)

<<<<<<< HEAD
<<<<<<< HEAD
Every `run` invocation creates a new ignored index under the fixture's `nodes/` directory. This prevents a warm index from being reported as a fresh indexing result. Failed and interrupted runs remain on disk with their partial logs and a non-completed manifest.
=======
The default run limits summaries to 500 LLM calls. Inseam uses its deterministic fallback after the summarizer spends that budget. Raise `--llm-call-budget` only after estimating the cost and runtime. Summaries ride the batch lane (`summarizer.llm_lane = "batch"`). This pinned benchmark sets the endpoint's job cap to 10,000 requests and parks up to 65,536 source planners, the endpoint queue's hard bound, so several full OpenRouter Batch API jobs can run concurrently. The 64 MiB serialized-job limit may split large requests sooner. Embeddings use base64 responses and pack up to 128 inputs per request, with four batches in flight. The lean source-plus-summary shape fills an embedding request from 256 search rows.
>>>>>>> f463417e (perf(benchmarks): fill concurrent summary batch jobs)
=======
The default run limits summaries to 500 LLM calls. Inseam uses its deterministic fallback after the summarizer spends that budget. Raise `--llm-call-budget` only after estimating the cost and runtime. Summaries ride the batch lane (`summarizer.llm_lane = "batch"`). This pinned benchmark sets OpenRouter's job cap to its 5,000-request limit and parks up to 65,536 source planners, so the endpoint can keep its eight job slots busy. The 64 MiB serialized-job limit may split large requests sooner. Embeddings use base64 responses and pack up to 128 inputs per request, with four batches in flight. The lean source-plus-summary shape fills an embedding request from 256 search rows.
>>>>>>> 951f25d0 (fix(indexing): respect OpenRouter batch request limit)

## Tests

The harness is tested without network or the real binary:

```sh
cd benchmarks && python3 -m unittest test_harness test_enterprise_rag_bench test_beir
```

## BEIR NFCorpus

BEIR (Benchmarking IR) datasets share one shape: a corpus, a set of queries, and graded relevance judgments (qrels). NFCorpus is the smallest corpus in the suite and the one to reach for when a full EnterpriseRAG-Bench run is too expensive: a 2.4 MB download, 3,633 documents, 323 test queries, 12,334 judgments graded 1 or 2. The harness pins the archive BEIR's own loader downloads by SHA-256 and by row counts.

### Set up the fixture

```sh
python3 benchmarks/beir.py setup
```

Setup downloads and verifies `nfcorpus.zip`, extracts it, writes each corpus row as `benchmark/fixtures/beir-nfcorpus/documents/<document-id>.txt` with the title on the first line, keeps only the test split's judged queries in `queries.jsonl`, copies the test qrels, and records the pins in `setup.json`. The file name is the BEIR document ID, which is how a Finder result address maps back to a judgment. Setup is idempotent and needs no API key. Git ignores the fixture directory.

### Run

```sh
export OPENROUTER_API_KEY=...
python3 benchmarks/beir.py run
```

A run indexes the corpus, runs `inseam repair`, sends every test query through `inseam query --json` one at a time, and scores the rankings. There is no agent step and no LLM judge: BEIR is a retrieval benchmark, and its score is deterministic given the ranking.

Bounded controls:

```sh
python3 benchmarks/beir.py run \
  --limit 323 \
  --query-limit 10 \
  --index-concurrency 8 \
  --llm-call-budget 500
```

`--limit` is how many test queries to run, in BEIR's order; indexing always covers the whole corpus. `--query-limit` is the number of Finder results per query and also the deepest metric cutoff. `inseam query` clamps its limit to 50, so the harness refuses a larger value instead of scoring a truncated ranking. The default of 10 reports nDCG@10, BEIR's headline number.

Unlike the EnterpriseRAG-Bench composition, the BEIR composition embeds every fragment (`vectors = "all"`) rather than only the 200-character summary. The corpus is 5.8 MB of text, so embedding each abstract whole costs cents and measures the product's default search surface. Summaries and full text still enter full-text search.

### Scores

The harness computes trec_eval's `ndcg_cut`, `map_cut`, `recall`, and `P` at cutoffs 1, 3, 5, and 10 (plus `--query-limit` when it is larger), averaged over the queries that ran, and rounds to five decimals the way BEIR reports them. Gains are the raw grades with a log2(rank + 1) discount; recall and average precision divide by every judged relevant document, not only the ones inside the cutoff; a grade above zero counts as relevant. The implementation was checked against `pytrec_eval` on real NFCorpus rankings and agrees to floating-point precision.

Each run also writes `run.trec`, the ranking in TREC run format, so anyone can rescore it independently:

```sh
trec_eval -m ndcg_cut.10 -m recall.10 benchmark/fixtures/beir-nfcorpus/qrels.tsv benchmarks/runs/beir-nfcorpus/<run-id>/run.trec
```

For a smoke check after setup:

```sh
python3 benchmarks/beir.py run --limit 1
```

### Resume a run

```sh
python3 benchmarks/beir.py resume <run-id>
```

Resume works exactly as it does for EnterpriseRAG-Bench below: same run directory, same options, verified pins and composition, completed index required, continue from the durable query checkpoint, and a new attempt record.

### Recorded artifacts

A run lives under `benchmarks/runs/beir-nfcorpus/<UTC timestamp>-<inseam commit>/`:

- `manifest.json`: status, phase, attempt history, machine specifications, Inseam identity, dataset pins, model assignments including the vector scope, options, index duration and completion counts, and `scores.beir`.
- `composition.toml`: the exact composition used.
- `queries.jsonl`: per-query timing, the raw Finder results, and the ranked document IDs.
- `query-scores.jsonl`: every metric for every query, for diagnosing which queries moved between runs.
- `run.trec`: the ranking in TREC run format.
- `logs/`: the index log plus attempt-specific query and repair output.

## EnterpriseRAG-Bench

[EnterpriseRAG-Bench](https://github.com/onyx-dot-app/EnterpriseRAG-Bench) is 500 questions over slightly more than 500,000 synthetic enterprise documents. The harness pins dataset release `v1.0.0`, verifies the published checksums, and checks out the evaluator at a pinned revision. Beyond the shared model policy, it uses `stealth/ox-alpha` for answers, citation cleanup, correctness scoring, and fact scoring. Its composition embeds one 200-character summary per source (`vectors = "summaries"`); source text still enters full-text search. Reserve at least 20 GB of local disk before a full run: the 1.26 GB download, its extracted files, and a fresh index.

### Set up the fixture

Run this once from the repository root:

```sh
python3 benchmarks/enterprise_rag_bench.py setup
```

Setup downloads and verifies `all_documents.zip` and `questions.jsonl`, extracts the documents, checks out only the evaluator code at the pinned upstream revision, and creates its Python virtual environment. Everything lands under `benchmark/fixtures/enterprise-rag-bench/`. Git ignores that directory's contents because the fixture is large and upstream asks that the dataset not enter training corpora.

The download resumes if interrupted. Setup refuses a checksum mismatch and refuses to overwrite local changes in the ignored evaluator checkout.

### Run

```sh
export OPENROUTER_API_KEY=...
python3 benchmarks/enterprise_rag_bench.py run
```

A full run creates a fresh index and evaluates all 500 questions. It can take hours and makes many embedding and LLM requests. `--limit` limits questions and evaluator work, but indexing still covers the full corpus so retrieval scores remain meaningful.

To summarize every source in the pinned 511,962-document fixture through the largest configured batch lane, opt into the full transform budget explicitly:

```sh
python3 benchmarks/enterprise_rag_bench.py run \
  --llm-call-budget 511962 \
  --index-concurrency 128
```

`--index-concurrency` remains the interactive-lane fallback. The summary batch lane uses its separately pinned `batch_concurrency = 65536`.

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

### Resume a run

Resume the same run after an indexing, query, answer, or evaluation failure:

```sh
python3 benchmarks/enterprise_rag_bench.py resume <run-id>
```

<<<<<<< HEAD
Use the directory name under `benchmarks/runs/enterprise-rag-bench/` as `<run-id>`. Resume loads the original options and composition, verifies the dataset and model pins, checks that the index command completed successfully, and reuses that run's ignored data directory. It refuses a missing or incomplete index. It starts with the first question that has no durable record, or goes directly to evaluation when every question is complete.
=======
Use the directory name under `benchmarks/runs/` as `<run-id>`. Resume loads the original options and composition, verifies the dataset and model pins, and reuses that run's ignored data directory. If indexing did not finish, it runs the same reconciling sweep again in that node. Sources already marked indexed are unchanged and incur no transform or embedding work; sources that had not reached their indexed mark run again. If indexing finished, resume starts with the first question that has no durable record, or goes directly to evaluation when every question is complete. Each indexing attempt gets its own log.
>>>>>>> 58598a7c (fix(indexing): resume interrupted batch sweeps)

Each invocation is recorded in `manifest.json` under `attempts`, including its start and finish time, duration, Inseam binary identity, search-index preparation timing and log, starting and ending question counts, status, error, and log directory. The top-level duration is the sum of attempt durations. The index record keeps its original duration and structured completion counts for sources, fragments, relations, transforms, embeddings, and spend.

### Recorded artifacts

A run lives under `benchmarks/runs/enterprise-rag-bench/<UTC timestamp>-<inseam commit>/`:

- `manifest.json`: start and finish time, attempt history, total active duration, current phase, OS, CPU, RAM, disk, Inseam CLI version, binary hash, source revision and dirty state, dataset pins, model names, command timeouts, options, index duration and completion counts, score summaries, and completion status.
- `composition.toml`: the exact Inseam composition used.
- `queries.jsonl`: per-question start and finish time, retrieval time, answer time, total time, answer, initial Finder document IDs, agent-cited document IDs, the combined evaluator document set, and every raw Finder result with its address and score.
- `answers.jsonl`: the candidate file sent to EnterpriseRAG-Bench.
- `enterprise-rag-bench-results.json`: the evaluator's per-question results and aggregate scores.
- `evaluator-dependencies.txt`: installed evaluator package versions.
- `logs/`: the one-time index log plus attempt-specific query, agent, and evaluator output.

The manifest's `scores.retrieval` block is computed from the initial ranked Finder results without an LLM. `scores.enterprise_rag_bench` evaluates the combined initial and agent-cited document set and records the upstream correctness, completeness, document recall, and invalid-extra-document aggregates. The raw result file remains authoritative.

## Recorded runs

Commit completed runs under `benchmarks/runs/<benchmark>/<UTC timestamp>-<inseam commit>/`. Only a manifest whose `status` is `completed` should enter comparisons. Compare runs only when dataset pins, model assignments, question or query count, and relevant options match, and call out dirty source trees and hardware differences instead of hiding them.
