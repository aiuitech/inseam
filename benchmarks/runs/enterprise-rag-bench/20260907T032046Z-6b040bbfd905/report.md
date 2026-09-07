# EnterpriseRAG-Bench, end to end: 69.0

Run `20260907T032046Z-6b040bbfd905`, inseam `6b040bb`, 2026-09-07. All 500 questions, answers by `z-ai/glm-5.3-flash` through `inseam agent` (12 tool turns, 10 documents per query), judged by the upstream evaluator at its pinned revision, also on `z-ai/glm-5.3-flash`, without the gold-correction flow (`--no-correction`).

## Result

| metric | inseam | leaderboard neighbours |
| --- | --- | --- |
| overall (correct × completeness) | **69.0** | Skyller 71.9 · OpenClaw 68.2 |
| correctness | 73.6 | Troml 83.8 · metor 82.0 · Skyller 77.0 · SovraRAG 74.6 |
| completeness | 75.4 | metor 86.2 · CDL 85.2 · Troml 81.8 · Skyller 79.1 |
| document recall | 77.1 | Troml 86.6 · metor 85.5 · CDL 80.5 · Skyller 81.6 · NVIDIA 72.6 |
| invalid extra documents | 9.6 | OpenClaw 0.5 · metor 5.0 · OpenAI File Search 15.7 |

Against the 25 entries on the public leaderboard that is 5th overall, behind metor.com (80.3), CDL (79.0), Troml (76.8), and Skyller (71.9), and ahead of OpenClaw (68.2), SovraRAG (65.6), fgroo (63.3), OpenAI File Search (61.0), and the two GPT-5.4 baselines (Bash agent 52.6, BM25 50.6). The comparison is indicative, not a submission: the leaderboard's judge is Onyx's, ours was GLM Flash; their baselines answer with GPT-5.4, we answered with GLM Flash; and we ran without the gold-correction flow, which on the leaderboard tends to move a point or two in a system's favour.

What it cost: the index is model-free (297 s and $0 on an idle machine; 1,000 s here under a dozen concurrent sessions), the 500 answers took 3.1 hours (median 16 s, p90 47 s, 58 of them ran the turn budget out and closed from what they had read), the judge took 12 minutes at 8 workers. OpenRouter's key accounting moved by $0.13 across answers and judge; GLM Flash is priced at $0.075 per million input tokens, so the true figure is a few dollars at most, and the agent's own meter reads zero because the endpoint returns no cost for this model.

## By question type

| type | n | overall | correct | complete | recall |
| --- | --- | --- | --- | --- | --- |
| info_not_found | 20 | 100.0 | 100.0 | 100.0 | – |
| intra_document_reasoning | 40 | 86.2 | 87.5 | 90.2 | 97.5 |
| miscellaneous | 20 | 85.0 | 85.0 | 87.5 | 90.0 |
| constrained | 30 | 81.5 | 83.3 | 93.0 | 95.0 |
| basic | 175 | 77.0 | 77.7 | 80.5 | 84.6 |
| completeness | 20 | 71.3 | 80.0 | 75.1 | 68.1 |
| conflicting_info | 20 | 64.5 | 70.0 | 76.4 | 77.5 |
| high_level | 10 | 54.7 | 70.0 | 74.7 | – |
| project_related | 40 | 52.4 | 70.0 | 70.2 | 74.1 |
| **semantic** | 125 | **48.6** | 56.0 | 55.3 | 56.0 |

Semantic questions are a quarter of the set and score 28 points below basic ones. They are written to share few words with their document ("the new top end 80GB accelerator" for the H200 launch; "the safest numeric mode" for a precision step-down gate), which is exactly what a keyword index cannot bridge. Every other type sits between 64 and 100.

## By source

| source | n | overall | correct | recall |
| --- | --- | --- | --- | --- |
| linear | 44 | 80.3 | 88.6 | 81.8 |
| jira | 60 | 77.9 | 85.0 | 85.0 |
| google_drive | 42 | 77.9 | 81.0 | 81.0 |
| gmail | 42 | 76.2 | 76.2 | 85.7 |
| multi-source | 98 | 70.6 | 80.6 | 75.6 |
| confluence | 64 | 64.0 | 68.8 | 74.8 |
| github | 39 | 62.6 | 64.1 | 76.9 |
| slack | 57 | 61.4 | 61.4 | 71.9 |
| hubspot | 33 | 53.8 | 54.5 | 69.7 |
| fireflies | 21 | 52.4 | 52.4 | 57.1 |

