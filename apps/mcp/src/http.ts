// The streamable HTTP transport (`--transport http`): one `/mcp` endpoint on
// a plain `node:http` server. Every request gets a fresh MCP server from
// the factory — the SDK's stateless serving — over the one node client, so
// the login session is shared while nothing else is. Host and Origin are
// checked before the handler sees a request (DNS-rebinding protection),
// and a bearer token gates every request when one is configured; a bind
// beyond loopback requires one (`options.ts`).

import { timingSafeEqual } from "node:crypto"
import {
  createServer as createHttpServer,
  type IncomingMessage,
  type Server,
  type ServerResponse,
} from "node:http"

import { createMcpHandler } from "@modelcontextprotocol/server"
import {
  hostHeaderValidation,
  originValidation,
  toNodeHandler,
} from "@modelcontextprotocol/node"

import type { NodeClient } from "./node.js"
import { parseBind, type Options } from "./options.js"
import { createServer } from "./server.js"

export const MCP_PATH = "/mcp"
const LOOPBACK_HOSTNAMES = ["localhost", "127.0.0.1", "[::1]"]

export type HttpServer = {
  server: Server
  close: () => Promise<void>
}

export async function serveHttp(
  options: Options,
  node: NodeClient,
  log: (line: string) => void
): Promise<HttpServer> {
  const allowed = [...LOOPBACK_HOSTNAMES, ...options.allowedHosts]
  const validateHost = hostHeaderValidation(allowed)
  const validateOrigin = originValidation(allowed)
  const handler = createMcpHandler(() => createServer(node), {
    onerror: (error) => log(`mcp: ${error.message}`),
  })
  const mcp = toNodeHandler(handler, {
    onerror: (error) => log(`http: ${error.message}`),
  })
  const bearer = options.bearerToken

  const server = createHttpServer((request, response) => {
    if (!validateHost(request, response)) return
    if (!validateOrigin(request, response)) return
    if (!pathIsMcp(request)) {
      answer(response, 404, "not_found", `only ${MCP_PATH} is served`)
      return
    }
    if (bearer !== undefined && !bearerMatches(request, bearer)) {
      response.setHeader("www-authenticate", 'Bearer realm="inseam-mcp"')
      answer(response, 401, "unauthorized", "a valid bearer token is required")
      return
    }
    void mcp(request, response)
  })

  const address = parseBind(options.bind)
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject)
    server.listen(address.port, address.host, () => {
      server.off("error", reject)
      resolve()
    })
  })
  return {
    server,
    close: async () => {
      await handler.close()
      await new Promise<void>((resolve) => server.close(() => resolve()))
    },
  }
}

function pathIsMcp(request: IncomingMessage): boolean {
  const url = request.url ?? ""
  const path = url.split("?")[0]
  return path === MCP_PATH
}

/** Constant-time comparison of the `Authorization` header against the
 * configured token, so the check leaks nothing about how many bytes
 * matched. A length mismatch is a mismatch. */
export function bearerMatches(
  request: { headers: { authorization?: string } },
  token: string
): boolean {
  const header = request.headers.authorization ?? ""
  const prefix = "Bearer "
  if (!header.startsWith(prefix)) {
    return false
  }
  const presented = Buffer.from(header.slice(prefix.length))
  const expected = Buffer.from(token)
  if (presented.length !== expected.length) {
    return false
  }
  return timingSafeEqual(presented, expected)
}

function answer(
  response: ServerResponse,
  status: number,
  code: string,
  message: string
): void {
  response.writeHead(status, { "content-type": "application/json" })
  response.end(JSON.stringify({ error: { code, message } }))
}
