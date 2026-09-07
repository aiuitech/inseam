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

Both runners name `google/gemini-2.5-flash-lite` through OpenRouter's batch lane for summaries, with reasoning disabled and the reasoning trace excluded from the response, and `openai/text-embedding-3-small` at 384 dimensions for embeddings; entity extraction and chunking are disabled. What each run actually buys is a run option. BEIR embeds every abstract whole (`--summary-target-chars` past the longest abstract makes each its own summary, no model call) and searches by full-text and vector seeds together. EnterpriseRAG-Bench's defaults are the shape its corpus rewarded ([design/benchmarking.md](../design/benchmarking.md)): no embedder (`--vectors none`), no structural transform, no model calls (`--llm-call-budget 0`), and a summary target past the longest document, so each document is one full-text row holding its whole text. Its answer model (`--answer-model`, through `inseam agent`) and judge model (`--evaluation-model`, through the upstream evaluator) default to `z-ai/glm-5.3-flash` and are recorded in the manifest; the leaderboard fixes neither — its baselines answer with GPT-5.4 and product submissions bring their own — and `stealth/ox-alpha`, the first choice here, has left OpenRouter.

Every `run` invocation creates a new ignored index under the fixture's `nodes/` directory. This prevents a warm index from being reported as a fresh indexing result. Failed and interrupted runs remain on disk with their partial logs and a non-completed manifest.

BEIR's default run limits summaries to 500 LLM calls, and EnterpriseRAG-Bench's makes none. Inseam uses its deterministic fallback after the summarizer spends its budget. Raise `--llm-call-budget` only after estimating the cost and runtime; a budget of at least the corpus size (3,633 for BEIR NFCorpus) asks the model for every summary, which is the run that measures the full indexing process. Summaries ride the batch lane (`summarizer.llm_lane = "batch"`). EnterpriseRAG-Bench sets OpenRouter's job cap to its 5,000-request limit and parks up to 65,536 source planners, so the endpoint can keep its eight job slots busy; the 64 MiB serialized-job limit may split large requests sooner. Embeddings use base64 responses and pack up to 128 inputs per request, with four batches in flight. The lean source-plus-summary shape fills an embedding request from 256 search rows.

Every run records the index's **footprint** beside its duration: the bytes of the node's data directory (database plus write-ahead log) against the bytes of the source documents it was built from, and the ratio between them, measured right after indexing (`indexing.footprint`) and again after the DiskANN build (`search_index_preparation.index_bytes`). The index summary also records how much of the run the node answered from its digest-keyed caches (`embeddings_reused`, `transforms_reused`); a fresh node reports zero for both.

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

Setup downloads and verifies `nfcorpus.zip`, extracts it, writes each corpus row as `benchmarks/fixtures/beir-nfcorpus/documents/<document-id>.txt` with the title on the first line, keeps only the test split's judged queries in `queries.jsonl`, copies the test qrels, and records the pins in `setup.json`. The file name is the BEIR document ID, which is how a Finder result address maps back to a judgment. Setup is idempotent and needs no API key. Git ignores the fixture directory.

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
  --llm-call-budget 500 \
  --summary-target-chars 200 \
  --summarization-lane batch
```

`--limit` is how many test queries to run, in BEIR's order; indexing always covers the whole corpus. `--query-limit` is the number of Finder results per query and also the deepest metric cutoff. `inseam query` clamps its limit to 50, so the harness refuses a larger value instead of scoring a truncated ranking. The default of 10 reports nDCG@10, BEIR's headline number.

`--summary-target-chars` is the summarizer's target length. Text within it is its own summary (`via=verbatim`) and costs no model call, so the two shapes worth comparing are:

- **model-free**: `--summary-target-chars 12000 --llm-call-budget 1 --summarization-lane interactive` — every abstract (the longest is 10,092 characters) is embedded whole and searchable whole, with its own extractive keywords beside it. The one budgeted call summarizes the corpus folder; it rides the interactive lane because a one-request batch job can sit in OpenRouter's queue for longer than the rest of the run.
- **model summaries**: `--summary-target-chars 400 --llm-call-budget 3633` — one query-shaped summary and the model's keywords per abstract, on the batch lane; the vector is over the summary, and full-text search sees the summary and the keywords, never the abstract.

Unlike the EnterpriseRAG-Bench composition, the BEIR composition sets `vectors = "all"`; with the structural transforms disabled that is still one vector per source, the summary's, and the difference between the shapes is what that summary is. The corpus is 5.8 MB of text, so embedding each abstract whole costs cents.

Four more dials isolate where a score comes from, each recorded in the manifest's `options` and `models`:

- `--embedding-dimensions` (384): the embedder's width, up to the model's native 1536. The footprint line shows what a wider vector costs.
- `--finder-seeds` (`both`): `full-text` or `vector` runs one seed list alone, which shows which search the fusion is carrying.
- `--corpus` (`text`): `markdown` indexes the variant setup writes beside the text one, `documents-markdown/<id>.md`, the same row with the title as a `#` heading, so the markdown transform reads the document as an outline and the summarizer leads with the title.
- `--structural` (`off`): `markdown` mounts the markdown structural transform, so each document also contributes its section as a content row beside its summary.

