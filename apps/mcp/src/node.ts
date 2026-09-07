// The node's owner HTTP API as one typed client (`docs/architecture/
// hosted-node.md`). This is the only file that knows the routes, the login
// exchange, and the error envelope; the tools call methods here and never
// see a URL. `fetch` is injected so the tests drive the client with an
// in-memory fake — no sockets, no timing.

const API_PREFIX = "/api/v1"
/** The node answers every route within 60 seconds; give it the same and a
 * little slack for the wire. */
const REQUEST_TIMEOUT_MS = 65_000
/** One re-login per request, beyond the sign-in that starts a session: a
 * session cookie lasts twelve hours, so a 401 means it expired or the node
 * restarted; a 401 straight after a fresh sign-in means the token itself
 * is wrong and retrying would loop forever. */
const RELOGINS_MAX = 1

export type FetchLike = (input: string, init: RequestInit) => Promise<Response>

export type NodeClientOptions = {
  nodeUrl: string
  ownerToken: string
  fetch: FetchLike
}

/** The node's error envelope, kept as a typed error so a tool can report
 * the code the node chose rather than an HTTP status. */
export class NodeError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = "NodeError"
    this.status = status
    this.code = code
  }
}

type ErrorEnvelope = { error?: { code?: string; message?: string } }

export type QueryRequest = { text: string; limit: number }
export type ExpandRequest = { address: string }
export type ScanRequest = { address: string; start: number; end: number }
export type FetchRequest = { address: string }
export type FetchBytesRequest = { address: string }
export type FetchBytesResponse = {
  address: string
  content_type: string
  /** Standard base64, as the operation carries it. */
  bytes: string
}
export type CatalogRequest = {
  host?: string
  filter: "all" | "indexed" | "pending"
  limit: number
}
export type IndexRequest = {
  host?: string
  root: string
  rebuild: boolean
  deep_budget?: "catalog_only" | "unlimited" | { sources: number }
}

/** Responses are passed through as the node shaped them: the operation
 * messages are the contract (`docs/finder/operations.md`), and retyping
 * them here would only add a place for the two to drift apart. */
export type Json = Record<string, unknown>

export class NodeClient {
  readonly #nodeUrl: string
  readonly #ownerToken: string
  readonly #fetch: FetchLike
  #cookie: string | undefined

  constructor(options: NodeClientOptions) {
    this.#nodeUrl = options.nodeUrl.replace(/\/$/, "")
    this.#ownerToken = options.ownerToken
    this.#fetch = options.fetch
  }

  query(request: QueryRequest): Promise<Json> {
    return this.#owner("/owner/query", request)
  }

  expand(request: ExpandRequest): Promise<Json> {
    return this.#owner("/owner/expand", request)
  }

  scan(request: ScanRequest): Promise<Json> {
    return this.#owner("/owner/scan", request)
  }

  fetch(request: FetchRequest): Promise<Json> {
    return this.#owner("/owner/fetch", request)
  }

  fetchBytes(request: FetchBytesRequest): Promise<FetchBytesResponse> {
    return this.#owner<FetchBytesResponse>("/owner/fetch_bytes", request)
  }

  hosts(): Promise<Json[]> {
    return this.#owner<Json[]>("/owner/hosts")
  }

  status(): Promise<Json> {
    return this.#owner("/owner/status")
  }

  catalog(request: CatalogRequest): Promise<Json> {
    return this.#owner("/owner/catalog", request)
  }

  index(request: IndexRequest): Promise<Json> {
    return this.#owner("/owner/index", request)
  }

  /** Whether the node is up and this client can sign in — what `main`
   * checks before it accepts a transport, so a bad token fails at start. */
  async login(): Promise<void> {
    this.#cookie = await this.#exchangeToken()
  }

  /** One owner call: sign in when there is no session yet, retry once
   * after a 401, and otherwise return the JSON body or the node's error.
   * The type parameter names the shape the route answers with; the body
   * is trusted as that shape, the way the console's client trusts it. */
  async #owner<T = Json>(path: string, body?: unknown): Promise<T> {
    if (this.#cookie === undefined) {
      this.#cookie = await this.#exchangeToken()
    }
    let relogins = 0
    for (;;) {
      const response = await this.#request(path, body, this.#cookie)
      if (response.status !== 401 || relogins >= RELOGINS_MAX) {
        return (await readJson(response)) as T
      }
      this.#cookie = await this.#exchangeToken()
      relogins += 1
    }
  }

  async #exchangeToken(): Promise<string> {
    const response = await this.#request("/session", {
      token: this.#ownerToken,
    })
    if (!response.ok) {
      await readJson(response)
      throw new NodeError(response.status, "login_failed", "login failed")
    }
    const header = response.headers.get("set-cookie")
    if (header === null) {
      throw new NodeError(
        response.status,
        "login_failed",
        "the node accepted the token but set no session cookie"
      )
    }
    return sessionCookie(header)
  }

  async #request(
    path: string,
    body: unknown,
    cookie?: string
  ): Promise<Response> {
    const headers: Record<string, string> = { accept: "application/json" }
    if (body !== undefined) {
      headers["content-type"] = "application/json"
    }
    if (cookie !== undefined) {
      headers.cookie = cookie
    }
    const init: RequestInit = {
      method: body === undefined ? "GET" : "POST",
      headers,
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    }
    if (body !== undefined) {
      init.body = JSON.stringify(body)
    }
    try {
      return await this.#fetch(`${this.#nodeUrl}${API_PREFIX}${path}`, init)
    } catch (error) {
      throw new NodeError(0, "unreachable", describeFailure(error))
    }
  }
}

/** The `name=value` pair of a `Set-Cookie` header, attributes dropped —
 * what the next request's `Cookie` header carries. */
export function sessionCookie(header: string): string {
  const pair = header.split(";")[0]?.trim() ?? ""
  const separator = pair.indexOf("=")
  if (separator <= 0) {
    throw new NodeError(0, "login_failed", "the session cookie is malformed")
  }
  return pair
}

async function readJson(response: Response): Promise<Json> {
  if (response.status === 204) {
    return {}
  }
  const text = await response.text()
  let parsed: unknown
  try {
    parsed = JSON.parse(text)
  } catch {
    throw new NodeError(
      response.status,
      "bad_response",
      `the node answered ${response.status} with a body that is not JSON`
    )
  }
  if (!response.ok) {
    const envelope = parsed as ErrorEnvelope
    throw new NodeError(
      response.status,
      envelope.error?.code ?? "request_failed",
      envelope.error?.message ?? `request failed with status ${response.status}`
    )
  }
  return parsed as Json
}

function describeFailure(error: unknown): string {
  if (error instanceof Error) {
    const cause = error.cause
    const detail = cause instanceof Error ? `: ${cause.message}` : ""
    return `${error.message}${detail}`
  }
  return String(error)
}
