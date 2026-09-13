# Closing the navigation gap on the 20-question sample

This is a diagnosis and proposed experiment order, not an implemented change.
It uses the [92-point baseline](20260913-gpt54-bookends.md), the public per-question
answers and judgments, and retrieval replays with the baseline's frozen binary.
No additional model evaluation or larger question set was run.

## What the leading systems disclose

[Metor](https://www.metor.com/blog/metor-memory-leads-enterpriserag-bench/)
attributes its result to disambiguation, counting, selective semantic matching,
and requiring the agent to read before making claims. It reports one agent and
one attempt per question, and says its own runs used Claude Code with Opus 5.
These are vendor disclosures, not a reproducible description of its retrieval
implementation. Its model and harness are another confound when comparing
Finder algorithms.

[CDL's Reasonara description](https://causaldynamics.com/research/ai-context-window-problem-reasonara)
describes typed concepts, short discovery labels backed by full detail,
relationships, relevance cues, and a learned refine/expand/stop policy.
That product description does not establish which components produced the
EnterpriseRAG submission. Inseam already has a graph and navigation operations;
this benchmark profile disables entity extraction and generated hints, so it
does not exercise equivalent semantic enrichment.

[Troml's submission](https://github.com/onyx-dot-app/EnterpriseRAG-Bench/issues/13)
discloses a multi-step GPT-5.4 system, but not enough retrieval detail to identify
an algorithm to copy. Published answers show what it got right, not how it
searched.

## Where our eight points went

Scores below are correctness-gated completeness, with a maximum of 100 per
question. Each question contributes one twentieth of that score to the sample.

| Question | Inseam | Metor | CDL | Troml | Inseam's lost sample points |
| --- | ---: | ---: | ---: | ---: | ---: |
| qst_0002 | 0 | 0 | 100 | 100 | 5 |
| qst_0009 | 80 | 100 | 100 | 100 | 1 |
| qst_0010 | 60 | 100 | 100 | 100 | 2 |

Sources: the public result files for
[Metor](https://huggingface.co/spaces/onyx-dot-app/EnterpriseRAG-Bench-Leaderboard/resolve/main/data/raw_data/results_metor.json),
[CDL](https://huggingface.co/spaces/onyx-dot-app/EnterpriseRAG-Bench-Leaderboard/resolve/main/data/raw_data/results_cdl.json),
and [Troml](https://huggingface.co/spaces/onyx-dot-app/EnterpriseRAG-Bench-Leaderboard/resolve/main/data/raw_data/results_troml.json).
Metor's three-point sample lead is entirely completeness on questions 9 and 10;
it also failed question 2. All other Inseam sample answers scored 100.

Question 2 asked for a specific metric for streaming time-limit finalization.
The original-question probe ranked its gold document third, but that probe is
independent of `inseam agent` and its results are not passed to the model.
The agent started with a rewritten query and five results, then made three
more queries. Replaying those exact query texts and limits with the frozen
binary found no gold document in any returned list. The final query introduced
an unsupported identifier and drifted toward capacity-limit documents. The
answer named `session_finalization_lag_ms` instead of `stream.timebox_finalized`.
The trace proves the wrong documents were read; the replay identifies a
candidate-loss mechanism without claiming to recover the exact historical
tool responses, which were not logged in full.

Question 9 read the correct email thread. The answer kept the 99.9% SLO but
omitted its scope, hosted instances in the US region. The source explicitly
contains that qualifier on line 123. The competitors' answers preserved it.

Question 10 fetched the complete correct source. Its answer omitted the
short-lived cohort aggregates and did not retain the service's lightweight
qualification. Both are in line 3 of the source. The baseline retained only
aggregate completeness, not each fact check's verdict, so the exact assignment
of its two failed fact checks cannot be proven from the saved judge output.
The competitors' answers explicitly describe the short-lived aggregates.

## Proposed experiments, one variable at a time

1. Preserve original-query candidates. Begin the agent with a bounded original
   question search and make those results available before optional rewrites.
   Keep an address-deduplicated candidate set when adding rewritten searches.
   This uses only the user's question and retrieved evidence, never gold IDs.
   It brings the evidence measured by the initial retrieval probe into the
   actual answer path. Verify that the agent can inspect the original results
   and that rewrites cannot erase them.

2. Check fact coverage before finishing. Build an answer from relevant claims
   in the read sources, preserving conditions, dates, regions, units, optional
   terms, and intermediate mechanism steps. Reserve at most one model call for
   comparing the draft with those sources and the user request. The check must
   not see benchmark facts or judge output. Missing evidence should prompt a
   bounded follow-up search or an explicit uncertainty, not invented detail.

3. Require support for the exact requested relationship. A document mentioning
   a related metric is insufficient if it does not connect that metric to the
   requested trigger. When plausible candidates disagree, compare their source
   passages and qualifiers before choosing. Do not require multiple documents
   when one directly supports the answer.

4. Make tool responses compact and structurally complete. The agent currently
   serializes a full response and truncates it at 12,000 characters. On the
   question 2 replays, ten-result replies were approximately 17,000 characters,
   with about 6,000 characters of metadata. The result arrays themselves fit
   within the cap on those replays, so truncation does not explain that miss.
   Still, an explicit model-facing representation should omit diagnostic
   metadata and budget complete results, with a continuation signal for omitted
   content, rather than cut JSON mid-field. Test real-sized envelopes and hints.

Use the same 20 questions, GPT-5.4 models, full index, and 12-turn budget for
the first comparisons. Report per-question changes alongside the aggregate,
check that abstention remains intact, and track latency and model cost.
Recovering the observed omissions could close the gap, but that is a hypothesis,
not a predicted score. Repeat an apparent win before accepting it; these
questions become a development set once they guide changes.

## What to defer

Do not rebuild half a million documents or add vectors to explain these three
losses. Two already had the right source in hand, and the third was found by
the original query. Generated semantic cues, richer entity relations, hybrid
retrieval, and reranking remain separate experiments for failures that actually
need them. The fixed 20-question sample cannot establish their value for the
semantic and multi-document categories it omits.

The performance tradeoff favors query-time changes first. One original-query
probe can replace the agent's first search, and the coverage check adds at most
20 model calls per sample. There are no index writes or corpus-wide model calls.
The baseline already spends 262 seconds answering and 44 seconds judging;
measure the added model latency instead of assuming it is negligible.