### Scores

The harness computes trec_eval's `ndcg_cut`, `map_cut`, `recall`, and `P` at cutoffs 1, 3, 5, and 10 (plus `--query-limit` when it is larger), averaged over the queries that ran, and rounds to five decimals the way BEIR reports them. Gains are the raw grades with a log2(rank + 1) discount; recall and average precision divide by every judged relevant document, not only the ones inside the cutoff; a grade above zero counts as relevant. The implementation was checked against `pytrec_eval` on real NFCorpus rankings and agrees to floating-point precision.

Each run also writes `run.trec`, the ranking in TREC run format, so anyone can rescore it independently:

```sh
trec_eval -m ndcg_cut.10 -m recall.10 benchmarks/fixtures/beir-nfcorpus/qrels.tsv benchmarks/runs/beir-nfcorpus/<run-id>/run.trec
```

For a smoke check after setup:

```sh
python3 benchmarks/beir.py run --limit 1
```

### Resume a run

```sh
python3 benchmarks/beir.py resume <run-id>
```

Resume works exactly as it does for EnterpriseRAG-Bench below: same run directory, same options, verified pins and composition, the index finished in the same node if its attempt did not complete, continue from the durable query checkpoint, and a new attempt record. A run whose process was killed outright (out of memory, a lost terminal) still reads `running`, because nothing was left to record an outcome; resume refuses it, since another process may own the run, unless you pass `--after-kill`, which closes that attempt as interrupted with no duration and continues. The manifest's `attempts_unmeasured` counts such attempts beside the total duration.

### Recorded artifacts

A run lives under `benchmarks/runs/beir-nfcorpus/<UTC timestamp>-<inseam commit>/`:

- `manifest.json`: status, phase, attempt history, machine specifications, Inseam identity, dataset pins, model assignments including the vector scope, options, index duration and completion counts, and `scores.beir`.
- `composition.toml`: the exact composition used.
- `query-scores.jsonl`: every metric for every query, for diagnosing which queries moved between runs.
- `run.trec`: the ranking in TREC run format.
- `queries.jsonl` (local only): per-query timing, the raw Finder results, and the ranked document IDs.
- `logs/` (local only): the index log plus attempt-specific query and repair output.

## EnterpriseRAG-Bench

