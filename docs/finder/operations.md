# Finder Operations

The boundary operations from [design/node-api.md](../../design/node-api.md), served by the `operations` plugin on its seam (`inseam-seams::operations`). Messages are plain serde types — JSON-serializable by construction, with no transport assumptions. The CLI, the agent demo, the HTTP owner transport ([../architecture/hosted-node.md](../architecture/hosted-node.md)), and the MCP server ([../architecture/mcp-server.md](../architecture/mcp-server.md)) are all thin wrappers over these.

## The ladder

| Rung | Operation | Cost | Returns |
| --- | --- | --- | --- |
| 1 | `query { text, limit }` | cheapest | ranked addresses + envelopes, each with a score, summary, fragment hints (text preview, score, and structured extent), and any `replicas` — other addresses of the same content, collapsed by content digest; plus a `meta` block describing the query itself (below) |
| 2 | `expand { address }` | index-only | the source's fragments (mimetype, extent, preview), its typed relations, and neighboring fragments beyond the source — keyed fragments such as entities and the sources they connect to |
| 3 | `scan { address, start, end }` | reads a slice | lines `start..=end` (1-based, inclusive) of a text source, read no further than line `end`; anything else is served from its largest text descendant (`served_from_fragment` set) — details below |
| 4 | `fetch { address }` | full content | the whole source as text |
| 4 | `fetch_bytes { address }` | full content | the raw bytes with their content type — a source that is not text (an image, a PDF), or the target of a fragment's content reference (an image a document links to); bounded at 32 MiB per message, base64 in JSON |

