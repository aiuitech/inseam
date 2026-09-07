// A fake node for the tests: the owner API's shape as an in-memory fetch,
// scripted per route. No sockets — the client under test receives a
// `Response` straight from here, so a test asserts on the requests the
// client made and the answers it shaped from them.

import type { FetchLike } from "./node.js"

export const FAKE_TOKEN = "0123456789abcdef0123456789abcdef"
export const FAKE_COOKIE = "inseam_owner=session-one"

export type Recorded = {
  method: string
  path: string
  body: unknown
  cookie: string | undefined
}

export type Route = (request: Recorded) => Response

export type FakeNode = {
  fetch: FetchLike
  requests: Recorded[]
  /** Replace what a route answers; the login route is answered by default. */
  route: (path: string, route: Route) => void
}

export function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  })
}

export function nodeError(
  status: number,
  code: string,
  message: string
): Response {
  return json(status, { error: { code, message } })
}

export function fakeNode(): FakeNode {
  const routes = new Map<string, Route>()
  const requests: Recorded[] = []
  routes.set("/session", (request) => {
    const body = request.body as { token?: string }
    if (body.token !== FAKE_TOKEN) {
      return nodeError(401, "invalid_credentials", "the token is wrong")
    }
    return new Response(null, {
      status: 204,
      headers: {
        "set-cookie": `${FAKE_COOKIE}; Path=/api/v1; HttpOnly; SameSite=Strict`,
      },
    })
  })
  const fetch: FetchLike = async (input, init) => {
    const url = new URL(input)
    const path = url.pathname.replace(/^\/api\/v1/, "")
    const headers = new Headers(init.headers)
    const recorded: Recorded = {
      method: init.method ?? "GET",
      path,
      body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
      cookie: headers.get("cookie") ?? undefined,
    }
    requests.push(recorded)
    const route = routes.get(path)
    if (route === undefined) {
      return nodeError(404, "route_not_found", `no route ${path}`)
    }
    return route(recorded)
  }
  return {
    fetch,
    requests,
    route: (path, route) => {
      routes.set(path, route)
    },
  }
}
