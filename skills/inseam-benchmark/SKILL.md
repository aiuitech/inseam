---
name: inseam-benchmark
description: Set up, run, rerun, diagnose, and record Inseam retrieval benchmarks. Use this skill whenever a user mentions benchmarking Inseam, EnterpriseRAG-Bench, BEIR, NFCorpus, nDCG, benchmark fixtures, retrieval scores, indexing performance, or comparing benchmark runs.
---

# Run an Inseam benchmark

Use the repository harness instead of assembling commands by hand. It pins the dataset and evaluator, verifies downloads, creates a fresh index, assigns every LLM role to the required model, and writes the evidence needed to compare runs.

Two benchmarks exist. Pick by what the user needs to measure:

- `benchmarks/beir.py` runs BEIR NFCorpus: retrieval only, 3,633 documents, 323 queries, scored with nDCG@10 and friends. Minutes and cents. Default for "did this retrieval change help".
- `benchmarks/enterprise_rag_bench.py` runs EnterpriseRAG-Bench: half a million documents, agent answers, and an LLM judge. Hours and dollars. Use when the user names it or needs answer quality measured.

## Before running

1. Read `docs/benchmarking.md` and `benchmarks/README.md` from the repository root.
2. Run `git status --short`. Preserve unrelated changes.
3. Run `inseam --version`. If source changed since the installed binary was built, run `cargo install --path crates/inseam-cli` before benchmarking.
4. Confirm `OPENROUTER_API_KEY` is present without printing its value. Setup needs no key; every run does.
5. Check free local disk. A full EnterpriseRAG-Bench run should start with at least 20 GB free. BEIR NFCorpus needs well under 1 GB.

## Set up the pinned fixture

For BEIR NFCorpus:

```sh
python3 benchmarks/beir.py setup
```

It must finish with `benchmarks/fixtures/beir-nfcorpus/setup.json` present. Setup writes one text file per corpus document, keeps only the judged test queries, and copies the test qrels.

For EnterpriseRAG-Bench:

```sh
python3 benchmarks/enterprise_rag_bench.py setup
```

The command is idempotent and resumes the large download. It must finish with `benchmarks/fixtures/enterprise-rag-bench/setup.json` present. Do not copy, commit, summarize, or inspect corpus contents. Do not edit the pinned questions or evaluator. Git intentionally ignores every fixture file except `benchmarks/fixtures/.gitkeep`.

If setup reports a checksum mismatch, report the named file and expected checksum. Do not bypass verification. If it reports local evaluator changes, preserve or remove those changes only with the user's direction.

## Run

BEIR NFCorpus, every test query:

```sh
python3 benchmarks/beir.py run
```

A one-query harness check that still indexes the whole corpus:

```sh
python3 benchmarks/beir.py run --limit 1
```

`--query-limit` (default 10) is both the Finder result count and the deepest metric cutoff; the harness refuses values above 50 because `inseam query` clamps there.

