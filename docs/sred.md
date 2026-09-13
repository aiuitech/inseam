# Inseam SR&ED technical project record

**Prepared:** 13 September 2026. **Status:** working technical dossier for claimant review, not a submitted claim or a determination of eligibility.

This document consolidates the work visible in the repository through `36eef31`, including the indexing and Finder experiments, unsuccessful approaches, implementation history, validation, costs, and unresolved questions. The reviewed history contains 192 commits, beginning on 9 August 2026. Those dates establish the available repository record, not the start of eligible SR&ED work or hours worked. Concurrent uncommitted vocabulary, routing, and call-capture changes are not represented as completed or experimentally validated here.

This is a retrospective consolidation of existing records. The linked commits, design documents, test artifacts, benchmark configurations, and run manifests are the underlying evidence. Where a hypothesis is reconstructed from a design or change description, the technical lead must confirm that it reflects the question actually investigated. Do not backdate this document or represent its preparation date as the date of an earlier experiment.

## 1. Project identification and filing information

| Field | Current record |
| --- | --- |
| Proposed project title | Inseam: bounded enterprise evidence discovery |
| Internal reference | Inseam indexing and Finder experimental development |
| Claimant legal name and business number | **TO CONFIRM** from corporate records. The repository name is not a legal claimant. |
| Tax year start and end | **TO CONFIRM.** Restrict any submitted narrative and expenditures to that year. |
| SR&ED project start | **TO CONFIRM:** date the relevant technological uncertainty was identified and investigation began. Earliest repository evidence is 2026-08-09. |
| Project completion or expected completion | **TO CONFIRM.** Research questions remain open at the preparation date. |
| Technical lead and qualifications | **TO CONFIRM.** Git identifies Greg Hunt as an author, but does not establish employment, qualifications, duties, or time allocation. |
| Other contributors, contractors, and collaborators | **TO CONFIRM** names, roles, employer/contract status, agreements, qualifications, and work actually performed. |
| Work locations | **TO CONFIRM** Canadian and any non-Canadian work locations by person and period. Machine names and account settings are not proof of where work occurred. |
| Previous claim or pre-claim approval | **TO CONFIRM.** No status is inferred from the repository. |
| T661 field codes | Software/information retrieval is the technical subject. Confirm the applicable line 206 code and line 207 CRDC field-of-research code. |
| Financial contact, expenditures, assistance, and contract payments | **TO CONFIRM** from payroll, invoices, contracts, and accounting records. |

