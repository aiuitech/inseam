# Node API

How anything outside a node talks to it. The ruling intent: **one operations layer, many transports**. Every way of reaching a node — HTTP, MCP, CLI, gRPC — is a thin adapter over the same protocol-neutral operations, so exposing inseam a new way is mechanical, never a redesign.

## The operations layer

The core defines a small set of typed operations: plain request/response messages with no transport assumptions (no headers, routes, or status codes), JSON-serializable by construction. Two scopes:

- **Boundary operations** — available to [external requesters](access-control.md):
  - `query` — run [discovery](discovery.md) with a topic/filters; returns ranked addresses + envelopes.
  - `fetch` — retrieve a source's content by address.
  - `verify` — initiate a property-verification flow (e.g. send a confirmation link), yielding a verified property the requester can subsequently present.
- **Owner operations** — managing the node itself: connections, hosts, plugins, index configuration, sync status. Same layer, separate scope; never exposed at the boundary.

## Transport adapters

Adapters translate a wire protocol into operation calls and back. Shipping in core:

- **HTTP/JSON** — `inseam serve` exposes the operations as a plain request/response API; the obvious integration path for external services.
- **MCP** — the same operations exposed as MCP tools (stdio for local agents, streamable HTTP for remote ones). This is how AI agents get inseam as a context engine without any custom integration.
- **CLI** — the same binary invoking operations directly: in-process against the local data, or against a running node.

Everything else (gRPC, language SDKs, …) is a mechanical wrapping of the same schemas; adapters carry no logic of their own, so the community can add them without touching the core.

## Anatomy of a boundary request

Every boundary request carries:

1. **Caller identity** — the external service authenticates itself to the node (registered caller + credential).
2. **Asserted properties** — the trust properties the request acts under, e.g. `email:some@user.com`.
3. **The operation** — query / fetch / verify.

Why the node believes the assertion: a registered caller is granted **verifier trust** for specific property namespaces — my chat service may assert `email:*` because I trust its email-verification flow. Callers without that grant can only use properties the node itself verified for them via `verify`. Either way, the [access-control](access-control.md) matching rule then governs what the request can see: sources carrying the asserted properties, plus anything the network exposes as public.

So the example flow: my external service calls `query("some topic", properties: [email:some@user.com])` over HTTP with its API key; the node fans out across the network with the property filter attached, and returns matching addresses + envelopes as JSON; the service then `fetch`es the sources worth reading.

## Paths not taken

- **HTTP-first API design.** Rejected: baking one transport's semantics into the operations makes every other transport a leaky translation. Transport concepts stay in adapters.
- **External requesters speaking the node↔node protocol.** Rejected: the internal protocol assumes trust-domain membership (full sync, no filters). The boundary is deliberately a separate, minimal surface.

## Open questions

- Schema/IDL for the operation messages (something WIT- or protobuf-neutral the adapters and plugins can both consume).
- Caller registration and credential scheme; scoping a caller's verifier trust.
- Streaming large fetch results through adapters.
- Boundary hardening: rate limits, quotas, audit logging of external access.
