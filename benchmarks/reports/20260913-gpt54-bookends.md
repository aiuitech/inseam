# GPT-5.4 navigation baseline, 13 September 2026

The first 10 and last 10 EnterpriseRAG questions scored **92.0/100** through
Finder navigation against the full 511,962-document corpus. The run took
341.54 seconds, including 44.46 seconds of judging. This establishes a baseline
for this sample; it does not establish an improvement over an earlier version.

[Run manifest](../runs/enterprise-rag-bench/20260913T181642Z-b414e6e7aa0f/agent/20260913T194431Z-b34a3ecaa1f4/manifest.json)
records the full configuration and origin index. The question IDs are
`qst_0001` through `qst_0010` and `qst_0491` through `qst_0500`.

| Questions | Count | Correctness | Completeness | Combined score |
| --- | ---: | ---: | ---: | ---: |
| All selected | 20 | 95.0% | 92.0% | 92.0 |
| Basic, first 10 | 10 | 90.0% | 84.0% | 84.0 |
| Information not found, last 10 | 10 | 100.0% | 100.0% | 100.0 |

The combined score is the upstream mean of completeness gated by correctness.
GPT-5.4 generated answers through `inseam agent` and judged them through the
unmodified upstream evaluator at `d36685e273713975ee20299bbf1ab64165575b3c`.
Both judge model roles used `openai/gpt-5.4` through OpenRouter. Gold correction
was disabled. All 20 rows were judged, none were skipped, and every whole-answer
judgment included its reasoning. The questions checksum matches the pinned
v1.0.0 release.

The index was reused without indexing or repair. It retains folders and their
navigation links, uses whole-document text with no vectors, and was queried
with seed k 60, RRF k 60, damping 0.5, and lexical weight 0.1. The agent kept
the 12-turn budget. A frozen copy of the installed binary ran every question;
its SHA-256 is in the manifest. The source revision was `b34a3ecaa1f4` plus the
tracked Rust edits saved in `source-diff.patch`. Those edits belonged to work
already in progress; this benchmark change did not modify the Finder.

The agent made 75 queries, 47 scans, 12 expansions, and 3 fetches. Answer
generation took 261.76 seconds and reported $1.6361 in provider cost. The
separate initial retrieval probes took 34.88 seconds. Judge cost is unavailable
from the upstream evaluator, so $1.6361 is not the total run cost.
`navigation-stats.json` records per-question timing, cost, and tool counts.

The initial top-eight ranking had 90.0% expected-document recall and 0.783333
MRR across the 10 questions with expected documents. The scorer's document
recall was also 90.0%. That field combines the initial Finder hits with IDs in
the agent trace, as the existing harness does; it is not final-citation recall.
Without gold correction, extra documents are not checked for alternative
validity, so the reported 7.3 extra documents should not be read as proven
irrelevance.

## What to compare next

The only incorrect answer was `qst_0002`. The gold document appeared in the
initial top-eight probe, but its ID did not appear in the agent trace. The
agent answered `session_finalization_lag_ms`; the judge expected
`stream.timebox_finalized`. This is a concrete navigation failure to track.
`qst_0009` and `qst_0010` were correct but only 80% and 60% complete.

Repeat the [fast navigation command](../README.md#fast-gpt-54-navigation-check)
after installing a Finder change. Compare these same IDs, the combined score,
the two type scores, and the per-question judgments. Keep the model, turn
budget, and index shape fixed unless they are the variable under test.

This sample is 4% of the question set and deliberately uses its bookends.
It covers basic and unanswerable questions only. It does not measure semantic,
constrained, or multi-document performance, and its score is not an estimate
of the full leaderboard score. One changed answer moves correctness by five
points. Small apparent gains need repeated runs; changes aimed at omitted
question types need a broader sample.

Validation: all 53 benchmark tests passed, `git diff --check` passed, and
`cargo install --path crates/inseam-cli` completed. The saved result IDs match
the selected question IDs exactly.
