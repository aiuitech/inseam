# Indexing and Finder experiment history

Start here for what we tried, what helped, what failed, and what remains
unmeasured. This is the experiment history; [indexing](indexing.md),
[Finder](finder.md), and [Finder refinement](finder-refinement.md) describe
design decisions. Detailed reports and run manifests are the evidence behind
each entry. Results on different corpora, question samples, or scoring methods
are separate comparisons.

## How to maintain this history

After an indexing, retrieval, or navigation experiment, add a dated entry with
the hypothesis, exact change, comparison scope, measured result, time/cost or
storage tradeoff, decision, and evidence link. Record regressions and abandoned
approaches as well as gains. Name the run and source revision in the linked
report. Keep earlier entries intact; append a correction or superseding result
when evidence changes. Update the affected design document when adopting a
decision.

Use explicit conclusions: measured improvement, regression, inconclusive,
operational fix, or unmeasured proposal. A bundled change does not establish
which component caused its gain. A development-set result does not establish
general benchmark improvement. Raw command logs stay local under the existing
[benchmark evidence policy](benchmarking.md); completed scores, configurations,
answers/rankings, and manifests are versioned.

## 7 September 2026: retrieval hints and vocabulary

These earlier measurements are recorded in
[the indexing design](indexing.md).
They use a 25,000-document EnterpriseRAG slice and its offline retrieval
comparisons, not the later GPT-5.4 bookend sample.

| Experiment | Measured result | Conclusion |
| --- | --- | --- |
| Add model-generated hints and keyed rows to the documents' full-text table | Recall fell from 86.1% to 74.7%. About 300,000 short keyed rows changed length statistics. | Regression. Split prose and lexical tables. |
| Separate prose and lexical tables | Recall recovered to 83.4%; subsequent offline comparisons use an 84.3% baseline. | Keep the separation; do not mix these baselines. |
| Use hint prose as full-text evidence | 83.9% recall against 84.3%. Gains on basic/completeness, loss on semantic. | Approximately neutral overall. |
| Spread keyed glossary hits to their sources | At best about 64% recall despite damping. Generic terms joined too many sources. | Reject this seeding approach. |
| Replace whole-document vectors with cue/synopsis vectors | Cue/synopsis vectors alone reached 52–54% recall. | Retain whole-document evidence. |
| Fuse cue vectors as an equal vote | Lost four recall points. | Reject equal weighting. |
| Give cue vectors weight 0.3 and admit only their top five ranks | 85.6% recall, 1.3 points above the offline baseline, with about 0.04 MRR gain. | Limited measured benefit; not a demonstrated end-to-end gain. |
| Rewrite cue prompt to describe things without their internal names | Question-word overlap rose from 13% to 23%; cosine rose from 0.52 to 0.55. | Diagnostic improvement only. Large-scale retrieval benefit remains unmeasured in this evidence. |

[Vocabulary](vocabulary.md) proposes mining corpus-local terms and grounding
clusters in response to these failures. Its validation gates distinguish the
proposal from demonstrated retrieval gains. An implementation in progress is
not a benchmark result; add its measurements here when available.

## 14 September 2026: vocabulary pass cost

Evidence: [vocabulary recall report](../benchmarks/reports/20260913-vocabulary-recall.md)
for the measurements that motivated the change; the reworked pass's own
timings are appended here as slice runs complete.

| Experiment | Measured result | Conclusion |
| --- | --- | --- |
| First vocabulary pass on the full corpus (stored `mentions` edge per match, host facet row, frequency recount over relations, two store reads per row in clustering) | Abandoned after eight hours: 86 minutes matching, four hours planting the host facet, hours in frequency and cluster work, 9.2 million candidates dropped at the cap; `for`, `to`, `in`, `with` the most frequent rows (510,284 sources). | Operational failure. The pass as built cannot be iterated on. |
| First vocabulary pass on BEIR NFCorpus | Index 63,832,129 bytes against 40,353,857 (+58.2%); 9,119 s to index, 9,060 s of it in sequential grounding; Recall@10 18.764 against 18.708. | Neutral recall; storage and time unacceptable. |
| Rework: no stored edges for mined rows (the full-text index is the anchor, read at query time), in-memory frequency and clustering, no host facet, normalized-spelling counting under a seen-filter, concurrent grounding, merges demote to aliases | 25,000-document slice, retrieval only, no model calls (`20260914T141017Z-ff096cca3922`): the pass took 19.8 s — mine 12.8 s, match 4.5 s, cluster 2.4 s — inside 45.6 s of indexing; 82,867 rows planted, 1,558,205 row–source matches counted, zero relations written; index 310,001,729 bytes against 290,394,177 for the same slice without rows (+6.8%). Recall@8 83.59%, hit rate 86.6%, MRR 0.674 against 83.73 / 86.81 / 0.649 on the row-less slice (`20260913T224122Z`). Exact grounding fired on 491 of 500 questions and carried walk mass into 20,979 results. | Measured operational fix: the pass is seconds on the slice and adds under seven percent of storage. Recall is neutral within run variation; the vocabulary as mined is not yet buying recall. The twenty most frequent rows (`pray`, `await`, `lots`, `reflect`, `expensive`, document frequency 493–499) are general English under the 2% band top, so the shape rule's function-word list is the next thing to widen. |