BEIR's summarizer target is a run option. Text within `--summary-target-chars` is its own summary and costs no model call, so a target past the longest abstract (10,092 characters) makes the run model-free — every abstract embedded and searched whole, with its own extractive keywords — and `--summarization-lane interactive` keeps the run's one remaining call (the corpus folder's summary) off a one-request batch job that can sit in OpenRouter's queue:

```sh
python3 benchmarks/beir.py run --summary-target-chars 12000 --llm-call-budget 1 --summarization-lane interactive
```

The model-summary shape — one query-shaped summary and the model's keywords per abstract — is `--summary-target-chars 400 --llm-call-budget 3633` on the default batch lane. Both are recorded in the manifest's `options` and `models.summarization_lane`, so runs of different shapes are never compared as one.

EnterpriseRAG-Bench, full:

```sh
python3 benchmarks/enterprise_rag_bench.py run
```

A harness check that still builds a valid full-corpus index but answers one question:

```sh
python3 benchmarks/enterprise_rag_bench.py run --limit 1 --skip-evaluation
```

Keep the defaults for a comparable rerun unless the user names a different experiment. Never silently reuse an index. Each invocation creates a fresh ignored node data directory and a new versioned run directory under `benchmarks/runs/<benchmark>/`.

The runner announces indexing, search-index preparation, each query's retrieval (and, for EnterpriseRAG-Bench, its answer), and evaluation. Search-index preparation runs `inseam repair` before questions so a legacy node's one-time compact-vector conversion and DiskANN build is visible, timed, logged, and resumable; it reuses resident vectors and does not rerun source indexing or embeddings. Long external commands emit an elapsed-time heartbeat every five seconds. While heartbeats continue, do not diagnose a quiet upstream command as hung. For a live or interrupted run, inspect the newest `manifest.json`: `phase` identifies the active phase, `attempts[-1].search_index_preparation` records the optimization when complete, and `queries_completed` shows durable progress. An intentional interruption records `status: interrupted` and preserves the partial run; do not commit it as a result.

If a run has `status: failed` or `status: interrupted`, resume it instead of starting another run:

```sh
python3 benchmarks/beir.py resume <run-id>
python3 benchmarks/enterprise_rag_bench.py resume <run-id>
```

Resume must target the same run. Do not copy its index into a new run or change its original options. The command verifies the stored pins and composition, restores the durable query checkpoint, and records a new attempt with separate timings, Inseam identity, errors, and logs. If the index completion record is absent, it reruns indexing in the same node. The reconciling sweep skips every source already marked indexed and repeats sources that had not reached that durable mark.

The model assignment is an invariant shared by both benchmarks:

- `google/gemini-2.5-flash-lite` for summaries on OpenRouter's batch lane (`summarizer.llm_lane = "batch"`), with reasoning disabled and its reasoning trace excluded.
- Entity extraction disabled.
- `openai/text-embedding-3-small` at 384 dimensions for embeddings. EnterpriseRAG-Bench embeds summaries only (`vectors = "summaries"`); BEIR embeds every fragment (`vectors = "all"`), recorded as `models.embedding_vectors`.
- EnterpriseRAG-Bench only: `stealth/ox-alpha` for agent answers and evaluation.

The default `--llm-call-budget 500` applies to summaries. State the cost implication before raising it. A value of 500,000 can cause one transform call per source during indexing. For BEIR, `--llm-call-budget 3633` (the corpus size) asks the model for every summary and is the run that measures the full indexing process; it costs well under a dollar on the batch lane.

`indexing.summary.embeddings_reused` and `transforms_reused` count what the node answered from its digest-keyed caches. A fresh run reports zero for both; a resumed attempt reports how much of the interrupted work was kept.

## Verify the run

Open the new `benchmarks/runs/<benchmark>/<run-id>/manifest.json` and check:

1. `status` is `completed`.
2. `queries_completed` matches the requested question or query count.
3. `indexing.duration_seconds` is present and positive, and `indexing.footprint` records the index bytes, source bytes, and their ratio; the final attempt's `search_index_preparation.index_bytes` is the index size after the DiskANN build.
4. The final attempt has `search_index_preparation.duration_seconds` and its log.
5. System specifications and the Inseam CLI version, binary hash, repository revision, and dirty state are present.
6. Every model entry matches the assignments above, including disabled entity extraction and the vector scope.
7. For BEIR: `scores.beir` has `ndcg@10`, `recall@10`, `map@10`, and `precision@10`, and `run.trec` plus `query-scores.jsonl` sit beside the manifest.
8. For EnterpriseRAG-Bench: `scores.retrieval` is present. Unless `--skip-evaluation` was requested, `scores.enterprise_rag_bench` and `enterprise-rag-bench-results.json` are also present.
9. `queries.jsonl` has one row per question or query with durations and raw Finder scores.

Treat the upstream raw results file (EnterpriseRAG-Bench) or `run.trec` (BEIR) as authoritative. Compare runs only when dataset pins, evaluator revision, model assignments, vector scope, question count, and relevant options match. Never compare a BEIR score with an EnterpriseRAG-Bench score. Call out dirty source trees and hardware differences instead of hiding them.

## Finish

Commit the completed run directory and any intentional harness or documentation changes. Git ignores the run's `logs/` and `queries.jsonl` on purpose; do not force-add them. Never commit an interrupted, failed, or running run, `benchmarks/fixtures/` contents, or the ignored Inseam data directory. Mention the run directory, aggregate scores, index duration, total duration, Inseam version, source revision, machine summary, and commit hash in the handoff.
