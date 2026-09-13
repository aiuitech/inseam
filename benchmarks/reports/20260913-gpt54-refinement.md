# Additive Finder navigation, 13 September 2026

The same 20-question EnterpriseRAG development sample improved from **92 to
99/100** after implementing additive discovery, relevant indexed excerpts,
size-aware source reading, combined tool actions, and a source-based completion
check. All 20 answers were correct. The full 511,962-document index was reused;
no indexing, embeddings, or separate reranking model was added.

[Run manifest](../runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/agent/20260913T205625Z-09bc883d9a0a/manifest.json)
and [navigation statistics](../runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/agent/20260913T205625Z-09bc883d9a0a/navigation-stats.json)
record the configuration, binary identity, timing, cost, and tool actions.

| Measurement | Baseline | Additive navigation |
| --- | ---: | ---: |
| Combined correctness-gated completeness | 92.0 | 99.0 |
| Correctness | 95% | 100% |
| Basic questions, combined score | 84.0 | 98.0 |
| Information-not-found questions, combined score | 100.0 | 100.0 |
| Initial expected-document recall | 90% | 90% |
| Total duration | 341.54 s | 445.06 s |
| Answer generation | 261.76 s | 351.81 s |
| Separate retrieval probes | 34.88 s | 27.17 s |
| Judge duration | 44.46 s | 65.68 s |
| Answer provider cost | $1.6361 | $2.8092 |

Judge cost is unavailable. Total duration increased 30%; answer cost increased
72%. These are observed wall times, not isolated performance measurements:
provider latency varies, and local validation/build work overlapped part of the
new run. The extra search/read actions and provider charges are directly logged.

## What changed in the answers

| Question | Baseline score | New score | Observed change |
| --- | ---: | ---: | --- |
| qst_0002 | 0 | 100 | Reads the original third-ranked candidate and correctly names `stream.timebox_finalized`. |
| qst_0009 | 80 | 100 | Preserves the hosted US-region scope of the 99.9% SLO. |
| qst_0010 | 60 | 80 | Includes short-lived cohort aggregates; one completeness item remains missing. |

Every other per-question score stayed at 100. The initial top-eight document
IDs and their order were identical to the baseline on all 20 questions. This
supports attributing the observed answer changes to navigation and answer
construction rather than a changed initial ranking. It does not separate the
effects of the individual tool and prompt changes or eliminate model variance.

Question 10 still omits the source's lightweight-service qualification. That is
a plausible remaining completeness gap, but the upstream saved result contains
only the aggregate completeness percentage, not individual fact verdicts.

The [earlier public-answer comparison](20260913-navigation-improvement-plan.md)
recorded 100 for CDL and 95 for Metor and Troml on these same IDs. Our 99 closes
most of that sample gap. Different systems and judging runs make this an
informal comparison, not leaderboard parity.

## Evidence that the new tool is used

The agent made 97 `find` calls, including 20 automatic original-question
searches. Those calls contained 161 searches, 93 scans, 10 expansions, 73
candidate inspections, and 69 explicit candidate removals. Forty-one batches
combined action types; 30 contained multiple source reads. The baseline made
75 queries, 47 scans, 12 expansions, and 3 fetches. A batch count is therefore
not directly comparable to the old count of individual tool calls.

Unanswerable questions consumed 142 searches, 66 scans, 262.74 answer seconds,
and $2.1406: about 76% of answer cost. They already scored 100 in the baseline.
The next speed experiment should reduce repeated searches after independent
attempts return no new supporting evidence, while checking that answerable
questions and abstention correctness do not regress. Keep this same sample
before expanding the evaluation set. Do not add a reranker to address a loss
that remains in completeness after the correct source was read.

## Reproducibility and validation

The run uses `qst_0001`–`qst_0010` and `qst_0491`–`qst_0500`, GPT-5.4 for answers
and both official judge roles, 12 agent turns plus the new tool-free completion
check, and the same Finder settings as the baseline. The pinned unmodified
upstream evaluator is `d36685e273713975ee20299bbf1ab64165575b3c`; gold correction
is disabled. All 20 selected IDs were answered and scored, none were skipped,
and every correctness judgment contains reasoning.

The evaluated source is `09bc883d9a0a`, with an empty tracked Rust source diff.
The repository-dirty flag reflects unrelated workspace content. The frozen
binary SHA-256 is recorded in the manifest. Follow-up commit `300afd9` fixes
continuation after a character-truncated 2,000-line window and adds a regression
test. It is installed locally but was not in the frozen benchmark binary. No
benchmark scan requested more than 396 lines, so that changed branch was not
exercised by this run.

Validation passed: 10 offline discovery integration tests, 2 agent unit tests,
53 benchmark tests, formatting/diff checks, and targeted Clippy with warnings
denied (`collapsible_if` allowed to preserve the repository's explicit nested
conditions). `cargo install --path crates/inseam-cli` completed after the final
code change. Raw answers, queries, traces, judge output, and manifests are saved
with the run.

This is one run on a development set already used to guide changes. Its bookend
selection covers basic and unanswerable questions only, not semantic,
constrained, or multi-document questions. Repeat the result before treating
the gain as stable; it cannot estimate a full leaderboard score.
