import assert from "node:assert/strict"
import { describe, it } from "node:test"

import { Client, InMemoryTransport } from "@modelcontextprotocol/client"

import {
  FAKE_TOKEN,
  fakeNode,
  json,
  nodeError,
  type FakeNode,
} from "./fake_node.js"
import { NodeClient } from "./node.js"
import { createServer } from "./server.js"

async function connected(fake: FakeNode): Promise<Client> {
  const node = new NodeClient({
    nodeUrl: "http://node.test",
    ownerToken: FAKE_TOKEN,
    fetch: fake.fetch,
  })
  const server = createServer(node)
  const [clientEnd, serverEnd] = InMemoryTransport.createLinkedPair()
  await server.connect(serverEnd)
  const client = new Client({ name: "test", version: "0.0.0" })
  await client.connect(clientEnd)
  return client
}

const IMAGE_PNG_BASE64 = "iVBORw0KGgo="

describe("tools", () => {
  it("cover the ladder and the owner views", async () => {
    const client = await connected(fakeNode())
    const tools = await client.listTools()
    assert.deepEqual(tools.tools.map((tool) => tool.name).sort(), [
      "catalog",
      "expand",
      "fetch",
      "fetch_bytes",
      "hosts",
      "index",
      "query",
      "scan",
      "status",
    ])
    const index = tools.tools.find((tool) => tool.name === "index")
    assert.equal(index?.annotations?.readOnlyHint, false)
    const query = tools.tools.find((tool) => tool.name === "query")
    assert.equal(query?.annotations?.readOnlyHint, true)
  })

  it("query passes text and limit and returns the node's answer", async () => {
    const fake = fakeNode()
    fake.route("/owner/query", (request) =>
      json(200, { results: [], meta: { asked: request.body } })
    )
    const client = await connected(fake)
    const result = await client.callTool({
      name: "query",
      arguments: { text: "kitchen" },
    })
    assert.equal(result.isError, undefined)
    assert.deepEqual(result.structuredContent, {
      results: [],
      meta: { asked: { text: "kitchen", limit: 8 } },
    })
    assert.deepEqual(result.content, [
      {
        type: "text",
        text: JSON.stringify({
          results: [],
          meta: { asked: { text: "kitchen", limit: 8 } },
        }),
      },
    ])
  })

  it("query refuses a limit past the bound before calling the node", async () => {
    const fake = fakeNode()
    const client = await connected(fake)
    const result = await client.callTool({
      name: "query",
      arguments: { text: "x", limit: 26 },
    })
    assert.equal(result.isError, true)
    assert.equal(
      fake.requests.filter((request) => request.path === "/owner/query").length,
      0
    )
  })

  it("scan refuses a range that ends before it starts", async () => {
    const fake = fakeNode()
    const client = await connected(fake)
    const result = await client.callTool({
      name: "scan",
      arguments: { address: "inseam://fs/a.md", start: 10, end: 2 },
    })
    assert.equal(result.isError, true)
    assert.deepEqual(result.structuredContent, {
      error: {
        code: "invalid_request",
        message: "`end` must not be before `start`",
      },
    })
    assert.equal(
      fake.requests.filter((request) => request.path === "/owner/scan").length,
      0
    )
  })

  it("scan forwards the range verbatim", async () => {
    const fake = fakeNode()
    fake.route("/owner/scan", (request) =>
      json(200, { ...(request.body as object), text: "l" })
    )
    const client = await connected(fake)
    const result = await client.callTool({
      name: "scan",
      arguments: { address: "inseam://fs/a.md", start: 3, end: 9 },
    })
    assert.deepEqual(result.structuredContent, {
      address: "inseam://fs/a.md",
      start: 3,
      end: 9,
      text: "l",
    })
  })

  it("reports the node's refusal as a tool error with its code", async () => {
    const fake = fakeNode()
    fake.route("/owner/fetch", () =>
      nodeError(400, "binary_fetch", "use fetch_bytes")
    )
    const client = await connected(fake)
    const result = await client.callTool({
      name: "fetch",
      arguments: { address: "inseam://fs/a.png" },
    })
    assert.equal(result.isError, true)
    assert.deepEqual(result.content, [
      { type: "text", text: "binary_fetch: use fetch_bytes" },
    ])
    assert.deepEqual(result.structuredContent, {
      error: { code: "binary_fetch", message: "use fetch_bytes" },
    })
  })

  it("fetch_bytes returns an image as image content", async () => {
    const fake = fakeNode()
    fake.route("/owner/fetch_bytes", () =>
      json(200, {
        address: "inseam://fs/a.png",
        content_type: "image/png",
        bytes: IMAGE_PNG_BASE64,
      })
    )
    const client = await connected(fake)
    const result = await client.callTool({
      name: "fetch_bytes",
      arguments: { address: "inseam://fs/a.png" },
    })
    assert.deepEqual(result.content, [
      { type: "image", data: IMAGE_PNG_BASE64, mimeType: "image/png" },
    ])
    assert.deepEqual(result.structuredContent, {
      address: "inseam://fs/a.png",
      content_type: "image/png",
    })
  })

  it("fetch_bytes returns anything else as an embedded resource", async () => {
    const fake = fakeNode()
    fake.route("/owner/fetch_bytes", () =>
      json(200, {
        address: "inseam://fs/a.pdf",
        content_type: "application/pdf",
        bytes: "JVBERi0=",
      })
    )
    const client = await connected(fake)
    const result = await client.callTool({
      name: "fetch_bytes",
      arguments: { address: "inseam://fs/a.pdf" },
    })
    assert.deepEqual(result.content, [
      {
        type: "resource",
        resource: {
          uri: "inseam://fs/a.pdf",
          mimeType: "application/pdf",
          blob: "JVBERi0=",
        },
      },
    ])
  })

  it("hosts wraps the node's list in an object", async () => {
    const fake = fakeNode()
    fake.route("/owner/hosts", () =>
      json(200, [{ id: "fs", roots: ["/data"] }])
    )
    const client = await connected(fake)
    const result = await client.callTool({ name: "hosts", arguments: {} })
    assert.deepEqual(result.structuredContent, {
      hosts: [{ id: "fs", roots: ["/data"] }],
    })
  })

  it("catalog and index fill their defaults", async () => {
    const fake = fakeNode()
    fake.route("/owner/catalog", (request) =>
      json(200, { asked: request.body })
    )
    fake.route("/owner/index", (request) => json(200, { asked: request.body }))
    const client = await connected(fake)
    const catalog = await client.callTool({ name: "catalog", arguments: {} })
    assert.deepEqual(catalog.structuredContent, {
      asked: { filter: "all", limit: 100 },
    })
    const index = await client.callTool({
      name: "index",
      arguments: { root: "/data", deep_budget: { sources: 5 } },
    })
    assert.deepEqual(index.structuredContent, {
      asked: { root: "/data", rebuild: false, deep_budget: { sources: 5 } },
    })
  })
})
