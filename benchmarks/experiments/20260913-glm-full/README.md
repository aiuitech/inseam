# Full GLM comparison

Stopped at the owner's request pending a new model choice. The owner requested the old full EnterpriseRAG index evaluated
with GLM, resumption of the killed vocabulary slice, then new full BEIR and
EnterpriseRAG comparisons. They subsequently requested lower reasoning effort.

`batch_replay.py <completed-full-index-run-id> --model z-ai/glm-5.3-flash
--bookend-count 250 --reasoning-effort low` evaluates all 500 release questions
with GLM low effort, twelve discovery turns, eight answer sessions
sharing one database owner, and four judge workers. `judge_low.py` sets the two
upstream model factories to low effort before running the pinned official scorer.
It does not modify judge prompts, scoring, gold data, or correction policy.

The command writes durable per-question records under `batch/attempt-N/` and retries
only missing execution results, at most three attempts. Judged quality never
triggers a retry. Answer files are merged in release order for the official scorer.
Per-question provider costs are unavailable; the shared meter is a batch total.
With BYOK, a zero OpenRouter cost is not evidence of zero upstream inference cost.

`full_replay.py` preserves the earlier sequential continuation path used before the
owner requested low effort. Its partial runs must not enter the low-effort score.
The rejected APFS-clone driver is retained locally at
`/tmp/inseam-glm-parallel-replay-rejected.py`; it produced no completed parallel
question and was replaced because checkpoint writes stalled the workers.

The isolated baseline build uses source `a4951ac828ca` plus the empty-review repair,
effort control, and batch transport. Its run records the binary hash and exact
source patch. The current build additionally contains the newer Finder and
vocabulary implementation. Compare same-question subsets against the prior GPT-5.4
runs, and distinguish changed answer/judge models from indexing improvements.

## Preliminary timing result and stop

Twelve completed matching questions averaged 50.04 seconds of GLM answer time
versus 7.19 seconds in the prior GPT-5.4 run, a 6.96 ratio. GLM used low effort
and eight concurrent sessions; GPT used its endpoint default and one session.
These are observed per-question times under different concurrency and provider
conditions, not a controlled estimate of intrinsic model speed. The sample excludes
unfinished questions and has no new judged quality score.

See `timing-comparison.json` for all pairs. The full GLM run and resumed slice were
interrupted at the owner's request. Their local checkpoints remain. No BEIR or
new full-corpus EnterpriseRAG run was started, and no background monitor was set.

The owner subsequently selected GPT-5.4 and authorized only 100 questions before
review. The batch driver defaults to GPT-5.4 and 50 questions from each end.
Without `--reasoning-effort`, it preserves the agent endpoint default and calls
the unmodified upstream scorer with its existing medium judge effort. The 100
question run must finish and report scores before any extension to 500.

The authorized GPT-5.4 100-question run completed with score 78.98 and 82% correctness.
The [report](../../reports/20260913-gpt54-100-bookends.md) records the comparisons.
Execution is paused at the owner's review gate; the scripts are not scheduled.
