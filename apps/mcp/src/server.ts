// One MCP server instance over one node client. Stdio builds it once; the
// HTTP handler builds one per request from the same client, so the login
// session is shared while the protocol state is not (`http.ts`).

import { McpServer } from "@modelcontextprotocol/server"

import type { NodeClient } from "./node.js"
import { registerTools } from "./tools.js"

export const SERVER_NAME = "inseam"
export const SERVER_VERSION = "0.1.0"

export function createServer(node: NodeClient): McpServer {
  const server = new McpServer(
    { name: SERVER_NAME, version: SERVER_VERSION },
    {
      instructions:
        "inseam is a discovery index over the owner's own sources. Climb the ladder: `query` first (cheapest), `expand` or `scan` the results worth a closer look, `fetch` only what earns it. Addresses are `inseam://<host>/<path>` strings the tools hand back.",
    }
  )
  registerTools(server, node)
  return server
}
