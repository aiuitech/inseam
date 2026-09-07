import assert from "node:assert/strict"
import { describe, it } from "node:test"

import {
  BIND_DEFAULT,
  NODE_URL_DEFAULT,
  OptionsError,
  isLoopback,
  parseBind,
  parseOptions,
} from "./options.js"

const token = "0123456789abcdef0123456789abcdef"
const env = { INSEAM_OWNER_TOKEN: token }

describe("parseOptions", () => {
  it("defaults to stdio against the local node", () => {
    const options = parseOptions([], env)
    assert.equal(options.transport, "stdio")
    assert.equal(options.nodeUrl, NODE_URL_DEFAULT)
    assert.equal(options.bind, BIND_DEFAULT)
    assert.equal(options.ownerToken, token)
    assert.equal(options.help, false)
  })

  it("reads flags over the environment", () => {
    const options = parseOptions(
      [
        "--transport",
        "http",
        "--node-url",
        "https://node.example",
        "--bind",
        "127.0.0.1:9000",
      ],
      {
        ...env,
        INSEAM_NODE_URL: "http://elsewhere:1",
        INSEAM_MCP_BIND: "127.0.0.1:1",
      }
    )
    assert.equal(options.transport, "http")
    assert.equal(options.nodeUrl, "https://node.example")
    assert.equal(options.bind, "127.0.0.1:9000")
  })

  it("collects every allowed host", () => {
    const options = parseOptions(
      ["--allowed-host", "node.example", "--allowed-host", "mcp.example"],
      env
    )
    assert.deepEqual(options.allowedHosts, ["node.example", "mcp.example"])
  })

  it("answers help without validating anything else", () => {
    const options = parseOptions(["--help"], {})
    assert.equal(options.help, true)
  })

  const refusals: Array<{
    name: string
    args: string[]
    env: Record<string, string>
    message: RegExp
  }> = [
    {
      name: "rejects a short owner token",
      args: [],
      env: { INSEAM_OWNER_TOKEN: "short" },
      message: /at least 32 bytes/,
    },
    {
      name: "rejects a missing owner token",
      args: [],
      env: {},
      message: /INSEAM_OWNER_TOKEN/,
    },
    {
      name: "rejects an unknown transport",
      args: ["--transport", "grpc"],
      env,
      message: /stdio or http/,
    },
    {
      name: "rejects an unknown flag",
      args: ["--port", "1"],
      env,
      message: /unknown flag --port/,
    },
    {
      name: "rejects a flag without a value",
      args: ["--bind"],
      env,
      message: /needs a value/,
    },
    {
      name: "rejects a node url with a path",
      args: ["--node-url", "http://n/api"],
      env,
      message: /no path/,
    },
    {
      name: "rejects a node url that is not http",
      args: ["--node-url", "ftp://n"],
      env,
      message: /http or https/,
    },
    {
      name: "rejects a bind without a port",
      args: ["--transport", "http", "--bind", "localhost"],
      env,
      message: /host:port/,
    },
    {
      name: "rejects a bind with a bad port",
      args: ["--transport", "http", "--bind", "localhost:99999"],
      env,
      message: /valid port/,
    },
    {
      name: "rejects a public bind without a bearer token",
      args: ["--transport", "http", "--bind", "0.0.0.0:7338"],
      env,
      message: /INSEAM_MCP_BEARER_TOKEN/,
    },
    {
      name: "rejects a public bind with a short bearer token",
      args: ["--transport", "http", "--bind", "0.0.0.0:7338"],
      env: { ...env, INSEAM_MCP_BEARER_TOKEN: "short" },
      message: /at least 32 bytes/,
    },
    {
      name: "rejects too many arguments",
      args: Array.from({ length: 34 }, () => "--help"),
      env,
      message: /too many arguments/,
    },
  ]
  for (const refusal of refusals) {
    it(refusal.name, () => {
      assert.throws(
        () => parseOptions(refusal.args, refusal.env),
        (error: unknown) => {
          assert.ok(error instanceof OptionsError)
          assert.match(error.message, refusal.message)
          return true
        }
      )
    })
  }

  it("accepts a public bind with a long bearer token", () => {
    const options = parseOptions(
      ["--transport", "http", "--bind", "0.0.0.0:7338"],
      {
        ...env,
        INSEAM_MCP_BEARER_TOKEN: token,
      }
    )
    assert.equal(options.bearerToken, token)
  })

  it("accepts a loopback bind without a bearer token", () => {
    const options = parseOptions(
      ["--transport", "http", "--bind", "[::1]:7338"],
      env
    )
    assert.equal(options.bearerToken, undefined)
  })
})

describe("parseBind", () => {
  it("splits host and port and unwraps IPv6 brackets", () => {
    assert.deepEqual(parseBind("127.0.0.1:7338"), {
      host: "127.0.0.1",
      port: 7338,
    })
    assert.deepEqual(parseBind("[::1]:7338"), { host: "::1", port: 7338 })
    assert.deepEqual(parseBind("localhost:80"), { host: "localhost", port: 80 })
  })
})

describe("isLoopback", () => {
  it("knows the loopback names and nothing else", () => {
    assert.equal(isLoopback("localhost"), true)
    assert.equal(isLoopback("127.0.0.1"), true)
    assert.equal(isLoopback("127.1.2.3"), true)
    assert.equal(isLoopback("::1"), true)
    assert.equal(isLoopback("0.0.0.0"), false)
    assert.equal(isLoopback("::"), false)
    assert.equal(isLoopback("node.example"), false)
  })
})
