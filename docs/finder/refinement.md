# Agent refinement

`inseam agent` exposes one `find` tool that combines searches, index expansion,
and source scans. The client-owned `inseam_seams::discovery::DiscoverySession`
composes the existing guarded `Operations` methods, so routing and permissions
still apply to each action. This is a client library and agent tool; the HTTP
and CLI `query` request formats remain unchanged.

The agent first searches the user's original question with limit 8. Every later
search adds address-deduplicated candidates with stable numeric IDs. The model
chooses which evidence is relevant; there is no separate reranking model.

```json
{
  "queries": [{"text": "streaming time-limit finalization metric", "limit": 8}],
  "expand": [{"source": 3}],
  "scan": [
    {"source": 2, "start": 1, "end": 18},
    {"source": "inseam://host/another-document", "start": 80, "end": 130}
  ]
}
```

`source` accepts a retained ID or an address discovered in evidence. `expand`
returns index structure and relationships; `scan` reads the source. The model
can read several short documents in one call and add another search for missing
information. Newly returned candidates are available to the next call.

The request allows eight actions total across `queries`, `expand`, `scan`,
`inspect`, and `forget`. Four actions run concurrently, each with a 60-second
timeout. Results remain ordered
by queries, expansions, scans, and inspections, then by array position. Each
failure occupies its own result slot and preserves other actions' results.
`forget` is applied before the other actions; do not forget an ID being read in
the same request. Empty requests and invalid limits fail before operations run.

## Candidate information

Results include address, title, source/content type, version metadata, known
line or byte length, indexed-summary character count, summary prefix,
query-relevant indexed excerpts, existing fragment hints, and read history.
Known source bytes and indexed-summary characters are different measurements;
summary length may underestimate source size. A full scan records the byte
length of the text returned as `read_text_bytes`.

Excerpt selection scans at most 65,536 indexed-summary characters using up to
24 query terms after stopword removal. It returns two matching windows of up
to 320 characters, with summary-character offsets. They are labeled
`indexed_summary` and do not claim original-source line coordinates. Existing
fragment hints retain their extents. A general summary prefix is limited to
240 characters. Up to six distinct indexed excerpts survive successive queries
of the same source; omitted older excerpts are counted. Existing Finder
digest-collapse behavior is preserved, with up to four replica addresses shown.

Use `{"inspect":[3,7]}` to recover retained candidate details and read windows.
Only 100 sources are retained; the response reports `unretained_count` when a
search exceeds capacity. Nothing is evicted automatically. Release unwanted
IDs with `{"forget":[3,7]}` and repeat a search if it had unretained results.
IDs are never reassigned within a session. Query refreshes invalidate recorded
reads when indexed version metadata changes. Discovery/read histories retain
16 entries each and flag capacity; they do not retain unlimited source bodies.

## Complete responses and continuation

Each response is valid JSON bounded to 24,000 serialized characters. Available
space is shared among actions. Search results that do not fit remain retained;
their IDs appear in `omitted_candidate_ids` and can be inspected without another
search. A metadata-heavy candidate may need an inspection call of its own.

A truncated scan returns a `next` object. Pass that object unchanged inside
the next request's `scan` array. It contains the source, line range, character
offset, and window digest needed to resume even within one very long line.
Continuing the same window fails if its text changed, so two versions cannot
silently be spliced together. A range exceeding the underlying 2,000-line scan
window continues at the next line. The requested final line survives a character continuation
inside a 2,000-line window, so following `next` still reaches later windows.
Expansion similarly returns `next` with an entry offset. Expansion text fields
are previews; use their source/ranges to read the full evidence. Expansion
pagination is a fresh view, not a snapshot.

The session accepts at most 512 batches. The agent allows at most eight tool
calls per model turn and 64 configured turns. Its normal benchmark limit is
12 turns, followed by one tool-free completion check against the user request
and evidence actually read. That check never receives benchmark gold answers.

See [the design](../../design/finder-refinement.md) for the reasoning and
[benchmarking](../benchmarking.md) for the fixed GPT-5.4 question sample.

If the final review returns empty content, the agent keeps its preceding completed
answer and logs the fallback. It never treats an assistant message that called a
tool as a completed answer. If neither reply contains an answer, it reports an
empty-answer error instead of suggesting that the turn limit was exhausted.

`inseam agent --reasoning-effort low` applies the selected effort to discovery
and final review. Omit the flag to retain the endpoint's default. The internal
`agent-batch` benchmark command runs at most eight independent sessions on one
node and writes a separate JSON record for each answer. Its provider cost is
reported once for the batch because concurrent calls cannot be attributed using
a shared cumulative spend counter.
