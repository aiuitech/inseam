# Finder Operations

The boundary operations from [design/node-api.md](../../design/node-api.md), served by the `operations` plugin on its seam (`inseam-seams::operations`). Messages are plain serde types — JSON-serializable by construction, with no transport assumptions. The CLI and the agent demo are both thin wrappers over these; HTTP and MCP adapters will wrap the same types.

## The ladder

| Rung | Operation | Cost | Returns |
| --- | --- | --- | --- |
| 1 | `query { text, limit }` | cheapest | ranked addresses + envelopes, each with a score, summary, fragment hints (text preview + line extent), and any `replicas` — other addresses of the same content, collapsed by content digest |
| 2 | `expand { address }` | index-only | the source's fragments (mimetype, extent, preview), its typed relations, and neighboring fragments beyond the source — keyed fragments such as entities and the sources they connect to |
| 3 | `scan { address, start, end }` | reads a slice | lines `start..=end` (1-based, inclusive) through the fetch path; media sources redirect to their largest text descendant (`served_from_fragment` set) |
| 4 | `fetch { address }` | full content | the whole source (text only, for now) |

Errors are typed and written as sentences (`no source at …`, `scan start line 200 is beyond the 41-line source`) so both humans and models can correct themselves.

## Owner operations

`operations.index { host?, root, rebuild }` — hands off to the `sweep` seam over a scope of one stewarded host, returning an `IndexReport` (counts per summary kind, fragments, relations, keyed fragments anchored, dollars spent). `host` may be omitted only while the node stewards exactly one host; with several, the error lists them. `operations.hosts` — every host this node stewards: id, kind, display name, the entry whose connection serves it, and its capabilities ([indexing/connections.md](../indexing/connections.md)). `operations.grants` / `authorize_grant { grant, redirect }` / `await_authorization { state }` / `complete_authorization { state, code, … }` / `revoke_grant { grant }` — the OAuth grants and the owner's authorization of them, shaped so a local transport can take the loopback redirect and a remote one can serve the redirect itself ([plugins/oauth.md](../plugins/oauth.md)). All owner-only; never exposed through a boundary adapter.

## The agent demo

`inseam agent "<question>"` (`crates/inseam-cli/src/agent.rs`, a consumer of the `operations` and `llm` seams — not a plugin) hands a live model exactly the four boundary operations as OpenAI-style tools and prints each rung it climbs. It's both a demo and a test of the ladder's usability: if the model can't navigate it, neither can a real client. Boundary enforcement is a deny-wins guard on dispatch (`OperationRequest`); no access-control listeners ship yet, so the network is one trust domain until the boundary layer lands.
