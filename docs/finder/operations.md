# Finder Operations

The boundary operations from [design/node-api.md](../../design/node-api.md), as served by the `operations` plugin on its seam (`inseam-seams::operations`). Messages are plain serde types — JSON-serializable by construction, no transport assumptions. The CLI and the agent demo are both thin adapters over these; HTTP and MCP adapters will wrap the same types.

## The ladder

| Rung | Operation | Cost | Returns |
| --- | --- | --- | --- |
| 1 | `query { text, limit }` | cheapest | ranked addresses + envelopes, each with score, summary, fragment hints (text preview + line extent) |
| 2 | `expand { address }` | index-only | the source's fragments (mimetype, extent, preview), typed relations, and neighbor fragments beyond the source — entities and the sources they connect to |
| 3 | `scan { address, start, end }` | reads a slice | lines `start..=end` (1-based, inclusive) through the fetch path; media sources redirect to their largest text descendant (`served_from_fragment` set) |
| 4 | `fetch { address }` | full content | the whole source (text only, for now) |

Errors are typed and sentence-shaped (`no source at …`, `scan start line 200 is beyond the 41-line source`) so both humans and models can self-correct.

## Owner operations

`operations.index { root, rebuild }` — delegate to the `sweep` seam over a scope of the mounted connection, returning an `IndexReport` (counts per summary kind, fragments, relations, entities, dollars spent). Owner scope; never exposed at a boundary adapter.

## The agent demo

`inseam agent "<question>"` hands a live model exactly the four boundary operations as OpenAI-style tools and prints each rung it climbs. It is both a demo and a conformance test of the ladder's ergonomics: if the model can't navigate it, neither can a real client. Boundary enforcement is a monotonic-deny guard on dispatch (`OperationRequest`); no access-control listeners ship yet, so the network is one trust domain until the boundary layer lands.
