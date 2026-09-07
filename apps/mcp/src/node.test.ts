import assert from "node:assert/strict"
import { describe, it } from "node:test"

import {
  FAKE_COOKIE,
  FAKE_TOKEN,
  fakeNode,
  json,
  nodeError,
} from "./fake_node.js"
import { NodeClient, NodeError, sessionCookie } from "./node.js"

function client(
  fake: ReturnType<typeof fakeNode>,
  token = FAKE_TOKEN
): NodeClient {
  return new NodeClient({
    nodeUrl: "http://node.test/",
    ownerToken: token,
    fetch: fake.fetch,
  })
}

describe("NodeClient", () => {
  it("signs in once and sends the session cookie on every call", async () => {
    const fake = fakeNode()
    fake.route("/owner/status", () => json(200, { sources: 3 }))
    const node = client(fake)
    assert.deepEqual(await node.status(), { sources: 3 })
    assert.deepEqual(await node.status(), { sources: 3 })
    assert.deepEqual(
      fake.requests.map((request) => [
        request.method,
        request.path,
        request.cookie,
      ]),
      [
        ["POST", "/session", undefined],
        ["GET", "/owner/status", FAKE_COOKIE],
        ["GET", "/owner/status", FAKE_COOKIE],
      ]
    )
  })

  it("posts the request body as JSON", async () => {
    const fake = fakeNode()
    fake.route("/owner/query", (request) => json(200, { echoed: request.body }))
    const result = await client(fake).query({ text: "kitchen", limit: 4 })
    assert.deepEqual(result, { echoed: { text: "kitchen", limit: 4 } })
  })

  it("signs in again once after a 401 and replays the call", async () => {
    const fake = fakeNode()
    let calls = 0
    fake.route("/owner/hosts", () => {
      calls += 1
      return calls === 1
        ? nodeError(401, "authentication_required", "sign in")
        : json(200, [{ id: "fs" }])
    })
    const node = client(fake)
    assert.deepEqual(await node.hosts(), [{ id: "fs" }])
    assert.deepEqual(
      fake.requests.map((request) => request.path),
      ["/session", "/owner/hosts", "/session", "/owner/hosts"]
    )
  })

  it("gives up after the second 401 instead of looping", async () => {
    const fake = fakeNode()
    fake.route("/owner/hosts", () =>
      nodeError(401, "authentication_required", "sign in")
    )
    await assert.rejects(client(fake).hosts(), (error: unknown) => {
      assert.ok(error instanceof NodeError)
      assert.equal(error.code, "authentication_required")
      assert.equal(error.status, 401)
      return true
    })
    assert.equal(
      fake.requests.filter((request) => request.path === "/session").length,
      2
    )
  })

  it("reports a wrong token as a login failure", async () => {
    const fake = fakeNode()
    await assert.rejects(
      client(fake, "not-the-token-but-long-enough-to-pass").login(),
      (error: unknown) => {
        assert.ok(error instanceof NodeError)
        assert.equal(error.code, "invalid_credentials")
        return true
      }
    )
  })

  it("carries the node's error code and message", async () => {
    const fake = fakeNode()
    fake.route("/owner/fetch", () =>
      nodeError(400, "binary_fetch", "use fetch_bytes")
    )
    await assert.rejects(
      client(fake).fetch({ address: "inseam://fs/a.png" }),
      (error: unknown) => {
        assert.ok(error instanceof NodeError)
        assert.equal(error.code, "binary_fetch")
        assert.equal(error.message, "use fetch_bytes")
        return true
      }
    )
  })

  it("refuses a body that is not JSON", async () => {
    const fake = fakeNode()
    fake.route("/owner/status", () => new Response("<html>", { status: 502 }))
    await assert.rejects(client(fake).status(), (error: unknown) => {
      assert.ok(error instanceof NodeError)
      assert.equal(error.code, "bad_response")
      return true
    })
  })

  it("reports a node it cannot reach", async () => {
    const node = new NodeClient({
      nodeUrl: "http://node.test",
      ownerToken: FAKE_TOKEN,
      fetch: async () => {
        throw new TypeError("fetch failed", {
          cause: new Error("ECONNREFUSED"),
        })
      },
    })
    await assert.rejects(node.login(), (error: unknown) => {
      assert.ok(error instanceof NodeError)
      assert.equal(error.code, "unreachable")
      assert.match(error.message, /ECONNREFUSED/)
      return true
    })
  })
})

describe("sessionCookie", () => {
  it("keeps the pair and drops the attributes", () => {
    assert.equal(
      sessionCookie("inseam_owner=abc; Path=/api/v1; HttpOnly"),
      "inseam_owner=abc"
    )
  })

  it("refuses a header without a pair", () => {
    assert.throws(() => sessionCookie("; Path=/"), NodeError)
  })
})
