---
name: inseam-benchmark
description: Set up, run, rerun, diagnose, and record Inseam retrieval benchmarks. Use this skill whenever a user mentions benchmarking Inseam, EnterpriseRAG-Bench, benchmark fixtures, retrieval scores, indexing performance, or comparing benchmark runs.
---

# Run an Inseam benchmark

Use the repository harness instead of assembling commands by hand. It pins the dataset and evaluator, verifies downloads, creates a fresh index, assigns every LLM role to the required model, and writes the evidence needed to compare runs.

## Before running

1. Read `docs/benchmarking.md` and `benchmarks/README.md` from the repository root.
2. Run `git status --short`. Preserve unrelated changes.
3. Run `inseam --version`. If source changed since the installed binary was built, run `cargo install --path crates/inseam-cli` before benchmarking.
4. Confirm `OPENROUTER_API_KEY` is present without printing its value.
5. Check free local disk. A full EnterpriseRAG-Bench run should start with at least 20 GB free.

## Set up the pinned fixture

Run:

```sh
python3 benchmarks/enterprise_rag_bench.py setup
```

The command is idempotent and resumes the large download. It must finish with `benchmark/fixtures/enterprise-rag-bench/setup.json` present. Do not copy, commit, summarize, or inspect corpus contents. Do not edit the pinned questions or evaluator. Git intentionally ignores every fixture file except `benchmark/fixtures/.gitkeep`.

If setup reports a checksum mismatch, report the named file and expected checksum. Do not bypass verification. If it reports local evaluator changes, preserve or remove those changes only with the user's direction.

## Run

For the full benchmark:

```sh
python3 benchmarks/enterprise_rag_bench.py run
```

For a harness check that still builds a valid full-corpus index but answers one question:

```sh
python3 benchmarks/enterprise_rag_bench.py run --limit 1 --skip-evaluation
```

Keep the defaults for a comparable rerun unless the user names a different experiment. Never silently reuse an index. Each invocation creates a fresh ignored node data directory and a new versioned run directory.

The model assignment is an invariant:

- `stealth/ox-alpha` for summarization, entity extraction, agent answers, and EnterpriseRAG-Bench evaluation.
- `openai/text-embedding-3-small` for embeddings.

The default `--llm-call-budget 500` applies independently to summaries and entities. State the cost implication before raising it. A value of 500,000 can cause close to one million transform calls during indexing.

## Verify the run

Open the new `benchmarks/runs/<run-id>/manifest.json` and check:

1. `status` is `completed`.
2. `queries_completed` matches the requested question count.
3. `indexing.duration_seconds` is present and positive.
4. System specifications and the Inseam CLI version, binary hash, repository revision, and dirty state are present.
5. Every language-model entry is `stealth/ox-alpha`; only the embedding entry differs.
6. `scores.retrieval` is present. Unless `--skip-evaluation` was requested, `scores.enterprise_rag_bench` and `enterprise-rag-bench-results.json` are also present.
7. `queries.jsonl` has one row per question with retrieval, answer, and total durations plus raw Finder scores.

Treat the upstream raw results file as authoritative. Compare runs only when dataset pins, evaluator revision, model assignments, question count, and relevant options match. Call out dirty source trees and hardware differences instead of hiding them.

## Finish

Commit the completed run directory and any intentional harness or documentation changes. Never commit `benchmark/fixtures/` contents or the ignored Inseam data directory. Mention the run directory, aggregate scores, index duration, total duration, Inseam version, source revision, machine summary, and commit hash in the handoff.
