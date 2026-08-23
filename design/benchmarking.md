# Benchmarking

Benchmarks measure an installed Inseam distribution through its public CLI. They do not call crate internals or add benchmark-only behavior to the node. This keeps the result tied to the binary a user can install and makes the binary hash in a run meaningful.

## Data and code pins

Large third-party corpora live under `benchmark/fixtures/`, outside version control. A setup command owns each fixture and records immutable release, source revision, size, and checksum pins. It verifies bytes before extraction. The repository keeps only `.gitkeep` in the fixture directory.

EnterpriseRAG-Bench release `v1.0.0` is the first fixture. The harness uses the release's `all_documents.zip` and `questions.jsonl`, plus a sparse checkout of evaluator revision `d36685e273713975ee20299bbf1ab64165575b3c`. Pinning the evaluator matters because its prompts and aggregation logic define the score as much as the questions do.

## Fresh runs

Every new run gets a fresh Inseam data directory under the ignored fixture tree. Reusing an index in a different run would make indexing time meaningless and could mix schema or configuration state from another revision. Resuming the same run is different: the runner verifies that its index completed, preserves the original indexing record, and continues from its durable question checkpoint. A run directory under `benchmarks/runs/` stores the small, reviewable evidence and points at its ignored data directory.

The runner uses one query at a time. Query concurrency would make individual latency depend on scheduling and provider contention. Indexing keeps Inseam's configured source concurrency because that is a product throughput control and the run records its value.

All loops and external commands have fixed limits. The harness accepts no more than 1,000 questions, 25 Finder results per query, 64 agent turns, 128 concurrent indexing sources, or 1,000,000 LLM transform calls per type. It also caps each Finder query at 10 minutes, each agent answer at 30 minutes, indexing at 72 hours, and evaluation at seven days. The manifest records those bounds.

## Model policy

`stealth/ox-alpha` handles summaries, entities, agent answers, and the upstream judge through OpenRouter. `openai/text-embedding-3-small` embeds summaries at 384 dimensions. The composition and run manifest state every assignment and the reduced width explicitly.

Transform call budgets stay explicit. The default is 500 summary calls and 500 entity calls per indexing run, followed by Inseam's deterministic fallback. A full 500,000-call budget for both transforms would approach one million model requests before question answering, so the operator must opt into that cost.

The benchmark composition bounds the vector surface to one 200-character summary per source and disables markdown splitting and chunking. Full source text remains in full-text search, and the bounded entity transform remains mounted. This trades structural vector recall for predictable indexing time and storage; benchmark scores, not an assumption, decide whether that trade is acceptable.

## Evidence in a run

`manifest.json` is the index for a run. It records time, machine specifications, dataset and evaluator pins, model assignments, exact Inseam identity, options, indexing duration and completion counts, completed query count, aggregate scores, and terminal status. Each invocation has an attempt record with its own timing, Inseam identity, checkpoint counts, error, and log directory. The exact composition, raw per-result Finder scores, per-query and per-answer durations, answer file, upstream results, dependency versions, and command logs sit beside it.

The runner writes its current phase before indexing, querying, and evaluation. During indexing it polls the public `inseam status` command every 30 seconds and reports indexed and cataloged sources against the pinned fixture count. A transient probe failure never changes the index result. Before the first query in every attempt it runs `inseam repair`, which makes any one-time search optimization explicit instead of hiding it inside startup, status, or query latency; the attempt records that duration and log. The repair reuses resident vectors and never reruns source indexing or embedding. After every answer, the runner atomically rewrites the query and answer checkpoints before updating the manifest count. Other external commands emit a five-second elapsed-time heartbeat. A crash leaves a useful failed, interrupted, or running record instead of an apparently complete score. Only a manifest whose status is `completed` should enter comparisons.

## Performance sketch

The fixed network floor is a 1.26 GB dataset download. Extraction produces more than 500,000 files, so metadata operations and local disk latency dominate setup and cataloging. The lean composition asks for roughly one 384-dimensional vector per source: at 512,000 sources that is about 0.79 GB of raw `f32` values before database overhead, versus about 3.15 GB for one 1,536-dimensional vector per source and substantially more when structural fragments are embedded. Each endpoint request carries a complete 128-row batch as base64, and four requests run concurrently, for at most 512 embedding inputs in flight. A full run should start with at least 20 GB free and use local SSD storage.

Question execution makes one timed Finder query and one timed agent loop per question. Vector retrieval is DiskANN candidate lookup rather than a scan over the raw 384-dimensional corpus, and graph propagation materializes only a bounded seed-local neighborhood. The default cap is 500 Finder queries and 6,000 agent turns. Evaluation then makes bounded upstream judge calls. The runner records these phases separately because combining them would hide whether a change affected search-index preparation, retrieval, answer generation, or judging.
