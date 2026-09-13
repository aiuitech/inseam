# Additive discovery and reading

The client-owned discovery session implements this contract for `inseam agent`.
Its current fields, bounds, and continuation rules are documented in
[agent refinement](../docs/finder/refinement.md). Existing server operations
remain stateless; the client composes their guarded search, expand, and scan
methods. This design grew from the September 2026 navigation evaluation.

## Let the caller follow the evidence

The querying model chooses which sources to read, which relationships to
follow, and which query to try next. Finder ranks bounded search candidates;
the model decides their relevance to the answer as it reads. A separate model
reranking stage is not required for this design. Its value would need evidence
from failures remaining after discovery and reading work well.

Successive searches add to an address-deduplicated candidate collection owned
by the calling agent or client. Existing tool messages already remain in the
CLI agent's conversation, but conversation history is not an explicit candidate
collection. Track discovery provenance, source version, excerpts, and read
coverage so new results do not erase earlier candidates or cause duplicate
reads. A source can gain new excerpts and relations without becoming a new
candidate. Preserve distinct sources with conflicting content. Use existing
digest/replica semantics when collapsing identical content.

Per-query scores are relative to that query; do not accumulate normalized
scores across unrelated searches and pretend they measure global relevance.
The caller can retain or dismiss candidates based on the evidence. An explicit
bound requires visible eviction or a recoverable continuation, never silent
loss of the oldest results.

## Results must support the next decision

Each candidate should expose its address, title, source type, content type,
version information, total line count when known, and byte or character size
when known. Lines determine scan coordinates; bytes or characters help estimate
the reading cost. Eighteen lines can hold thousands of characters, so line
count alone is insufficient. Unknown sizes remain unknown rather than being
replaced with fabricated estimates. Any token estimate must be labeled with
its estimation method.

Provide bounded query-relevant source excerpts, alongside any general summary,
and a precise scan range when the excerpt can be mapped to original source
lines. Mark whether a passage is verbatim source text or derived summary text.
Never invent source line numbers for normalized summaries. When indexed text
does not preserve source offsets, either read a bounded source window to obtain
an excerpt or return a preview without claiming a source range. Source reads
must use the normal operations and access checks, including for remote hosts.

This addresses a measured gap. All 160 initial results in the 20-question
baseline had empty `hints`. That benchmark stores whole documents as summaries
and disables structural extraction; Finder deliberately excludes summary and
keyword fragments from scan hints. The CLI then exposes only the first 500
summary characters. Question 2's gold source has 18 lines and 4,155 bytes; the
metric is on source line 11, around character 2,772 in the normalized summary.
Its length is available today, but the excerpt does not reveal that passage.

## Search, expand, and read in one step

Expose one bounded refinement request that can combine new searches with
expansion and reading of previously discovered addresses. An illustrative
shape, with field names subject to the API implementation:

```json
{
  "queries": [{"text": "streaming time-limit finalization metric", "limit": 8}],
  "expand": [{"source": "inseam://host/document-a"}],
  "scan": [
    {"source": "inseam://host/document-b", "start": 1, "end": 18},
    {"source": "inseam://host/document-c", "start": 80, "end": 130}
  ]
}
```

Expansion retains its existing meaning: index structure and relationships.
Scanning reads source content. Keeping these explicit lets a caller ask for
both without confusing discovery with having read a document. A read-only
step is valid when current candidates already contain enough promising work.
Newly returned addresses become available for the next refinement step; the
operation does not autonomously follow an unbounded search-and-read loop.

Independent actions execute with bounded concurrency and return correlated
results in request order. Per-action failures must not discard successful
reads. Preserve node routing, deny-wins authorization, and source-version
checks by composing the existing operations. Client-owned accumulation keeps
independent users and conversations from sharing hidden server search state.

The implemented bounds are eight actions per request, four concurrent
actions, 100 retained candidate sources, and 24,000 characters of returned
content. Every
response must be valid structured data, with per-item completeness and a
continuation range for cut content. Reserve room for each requested action's
status before distributing the content budget. Do not serialize arbitrary
objects and cut the resulting JSON string.

## Agent behavior

Start from the user's original question and retain its candidates. Encourage
the model to use excerpts and sizes to choose several worthwhile reads in one
turn, then add searches for gaps revealed by those reads. Reading several short
documents can be cheaper than repeatedly guessing query wording. The current
blanket preference for scans over full reads should account for document size:
a short document can be read completely in one scan.

Keep requested qualifiers and supported facts connected to source passages.
The model chooses when evidence answers the request. A completion check should
inspect that evidence and the user's question, without using benchmark gold
answers. Multiple find calls remain sequential because they share candidate state.
Independent actions within a find call run concurrently, and the agent is
instructed to batch worthwhile reads.

## Cost and validation

Accumulate candidate metadata once and return updates rather than resending
the entire collection on every step. At 100 candidates and roughly 1 KB of
metadata/excerpts each, the client collection is approximately 100 KB plus
read-content storage. Give read content its own bound. Twenty-four thousand
output characters represent roughly 6,000 tokens for typical English, with
language and tokenizer variation. Four concurrent remote reads reduce serial
network waits; they do not reduce the model's total input volume. Local disk
work is bounded by action and content limits, and no full index rebuild is
required by batching or accumulation.

Tests should verify additive deduplication, version-aware updates, full result
framing under realistic sizes, correct excerpt-to-source ranges, continuation
without missing text, partial failures, and the concurrency limit. Scripted
agents should demonstrate a search followed by several reads plus another
search, preserving the original candidates. Run the same 20-question GPT-5.4
sample for score, omissions, unsupported answers, calls, and latency before
expanding the evaluation set.

## Full benchmark execution and effort control

Full answer evaluation can hold eight independent `DiscoverySession` values in
one CLI node through the internal `agent-batch` command. The operations and model
provider are shared; candidate IDs and conversation history remain per question.
Input is bounded to 500 questions and two megabytes. IDs are unique and cannot
contain path separators. Each answer is atomically written to its own file before
the next question replaces its worker. A failed question does not erase completed
answers. Provider spend is a batch total, not a per-question measurement.

The performance estimate motivating this path is 500 questions times 45 seconds,
about 375 minutes sequentially or 47 minutes at eight sessions before contention
and judging. Eight transcripts with twelve 24,000-character tool responses are
roughly 2.3 million characters before model messages and retained candidates.
The database and source corpus remain shared. These are estimates, not measured
benchmark results. An earlier attempt with separate APFS database clones stalled
in SQLite checkpoint writes and was stopped; those disposable clones were removed.

`--reasoning-effort` controls both the discovery loop and final review. Omitting
it preserves the endpoint default. GLM full runs requested on 13 September use
`low`. The pinned judge is launched through a small wrapper that supplies `low`
to both upstream model factories without changing scoring or gold data.
