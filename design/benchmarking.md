# Benchmarking

Benchmarks measure an installed Inseam distribution through its public CLI. They do not call crate internals or add benchmark-only behavior to the node. This keeps the result tied to the binary a user can install and makes the binary hash in a run meaningful.

## Data and code pins

Large third-party corpora live under `benchmark/fixtures/`, outside version control. A setup command owns each fixture and records immutable release, source revision, size, and checksum pins. It verifies bytes before extraction. The repository keeps only `.gitkeep` in the fixture directory.

EnterpriseRAG-Bench release `v1.0.0` is the first fixture. The harness uses the release's `all_documents.zip` and `questions.jsonl`, plus a sparse checkout of evaluator revision `d36685e273713975ee20299bbf1ab64165575b3c`. Pinning the evaluator matters because its prompts and aggregation logic define the score as much as the questions do.

## Fresh runs

Every run gets a fresh Inseam data directory under the ignored fixture tree. Reusing an index would make indexing time meaningless and could mix schema or configuration state from another revision. A run directory under `benchmarks/runs/` stores the small, reviewable evidence and points at its ignored data directory.

The runner uses one query at a time. Query concurrency would make individual latency depend on scheduling and provider contention. Indexing keeps Inseam's configured source concurrency because that is a product throughput control and the run records its value.

All loops and external commands have fixed limits. The harness accepts no more than 1,000 questions, 25 Finder results per query, 64 agent turns, 128 concurrent indexing sources, or 1,000,000 LLM transform calls per type. It also caps each Finder query at 10 minutes, each agent answer at 30 minutes, indexing at 72 hours, and evaluation at seven days. The manifest records those bounds.

## Model policy

`stealth/ox-alpha` handles every language-model role in this benchmark: summaries, entities, agent answers, and the upstream LLM judge. The composition and run manifest state that assignment explicitly. Embeddings are a separate model class and remain `openai/text-embedding-3-small`.

Transform call budgets stay explicit. The default is 500 summary calls and 500 entity calls per indexing run, followed by Inseam's deterministic fallback. A full 500,000-call budget for both transforms would approach one million model requests before question answering, so the operator must opt into that cost.

## Evidence in a run

`manifest.json` is the index for a run. It records time, machine specifications, dataset and evaluator pins, model assignments, exact Inseam identity, options, indexing duration, completed query count, aggregate scores, and terminal status. The exact composition, raw per-result Finder scores, per-query and per-answer durations, answer file, upstream results, dependency versions, and command logs sit beside it.

The runner writes its current phase before indexing, querying, and evaluation, then updates the manifest after every question. It also emits a five-second elapsed-time heartbeat around each external command. A crash leaves a useful failed, interrupted, or running record instead of an apparently complete score. Only a manifest whose status is `completed` should enter comparisons.

## Performance sketch

The fixed network floor is a 1.26 GB dataset download. Extraction produces more than 500,000 files, so metadata operations and local disk latency dominate setup and cataloging. Endpoint embeddings dominate indexing network traffic and typically dominate elapsed indexing time; the LLM transforms add at most twice the configured call budget. At 1,536 float dimensions, one vector per document already represents about 3.1 GB of raw `f32` values before fragment vectors and database overhead. A full run should start with at least 20 GB free, use local SSD storage, and optimize embedding batching and disk writes before query execution.

Question execution makes one timed Finder query and one timed agent loop per question. The default cap is 500 Finder queries and 6,000 agent turns. Evaluation then makes bounded upstream judge calls. The runner records these phases separately because combining them would hide whether a change affected retrieval, answer generation, or judging.
