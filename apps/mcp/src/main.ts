#!/usr/bin/env node
// The `inseam-mcp` entry: parse options, sign in to the node once so a bad
// token fails here rather than on the first tool call, then serve over the
// chosen transport. Over stdio every log line goes to stderr — stdout is
// the protocol channel and one stray line would corrupt it.

import { StdioServerTransport } from "@modelcontextprotocol/server/stdio"

import { serveHttp } from "./http.js"
import { NodeClient, NodeError } from "./node.js"
import { OptionsError, parseOptions, USAGE } from "./options.js"
import { createServer } from "./server.js"

function log(line: string): void {
  process.stderr.write(`${line}\n`)
}

async function main(): Promise<void> {
  const options = parseOptions(process.argv.slice(2), process.env)
  if (options.help) {
    process.stdout.write(USAGE)
    return
  }
  const node = new NodeClient({
    nodeUrl: options.nodeUrl,
    ownerToken: options.ownerToken,
    fetch: (input, init) => fetch(input, init),
  })
  await node.login()
  log(`inseam-mcp: signed in to ${options.nodeUrl}`)

  if (options.transport === "stdio") {
    const server = createServer(node)
    await server.connect(new StdioServerTransport())
    log("inseam-mcp: serving over stdio")
    return
  }
  const http = await serveHttp(options, node, log)
  log(`inseam-mcp: serving at http://${options.bind}/mcp`)
  const shutdown = () => {
    void http.close().then(() => process.exit(0))
  }
  process.once("SIGINT", shutdown)
  process.once("SIGTERM", shutdown)
}

main().catch((error: unknown) => {
  if (error instanceof OptionsError) {
    log(`inseam-mcp: ${error.message}\n`)
    log(USAGE)
    process.exit(2)
  }
  if (error instanceof NodeError) {
    log(`inseam-mcp: cannot use the node: ${error.code}: ${error.message}`)
    process.exit(1)
  }
  log(`inseam-mcp: ${error instanceof Error ? error.message : String(error)}`)
  process.exit(1)
})
