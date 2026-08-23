export type Session = { authenticated: boolean }
export type IndexRoot = { id: string }
export type OwnerInfo = {
  version: string
  index_roots: IndexRoot[]
  oauth_callback_url: string
}

export type GrantState =
  | { state: "missing_secret"; env: string }
  | { state: "unauthorized" }
  | {
      state: "authorized"
      expires_at: number | null
      scopes: string[]
      account?: string
    }

export type Grant = {
  id: string
  provider: string
  scopes: string[]
  client_id_env: string
  client_secret_env?: string
  state: GrantState
}

export type AuthorizationStarted = {
  grant: string
  url: string
  state: string
  redirect_uri: string
}

export type StatusReport = {
  sources: number
  indexed_sources: number
  fragments: number
  relations: number
  keyed_fragments: number
  search_rows: number
  store_bytes: number
  content_bytes: number
  embedding_model: string | null
  embedding_dimensions: number
  reembed_pending: boolean
}

export type Host = {
  id: string
  kind: string
  display_name: string
  entry: string
  capabilities: { enumerates: boolean; change_feed: boolean; writable: boolean }
}

export type QueryResult = {
  address: string
  score: number
  summary: string | null
  envelope: {
    source_type: string
    content_type: string
    length: string
    created?: string
    modified?: string
    title?: string
  }
  hints: Array<{
    fragment: number
    mimetype: string
    extent?: string
    text: string
  }>
}

export type QueryResponse = { results: QueryResult[] }
export type Fragment = {
  id: number
  mimetype: string
  extent?: string
  text?: string
  source?: string
}
export type ExpandResponse = {
  address: string
  summary: string | null
  fragments: Fragment[]
  relations: Array<{ from: number; kind: string; to: number }>
  neighbors: Fragment[]
}
export type FetchResponse = {
  address: string
  content_type: string
  text: string
}
export type PluginState =
  | { state: "active" }
  | { state: "pending" }
  | { state: "failed"; reason: string }

export type Plugin = {
  id: string
  plugin: string
  state: PluginState
  effects: string[]
  missing: string[]
  missing_secrets: Array<{ env: string; purpose: string }>
}

/** One file of a plugin directory, contents as standard base64. */
export type PluginFile = { path: string; bytes: string }

export type IndexReport = {
  sources_seen: number
  indexed: number
  unchanged: number
  fragments: number
  relations: number
  spent: number
}

type ErrorEnvelope = { error?: { code?: string; message?: string } }

export class ApiError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = "ApiError"
    this.status = status
    this.code = code
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`/api/v1${path}`, {
    credentials: "same-origin",
    ...init,
    headers: {
      ...(init?.body ? { "content-type": "application/json" } : {}),
      ...init?.headers,
    },
  })
  if (!response.ok) {
    const body = (await response.json().catch(() => ({}))) as ErrorEnvelope
    throw new ApiError(
      response.status,
      body.error?.code ?? "request_failed",
      body.error?.message ?? `request failed with status ${response.status}`
    )
  }
  if (response.status === 204) return undefined as T
  return (await response.json()) as T
}

function post<T>(path: string, body: unknown): Promise<T> {
  return request<T>(path, { method: "POST", body: JSON.stringify(body) })
}

export const api = {
  session: () => request<Session>("/session"),
  login: (token: string) => post<void>("/session", { token }),
  logout: () => request<void>("/session", { method: "DELETE" }),
  info: () => request<OwnerInfo>("/owner/info"),
  status: () => request<StatusReport>("/owner/status"),
  hosts: () => request<Host[]>("/owner/hosts"),
  query: (text: string) =>
    post<QueryResponse>("/owner/query", { text, limit: 12 }),
  expand: (address: string) =>
    post<ExpandResponse>("/owner/expand", { address }),
  fetch: (address: string) => post<FetchResponse>("/owner/fetch", { address }),
  index: (root: string, host?: string) =>
    post<IndexReport>("/owner/index", { root, host, rebuild: false }),
  grants: () => request<Grant[]>("/owner/grants"),
  authorizeGrant: (grant: string) =>
    post<AuthorizationStarted>("/owner/grants/authorize", { grant }),
  revokeGrant: (grant: string) =>
    post<Grant>("/owner/grants/revoke", { grant }),
  plugins: () => request<Plugin[]>("/owner/plugins"),
  installPlugin: (id: string, files: PluginFile[]) =>
    post<Plugin>("/owner/plugins/install", { id, files }),
}
