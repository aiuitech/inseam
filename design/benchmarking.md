# Benchmarking

Benchmarks measure an installed Inseam distribution through its public CLI. They do not call crate internals or add benchmark-only behavior to the node. This keeps the result tied to the binary a user can install and makes the binary hash in a run meaningful.

## Two benchmarks, one harness

EnterpriseRAG-Bench measures the whole product: indexing at scale, retrieval, agent answers, and an LLM judge. It is the benchmark to trust and the expensive one to run. BEIR NFCorpus measures retrieval alone on a corpus small enough to index in minutes for cents. It exists so a retrieval change can be measured on every iteration instead of once a week, and so the harness itself can be exercised end to end without a 20 GB fixture.

Both runners are thin. Everything that is the same for every benchmark lives in one shared module: bounded external commands with heartbeats, verified downloads, run identity and machine specifications, the `inseam index` and `inseam repair` steps, and the attempt ledger. Each runner owns only what makes its benchmark different: dataset pins, composition, the per-question commands, and scoring. Duplicating the plumbing would let the two runners drift apart in exactly the places where comparability depends on them staying identical.

## Data and code pins

Large third-party corpora live under `benchmark/fixtures/`, outside version control. A setup command owns each fixture and records immutable release, source revision, size, and checksum pins. It verifies bytes before extraction. The repository keeps only `.gitkeep` in the fixture directory.

EnterpriseRAG-Bench release `v1.0.0` is the first fixture. The harness uses the release's `all_documents.zip` and `questions.jsonl`, plus a sparse checkout of evaluator revision `d36685e273713975ee20299bbf1ab64165575b3c`. Pinning the evaluator matters because its prompts and aggregation logic define the score as much as the questions do.

BEIR NFCorpus is the second fixture. It is pinned by the SHA-256 of the archive BEIR's own loader downloads, and by its row counts: 3,633 documents, 323 test queries, 12,334 judgments. Row counts are a second pin because the archive carries three splits and the harness must select exactly the test split BEIR publishes numbers for. NFCorpus was chosen over the other small BEIR datasets (SciFact, ArguAna) because it has the smallest corpus, graded rather than binary judgments, and many judged documents per query, which makes recall and nDCG both informative on a ranking of ten.

## Fresh runs

Every new run gets a fresh Inseam data directory under the ignored fixture tree. Reusing an index in a different run would make indexing time meaningless and could mix schema or configuration state from another revision. Resuming the same run is different: the runner verifies that its index completed, preserves the original indexing record, and continues from its durable question checkpoint. A run directory under `benchmarks/runs/<benchmark>/` stores the small, reviewable evidence and points at its ignored data directory.

The runner uses one query at a time. Query concurrency would make individual latency depend on scheduling and provider contention. Indexing keeps Inseam's configured source concurrency because that is a product throughput control and the run records its value.

All loops and external commands have fixed limits. The EnterpriseRAG-Bench harness accepts no more than 1,000 questions, 25 Finder results per query, 64 agent turns, 128 concurrent indexing sources, or 1,000,000 LLM transform calls per type. It also caps each Finder query at 10 minutes, each agent answer at 30 minutes, indexing at 72 hours, and evaluation at seven days. The BEIR harness accepts no more than 1,000 queries, 10,000 documents, 100,000 judgments, and 50 Finder results per query, the last because `inseam query` clamps its limit there and a deeper cutoff would silently score a truncated ranking. The manifest records those bounds.

## Model policy

`google/gemini-2.5-flash-lite` handles summaries on OpenRouter's batch lane (`summarizer.llm_lane = "batch"`, `design/indexing.md`), with low reasoning effort and the unused reasoning trace excluded. The sweep parks thousands of planners on their summary calls and one batch job carries them, instead of one HTTP round trip apiece. Entity extraction is disabled. `stealth/ox-alpha` handles agent answers and the upstream judge, while `openai/text-embedding-3-small` embeds at 384 dimensions. The composition and run manifest state every assignment and the reduced width explicitly. The policy is one policy: both runners import the same model constants, so a change to it moves both benchmarks together and shows up in both manifests.

Transform call budgets stay explicit. The default is 500 summary calls per indexing run, followed by Inseam's deterministic fallback. A full 500,000-call budget therefore means at most one model request per source before question answering, and the operator must opt into that cost.

