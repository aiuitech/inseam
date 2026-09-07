# MCP Server

`apps/mcp` is the MCP adapter from [design/node-api.md](../../design/node-api.md):
the node's operations as [Model Context Protocol](https://modelcontextprotocol.io)
tools, so an agent host uses inseam without adopting the CLI. It is a
TypeScript package on the official SDK (`@modelcontextprotocol/server` v2,
protocol revision 2026-07-28) and a client of the owner HTTP transport
([hosted-node.md](hosted-node.md)) — the same routes and messages the web
console speaks. It holds no node logic: it signs in, validates arguments,
calls one route per tool, and shapes the answer.

## Transports

- **stdio** (default) — for a host that launches the server itself (Claude
  Desktop, Claude Code). Logs go to stderr; stdout is the protocol channel.
- **streamable HTTP** (`--transport http`) — one `/mcp` endpoint on
  `127.0.0.1:7338` by default. Every request is served by a fresh MCP
  server instance over one shared node session (the SDK's stateless
  serving), `Host` and `Origin` are validated against loopback plus each
  `--allowed-host`, and a bind beyond loopback refuses to start without
  `INSEAM_MCP_BEARER_TOKEN`, which every request must then present.

## Signing in

The server takes `INSEAM_OWNER_TOKEN` (never a flag, so it stays out of
process listings), exchanges it once at start for the node's session cookie,
and fails there if the node is down or the token is wrong. A `401` on a
later call — the twelve-hour session expired, or the node restarted — is
answered by one re-login and a replay; a second `401` is reported, because
retrying a wrong token would loop.

## Tools

| Tool | Operation | Notes |
| --- | --- | --- |
| `query` | `query` | `text`, `limit` 1..25 (default 8) |
| `expand` | `expand` | `address` |
| `scan` | `scan` | `address`, `start`, `end`; `end < start` is refused before the call |
| `fetch` | `fetch` | `address`; a binary source is refused by the node, naming `fetch_bytes` |
| `fetch_bytes` | `fetch_bytes` | a raster image comes back as image content, anything else as an embedded resource with its content type |
| `hosts` | `hosts` | no arguments |
| `status` | `status` | no arguments |
| `catalog` | `catalog` | `host?`, `filter`, `limit` 1..1000 |
| `index` | `index` | `host?`, `root`, `rebuild`, `deep_budget?`; the one tool annotated as not read-only |

Every success carries the node's JSON as both text content and
`structuredContent`. No tool declares an `outputSchema`: the operation
messages are the contract, and a schema copied here would refuse a field
the node added — a version skew failing at the worst place. A refusal is an
`isError` result whose text is `<code>: <message>` with the node's own error
code, so the model sees `binary_fetch` or `not_found` rather than a
protocol failure.

## Paths not taken

- **A Rust transport plugin in the binary.** The design allows it and it
  may still come. The TypeScript SDK is where the protocol's newest
  revision lands first, agent hosts already run Node, and the owner HTTP
  transport already carries every operation as JSON — so the shortest
  correct adapter was a client of that transport. It costs one running
  process beside the node.
- **Owner operations for grants, plugins, and settings as tools.** Left
  out: an agent reconfiguring the node it is searching is a surprise, not a
  feature. The console and the CLI own those.
