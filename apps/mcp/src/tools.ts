// The node's operations as MCP tools. One tool per operation, named as the
// operation is named everywhere else (`docs/finder/operations.md`), so a
// transcript reads the same whether the agent came in over MCP, the CLI,
// or the HTTP console. No logic lives here: each tool validates its
// arguments, calls the node, and hands the answer back both as JSON text
// (for hosts that read only text) and as structured content.

import type { CallToolResult, McpServer } from "@modelcontextprotocol/server"
import { z } from "zod"

import { NodeClient, NodeError, type Json } from "./node.js"

/** Matches the agent demo's bound; the node clamps further if it must. */
const QUERY_LIMIT_MAX = 25
const QUERY_LIMIT_DEFAULT = 8
/** The node serves at most this many lines per scan (`SCAN_LINES_MAX`). */
const SCAN_LINES_MAX = 2000
const CATALOG_LIMIT_MAX = 1000
const CATALOG_LIMIT_DEFAULT = 100
/** Raster types a host may show inline; the same list the node's `raw`
 * route renders inline, so both surfaces agree on what an image is. */
const IMAGE_CONTENT_TYPES = [
  "image/png",
  "image/jpeg",
  "image/gif",
  "image/webp",
]

const address = z
  .string()
  .min(1)
  .describe(
    "An inseam:// address, as a query result or a catalog entry names it."
  )

const host = z
  .string()
  .min(1)
  .optional()
  .describe(
    "The host to act on (see `hosts`). Omit when the node stewards one host."
  )

export function registerTools(server: McpServer, node: NodeClient): void {
  registerLadder(server, node)
  registerOwnerViews(server, node)
  registerIndex(server, node)
}

function registerLadder(server: McpServer, node: NodeClient): void {
  server.registerTool(
    "query",
    {
      title: "Search the index",
      description:
        "Search the discovery index. Returns ranked sources with scores, summaries, and matching-fragment hints. The cheapest rung: start here, then `expand` or `scan` the promising results and `fetch` only what earns it.",
      inputSchema: z.object({
        text: z
          .string()
          .min(1)
          .describe("What to look for; plain words work best."),
        limit: z
          .number()
          .int()
          .min(1)
          .max(QUERY_LIMIT_MAX)
          .default(QUERY_LIMIT_DEFAULT)
          .describe(`Most results to return (1..${QUERY_LIMIT_MAX}).`),
      }),
      annotations: readOnly(),
    },
    ({ text, limit }) => serve(() => node.query({ text, limit }))
  )

  server.registerTool(
    "expand",
    {
      title: "Expand a source",
      description:
        "One source's fragments and relations from the index: its sections with line extents, plus related entities and the other sources they connect to. Index-only, no content read.",
      inputSchema: z.object({ address }),
      annotations: readOnly(),
    },
    ({ address }) => serve(() => node.expand({ address }))
  )

  server.registerTool(
    "scan",
    {
      title: "Read a line range",
      description: `Read lines start..end (1-based, inclusive) of a text source without fetching it all. At most ${SCAN_LINES_MAX} lines per call; the response's \`end\` and \`lines_total\` say where it stopped and how much there is. A source that is not text is served from its largest text descendant (a transcript, an OCR layer).`,
      inputSchema: z.object({
        address,
        start: z.number().int().min(1).describe("First line, 1-based."),
        end: z.number().int().min(1).describe("Last line, inclusive."),
      }),
      annotations: readOnly(),
    },
    ({ address, start, end }) => {
      if (end < start) {
        return refuse("invalid_request", "`end` must not be before `start`")
      }
      return serve(() => node.scan({ address, start, end }))
    }
  )

  server.registerTool(
    "fetch",
    {
      title: "Fetch a source as text",
      description:
        "A text source's whole content. The most expensive rung; prefer `scan`. Refuses a binary source by name — use `fetch_bytes` for those.",
      inputSchema: z.object({ address }),
      annotations: readOnly(),
    },
    ({ address }) => serve(() => node.fetch({ address }))
  )

  server.registerTool(
    "fetch_bytes",
    {
      title: "Fetch raw bytes",
      description:
        "The raw bytes of a source that is not text (an image, a PDF) or of a fragment's content reference (an image a document links to), with their content type. Images come back as image content; anything else as an embedded resource. Bounded at 32 MiB.",
      inputSchema: z.object({ address }),
      annotations: readOnly(),
    },
    ({ address }) => serveBytes(() => node.fetchBytes({ address }))
  )
}