Structured records with a title and fields (tickets, issues) do best. Transcripts (fireflies) and CRM records (hubspot) do worst: a transcript buries one fact in twenty thousand characters of talk, and a CRM record is a stack of near-identical accounts that differ in one figure.

## Where the gold documents ended up

The 470 questions with gold documents name 741 of them. After retrieval (top 10 per query) and the agent's own follow-up searches:

| fate | gold docs | in wrong answers |
| --- | --- | --- |
| cited by the agent | 427 | 37 |
| in the first top 10, never cited | 113 | 51 |
| ranked 11–25 only | 42 | 17 |
| not in the top 25 | 159 | 103 |

62 of the cited documents were not in the first top ten: the agent's second and third searches found them. Only 6 wrong answers had every gold document cited, so the model rarely misreads what it reads; the losses are in what reaches it and what it chooses to read.

## Shortcomings, with examples

### 1. Near-duplicate siblings win the ranking, and the agent answers from the sibling

The corpus is built with near-duplicates that carry updated or conflicting facts. A keyword index ranks the sibling that shares the most words with the question, and the agent, reading one or two results, answers confidently from it. This is the largest single failure mode: it accounts for most of the 48 wrong answers whose gold document was in the top ten but never read, and for many of the 65 where the gold document was not in the top 25 because a sibling took its place.

- **qst_0035** (basic, confluence). *What is the tenant-facing monthly availability target for regulated private VPC deployments?* Gold: 99.95%. Ours: 99.9%, read from the "Redwood Inference for Financial Services one-pager" draft, whose sample SLO table says 99.9%. The gold document was ranked in the top ten and not opened.
- **qst_0177** (semantic, gmail). *Final concession terms for the retail partner marketplace purchase.* Gold: 30% off for 12 months, 15% then 8% referral. Ours: 22% off, from the Nimblr co-sell approval thread, a different deal with the same vocabulary. Gold in the top ten, unread.
- **qst_0044** (basic, gmail). *Support response commitments Acme approved for its marketplace listing.* Gold: P1 = 4 hours, P2 = 8 business hours. Ours: P1 < 15 minutes, P2 < 2 hours, from a timezone-SLA audit thread about the same customer. Gold in the top ten, unread.
- **qst_0043** (basic, slack). *What caused the p99 jump on us-west-2 text generation?* Gold: a tenant-32 long-context burst causing KV-cache eviction. Ours: a kernel-selector rollout, from a different incident thread on the same endpoint. Gold not in the top 25.
- **qst_0176** (semantic, slack). *When does booking open for the new 80 GB accelerator in EU Central and India South?* Ours: 2026-04-08, from the launch announcement, and it even listed the two drafts giving 04-06 and 04-07. Gold is the 04-06 draft. This one is arguably the dataset's noise rather than ours, but it shows the shape: three documents, one fact each, and nothing in the ranking says which is authoritative.

### 2. Paraphrased questions do not reach their document

Semantic questions replace the document's terms with descriptions. BM25 has nothing to match: 55 of the 132 wrong answers are semantic, and for 103 of the 159 gold documents outside the top 25 the answer was wrong.

- **qst_0178** (semantic, github). *Default pass rate before a machine may step down from the safest numeric mode.* Gold: 0.92, from a PR about low-bit numerics. The words "pass rate", "numeric", "step down" pulled a route-sampler PR instead; ours reported its 99.99% mismatch gate and said the figure could not be confirmed.
- **qst_0179** (semantic, jira). *Why an EU tenant was routed to Southeast Asia during a March 2026 spike on the vectorization service.* Gold: a stale residency stamp after control-plane heartbeat lag. Ours: a routing-automation CSV bug from a different, similar incident, with different latency numbers.
- **qst_0039** (basic, google_drive). *The three go/no-go gates in the 3-hour workshop plan.* The gold document is a workshop plan the index never surfaced; the agent read a co-design sprint blueprint with six gates and reported that no three-gate taxonomy exists.

### 3. The agent reads too little of what it is shown

Even after the fix that shows every result as an excerpt (`6b040bb`), 113 gold documents in the top ten were never opened. The model typically opens one or two results, and the 500-character excerpt of a document that opens with boilerplate (an email header, a channel name, a ticket template) does not reveal that the answer is further down.

