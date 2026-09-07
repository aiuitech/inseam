import assert from "node:assert/strict"
import { request as httpRequest } from "node:http"
import { createServer as createNetServer, type AddressInfo } from "node:net"
import { after, before, describe, it } from "node:test"

import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client"

import { FAKE_TOKEN, fakeNode, json } from "./fake_node.js"
import { bearerMatches, serveHttp, type HttpServer } from "./http.js"
import { NodeClient } from "./node.js"
import { parseOptions } from "./options.js"

const BEARER = "bearer-token-for-the-tests-0123456789"

/** A port the kernel hands out and releases; the server under test binds
 * it a moment later. Loopback only — the test never leaves the machine. */
async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createNetServer()
    probe.once("error", reject)
    probe.listen(0, "127.0.0.1", () => {
      const port = (probe.address() as AddressInfo).port
      probe.close(() => resolve(port))
    })
  })
}

describe("bearerMatches", () => {
  it("accepts the configured token and nothing else", () => {
    assert.equal(
      bearerMatches({ headers: { authorization: `Bearer ${BEARER}` } }, BEARER),
      true
    )
    assert.equal(
      bearerMatches(
        { headers: { authorization: `Bearer ${BEARER}x` } },
        BEARER
      ),
      false
    )
    assert.equal(
      bearerMatches({ headers: { authorization: BEARER } }, BEARER),
      false
    )
    assert.equal(bearerMatches({ headers: {} }, BEARER), false)
  })
})

describe("serveHttp", () => {
  let http: HttpServer
  let port: number
  const fake = fakeNode()
  fake.route("/owner/status", () => json(200, { sources: 1 }))

  before(async () => {
    port = await freePort()
    const options = parseOptions(
      ["--transport", "http", "--bind", `127.0.0.1:${port}`],
      {
        INSEAM_OWNER_TOKEN: FAKE_TOKEN,
        INSEAM_MCP_BEARER_TOKEN: BEARER,
      }
    )
    const node = new NodeClient({
      nodeUrl: "http://node.test",
      ownerToken: FAKE_TOKEN,
      fetch: fake.fetch,
    })
    http = await serveHttp(options, node, () => {})
  })

  after(async () => {
    await http.close()
  })

  it("serves tools at /mcp to a bearer-authenticated client", async () => {
    const transport = new StreamableHTTPClientTransport(
      new URL(`http://127.0.0.1:${port}/mcp`),
      {
        requestInit: { headers: { authorization: `Bearer ${BEARER}` } },
      }
    )
    const client = new Client({ name: "test", version: "0.0.0" })
    await client.connect(transport)
    const result = await client.callTool({ name: "status", arguments: {} })
    assert.deepEqual(result.structuredContent, { sources: 1 })
    await client.close()
  })

  it("answers 401 without the bearer token", async () => {
    const response = await fetch(`http://127.0.0.1:${port}/mcp`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "ping" }),
    })
    assert.equal(response.status, 401)
    assert.equal(
      response.headers.get("www-authenticate"),
      'Bearer realm="inseam-mcp"'
    )
  })

  it("answers 404 off /mcp", async () => {
    const response = await fetch(`http://127.0.0.1:${port}/other`, {
      headers: { authorization: `Bearer ${BEARER}` },
    })
    assert.equal(response.status, 404)
  })

  it("refuses a Host it was not told to accept", async () => {
    // `fetch` refuses to forge `Host`, so this one goes through `node:http`.
    const status = await new Promise<number | undefined>((resolve, reject) => {
      const request = httpRequest(
        {
          host: "127.0.0.1",
          port,
          path: "/mcp",
          method: "POST",
          headers: { host: "evil.example", authorization: `Bearer ${BEARER}` },
        },
        (response) => {
          response.resume()
          resolve(response.statusCode)
        }
      )
      request.once("error", reject)
      request.end("{}")
    })
    assert.equal(status, 403)
  })

  it("refuses a browser Origin it was not told to accept", async () => {
    const response = await fetch(`http://127.0.0.1:${port}/mcp`, {
      method: "POST",
      headers: {
        origin: "https://evil.example",
        authorization: `Bearer ${BEARER}`,
      },
      body: "{}",
    })
    assert.equal(response.status, 403)
  })
})