## 13 September 2026: indexing and initial retrieval

Evidence: [retrieval audit](../benchmarks/reports/20260913-retrieval-audit.md),
its linked manifests, and the
[incremental indexing experiment](../benchmarks/experiments/20260913-incremental/README.md).

| Experiment | Measured result | Conclusion |
| --- | --- | --- |
| Lower lexical contribution from 1.0 to 0.1 or 0.25 on the unchanged full Enterprise index | Recall@8 rose from 63.37% to 66.42%; MRR from 0.432677 to 0.596429. Folder slots fell from 1,170 to 13 or 20 out of 4,000. | Measured improvement for this profile. Both weights tied on retrieval metrics. |
| Reduce BEIR RRF k from 60 to 10 on the same index | nDCG@10 rose from 0.36885 to 0.37926; repeated runs supported k=10 but varied. | Use k=10 for that BEIR profile; not a universal default. |
| Reduce BEIR vectors from 384 to 192 dimensions | About 27% less index storage; nDCG@10 fell to 0.35222. | Retain 384 dimensions for this comparison. |
| Shorten folder summaries to 400 characters on the Enterprise slice | Index shrank about 3.3%; best short-folder recall exceeded the long-folder 0.1 profile by only 0.12 points. | Keep long summaries in full-corpus navigation comparisons; compact folders remain an option. |
| Remove graph propagation | Slice recall fell to 74.45%, versus 79.78% in the corresponding zero-keyword comparison. | Retain propagation for this profile. |
| Expand seed pool from 60 to 120 at lexical weight 0.1 | Slice recall fell from 84.16% to 83.92%. | Reject the larger pool for this setting. |
| Compare keyword caps 12 and 0 | Slice recall was 79.83% versus 79.78%. | Inconclusive evidence for a ranking benefit. |
| Reindex only changed folder outputs | Rebuilt 275 folders while retaining 25,000 files. Finder-only replay avoided indexing entirely. | Verified reuse. Timings under concurrent load are descriptive, not isolated speedups. |
| Poll status while indexing | An early full run failed with a SQLite lock because status also boots a writer. | Operational fix: use elapsed-time heartbeats during indexing. |
| Remove folders before calculating MRR | Could overstate MRR by hiding occupied result ranks. | Scoring fix: retain folder slots. Older MRR from that method is not comparable. |

The retrieval audit preferred lexical weight 0.25 for a subsequent navigation
experiment. The actual GPT-5.4 runs below fixed it at 0.1. The tied retrieval
scores do not establish that these weights have identical navigation behavior.
The manifests, rather than a proposed next setting, identify what ran.

## 13 September 2026: GPT-5.4 navigation

All three runs use the full 511,962-document index. Answer and official judge
models are GPT-5.4, with gold correction disabled. Costs below are answer
provider costs only; the judge does not report its cost.

| Experiment | Result | Conclusion and evidence |
| --- | --- | --- |
| Establish a first-10/last-10 baseline | Score 92; 95% correctness; 5m 42s; $1.64. | Three questions exposed candidate loss and incomplete answers. [Baseline](../benchmarks/reports/20260913-gpt54-bookends.md). |
| Add stable accumulated candidates, original-question search, relevant excerpts, document sizes, combined search/read actions, and a completion check | Same 20 scored 99; 100% correctness; 7m 25s; $2.81. Initial rankings were identical. | Measured gain for the bundle, with 30% more runtime and 72% more answer cost. Individual contributions were not isolated. [Report](../benchmarks/reports/20260913-gpt54-refinement.md). |
| Expand to first 25 and last 25 questions | All 50 scored 87.33 with 90% correctness; repeated 20 scored 100; added 30 scored 78.89. Runtime 18m 44s; answer cost $6.61. | Broader coverage exposed failures, not regression on the original 20. [Report](../benchmarks/reports/20260913-gpt54-50-bookends.md). |