[EnterpriseRAG-Bench](https://github.com/onyx-dot-app/EnterpriseRAG-Bench) is 500 questions over slightly more than 500,000 synthetic enterprise documents. The harness pins dataset release `v1.0.0`, verifies the published checksums, and checks out the evaluator at a pinned revision. Beyond the shared model policy, `--answer-model` and `--evaluation-model` (default `z-ai/glm-5.3-flash`) answer and judge. Its default composition mounts no embedder and holds each document as one full-text row of its whole text (`--vectors none`, `--structural off`, `--summary-target-chars 24000`, `--llm-call-budget 0`); see the slice and retrieval-only section below for how that shape was chosen and how to try another. Reserve at least 20 GB of local disk before a full run: the 1.26 GB download, its extracted files, and a fresh index.

The point-in-time [leaderboard research](enterprise-rag-leaderboard-research.md) records what the public results and system disclosures imply for Finder experiments. It separates verified submission facts from vendor descriptions and speculation.

### Set up the fixture

Run this once from the repository root:

```sh
python3 benchmarks/enterprise_rag_bench.py setup
```

Setup downloads and verifies `all_documents.zip` and `questions.jsonl`, extracts the documents, checks out only the evaluator code at the pinned upstream revision, and creates its Python virtual environment. Everything lands under `benchmarks/fixtures/enterprise-rag-bench/`. Git ignores that directory's contents because the fixture is large and upstream asks that the dataset not enter training corpora.

The download resumes if interrupted. Setup refuses a checksum mismatch and refuses to overwrite local changes in the ignored evaluator checkout.

### Run

```sh
export OPENROUTER_API_KEY=...
python3 benchmarks/enterprise_rag_bench.py run
```

A full run creates a fresh index and evaluates all 500 questions. It can take hours and makes many embedding and LLM requests. `--limit` limits questions and evaluator work, but indexing still covers the full corpus so retrieval scores remain meaningful.

To summarize every source in the pinned 511,962-document fixture through the largest configured batch lane, opt into the full transform budget explicitly, and give the summarizer a target the documents do not fit in, or it has nothing to shorten:

```sh
python3 benchmarks/enterprise_rag_bench.py run \
  --llm-call-budget 511962 \
  --summary-target-chars 400 \
  --index-concurrency 128
```

`--index-concurrency` remains the interactive-lane fallback. The summary batch lane uses its separately pinned `batch_concurrency = 65536`. `--llm-call-budget 0` makes the index model-free: every summary is extractive, and the run costs only its embeddings.

### Iterate on a slice, retrieval only

A full index takes hours, so a change to how the corpus is indexed is compared on a **slice** first:

```sh
python3 benchmarks/enterprise_rag_bench.py run \
  --skip-agent \
  --corpus-slice 25000 \
  --llm-call-budget 0 \
  --structural markdown \
  --summary-target-chars 600
```

`--corpus-slice N` indexes every document any question expects plus a seeded random sample of the rest, N documents in all, hard-linked once under `benchmarks/fixtures/enterprise-rag-bench/slices/<N>/documents`. The seed is fixed, so every slice of a size holds the same documents. `--skip-agent` runs only the Finder query per question and scores the retrieval block (document recall, hit rate, mean reciprocal rank over the top `--query-limit`); it implies `--skip-evaluation`. Slice scores are development numbers — the distractor set is a fraction of the corpus, so compare them only with other slices of the same size, never with a full run — and the manifest records `models.corpus` as `slice-<N>` so the two are never confused. The strategies measured this way, and what each cost, are in [design/benchmarking.md](../design/benchmarking.md).

The composition dials, each recorded under the manifest's `options` and `models`:

- `--structural` (`off`): `markdown` mounts the markdown structural transform, which also claims plain text, so each document's text reaches full-text search through its sections. With it off, the summary and keywords are the only text rows.
- `--summary-target-chars` (200): the summarizer's target; text within it is its own summary. A target past the longest document embeds every document whole.
- `--keywords-max` (12): keywords planted beside each summary; 0 plants none.
- `--vectors` (`summaries`): the embedder's scope; `all` embeds every section too.
- `--finder-seeds` (`both`): `full-text` or `vector` runs one seed list alone, a diagnostic for which search the fusion is carrying.
- `--hints-llm-call-budget` (0): mounts the hints transform with that many calls per run, one per document, planting cues, a synopsis, discriminators, and shared glossary, identifier, and entity fragments ([design/indexing.md](../design/indexing.md)); about $4 on the batch lane for a 25,000-document slice, and the batch jobs can take hours to return.

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

Use the directory name under `benchmarks/runs/enterprise-rag-bench/` as `<run-id>`. Resume loads the original options and composition, verifies the dataset and model pins, and reuses that run's ignored data directory. If indexing did not finish, it runs the same reconciling sweep again in that node. Sources already marked indexed are unchanged and incur no transform or embedding work; sources that had not reached their indexed mark run again, taking any summary or vector the node's caches already hold. If indexing finished, resume starts with the first question that has no durable record, or goes directly to evaluation when every question is complete. Each indexing attempt gets its own log.

Each invocation is recorded in `manifest.json` under `attempts`, including its start and finish time, duration, Inseam binary identity, search-index preparation timing and log, starting and ending question counts, status, error, and log directory. The top-level duration is the sum of attempt durations. The index record keeps its original duration and structured completion counts for sources, fragments, relations, transforms, embeddings, and spend.

### Recorded artifacts

A run lives under `benchmarks/runs/enterprise-rag-bench/<UTC timestamp>-<inseam commit>/`:

- `manifest.json`: start and finish time, attempt history, total active duration, current phase, OS, CPU, RAM, disk, Inseam CLI version, binary hash, source revision and dirty state, dataset pins, model names, command timeouts, options, index duration and completion counts, score summaries, and completion status.
- `composition.toml`: the exact Inseam composition used.
- `queries.jsonl` (local only): per-question start and finish time, retrieval time, answer time, total time, answer, initial Finder document IDs, agent-cited document IDs, the combined evaluator document set, and every raw Finder result with its address and score.
- `answers.jsonl`: the candidate file sent to EnterpriseRAG-Bench.
- `enterprise-rag-bench-results.json`: the evaluator's per-question results and aggregate scores.
- `evaluator-dependencies.txt`: installed evaluator package versions.
- `logs/` (local only): the one-time index log plus attempt-specific query, agent, and evaluator output.

The manifest's `scores.retrieval` block is computed from the initial ranked Finder results without an LLM. `scores.enterprise_rag_bench` evaluates the combined initial and agent-cited document set and records the upstream correctness, completeness, document recall, and invalid-extra-document aggregates. The raw result file remains authoritative.

## Recorded runs

Commit completed runs under `benchmarks/runs/<benchmark>/<UTC timestamp>-<inseam commit>/`. Git versions only the result of a run: the manifest, the composition, the scores, and the ranking or answers the scores were computed from. The `logs/` directory and `queries.jsonl` stay local because they are large, reproducible from the pins, and never needed to compare two runs; `benchmarks/.gitignore` enforces this. Runs whose status is `interrupted`, `failed`, or `running` are not results and must not be committed. Only a manifest whose `status` is `completed` should enter comparisons. Compare runs only when dataset pins, model assignments, question or query count, and relevant options match, and call out dirty source trees and hardware differences instead of hiding them.