`fetch` on a binary source refuses with `BinaryFetch`, naming `fetch_bytes`. A folder source (`inode/directory`) fetches and scans as text: its host serves the names it holds, one per line ([../indexing/filesystem-host.md](../indexing/filesystem-host.md#serving)); the index's richer view of it — entries with addresses, the summary — is `expand`. `fetch_bytes` serves only what the index has a record of: a cataloged source (its envelope gives the content type) or an address some fragment references (that fragment's mimetype does). Every fragment view in `expand` carries `content_address` when the fragment holds a reference instead of text ([../indexing/transforms.md](../indexing/transforms.md)).

Errors are typed and written as sentences (`no source at …`, `scan start line 200 is beyond the 41-line source`) so both humans and models can correct themselves.

## What a result carries for the next rung

A query result is a decision point, so each one carries what the follow-up needs without another round trip:

| Field | Meaning |
| --- | --- |
| `envelope.length` | `{"unit": "lines", "value": n}` for text the index has read — the last line a `scan` can reach — or `{"unit": "bytes", …}` for a binary or a text source too large to have been read |
| `hints[].extent` | where the matching fragment sits in its source, `{"unit": "lines", "start", "end"}` for text; `scan` accepts the numbers verbatim, and a client widens around them for context |
| `hints[].score` | the fragment's own score on the query's scale (the top result's source is `1.0`), so the client sees which hint made the hit |
| `envelope.content_digest` | the source's BLAKE3 content digest when its steward has one — the key results collapse by, carried so a merge across nodes collapses the same way one node's results do |
| `via` | the node whose index produced the result when a fan-out did; absent for the answering node's own results |

`expand` renders every fragment's `extent` the same structured way. The CLI prints extents as `[lines 5-9]`; `--json` carries the objects.

## Across the network

When the `routing` entry is mounted ([../network/routing.md](../network/routing.md)), the ladder reaches sources other nodes steward:

- `query` runs the local finder and the fan-out concurrently, then merges: reciprocal rank fusion (the finder's own `k = 60`) across the local list and each remote list; one copy per address — the local copy when this node ranked the address, else the best-ranked remote copy, its `via` set; then digest-equal copies collapse into `replicas`; then the list is cut to `limit` with the top result at `1.0`. With no remote results the local list stands untouched. A node that timed out or refused appears in `meta.remote` with its error, and the query still succeeds ([../network/discovery.md](../network/discovery.md)).
- `expand` is served from this node's own index when it stewards the host or holds the source's subtree, else routed to the steward's index.
- `scan` of a remote text source reads its lines through routing after the same range check and clamp; a remote source that is not text is served from this node's own text fragments when it deep-indexed the source, and refused with `NothingToScan` otherwise.
- `fetch` and `fetch_bytes` refuse before dialing — a binary text fetch, a byte fetch the catalog already knows is past 32 MiB — and then read through routing.

A host no node stewards is `UnknownHost`; one whose stewards were tried and none answered is `Unreachable`, naming who was tried. Without the `routing` entry, a source of an unstewarded host is `UnknownHost`, exactly as before.

## Scan

`scan` reads lines of **text sources only** — the unit extents and scan share is the line, and lines are defined for text. "Text" is the one list the index shares with `fetch` and the chunker (`inseam-seams::text::is_indexable_text`): all of `text/*` plus the structured application types — JSON, YAML, TOML, XML, JavaScript, shell, SQL, SVG. A source that is not text (a video, a PDF, an image) is scanned through its largest text fragment that is source content rather than derived understanding — a transcript, never a summary or an entity — and the response names it in `served_from_fragment`; a source with no such fragment refuses with `NothingToScan`.

The range is checked before anything is read. A zero `start` or an `end` before it is `ScanRange`; a `start` past the last line is `ScanBeyondEnd`, naming the line count so the client can correct itself. Both map to HTTP 400. `end` is clamped twice: to the last line, and to at most 2000 lines after `start` (`SCAN_LINES_MAX`) — one scan is a window, `fetch` is the rung for the whole thing. The response reports the window actually served:

| Field | Meaning |
| --- | --- |
| `start`, `end` | the lines served, after clamping |
| `lines_total` | the scanned text's line count when the index knows it: a text source's recorded length, or the stand-in fragment's; absent for a text source the index never read as text |
| `mimetype` | the text type of what was read |
| `text` | the lines, joined by `\n` |

Hosts read no further than line `end`: the filesystem host reads a buffered file line by line and stops, and the web host streams the response body and drops the connection once line `end` has arrived, so the head of a log the byte cap refuses to fetch whole is still one scan away ([../indexing/web-host.md](../indexing/web-host.md)). Lines are the same lines `fetch` would return: a range read this way equals the same range cut from the whole text.

## Query meta

Every `query` response carries a `meta` object beside `results`, so a slow or thin answer can be inspected without a debugger:

| Field | Meaning |
| --- | --- |
| `elapsed_ms` | wall-clock for the whole operation, dispatch to response, including rendering the results |
| `limit` | the limit actually served after clamping the request to 1..=50 |
| `seeds_ms`, `graph_ms`, `rollup_ms` | the finder's three phases ([algorithm.md](algorithm.md)): hybrid seed retrieval (full-text, query embedding, vector search, fusion), the relation graph load plus relevance walk, and grouping by source plus dressing with envelopes, summaries, and hints |
| `fts_hits`, `lexical_hits`, `vector_hits`, `seeds` | fragments returned by prose full-text, lexical names and tokens, and vector search (vector after the distance floor; zero on a node without an embedder), and distinct fragments left after rank fusion |
| `relations` | relations loaded around the seeds for the walk |
| `candidate_sources` | distinct sources holding a scored fragment, before the limit cut — how much competition the results won |
| `remote` | one entry per node the query fanned out to: `node`, `results` it contributed before the merge, `elapsed_ms`, and `error` when it contributed none; absent when the query fanned out to nobody |

`elapsed_ms` always contains the three phases; the remainder is dispatch, the access guard, and building the views. `inseam query` prints the same numbers as one footer line; `--json` carries them verbatim.

## Owner operations

`operations.index { host?, root, rebuild }` — hands off to the `sweep` seam over a scope of one stewarded host, returning an `IndexReport` (counts per summary kind, fragments, relations, keyed fragments anchored, dollars spent). `host` may be omitted only while the node stewards exactly one host; with several, the error lists them. `operations.hosts` — every host this node stewards: id, kind, display name, the entry whose connection serves it, and its capabilities ([indexing/connections.md](../indexing/connections.md)). `operations.grants` / `authorize_grant { grant, redirect }` / `await_authorization { state }` / `complete_authorization { state, code, … }` / `revoke_grant { grant }` — the OAuth grants and the owner's authorization of them, shaped so a local transport can take the loopback redirect and a remote one can serve the redirect itself ([plugins/oauth.md](../plugins/oauth.md)). All owner-only; never exposed through a boundary adapter.

`operations.status` reports `remote_sources` — cataloged sources learned from peers' logs rather than stewarded here, counted within `sources` — and each `operations.catalog` entry carries `origin`, the steward's node id, for such a row.

The network operations, owner-only like the rest:

| Operation | Does |
| --- | --- |
| `network` | the network as this node sees it: its own record; every admitted node with `is_local`, `live` (a session is open or the last exchange succeeded), `last_sync` as `YYYY-MM-DD`, `last_error`, and the hosts it stewards; every known host with its stewards; and the replicated log's entry and origin counts |
| `invite` | mints an invitation through this node; the owner carries its text form (`inseam-invite:…`) to the joining node |
| `join { invitation }` | parses the text, refuses one already expired by name, dials the inviter with the token, syncs once, and returns `network` |
| `expel { node }` | publishes the expulsion through the roster and returns `network` |
| `sync_now` | one sync round with every dialable node, then `network` |

Each answers `Unavailable`, naming the entry it lacks (`roster`, `sync`, `node`), on a node composed without the network plugins. `inseam network` and its subcommands ([../cli.md](../cli.md)) and the `/api/v1/owner/network` routes ([../architecture/hosted-node.md](../architecture/hosted-node.md#network)) are thin wrappers over these five; [../network/README.md](../network/README.md) covers the plugins behind them.

## The agent demo

`inseam agent "<question>"` consumes the `operations` and `llm` seams through
the client-owned [refinement session](refinement.md). Its `find` tool combines
bounded searches, expansions, and scans, retains candidates across calls, and
prints each request. The original question is searched before model-directed
refinement begins. Boundary enforcement remains a deny-wins guard on each
underlying operation (`OperationRequest`); no access-control listeners ship
yet, so the network is one trust domain until the boundary layer lands.