The EnterpriseRAG-Bench composition bounds the vector surface to one 200-character summary per source and disables markdown splitting, chunking, and entities. Full source text remains in full-text search. This trades structural and graph recall for predictable indexing time and storage; benchmark scores, not an assumption, decide whether that trade is acceptable.

The BEIR composition keeps everything else identical but embeds every fragment (`vectors = "all"`). At 3,633 short abstracts the full surface costs cents, and a retrieval benchmark should measure the search surface a user gets by default rather than the lean shape adopted for a half-million-document corpus. The manifest records the scope under `models.embedding_vectors` so the two benchmarks are never compared as if they shared a composition.

## Scoring

EnterpriseRAG-Bench's score is the upstream evaluator's, run at its pinned revision; the harness adds a retrieval block computed from the initial Finder ranking without an LLM.

BEIR's score is computed in the harness rather than through a pinned upstream package. The metrics are deterministic functions of a ranking and the judgments, trec_eval's `ndcg_cut`, `map_cut`, `recall`, and `P`, so pinning an evaluator buys nothing that a tested implementation does not, and it would add a compiled dependency and a virtual environment to a benchmark whose point is to be cheap. The implementation follows trec_eval's conventions exactly (linear gains, log2(rank + 1) discount, recall and average precision over every judged relevant document, grade above zero counts as relevant) and was checked against `pytrec_eval` on real NFCorpus rankings. Each run also writes the ranking in TREC run format so the score can be reproduced with any standard tool; that file, not the harness, is the evidence of record if the two ever disagree.

## Evidence in a run

`manifest.json` is the index for a run. It records time, machine specifications, dataset and evaluator pins, model assignments, exact Inseam identity, options, indexing duration and completion counts, completed query count, aggregate scores, and terminal status. Each invocation has an attempt record with its own timing, Inseam identity, checkpoint counts, error, and log directory. The exact composition, raw per-result Finder scores, per-query durations, and command logs sit beside it. EnterpriseRAG-Bench adds the answer file, upstream results, and dependency versions; BEIR adds per-query metrics and the TREC run file.

The runner writes its current phase before indexing, querying, and evaluation. During indexing it polls the public `inseam status` command every 30 seconds and reports indexed and cataloged sources against the pinned fixture count. A transient probe failure never changes the index result. Before the first query in every attempt it runs `inseam repair`, which makes any one-time search optimization explicit instead of hiding it inside startup, status, or query latency; the attempt records that duration and log. The repair reuses resident vectors and never reruns source indexing or embedding. After every answer, the runner atomically rewrites the query checkpoint before updating the manifest count. Other external commands emit a five-second elapsed-time heartbeat. A crash leaves a useful failed, interrupted, or running record instead of an apparently complete score. Only a manifest whose status is `completed` should enter comparisons.

## Performance sketch

EnterpriseRAG-Bench's fixed network floor is a 1.26 GB dataset download. Extraction produces more than 500,000 files, so metadata operations and local disk latency dominate setup and cataloging. With the default 500-call transform budget, the batch lane parks every summary call of the run and one OpenRouter batch job carries all 500 rather than 500 synchronous request round trips. The lean composition asks for roughly one 384-dimensional vector per source: at 512,000 sources that is about 0.79 GB of raw `f32` values before database overhead, versus about 3.15 GB for one 1,536-dimensional vector per source and substantially more when structural fragments are embedded. A 256-search-row sweep batch contains about 128 summary vectors and fills one base64 endpoint request; four batches run concurrently, for at most 512 embedding inputs in flight. A full run should start with at least 20 GB free and use local SSD storage.

Question execution makes one timed Finder query and one timed agent loop per question. Vector retrieval is DiskANN candidate lookup rather than a scan over the raw 384-dimensional corpus, and graph propagation materializes only a bounded seed-local neighborhood. The default cap is 500 Finder queries and 6,000 agent turns. Evaluation then makes bounded upstream judge calls. The runner records these phases separately because combining them would hide whether a change affected search-index preparation, retrieval, answer generation, or judging.

BEIR NFCorpus is small in every dimension: a 2.4 MB download, 3,633 files totalling 5.8 MB of text, roughly 1.5 million embedding tokens for the full-fragment surface (a few cents), the same 500-call summary budget carried by one batch job, and 323 Finder queries with no agent loop. Wall-clock time is dominated by the batch job's turnaround and the embedding requests, not by local disk or CPU; the whole run fits in the time an EnterpriseRAG-Bench run spends cataloging.