function registerOwnerViews(server: McpServer, node: NodeClient): void {
  server.registerTool(
    "hosts",
    {
      title: "List hosts",
      description:
        "The hosts this node stewards: id, kind, display name, what each connection supports, and the folders configured on it (each is a root `index` accepts).",
      inputSchema: z.object({}),
      annotations: readOnly(),
    },
    () => serve(async () => ({ hosts: await node.hosts() }))
  )

  server.registerTool(
    "status",
    {
      title: "Index status",
      description:
        "Store and index statistics: sources, indexed sources, fragments, relations, store and content bytes, the embedding identity, and whether re-embedding is pending.",
      inputSchema: z.object({}),
      annotations: readOnly(),
    },
    () => serve(() => node.status())
  )

  server.registerTool(
    "catalog",
    {
      title: "List the catalog",
      description:
        "The catalog as this node holds it — every source it knows about, deep-indexed or still waiting on budget — with counts over the selection and the first `limit` entries.",
      inputSchema: z.object({
        host,
        filter: z
          .enum(["all", "indexed", "pending"])
          .default("all")
          .describe("Which cataloged sources to list."),
        limit: z
          .number()
          .int()
          .min(1)
          .max(CATALOG_LIMIT_MAX)
          .default(CATALOG_LIMIT_DEFAULT)
          .describe(
            `Entries to return (1..${CATALOG_LIMIT_MAX}); the counts cover the whole selection.`
          ),
      }),
      annotations: readOnly(),
    },
    ({ host, filter, limit }) =>
      serve(() => node.catalog({ host, filter, limit }))
  )
}

function registerIndex(server: McpServer, node: NodeClient): void {
  server.registerTool(
    "index",
    {
      title: "Index a root",
      description:
        "Reconcile the index over one root of a host: new, changed, and deleted sources are picked up. `root` names a folder configured on the host (see `hosts`) or a root id the node's operator approved. This spends the node's transform and embedding budget; ask before running it on a large root.",
      inputSchema: z.object({
        host,
        root: z
          .string()
          .min(1)
          .describe("A configured folder or an approved root id."),
        rebuild: z
          .boolean()
          .default(false)
          .describe("Re-index every source even when nothing changed."),
        deep_budget: z
          .union([
            z.literal("catalog_only"),
            z.literal("unlimited"),
            z.object({ sources: z.number().int().min(0) }),
          ])
          .optional()
          .describe(
            "This run's deep budget: `catalog_only` ingests without deep-indexing, `{ sources: N }` deep-indexes at most N, `unlimited` lifts the composition's cap. Omit to take the composition's."
          ),
      }),
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: true,
      },
    },
    ({ host, root, rebuild, deep_budget }) =>
      serve(() => node.index({ host, root, rebuild, deep_budget }))
  )
}

function readOnly() {
  return { readOnlyHint: true, destructiveHint: false, idempotentHint: true }
}

/** Run one operation and shape its answer: the node's JSON as both text
 * and structured content, or its error as an `isError` result so the
 * model sees the code the node chose instead of a protocol failure. */
async function serve(operation: () => Promise<Json>): Promise<CallToolResult> {
  try {
    const result = await operation()
    return {
      content: [{ type: "text", text: JSON.stringify(result) }],
      structuredContent: result,
    }
  } catch (error) {
    return failure(error)
  }
}

async function serveBytes(
  operation: () => Promise<{
    address: string
    content_type: string
    bytes: string
  }>
): Promise<CallToolResult> {
  try {
    const result = await operation()
    const isImage = IMAGE_CONTENT_TYPES.includes(result.content_type)
    return {
      content: [
        isImage
          ? { type: "image", data: result.bytes, mimeType: result.content_type }
          : {
              type: "resource",
              resource: {
                uri: result.address,
                mimeType: result.content_type,
                blob: result.bytes,
              },
            },
      ],
      structuredContent: {
        address: result.address,
        content_type: result.content_type,
      },
    }
  } catch (error) {
    return failure(error)
  }
}

function failure(error: unknown): CallToolResult {
  if (error instanceof NodeError) {
    return refuse(error.code, error.message)
  }
  const message = error instanceof Error ? error.message : String(error)
  return refuse("failed", message)
}

function refuse(code: string, message: string): CallToolResult {
  return {
    content: [{ type: "text", text: `${code}: ${message}` }],
    structuredContent: { error: { code, message } },
    isError: true,
  }
}
