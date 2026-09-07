# inseam MCP server

`inseam-mcp` exposes a node's operations as [MCP](https://modelcontextprotocol.io)
tools, so any agent host — Claude Desktop, Claude Code, an IDE — gets inseam
as a context engine with no custom integration. It speaks only the
authenticated `/api/v1/owner/*` HTTP transport of a running node
([docs/architecture/hosted-node.md](../../docs/architecture/hosted-node.md)).
Node logic stays in Rust; this package is the adapter and nothing else.

## Run

With `inseam` on PATH (`cargo install --path crates/inseam-cli` from the repo
root) and a node serving:

```sh
export INSEAM_OWNER_TOKEN=...        # the token the node was started with
inseam serve                          # 127.0.0.1:7337 by default

cd apps/mcp
pnpm install                          # installs the whole pnpm workspace
pnpm build
node build/main.js --help
```

The server signs in once at start, so a wrong token or an unreachable node
fails there rather than on the first tool call.

## Connect an agent

Over **stdio** (the default) the host launches the server itself. Claude
Desktop's `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "inseam": {
      "command": "node",
      "args": ["/ABSOLUTE/PATH/TO/inseam/apps/mcp/build/main.js"],
      "env": { "INSEAM_OWNER_TOKEN": "..." }
    }
  }
}
```

Claude Code: `claude mcp add inseam -e INSEAM_OWNER_TOKEN=... -- node /ABSOLUTE/PATH/TO/inseam/apps/mcp/build/main.js`.

Over **streamable HTTP** the server listens and hosts connect to `/mcp`:

```sh
node build/main.js --transport http                    # http://127.0.0.1:7338/mcp
INSEAM_MCP_BEARER_TOKEN=... node build/main.js --transport http --bind 0.0.0.0:7338 \
  --allowed-host mcp.example
```

A loopback bind needs no bearer token. Any other bind refuses to start
without `INSEAM_MCP_BEARER_TOKEN` (at least 32 bytes), and every request
must then carry it as `Authorization: Bearer`. `Host` and `Origin` are
checked against the loopback names plus each `--allowed-host`, so a page in
a browser cannot reach the endpoint through DNS rebinding. Put TLS in front
of it, the way the hosted node's Compose example does.

## Tools

One tool per operation, named as [docs/finder/operations.md](../../docs/finder/operations.md)
names them: `query`, `expand`, `scan`, `fetch`, `fetch_bytes`, then `hosts`,
`status`, `catalog`, and `index`. Every result carries the node's JSON both
as text and as `structuredContent`; a refusal comes back as an `isError`
result whose text is `<code>: <message>` with the node's own code.
`fetch_bytes` answers with image content for a raster image and an embedded
resource for anything else.

## Develop

```sh
pnpm typecheck
pnpm test        # node:test over the built output; no node needed
pnpm format
```

The tests drive the tools through the SDK's in-memory transport against a
scripted fake of the owner API (`src/fake_node.ts`); the HTTP transport's
tests bind a loopback port. Nothing leaves the machine.
