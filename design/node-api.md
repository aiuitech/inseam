# Node API

How anything outside a node talks to it. The ruling intent: **one operations layer, many transports**. Every way of reaching a node — HTTP, MCP, CLI, gRPC — is a thin adapter over the same protocol-neutral operations, so exposing inseam a new way is mechanical, never a redesign.

## The operations layer

Operations are typed request/response messages with no transport assumptions (no headers, routes, or status codes), JSON-serializable by construction. The `operations` [seam](services.md) is a registry: plugins register the operations their features expose, transports consume the registry — so a new capability (a connection's owner ops, a verification flow) brings its operations with it instead of editing a central table. Every dispatch runs through a waterfall where boundary policy lives: [access-control](access-control.md) property filtering, and later rate limits and audit, are listeners there, with denial monotonic — no listener can force-allow what another denied. Two scopes:

- **Boundary operations** — available to [external requesters](access-control.md):
  - `query` — run [discovery](discovery.md) with a topic/filters; returns ranked results: addresses + envelopes, each with score, summary, and fragment hints ([Finder](finder.md)).
  - `expand` — return one source's fragments and relations from the serving node's index, for navigating a result's structure instead of re-searching.
  - `scan` — read a range of a source (lines for text; media redirects to descendant text fragments), so a client peeks into a large source without fetching it all.
  - `fetch` — retrieve a source's full content by address.
  - `verify` — initiate a property-verification flow (e.g. send a confirmation link), yielding a verified property the requester can subsequently present.

  `query` → `expand`/`scan` → `fetch` is the incremental-discovery ladder ([Finder](finder.md)): each rung costs more context than the last, and an AI client climbs only where the previous rung earned it.
- **Owner operations** — managing the node itself: connections, hosts, plugins, index configuration and repair, sync status. Same layer, separate scope; never exposed at the boundary.

## Transport adapters

Adapters are transport [plugins](plugins.md) translating a wire protocol into operation calls and back. Shipping in the first distributions:

- **HTTP/JSON** — `inseam serve` exposes an authenticated owner API and may serve the shared Vite console. The external property-filtered boundary remains a separate route scope and is not built yet.
- **MCP** — the same operations exposed as MCP tools (stdio for local agents, streamable HTTP for remote ones). This is how AI agents get inseam as a context engine without any custom integration.
- **CLI** — the same binary invoking operations directly: in-process against the local data, or against a running node.

Everything else (gRPC, language SDKs, …) is a mechanical wrapping of the same schemas; adapters carry no logic of their own, so the community can add them as plugins without touching anything.

## Anatomy of a boundary request

Every boundary request carries:

1. **Caller identity** — the external service authenticates itself to the node (registered caller + credential).
2. **Asserted properties** — the trust properties the request acts under, e.g. `email:some@user.com`.
3. **The operation** — query / fetch / verify.

Why the node believes the assertion: a registered caller is granted **verifier trust** for specific property namespaces — my chat service may assert `email:*` because I trust its email-verification flow. Callers without that grant can only use properties the node itself verified for them via `verify`. Either way, the [access-control](access-control.md) matching rule then governs what the request can see: sources carrying the asserted properties, plus anything the network exposes as public.

So the example flow: my external service calls `query("some topic", properties: [email:some@user.com])` over HTTP with its API key; the node fans out across the network with the property filter attached, and returns matching results as JSON; the service then `expand`s or `scan`s the promising ones and `fetch`es only the sources that earn it.

## Paths not taken

- **HTTP-first API design.** Rejected: baking one transport's semantics into the operations makes every other transport a leaky translation. Transport concepts stay in adapters.
- **External requesters speaking the node↔node protocol.** Rejected: the internal protocol assumes trust-domain membership (full sync, no filters). The boundary is deliberately a separate, minimal surface.

## Open questions

- Schema/IDL for the operation messages (something WIT- or protobuf-neutral the adapters and plugins can both consume).
- Caller registration and credential scheme; scoping a caller's verifier trust.
- Streaming large fetch results through adapters.
- Boundary hardening: rate limits, quotas, audit logging of external access.

## Settled since

- **The first HTTP transport is owner-only.** It uses a signed `HttpOnly`
  session cookie and route-level owner authentication. This makes the hosted
  node useful without claiming that caller registration and verified-property
  filtering are finished. Index calls name an operator-configured root ID;
  the browser never sends an arbitrary server path.
- **Connecting an account is an owner operation, in every transport.**
  `grants` / `authorize_grant` / `await_authorization` /
  `complete_authorization` / `revoke_grant` ride the operations seam, so the
  CLI, the FFI app, and the HTTP console stay logic-free skins over one flow
  ([connections](connections.md)). The HTTP transport carries one consequence
  of being remote: a second unauthenticated route, `/api/v1/oauth/callback`,
  where the provider returns the owner's browser — unauthenticated because a
  `SameSite=Strict` cookie never rides a cross-site redirect, safe because the
  operation accepts nothing but the `state` it issued, and addressed by an
  explicit public URL (`--public-url`) rather than a trusted `Host` header.
- **Plugins are owner operations too.** `plugins` (every entry as the
  kernel runs it) and `install_plugin` (a plugin directory's files, mounted
  into the running node) ride the seam, so the console, the CLI, and any
  future transport share one install path and one vocabulary for "what is
  this node running" ([composition](composition.md) has the mechanism).
  The upload is the operation message itself — files as standard base64 in
  JSON, bounded in count and bytes — not a transport-specific multipart
  form; the HTTP adapter's only contribution is a larger body limit on that
  one route. Upgrading a mounted plugin is remove-then-install for now.