The [failure diagnosis](../benchmarks/reports/20260913-navigation-improvement-plan.md)
explains the original misses and compares them with public answers. The
implementation landed in `09bc883`; `300afd9` corrected long-window scan
continuation. The 50-question results and command were committed in `0e0863e`.

What worked in the observed navigation traces: reading the original third-ranked
candidate recovered `stream.timebox_finalized`; preserving source qualifiers
recovered the US-region scope of an SLO. The completion of question 10 varied
from 80 to 100 across the two revised runs, so the later point is not another
algorithm improvement.

What remains weak: the expanded sample had two incorrect basic answers, three
incorrect high-level answers, and three incomplete basic answers. Only two of
five high-level questions were correct. All 20 unanswerable questions scored
100 but consumed $4.34 of the $6.61 answer cost. The report preserves question
IDs, judge explanations, and the observed source-selection mistakes.

## Open experiments, not established conclusions

- Check coverage and selection of governing company overview and policy sources.
  Related operational documents produced the wrong revenue categories,
  commercial add-ons, and department lists. Index absence versus retrieval or
  reading failure still needs to be separated.
- Reduce repeated searches on unanswerable questions while retaining correct
  abstention and answerable-question recall.
- Isolate the contributions of accumulation, excerpt selection, batching, and
  the completion check if deciding which costs are necessary.
- Evaluate vocabulary proposals against their stated gates. Keep unmeasured
  implementations separate from adopted, benchmark-supported choices.
- A separate reranking model has not been tested in this navigation series.
  Not adding one is not evidence that it could never help.
- The 50-question bookends cover basic, high-level, and unanswerable questions.
  Semantic and other omitted categories still need their own evaluation.

### Full GLM run preparation, 13 September 2026

Full 500-question GLM answer/judge evaluation and the interrupted vocabulary slice
`20260913T224507Z-812f3f4af037` are in progress. The first full answer attempt stopped
at question 2 because an empty final model review erased an existing draft. The
agent now preserves a completed draft in that case and reports a true empty-answer
error when neither response contains an answer. Four new offline behavior tests
pass. This is an execution repair, not evidence of a retrieval improvement.
See the [SR&ED continuation](../docs/sred.md) for the probes and measurement plan.

The GLM attempt was subsequently abandoned at the owner's request, pending a new
model choice. Twelve completed same-question pairs showed 50.04 seconds average
answer time for GLM low versus 7.19 for the prior GPT-5.4 run. Different concurrency
settings and unfinished questions limit the comparison. Both the answer run and
resumed slice were stopped, with checkpoints retained. See the
[partial experiment record](../benchmarks/experiments/20260913-glm-full/README.md).

### GPT-5.4 100-question review point, 13 September 2026

The [100-question report](../benchmarks/reports/20260913-gpt54-100-bookends.md)
records 78.98 combined score and 82% correctness. The repeated fifty score 85.80
versus 87.33 previously; their initial rankings are identical. The added fifty
score 72.17. High-level questions score 35, while unanswerable questions score 100.
Eleven of twelve incorrect questions with expected documents never expose those
documents in the initial or agent-trace union. No initial retrieval gain is shown.
The original twenty still score 100, so that small sample is inadequate for this gap.

One database owner with eight independent answer sessions completed answers in
427.18 seconds; total with judging was 634.14 seconds. Answer charges were $11.00,
judge charges unrecorded. Both workload mix and concurrency differ from the earlier
50-question run. The existing full index was reused; vocabulary enrichment remains
unmeasured. Stopped at the owner's gate: no 500-question extension or indexing resume.

## Vocabulary-enriched recall comparison, 13 September 2026

The owner authorized fresh vocabulary-enriched indexing and recall-only evaluation on BEIR NFCorpus and full EnterpriseRAG after the latest algorithm changes. The [comparison report](../benchmarks/reports/20260913-vocabulary-recall.md) records the methodology, controls, run identities, indexing observations, and completed results. No agent answers or LLM judging are involved; vocabulary grounding still uses model calls.

BEIR Recall@10 is 18.764% after enrichment, versus 18.708% historically and 18.767% with the current Finder on the old index. These changes are within earlier retrieval variation and do not demonstrate improvement. All 323 test queries ran. Indexing took 9,118.744 seconds, chiefly 500 sequential grounding calls, and the settled index grew 58.2% to 63,832,129 bytes. The full EnterpriseRAG old-index control remains at 66.42% Recall@8. The owner abandoned fresh EnterpriseRAG enrichment after eight hours, before recall scoring. Both processes exited, and no restart is scheduled. Full enriched EnterpriseRAG recall remains unknown. The report records host-facet paging, common-word vocabulary, candidate-cap drops, and prolonged clustering as evidence for a later indexing investigation.