- **qst_0002** (basic, github). *The metric for streaming sessions finalized on the time limit.* Gold `stream.timebox_finalized` was in the top ten. The agent opened a Linear ticket about session watermarks, found `session_finalization_latency`, and reported that no exact metric could be confirmed.
- **qst_0053** (basic, slack). *The label allowlist and caps for v1 shadow-traffic metrics.* Gold in the top ten; the agent opened ENG-5410, a related Linear ticket, and reported its (different) allowlist.

### 4. Long documents are read in windows, and the second fact is outside the window

Transcripts and long plans hold two facts far apart. The agent scans a line range around a hint and answers from it; the second fact is elsewhere.

- **qst_0310** (intra-document, fireflies). Attendees named correctly from the header; the two checkpoint dates (2026-10-06 and 10-08) later in the transcript were never read.
- **qst_0333** (intra-document, fireflies). The 150 ms p95 target was found at [00:30]; "upload the samples by Friday" later in the transcript was not, and the answer said so.

### 5. Hedged non-answers

22 of the 132 wrong answers say "could not confirm" or "partially answered". The judge scores these as wrong with zero completeness, the same as a confident wrong answer. Several of them had the right document in hand (qst_0002, qst_0333) and would have scored with one more read.

### 6. Documents submitted

We submit the ten retrieved documents plus anything the agent cited, 10.9 per question on average, and 9.6 of those are judged neither gold nor valid. This does not enter the overall score, but it is the worst column on the leaderboard for us (OpenClaw submits 1.6 documents per question, metor 6.5), and a submission would want the cited set plus the top three instead.

### 7. Judge and gold noise

The gold-correction flow was off, so nothing we retrieved could amend a gold set; leaderboard submissions get that flow. In qst_0120 the gold answer lists a signed DPA the CRM record itself does not name as a precondition; in qst_0176 the gold is a superseded draft. A handful of points move either way on such cases, and a GLM Flash judge is not Onyx's judge.

## What would move the score

Ranked by the size of the bucket each attacks, cheapest first within a bucket.

1. **Read the near-duplicates, not the first hit.** 113 gold documents were shown and never opened. Two cheap levers: tell the agent that results with similar titles are siblings that differ in facts and to open each before committing to a figure; and lift the excerpt from the document's first 500 characters to its first 500 *telling* characters (the extractive selection the summarizer already has), so a ticket's or email's boilerplate does not hide its subject. A third, dearer lever is a rerank of the top ten by a small model reading full texts against the question, which is what the systems above us appear to do.

2. **Rewrite the question before searching.** 159 gold documents never entered the top 25, and semantic questions are the bulk of them. A single cheap LLM call that turns the question into search terms — product names, codenames, the corpus's own vocabulary ("h200-80", "eu-central-1", "residency stamp") — before the BM25 seed would bridge the paraphrase gap at a cent a question. This is the same lever as query-time vectors but without the fusion problem measured on the slice, where vectors as an equal vote lowered the ranking; a rewrite adds recall to the list BM25 already carries.

3. **Vectors as a filler, not a vote.** For the semantic quarter specifically, a vector list that only fills ranks 8–10 (or only contributes candidates BM25 lacks) keeps the measured full-text ranking intact while giving paraphrases a way in. Cost: one embedding per document at 384 dimensions, about $1 for the corpus, and 25% more index.

4. **Read long documents whole when they are the answer.** For a source under some size (transcripts here are 6–20 KB), `fetch` instead of `scan` when the question names two facts, and raise the per-call cap for `fetch` so a whole transcript reaches the model. Intra-document questions are already at 86; this is worth a few points there and on completeness.

5. **Never close on "could not confirm" while an unread top-ten result remains.** The closing prompt can say so: before giving up, open the unread results. Cheap, and it converts some of the 22 hedges.

6. **Entities on, for the project questions.** Project-related questions (52.4) need several documents about one initiative. The personalized PageRank walk had nothing to walk in this composition; entity fragments (people, tickets, customers) would link siblings and let one good hit pull in its neighbours. Cost: one LLM call per document at index time, the price this shape was chosen to avoid, so measure it on a slice first.

7. **A stronger answer model for the number of record.** Only 6 wrong answers had every gold document cited, so the model is not the bottleneck today; but the leaderboard's baselines answer with GPT-5.4, and once retrieval improves the model's reading will matter more. Run the same index with a frontier model once, for calibration.

8. **Submit the cited set plus the top three.** Cosmetic for the overall score; halves the invalid-extra column.

The single experiment to run next is (1) plus (2) on the 25k slice with `--skip-agent` off for a 50-question subset: both are query-time changes, so no re-index, and together they address 272 of the 314 gold documents that went unfound or unread.