The current CRA form reviewed for this draft is **T661 E (26)**. It retains technical descriptions at lines 242, 244, and 246, with maximum lengths of 350, 700, and 350 words. It also includes the CRDC field code at line 207. The CRA says its guide is being revised and supplies interim instructions for the updated form. [Current T661 form](https://www.canada.ca/content/dam/cra-arc/formspubs/pbg/t661/t661-26e.pdf), [CRA update instructions](https://www.canada.ca/en/revenue-agency/services/scientific-research-experimental-development-tax-incentive-program/sred-updates.html).

The project proposed here is the indexing/retrieval investigation. Related platform work is catalogued in section 7 so it is not lost, but its inclusion in the same SR&ED project requires a demonstrated connection to the same advancement. Software delivery, an improved score, or using AI does not by itself establish eligibility. CRA requires an advancement sought through systematic investigation or search by experiment or analysis, including evidence of the work. [CRA eligibility guidelines](https://www.canada.ca/en/revenue-agency/services/scientific-research-experimental-development-tax-incentive-program/sred-policies-guidelines/guidelines-eligibility-work-sred-tax-incentives.html).

## 2. Technical objective and starting knowledge

Inseam seeks to let an agent discover and read relevant evidence from files, email, chat, tickets, transcripts, and other records through a bounded interface. Different nodes can maintain different local indexes while routing source reads through authorized operations. The local derived index can contain source text, summaries, vectors, and graph relations; the privacy objective is not a claim that the index contains no document text.

The experimental objective is to determine which combinations of source representation, search channels, graph propagation, and incremental reading preserve the facts needed for correct answers at practical storage and execution costs. The workload includes a 511,962-document enterprise corpus with near-duplicate records, internal terminology, conflicting facts, and questions requiring different amounts of evidence.

Established components available at the start included full-text ranking with BM25, dense embeddings, reciprocal-rank fusion, personalized PageRank, approximate nearest-neighbour search, relational storage, language-model summaries, and tool-using agents. These were building blocks, not inventions claimed by this project. The investigation concerns their interactions in the proposed system: changing row granularity changes ranking statistics; adding shared terms changes graph connectivity; a relevant document can be retrieved but remain unread; and a compact response can hide a necessary qualifier.

The repository records architecture choices and measured failures, but does not establish a complete contemporaneous review of all reasonably available public knowledge. Before filing, the technical lead should supply the team's starting capabilities, alternatives considered at the time, and why a competent professional could not resolve the particular uncertainty through established practice alone. Parameter tuning that follows known methods must not be presented as experimental development merely because it required multiple runs.

### Uncertainty register

| ID | Technical question investigated | Evidence of difficulty | Status at this cutoff |
| --- | --- | --- | --- |
| U1 | Can compact, heterogeneous index representations retain the distributed terms and discriminating facts needed for retrieval without multiplying storage and model work? | Short-summary-only retrieval lost facts; sectioning and vectors changed ranking and storage in different directions. | Partly resolved for short enterprise exports; not established for arbitrary long or multimodal sources. |
| U2 | Can generated hints, lexical entries, and shared graph terms improve recall without changing text statistics or producing high-degree hubs that overwhelm relevant evidence? | Added keyed rows reduced recall; generic glossary terms connected many unrelated sources; equal-weight vector fusion regressed. | Failure mechanisms characterized; corpus-local vocabulary remains a proposal requiring validation. |
| U3 | Can a bounded agent preserve candidates and acquire enough source context to distinguish related records and retain all requested facts? | Gold documents appeared in initial results but were not read; correct answers omitted conditions; broad questions selected related but unsuitable sources. | Additive navigation helped the tested sample; company-level source selection and completeness remain unresolved. |
| U4 | Can experimentation and index maintenance change only affected work while preserving source identity, authorization, and reproducible measurement? | Repeated corpus work was costly; polling status caused a writer lock; ranking instrumentation initially hid folder positions; long-read continuation could lose later windows. | Specific maintenance and response invariants verified; broader performance and robustness claims remain bounded by recorded tests. |

These are candidate SR&ED uncertainties, not eligibility findings. U4 includes ordinary engineering and measurement corrections that may support the investigation but should not automatically be treated as separate technological advancements.

## 3. Draft T661 technical descriptions

The following text is prepared for technical-lead review and tax-year selection. It assumes the described August/September 2026 work falls within the eventual claim year. Edit the dates and scope before transferring it to a form. The longer experiment record below supplies the detail behind these condensed descriptions.

### Line 242 draft: uncertainty

We sought to determine how a bounded local indexing and evidence-navigation system could retrieve the facts needed for reliable answers from heterogeneous enterprise records without requiring an expensive representation of every document. The test corpus contained approximately 512,000 records, including near-duplicates, conflicting details, internal identifiers, and questions with little vocabulary overlap with the relevant source.

We used established full-text ranking, embeddings, rank fusion, graph propagation, and language-model tools. The uncertainty concerned how their representations and interactions affected preservation and selection of evidence under resource limits. Short summaries might omit decisive terms. Splitting text into sections might separate terms needed to identify one document. Adding generated lexical entries or shared entities might improve discovery or instead distort ranking statistics and create broadly connected graph hubs. A relevant retrieved document might remain unread because its preview hid the answer or subsequent query reformulation displaced attention from it.

We investigated whether full-document text, separately ranked lexical representations, bounded graph traversal, and accumulated source-reading context could resolve these interacting losses. We also needed to distinguish representation failures from approximate-search effects, source-selection errors, and answer omissions. A single aggregate score could not make those distinctions, and an improvement on a small question sample could conceal failures on broader information needs.

The intended advance was an evidence-based understanding of which representations and navigation controls preserve relevant facts, which introduce noise, and what storage, computation, and model-call costs those choices impose in this system. It was not simply to connect an existing model API or to produce a higher benchmark score.

### Line 244 draft: work performed

We implemented an instrumented indexing and retrieval system and tested alternate representations on EnterpriseRAG-Bench and BEIR NFCorpus. We recorded configurations, source revisions, binary identities, rankings, answers, timings, and scores. Enterprise experiments used both a 25,000-document development slice and the full 511,962-document corpus. Comparisons were kept separate by corpus size, model, and question selection.

We first compared short summaries, section text, longer summaries, whole-document rows, and vector versus full-text seeds. The short-summary configuration achieved 48.0% recall on the slice. Adding section text increased recall to 80.7%. Whole-document text in one full-text row without sectioning or embeddings reached 86.1%, with a 252 MB index and a recorded 13-second build. A sectioned, embedded whole-document configuration used 998 MB and reached 84.1%. We retained whole-document text for this enterprise profile rather than assuming that additional vectors or fragments would improve it.

We then tested generated hints and shared lexical rows. The first hints configuration reduced recall from 86.1% to 74.7%. Separating prose and lexical full-text tables recovered recall, and later offline comparisons used an 84.3% baseline. Generic glossary terms spread relevance across too many sources and reached only about 64% recall at best. Equal-weight cue-vector fusion regressed; a limited vote with weight 0.3 and only five ranks reached 85.6%. We retained the failures and treated the narrower fusion gain as workload-specific.

We investigated ranking and maintenance independently. On an unchanged full enterprise index, lowering lexical contribution increased recall from 63.37% to 66.42% while sharply reducing folder occupancy in result slots. BEIR experiments supported a different rank-fusion setting and showed a quality loss when vector dimensions were reduced. An incremental experiment rebuilt 275 folders while retaining 25,000 files. We corrected a status-polling writer-lock problem and a scoring method that had hidden folder positions. These corrections improved the reliability of the experiments rather than being counted as retrieval gains.

A 500-question run with GLM Flash for answers and judging scored 69.0. Its analysis found relevant documents that were retrieved but unread, paraphrase failures, near-duplicate confusion, and missed facts in longer sources. We did not compare that score directly with subsequent GPT-5.4 subset scores.

We established a fixed GPT-5.4 first-ten/last-ten baseline scoring 92. We then implemented stable accumulated candidates, an initial search using the original question, relevant indexed excerpts, source-size metadata, combined search/expand/scan requests, and a source-based completion check. Offline tests covered retention, bounded responses, partial failure, source changes, and continuation. The same twenty questions scored 99 with identical initial rankings, but answer cost rose from $1.64 to $2.81 and runtime from 342 to 445 seconds.

We expanded to fifty bookend questions without changing the algorithm during the run. The overall score was 87.33; the repeated twenty scored 100 and the thirty added questions scored 78.89. Only two of five high-level questions were correct, while all twenty unanswerable questions scored 100. We preserved these failures, rather than treating the smaller sample's gain as general success. The remaining investigation concerns source authority and coverage, semantic matching, completeness, and the cost of repeated searches before abstention.

### Line 246 draft: advancement sought and knowledge gained

The experiments developed a more specific understanding of evidence loss in this indexing and navigation system. For the tested short enterprise exports, retaining whole-document text in a single full-text row was both smaller and more effective than the initial summary-only or sectioned/embedded alternatives. More enrichment was not consistently better: short keyed rows changed full-text ranking statistics, and generic shared glossary terms created connections that reduced discrimination. Separating prose and lexical representations addressed one failure mechanism, while bounded weighting of weaker search channels offered only a limited, workload-specific benefit.

We distinguished retrieving a relevant source from actually using its evidence. On a fixed twenty-question sample with unchanged initial rankings, accumulated candidates, better previews, explicit reading affordances, and a completion check improved the combined answer score from 92 to 99. This establishes a benefit for the implemented bundle on that sample, not the individual contribution of each component. It also quantified a cost increase.

The expanded fifty-question evaluation established a boundary to that progress. The original questions remained strong, but added questions exposed incorrect company-wide source selection and incomplete basic answers. The resulting 87.33 score prevents a claim of general resolution or full-benchmark parity. Earlier full-set results also showed that semantic questions remained much weaker than direct questions.

We additionally verified bounded candidate retention, recoverable source-reading continuation, partial-failure handling, and selective folder rebuilding through implementation tests and recorded runs. These provide experimental controls and operational mechanisms for further investigation; they do not prove universal retrieval reliability or a novel general-purpose search algorithm.

The proposed corpus-local vocabulary, cluster grounding, and source-authority mechanisms remain unvalidated research directions at this cutoff. Their expected gains are not included as achieved advancements.

## 4. Experimental method and measurement controls

The benchmark programs exercise the installed public CLI rather than a benchmark-only search implementation. A run manifest identifies the dataset release, origin index, composition, model assignments, source revision, binary hash, question selection, timings, and completion status. Query-only replays reuse an index without indexing or repair. Answer and retrieval checkpoints preserve completed work. [Benchmark design](../design/benchmarking.md), [commands](../benchmarks/README.md).

The Enterprise corpus contains 511,962 documents and 500 questions. Early shape experiments use a 25,000-document development slice containing expected documents and seeded distractors. Such slice recall must not be equated with full-corpus recall. BEIR NFCorpus uses 3,633 abstracts and 323 queries and measures retrieval rather than generated answers.

Metrics used in this record:

- **Recall@8:** coverage of expected documents among the first eight results, on questions with expected document IDs.
- **MRR:** reciprocal rank of the first expected document, averaged across the applicable questions. Folder result slots must retain their actual positions.
- **nDCG@10:** graded relevance quality of the first ten BEIR results.
- **Answer correctness:** the official evaluator's whole-answer judgment.
- **Combined answer score:** mean completeness after gating each answer by its correctness. An incorrect answer with partial completeness still contributes zero.
- **Time, storage, and provider cost:** operational observations, with the scope and missing charges stated alongside them.

The September 13 GPT-5.4 runs use the pinned upstream evaluator at `d36685e273713975ee20299bbf1ab64165575b3c`, with GPT-5.4 for answer generation and both judge roles. Gold correction is disabled. The agent retains a twelve-turn allowance and, after the refinement change, one tool-free completion check that sees the question and retrieved evidence, not gold answers.

Limitations are part of the results. Model-generated answers and judgments can vary. Approximate candidate selection and tied rankings showed variation in BEIR. Concurrent machine load affected some indexing and runtime measurements. Several changes were tested as a bundle. The bookend sample became a development set once failures guided changes. Earlier MRR that removed folder slots is not directly comparable to the corrected scorer. Source-code dates, run wall times, and model token counts do not measure human labour.

## 5. Dated experiment and implementation record

The hypothesis labels below organize existing evidence. They do not assert that every experiment began with a separately signed hypothesis document.

### E01. 9 to 23 August: local architecture, source identity, and bounded indexing

**Question:** how to support replaceable indexing components, source-level permissions, and incremental replacement without inconsistent copies of catalog and search state or uncontrolled work queues.

**Work recorded:** initial index/Finder design; a lean plugin kernel; typed operation interfaces; a move from LanceDB to a single libSQL catalog/search database; atomic deletion of associated search rows; source-owned fragments with index-wide keyed fragments; parallel source planning with per-source transactional landing; bounded deactivation, graph expansion, and ID lists. Content-digest handling retained individual source identities while collapsing equivalent result copies.

**Evidence:** commits `018c479`, `5c6b9cf`, `26eaefc`, `4380c35`, `45bd768`, `2f20124`, `5b4c0fc`, `2b07b82`, `7f2fd58`, `7f2f5c3`, `d6949bd`; [indexing](../design/indexing.md), [maintenance](../design/index-maintenance.md), [runtime](../design/runtime.md).

**Conclusion and boundary:** these establish the implemented experimental platform and the reasons for several architectural decisions. The repository does not provide a controlled LanceDB-versus-libSQL performance trial in this record. Do not invent a measured speedup or treat the database migration itself as proof of technological advancement. Shared graph subtrees across sources were rejected in design because they complicate source-level authorization; expensive artifacts were instead cached by digest.

### E02. 23 August to 6 September: model work, vector maintenance, and resource limits

**Hypotheses:** batching and reusing equivalent derived work can reduce repeated network operations; approximate-vector indexing should not silently lose seeds relative to exact search; changes to embedding shape should not force unrelated text transformation.

**Work recorded:** model/dimension validation, summary-only vector scope, local-model configuration, interactive and batch model lanes, deferred/rebuilt DiskANN indexes for large landings, resumable repair, file-read concurrency gates after descriptor exhaustion, digest-keyed transform and embedding caches, and compact search-text/cache storage. A later DiskANN change widened graph search to address seed disagreement with exact scans.

**Evidence:** commits `2d85aa6`, `36ceaf0`, `9f08979`, `074aa21`, `878ee24`, `936fffd`, `148f389`, `e2baa16`, `a3b1c8e`; [indexing decisions](../design/indexing.md), [maintenance](../design/index-maintenance.md), and the BEIR manifests linked through the benchmark history.

**Conclusion and boundary:** code and tests support specific bounds, cache identities, and recovery paths. The descriptor fix and API compatibility work are operational engineering. A particular vendor discount, hardware speedup, or paid-hour saving is not inferred from the existence of a batch lane. The later incremental experiment provides measured reuse evidence.

### E03. 6 to 7 September: preserving source text versus compressing it

**Hypothesis:** small summaries and selected fragments would retain enough retrieval evidence while reducing index cost.

**Method:** compare representation shapes on the 25,000-document Enterprise slice, all 500 questions, retrieval only. The initial configuration disabled structural transforms and searched only short summaries, contrary to the original assumption that source text still reached full-text search.

| Representation | Recall % | MRR | Recorded index size | Recorded build time |
| --- | ---: | ---: | ---: | ---: |
| 200-character extractive summaries, keywords, and vectors | 48.0 | 0.405 | 308 MB | 50 s |
| Add section text to full-text search | 80.7 | 0.664 | 678 MB | 76 s |
| Sections and 600-character summaries | 82.1 | 0.629 | 713 MB | 73 s |
| Title-led 600-character summaries with sections | 81.8 | 0.644 | 713 MB | 75 s |
| Sections and 1,200-character summaries | 82.1 | 0.666 | 759 MB | 81 s |
| Sections and embedded whole-document summary | 84.1 | 0.726 | 998 MB | 88 s |
| Same index, full-text seeds only | 85.3 | 0.775 | Unchanged | Query-only comparison |
| Same index, vector seeds only | 57.1 | 0.445 | Unchanged | Query-only comparison |
| Same index, tighter vector-distance threshold | 83.1 | 0.729 | Unchanged | Query-only comparison |
| Sections and 600-character summaries, no embeddings | 84.8 | 0.734 | 305 MB | 13 s |
| Whole-document full-text row, no sections or embeddings | 86.1 | 0.801 | 252 MB | 13 s |

**Result:** compression had removed necessary evidence. Full text and document-level term co-occurrence mattered more than the presumed benefit of richer indexing on this workload. Equal fusion with a weaker vector list could reduce quality. The tighter distance threshold did not resolve that failure.

**Decision:** adopt the model-free whole-document shape for the enterprise comparison, while retaining configurable representations for other corpora. This is not a general rejection of chunking, summaries, or vectors. Historical MRR belongs to the scorer used then and is not used as a before/after comparison with the corrected September 13 scorer.

**Evidence:** [recorded shape matrix](../design/benchmarking.md#what-the-corpus-taught), commits `a68abc0`, `e6ccce7`, `0421c9c`.

### E04. 6 to 7 September: full-corpus scale and query vocabulary

**Hypothesis:** conversational function words in an OR-expanded full-text query admitted too many rows and added work without identifying relevant records.

**Method/result:** the design records same-index full-corpus comparisons with and without function words: recall 66.8% to 68.1%, MRR 0.595 to 0.605, and median query latency approximately 2.5 seconds to 0.45 seconds. The slice's higher recall did not survive the much larger distractor set unchanged.

**Decision:** remove function words from the seed query. Treat the reported latency as an observation under that experiment's conditions, not a hardware-independent guarantee.

**Evidence:** commit `4eede91`; runs `20260907T001929Z-a3b1c8e5ec80` and `20260907T005047Z-4eede9167a0d`, described in [benchmark design](../design/benchmarking.md). Preserve the historical scorer limitation noted above.

### E05. 6 to 7 September: generated hints, text statistics, and graph hubs

**Hypothesis:** synopses, answer cues, discriminators, identifiers, entities, and shared glossary rows could connect questions with relevant documents that plain keyword matching missed.

**Method:** add generated per-source hints and shared keyed rows on the Enterprise slice, then isolate text-table effects, hint prose, graph spreading, and vector fusion.

| Change | Observation | Decision/knowledge gained |
| --- | --- | --- |
| Initial hints and keyed rows in the same full-text table | Recall 86.1% to 74.7%; roughly 300,000 short rows altered text statistics. | Regression. Separate prose and lexical tables. |
| Separate tables | Recall recovered to 83.4%; later offline comparisons used 84.3% as their own baseline. | Distinguish representation interference from hint usefulness. |
| Hint prose as full-text evidence | 83.9% against 84.3%, with different effects by question type. | Approximately neutral overall. |
| Glossary hits propagated to sources | Best reported recall around 64% despite damping. | Generic terms formed broad hubs and reduced discrimination. |
| Cue/synopsis vectors alone | Approximately 52 to 54% recall. | Do not replace whole-document evidence. |
| Equal cue-vector fusion vote | Four-point loss. | Reject equal weighting in this comparison. |
| Cue-vector weight 0.3, first five ranks only | 85.6%, 1.3 points above the offline baseline, with about 0.04 MRR gain. | Limited measured benefit, not an end-to-end result. |
| Sharper cue prompt avoiding internal names | Word overlap 13% to 23%; cosine 0.52 to 0.55. | Diagnostic change; downstream recall gain not yet established. |

The report records $4.22 of model calls for the initial hints work. This is a reported experimental cost requiring invoice reconciliation, not a complete or classified SR&ED expenditure.

**Evidence:** commits `7dcde2f`, `9c350f8`, `4d5a655`, `0532576`; [indexing design](../design/indexing.md), [experiment history](../design/indexing-experiments.md), run reference `20260907T025207Z`.

### E06. 7 September: first complete answer-and-judge evaluation

**Method:** all 500 Enterprise questions; `z-ai/glm-5.3-flash` for answers and upstream judging; twelve tool turns; ten initial results per query; gold correction disabled.

**Result:** combined score 69.0; correctness 73.6%; reported completeness 75.4%; reported document recall 77.1%. Semantic questions scored 48.6 compared with 77.0 for basic questions. Among 741 expected documents, the analysis classified 427 as cited, 113 as initially shown but uncited, 42 at ranks 11 to 25, and 159 outside the first 25. These are the earlier report's document classifications, not the later benchmark's citation metric.

**Observed failure modes:** selecting a related or superseded sibling, missing paraphrased terminology, failing to open promising initial results, reading only one of two relevant windows in a long document, and abstaining despite incomplete exploration. The report includes question-level examples.

**Limitations:** this model and full question set differ from the later GPT-5.4 runs. Its historical public-ranking comparison is not a current leaderboard standing or an SR&ED advancement measure. Cost telemetry for this model was incomplete; do not turn the reported key-accounting movement into a verified total expense.

**Evidence:** [full report](../benchmarks/runs/enterprise-rag-bench/20260907T032046Z-6b040bbfd905/report.md), commit `8ec609a`, source `6b040bb`.

### E07. 13 September: folder-aware retrieval controls and storage tradeoffs

**Hypothesis:** lexical and folder-derived evidence should remain available but need not contribute equally to prose during ranking and graph propagation.

**Method:** replay the full Enterprise index unchanged and test a separate BEIR index; vary weights, rank-fusion constants, seed counts, folder summary lengths, graph propagation, keyword caps, and vector dimensions. Record folder occupancy rather than silently filtering it.

| Experiment | Result | Decision |
| --- | --- | --- |
| Full Enterprise lexical weight 1.0 to 0.1 | Recall@8 63.37% to 66.42%; MRR 0.432677 to 0.596429; folder slots 1,170 to 13 out of 4,000. | Measured profile-specific improvement without deleting candidates. |
| Full Enterprise lexical weight 0.25 | Same retrieval scores as 0.1; 20 folder slots. | Tied retrieval result. Does not prove identical agent behaviour. |
| BEIR RRF k 60 to 10 on one index | nDCG@10 0.36885 to 0.37926; repeated results varied but supported k=10. | Retain profile-specific tuning rather than one universal setting. |
| BEIR vector dimensions 384 to 192 | About 27% less index storage; nDCG@10 0.35222. | Reject the storage reduction for this quality target. |
| Folder summaries reduced to 400 characters | Slice storage fell about 3.3%; best short-folder result exceeded the long-folder 0.1 setting by only 0.12 recall points. | Keep long summaries for full navigation comparisons. |
| Remove graph propagation | Slice recall 74.45% versus 79.78% for the corresponding zero-keyword comparison. | Retain propagation. |
| Seed pool 60 to 120 at lexical weight 0.1 | Recall 84.16% to 83.92%. | Reject the larger pool for that configuration. |
| Keyword cap 12 versus 0 | Recall 79.83% versus 79.78%. | Inconclusive evidence of a ranking benefit. |

The audit preferred weight 0.25 for a subsequent experiment; the actual GPT-5.4 navigation runs used 0.1. The manifest is the record of the executed setting. The full selected comparison used a 5,225,340,993-byte index. Repeated BEIR rankings varied, including 144 changed rankings between two nominally identical RRF-5 replays; the exact source of candidate/tie variation remained open.

**Evidence:** [retrieval audit and all linked runs](../benchmarks/reports/20260913-retrieval-audit.md), [audit data](../benchmarks/reports/20260913-retrieval-audit.json), commit `dae72df`.

### E08. 13 September: incremental reuse and experimental reliability

**Hypothesis:** changing folder transformation settings should rebuild folders while preserving unaffected file work, and Finder-only changes should require no source rebuilding.

**Result:** the controlled sequence retained 25,000 files while rebuilding 275 folders. Fresh sweep: 48.51 seconds; first folder-only sweep: 14.01 seconds; second folder change: 8.54 seconds; Finder-only no-change sweep: 4.56 seconds. The latter indexed zero sources and retained all 25,275. Public status after restoring the folder target recorded 25,275 sources, 70,042 fragments, and 44,767 relations.

**Interpretation:** source reuse counts verify the intended invalidation boundary. Concurrent workload makes those wall times descriptive rather than isolated speed ratios. Unchanged database file size does not prove unchanged rows because freed pages can remain allocated.

**Operational corrections:** an early full indexing attempt failed when concurrent `status` opened another store writer. The runner now uses elapsed-time heartbeats. A resumed attempt reused 182,438 indexed sources and indexed 329,823 remaining sources. Its 617.658-second duration excludes a 192.032-second failed attempt. The scorer was also corrected to retain folder positions when calculating MRR. These are recovery and measurement corrections, not evidence of a new retrieval method.

**Evidence:** [incremental protocol](../benchmarks/experiments/20260913-incremental/README.md), [results](../benchmarks/experiments/20260913-incremental/results.json), [audit](../benchmarks/reports/20260913-retrieval-audit.md).

### E09. 13 September: fixed GPT-5.4 navigation baseline

**Hypothesis under investigation:** some answer failures arise after candidate retrieval, because the agent rewrites the query, overlooks a promising source, or omits a fact it has read.

**Method:** first ten and last ten release-order questions, using the full index, GPT-5.4 answers and official judging, twelve turns, and lexical weight 0.1. This reduced evaluation time without reducing corpus distractors.

**Result:** score 92; correctness 95%; basic-question score 84; unanswerable score 100. Total 341.54 seconds and $1.6361 in answer-provider cost. Question 2's expected metric source was third in an independent original-question probe, but the agent's rewritten searches did not lead it to read that source. Questions 9 and 10 were correct but only 80% and 60% complete. All 160 initial candidate hints were empty in the lean index profile, and the agent's preview used a fixed summary prefix.

**Evidence:** [baseline report](../benchmarks/reports/20260913-gpt54-bookends.md), [failure diagnosis](../benchmarks/reports/20260913-navigation-improvement-plan.md), source/run identity in section 9.

### E10. 13 September: additive discovery and size-aware reading

**Hypothesis:** retaining original candidates and giving the model relevant previews, reading-cost information, and batched source access would reduce avoidable evidence loss without requiring a corpus rebuild or separate reranking model.

**Implemented change:** a caller-owned discovery session keeps address-deduplicated candidates with stable IDs. One `find` request accepts new queries, expansion, scans, inspection, and explicit removal. It allows eight actions, four concurrent actions with per-action timeouts, one hundred retained candidates, and 24,000 serialized response characters. Responses remain complete JSON. Omitted content has continuation information. Source sizes distinguish known lines/bytes from indexed-summary characters. Excerpts identify their origin and do not invent source-line coordinates. Query version changes invalidate stale read coverage, and source-window digests reject changed continuations. The original question is searched before optional rewrites, and the final completion check uses read evidence without gold answers.

**Validation recorded:** ten offline discovery integration tests, two agent unit tests, and 53 benchmark tests. The discovery tests cover additive retention, excerpts beyond the prefix, partial failures, bounded metadata, explicit capacity, source changes, Unicode continuation, and preserving later windows after a character-truncated 2,000-line scan. Targeted Clippy, formatting, diff checks, and local binary installation completed. These historical checks were not rerun merely to prepare this document.

**Result:** the same twenty questions scored 99 with 100% correctness. Initial ranked document IDs and order matched the baseline on all twenty. Question 2 now read the expected source and named `stream.timebox_finalized`; question 9 retained the US-region SLO condition; question 10 improved to 80% completeness. Runtime rose to 445.06 seconds and answer cost to $2.8092. The agent used 97 `find` calls containing 161 queries and 93 scans, including mixed-action and multi-read batches.

**Conclusion:** the bundle improved the measured sample, with higher cost. It did not isolate the benefit of each component, establish an optimal stopping rule, or demonstrate that reranking could never help. The continuation correction in `300afd9` followed the frozen `09bc883` run; that run never requested the longer-than-2,000-line branch.

**Evidence:** [design](../design/finder-refinement.md), [implemented contract](finder/refinement.md), [report](../benchmarks/reports/20260913-gpt54-refinement.md), commits `09bc883` and `300afd9`.

### E11. 13 September: expand evaluation to fifty questions

**Purpose:** test whether the twenty-question result survived additional questions and record failures without changing the algorithm during the run.

**Method:** first 25 and last 25 questions; all fifty answers generated afresh using a frozen binary and the same full index and GPT-5.4 settings. The sample has 25 basic, five high-level, and twenty information-not-found questions.

| Group | Count | Correctness | Combined score |
| --- | ---: | ---: | ---: |
| All selected | 50 | 90.00% | 87.33 |
| Repeated original questions | 20 | 100.00% | 100.00 |
| Added questions | 30 | 83.33% | 78.89 |
| Basic | 25 | 92.00% | 86.67 |
| High-level | 5 | 40.00% | 40.00 |
| Information not found | 20 | 100.00% | 100.00 |

The cohort split and the type split are alternative partitions of the same fifty questions. The original questions had identical initial rankings to the preceding run. Question 10's completeness varied from 80 to 100; that repeat difference is not another algorithm change.

Five answers were incorrect: contractor access expiry (`qst_0016`), required quantization-document review roles (`qst_0023`), revenue streams (`qst_0477`), commercial add-on categories (`qst_0478`), and company departments (`qst_0480`). Three basic answers were incomplete: rollback (`qst_0014`, 33.33), bridge mode (`qst_0019`, 66.67), and null versus omitted `max_tokens` (`qst_0020`, 66.67). The raw judge result does not preserve every individual completeness fact verdict, so exact missing-fact attribution is not claimed for those three.

The incorrect high-level answers read related operational material rather than establishing the requested company-wide categories. That suggests a source-selection/coverage investigation; it does not prove whether governing evidence is absent from the index, poorly retrieved, or left unread. Initial expected-document recall was 84% across the twenty-five basic questions with gold IDs. The high-level questions provide no expected-document IDs, so their retrieval recall is unavailable.

The run took 1,124.36 seconds, or 18m 44s. Answer generation cost $6.6095. The agent made 241 `find` calls, 376 queries, and 265 scans. Unanswerable questions accounted for $4.3416 and 596.42 answer seconds; two used all twelve turns. All fifty IDs matched across selection, answers, and judging; none were skipped, and every correctness judgment included reasoning.

**Conclusion:** broader coverage exposed unresolved failures despite a strong repeated subset. The result cannot be presented as full-benchmark performance or as a fall from 99 on the same question set.

**Evidence:** [fifty-question report](../benchmarks/reports/20260913-gpt54-50-bookends.md), [sample comparison](../benchmarks/runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/agent/20260913T214310Z-a4951ac828ca/sample-comparison.json), commit `0e0863e`.

### E12. 13 September: diagnostic tools and ongoing recordkeeping

The index explorer added bounded graph inspection, working-index selection, and per-result retrieval explanations. Its implementation and tests are recorded in `e647817`. The benchmark runner records retrieval observations and fixed question subsets; the experiment history links measured gains and rejected alternatives. These tools make subsequent hypotheses testable and results inspectable, but their existence is not itself proof of a retrieval improvement.

**Evidence:** [explorer design](../design/index-explorer.md), [explorer operation](index-explorer.md), [experiment history](../design/indexing-experiments.md), commits `30f25c9`, `e647817`, `116385a`, `36eef31`.

## 6. Unresolved work and untested proposals

The evidence does not establish a general solution for semantic paraphrases, near-duplicate authority, or company-wide source selection. The next proposed vocabulary design mines corpus-local terms, matches them to actual source content, groups co-occurring terms, and optionally grounds clusters with a model. Its proposed gates require an offline recovery ceiling, exact grounding with hub limits, cluster/alias tests, extractor integration, and a separately measured authority prior. Implementing a gate does not mean it has passed. [Vocabulary proposal](../design/vocabulary.md).

Other proposed directions include contrastive document distinctions, exact identifiers and metadata fields, source-authority signals, query routing, and selective rewriting. They appear in [retrieval research](../benchmarks/enterprise-rag-leaderboard-research.md) and earlier reports as options. They are not recorded here as completed trials unless an experiment above supplies evidence. Public competitor descriptions motivated hypotheses; they do not prove Inseam's novelty or a competitor's internal implementation.

Open checks include:

- Separate missing governing documents from candidate-generation and reading failures.
- Test semantic and other omitted categories beyond the fifty bookends.
- Compare independent changes within the additive-navigation bundle.
- Reduce repeated search cost on unanswerable questions without increasing false answers or premature abstention.
- Stabilize or characterize candidate/tie variation in repeated retrieval runs.
- Confirm long-source, remote-source, and concurrent-change behavior beyond the offline cases already tested.

## 7. Other project work preserved for scope review

The repository includes substantial work beyond the measured indexing series. This inventory prevents that work from disappearing from the record while distinguishing implementation from an experimentally demonstrated advancement. A preparer should connect any included support work to a specific investigation, or assess a separate project if it has a different uncertainty.

| Period and work | What the repository supports | Evidence and claim boundary |
| --- | --- | --- |
| August 9 to 16: workspace, kernel, plugin composition, loaded WASM tier | Typed services, linked/loaded plugins, capability restrictions, resource limits, OCR component, conformance/admission/golden checks. | `5c6b9cf`, `ea38293`, `580f05e`; [kernel](architecture/kernel.md), [loaded plugins](plugins/loaded.md), [validation](plugins/validation.md). Code/tests demonstrate mechanisms; do not claim complete security or a measured general plugin-safety advance. |
| August 16 onward: registry, distribution, installation, updates | Registry hardening, hash/signature checks, cooldown design, runtime installation/rollback paths, signed self-update. | `ebc84ae`, `dc7618e`, `6afa8fb`, `811ebcd`, `4e52e62`, `8c4456f`; [registry](plugins/registry.md), [releases](releases.md). Distinguish implementation and ordinary release engineering from experimental work. |
| August 17 to September 6: settings, grants, connectors, indexing controls | Secret declarations, parked plugin health, host-scoped ignore rules, Google Workspace/OAuth, deep-work budgets, text/byte reads, web references, image OCR, bounded crawl depth. | `4bcab36`, `98513ae`, `cf340c9`, `09c470d`, `fb49fb5`, `c5751dd`, `d33636c`, `2b3da37`, `461eb9b`; [connections/indexing docs](indexing/README.md). API integration and routine defects are not automatically SR&ED. |
| September 6 to 7: distributed discovery and identity | Persistent replicated records, identity and roster, iroh transport, sync and routing plugins, owner operations and two-node proof work. | `c7b1c99`, `cbaa875`, `709a7cf`, `4c2708f`, `8095f8a`; [network docs](network/README.md). No controlled distributed recall/latency result is included in this dossier. |
| August to September: desktop, web, FFI, MCP, and mobile clients | Public operation access through multiple clients; iOS leaf-node and local-model support; configuration and install interfaces. | `4d72d23`, `a58a867`, `f82f64e`, `3058569`, `a01f769`; [architecture docs](architecture/README.md). Catalogue client work without claiming every UI/FFI change resolves technological uncertainty. |
| September 13: stereo meeting capture and provenance | Lossless capture, actual channel metadata, explicit mono fallback, fixed orientation, interruption handling, sidecars, compatibility tests, bounded duration/storage design. | `15808e0`; [meeting design](../design/meeting-recording.md), [iOS docs](architecture/ios-app.md). Physical-device acoustic quality, speaker identification, and separation are not established by compilation or metadata tests. |
| September 13: telephone capture research | Evaluated platform capture and import/callback options and recorded constraints. | `c4fc0ae`, `ffcf8b4`, `1aeba76`; [call-context research](../design/phone-call-context.md). Research/options analysis is not proof of a completed call-capture experiment. |
| Product, hosted-service, and vocabulary proposals | Architectural intent and validation plans. | [hosted service](../design/hosted-service.md), [vocabulary](../design/vocabulary.md). Do not claim planned deployments or experiments as completed work. |
| Branding, landing pages, documentation presentation, TODO edits, routine packaging | Product/commercial/support activity is visible in history. | Preserved as context; no technical uncertainty or experiment is asserted for these changes. |

CRA distinguishes the advancement sought from business improvements and allows failed investigations to yield knowledge. Actual work and its relationship to the investigation still determine eligibility; a commit label is not a classification. [CRA work eligibility](https://www.canada.ca/en/revenue-agency/services/scientific-research-experimental-development-tax-incentive-program/sred-eligibility.html).

## 8. Resource and expenditure record

These figures are observed experimental resource use, not a completed claim calculation. Retain the underlying invoices and allocation records. API-provider dollar meters do not establish the claimant, payment date, tax treatment, Canadian-dollar amount, or eligible expenditure category.

| Recorded work | Observed spend/resource | Evidence qualification |
| --- | --- | --- |
| Initial retrieval-hints work | Reported $4.22 of model calls | Confirm currency, billing account, invoices, and coverage of the reported amount. |
| Twenty-question GPT-5.4 baseline | USD $1.6361 answer meter; 341.54 total seconds | Judge cost unavailable. |
| Twenty-question additive navigation | USD $2.8092 answer meter; 445.06 total seconds | Judge cost unavailable; some local validation/build activity overlapped timing. |
| Fifty-question expansion | USD $6.6095 answer meter; 1,124.36 total seconds | Judge cost unavailable. |
| Subtotal of the three GPT-5.4 answer meters | USD $11.0548 | Arithmetic subtotal only, not total project cost or a proposed tax deduction. |
| September 7 GLM Flash full run | Reliable total not established by its meter | Earlier report describes incomplete provider cost telemetry. Do not report it as free. |
| Model-free enterprise indexing profiles | Zero transform-model calls in those runs | Does not mean zero labour, hardware, storage, electricity, or project expense. |
| BEIR embeddings and other models | Separate charges not fully totalled here | The summarizer's spend counter does not include every embedding charge. |
| Hardware and storage | Apple M2 Pro/arm64 environment and approximately 17 GB RAM recorded for recent runs; full selected index about 5.23 GB | Equipment ownership, purchase dates, use allocations, and claim treatment remain unconfirmed. |

Required financial records include dated person/project work allocations, payroll and employment status, contractor agreements and invoices, provider statements and usage exports, payment/currency records, equipment-use records where relevant, assistance and contract-payment records, and a reconciliation to the general ledger. Record actual human analysis, design, experiment execution, and interpretation time. Do not count agent runtime as employee hours or invent a time allocation from commit timestamps.

AI assistance was used for implementation, execution, analysis, and this document. Identify the responsible human reviewer and actual contributors. A repository author field does not establish who performed every task, and AI-generated technical text requires verification against the evidence. No human qualification, employment relationship, salary, contractor status, or Canadian work location is invented here.

The applicable expenditure method, classification, and eligibility need to be selected from the claimant's facts. This dossier does not calculate an investment tax credit or elect a method. [CRA guidance on linking work and expenditures](https://www.canada.ca/en/revenue-agency/services/scientific-research-experimental-development-tax-incentive-program/sred-claim/allowable-expenditures.html).

## 9. Evidence register and preservation

### Principal technical records

| Record | Location | What it supports |
| --- | --- | --- |
| Consolidated experiment history | [Indexing experiments](../design/indexing-experiments.md) | Tried/rejected directions and evidence links. |
| Indexing and maintenance designs | [Indexing](../design/indexing.md), [maintenance](../design/index-maintenance.md) | Representations, invalidation, caches, alternatives, and recorded measurements. |
| Finder and refinement designs | [Finder](../design/finder.md), [refinement](../design/finder-refinement.md) | Ranking and candidate/reading decisions. |
| Benchmark protocol | [Design](../design/benchmarking.md), [commands](../benchmarks/README.md) | Dataset/model pins, limits, scoring, run evidence, replay procedure. |
| Full GLM Flash evaluation | [Report](../benchmarks/runs/enterprise-rag-bench/20260907T032046Z-6b040bbfd905/report.md) | Full-set findings, question examples, limitations. |
| Folder/retrieval audit | [Report](../benchmarks/reports/20260913-retrieval-audit.md), [data](../benchmarks/reports/20260913-retrieval-audit.json) | Configuration sweeps, gains/regressions, per-type effects, timing limits. |
| Incremental experiment | [Protocol](../benchmarks/experiments/20260913-incremental/README.md), [results](../benchmarks/experiments/20260913-incremental/results.json) | Changed-source counts and sweep sequence. |
| GPT-5.4 baseline | [Report](../benchmarks/reports/20260913-gpt54-bookends.md) | Fixed twenty-question starting point. |
| Failure diagnosis | [Report](../benchmarks/reports/20260913-navigation-improvement-plan.md) | Candidate-loss replay and completeness hypotheses. |
| Revised navigation | [Report](../benchmarks/reports/20260913-gpt54-refinement.md) | Bundle implementation, twenty-question gain, increased cost, validation. |
| Expanded sample | [Report](../benchmarks/reports/20260913-gpt54-50-bookends.md) | Fifty-question result, repeated/added cohorts, new failures. |
| Behavioural tests | [Discovery integration tests](../crates/inseam-plugins/tests/discovery.rs), [agent tests](../crates/inseam-cli/src/agent.rs) | Tested session/read bounds and agent setup. Pin to the relevant commit when reviewing later. |

### Recent frozen run identities

All three runs below are under origin `20260913T181642Z-b414e6e7aa0f` and retain the full enterprise index.

| Run | Source state | Manifest |
| --- | --- | --- |
| `20260913T194431Z-b34a3ecaa1f4` | `b34a3ec` plus tracked Rust edits saved with the run | [Baseline manifest](../benchmarks/runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/agent/20260913T194431Z-b34a3ecaa1f4/manifest.json) |
| `20260913T205625Z-09bc883d9a0a` | `09bc883`, empty Rust source diff | [Refinement manifest](../benchmarks/runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/agent/20260913T205625Z-09bc883d9a0a/manifest.json) |
| `20260913T214310Z-a4951ac828ca` | `a4951ac`, empty Rust source diff, includes the continuation correction | [Fifty-question manifest](../benchmarks/runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/agent/20260913T214310Z-a4951ac828ca/manifest.json) |

Each manifest supplies the binary SHA-256 and executed options. Nearby committed files include answers, selected questions, evaluation results, navigation statistics, composition, dependency versions, and retrieval observations. A dirty repository flag is not evidence that uncommitted later work was in a frozen executable; inspect the saved source diff and binary identity.

Detailed command logs and raw per-query dumps remain in local run directories under the benchmark's Git policy. Temporary frozen binaries were also placed under `/private/tmp/` during execution. Their continuing existence and backup are not established by this document. Before cleaning machines, preserve the evidence needed for review in a durable access-controlled archive, including raw logs, relevant binaries or reproducible build inputs, manifests, source snapshots, and financial records. Record archive location and checksums here: **TO CONFIRM**. A future rerun cannot reproduce the exact historical model output.

CRA's guide describes contemporaneous technical and financial materials as supporting evidence. This summary complements those materials and does not replace them. [T4088 evidence guidance](https://www.canada.ca/en/revenue-agency/services/forms-publications/publications/t4088/guide-form-t661-scientific-research-experimental-development-expenditures-claim-guide-form-t661.html).

## 10. Items to resolve before filing

1. Confirm claimant, fiscal period, project boundaries, start date, and claim history. Select the applicable work rather than submitting the complete repository history indiscriminately.
2. Have the technical lead validate uncertainties, starting knowledge, hypotheses, dates, and conclusions. Add the contemporaneous alternatives review and explain why the unresolved issue exceeded established practice.
3. Identify actual people, qualifications, duties, work locations, employment/contract status, and time records. Confirm responsibility for AI-assisted work and this narrative.
4. Reconcile experiments to payroll, provider invoices, contracts, assistance, and accounting records. Resolve missing judge/embedding costs and currency conversion. Classify expenditures separately from this technical record.
5. Map any supporting platform work to the investigation it directly supported. Keep routine product work and unrelated projects separate unless their own evidence supports a different treatment.
6. Preserve local-only evidence and verify the archive. Keep unsuccessful work and conflicting observations, not just selected positive runs.
7. Restrict and review the three form drafts for the selected tax year, word limits, field codes, and current form. Complete the remaining T661 and tax-return information using verified claimant facts.

These are missing factual inputs and review steps, not a claim that eligibility has been established. No application has been filed by preparing this document.

## 11. Ongoing entry format

Maintain this file as the codebase evolves, as required by `AGENTS.md`. Append new entries rather than rewriting prior outcomes to match the latest theory. Record corrections explicitly.

```text
Entry ID and work dates:
Recorded on:
Tax-year allocation:
Responsible people and actual work locations:
Uncertainty ID and starting knowledge:
Hypothesis, including whether recorded before testing:
Change and control configuration:
Dataset/question IDs and source/binary identity:
Experiment or analysis performed:
Measured results, including failures and costs:
Conclusion and limits of inference:
Next hypothesis or reason for stopping:
Commits, tests, run artifacts, and archive location:
Time records and invoice references:
Technical reviewer and review date:
```

## 12. Preparation history

| Date | Update |
| --- | --- |
| 2026-09-13 | Initial retrospective consolidation from repository history through `36eef31`, existing designs, reports, experiment artifacts, and current CRA form/guidance. Draft technical narratives and evidence/financial gaps recorded. No new benchmark or eligibility determination was performed for this documentation task. |

### 2026-09-13 continuation: full GLM evaluation and interrupted vocabulary slice

The owner requested a full 500-question EnterpriseRAG evaluation with
`z-ai/glm-5.3-flash` for answers and both official evaluator roles, resumption of
the killed vocabulary slice, then full retrieval and answer comparisons. The
owner clarified that the interrupted run is `20260913T224507Z-812f3f4af037`.
The adjacent `20260913T224122Z-812f3f4af037` had already completed its 25,000-document
index and 500 retrieval queries. Resume attempt 2 reuses the interrupted node.
Results of the requested new comparisons are pending at this entry's preparation.

A baseline attempt with the frozen binary from the GPT-5.4 50-question run stopped
on question 2 after completing question 1. A diagnostic proxy recorded provider
response metadata without authorization headers. Repeated GLM responses from
DeepInfra reported a normal stop with empty content and nonzero completion tokens.
In two probes, the first response contained a completed answer; the final review
returned empty content. An explicit low reasoning setting and an explicit
`tool_choice=none` also produced empty review replies. These observations do not
establish a retrieval failure or prove a specific provider implementation defect.
Disabling reasoning was rejected by the endpoint. Provider upstream-cost fields
were nonzero while the OpenRouter cost field was zero under BYOK, so those zeros
must not be interpreted as free inference.

The agent previously discarded a completed draft when the optional final review
was empty, reporting a misleading turn-limit error. The repair retains only the
immediately preceding completed assistant answer, records a warning, and still
fails if neither reply has an answer. Tool-call preambles are excluded. Four
new offline tests through the agent's public entry point cover draft preservation,
successful replacement, empty-answer rejection, and preamble rejection. All six
agent tests, strict CLI clippy with the repository's nested-if exception, and
66 Python benchmark tests passed. The repair is applied to an isolated old Finder
build for the model baseline as well as the current implementation. This preserves
the ability to distinguish the retrieval/indexing change from this response-handling
change. No successful full GLM score is claimed yet.

The owner subsequently requested lower reasoning effort. The answer loop and final
review now accept an explicit effort, and the new full runs use `low`. A wrapper
passes `low` into the pinned upstream judge's two model factories; the evaluator's
scoring code and gold data remain unchanged. Earlier partial default-effort runs
are retained as failed/interrupted execution evidence, not mixed into the new score.

Eight separate APFS node clones were tried to parallelize answers. The first
queries stalled during SQLite shutdown/checkpoint writes, confirmed by a process
stack sample. The workers were stopped and the disposable clones removed; no
parallel question had completed. The replacement keeps one database owner and
runs up to eight independent discovery sessions in that node. Tests cover separate
question records, duplicate/unsafe ID rejection before model calls, and the earlier
empty-review paths. All eight agent tests and strict CLI clippy passed. The shared
provider meter is recorded as a batch total, with no invented per-question spend.
The batch path and low-effort option are execution changes, and their effects on
runtime are to be measured separately from retrieval quality.

The owner then requested a speed comparison before continuing and instructed us to
abandon GLM and wait for a replacement model. At interruption, twelve matching
completed questions averaged 50.04 seconds with GLM low effort versus 7.19 seconds
in the earlier GPT-5.4 run. The observed ratio is 6.96, with different concurrency
settings, eight versus one, and incomplete-question selection. It is a preliminary
latency observation, not a completed benchmark or a controlled model-speed estimate.
The pair-level evidence is in
[the timing comparison](../benchmarks/experiments/20260913-glm-full/timing-comparison.json).
Both answer evaluation and the resumed vocabulary indexing process were stopped;
checkpoints remain locally. No new BEIR, full vocabulary-corpus evaluation, or
judged GLM quality comparison was completed. No automatic continuation is scheduled.

The owner then selected GPT-5.4 again and authorized a 100-question check only,
followed by a score review and a pause before any 500-question extension. The
selection is the first fifty and last fifty release questions on the existing
full index. Agent effort returns to the endpoint default and judging uses the
original upstream medium setting. Vocabulary indexing remains interrupted.

The authorized 100-question GPT-5.4 check completed with all questions judged, no
skips, and no correction. Combined score was 78.98 and correctness 82%. The same
fifty questions scored 85.80 versus 87.33 before, with identical initial rankings;
the added fifty scored 72.17. High-level questions scored 35 and unanswerable ones
100. Eleven of twelve incorrect questions with expected document IDs had zero
expected-document recall in the initial-plus-agent trace. One wrong answer had the
correct source exposed but substituted facts from related material. These findings
support further investigation of missing-source discovery and source selection,
not a demonstrated indexing improvement. The original twenty-question sample still
scores 100 and would have hidden these failures.

Answer generation took 427.18 seconds with eight sessions sharing one database;
total with the official judge was 634.14 seconds. Reported answer charges were $11.00;
judge charges remain unknown. Different concurrency and question mix prevent a
causal runtime comparison with the earlier sequential fifty. The replay used the
existing full index, not a completed vocabulary-enriched index. The owner-required
pause is in effect before 500 questions or more indexing. Evidence and per-question
comparisons are in the [100-question report](../benchmarks/reports/20260913-gpt54-100-bookends.md).
