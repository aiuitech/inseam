# Connections

A **connection** is an edge owned by a node: the concrete, configured means of reaching one other party. Connections are where protocols, credentials, and capabilities live.

## Two kinds

- **Node ↔ node.** Carries the inseam protocol: address sync, discovery queries, and fetch routing. Both ends are inseam instances, inside the trust domain.
- **Node → host.** How a steward reaches a host it serves. For a local host this is a local protocol (filesystem, OS APIs); for a remote host it is the service's own protocol (Gmail API, IMAP, a REST endpoint…). Used to enumerate sources, extract envelopes, and fetch content on demand. The host is unaware of inseam either way.

External requesters ([access-control](access-control.md)) are neither: they don't hold connections into the network — they arrive through a node's [API](node-api.md) and stay outside it.

## Anatomy

Every connection specifies:

- **Protocol** — how to speak: inseam-native over WebSocket or HTTP, or a service protocol supplied by a [plugin](plugins.md).
- **Credentials** — OAuth grants, API keys, mutual keys between nodes. Held locally by the owning node, never synced.
- **Capabilities** — what the edge supports: read-only vs. read-write, sync-capable (node↔node only) vs. fetch-only, enumeration support, and whether it offers a **change feed** (FSEvents, Gmail history API, …) that lets [index maintenance](index-maintenance.md) run targeted sweeps instead of full ones.

Capabilities of node→host connections are published network-wide as stewardship records in the [roster](roster.md), so routing knows which stewards can serve (and write to) which hosts. Credentials never leave the owning node; the roster carries the *fact* of the connection, never the means.

## Every connection type is a plugin

Connection types are providers on the `connection` [seam](services.md): the filesystem connection and the inseam-native node↔node protocols ship as native plugins in the distributions, and every service-specific type (Gmail, Slack, a CalDAV server) arrives as a sandboxed [plugin](plugins.md). Adding a new kind of host to the network means writing a connection plugin, not touching anything. A connection's capabilities — change feed, writability — surface as capability facts consumers branch on ([kernel](kernel.md)).

Node↔node connections authenticate by mutual proof of the peers' keypairs — a node's identity *is* its public key, distributed via its [roster](roster.md) node record.

## Open questions

- Connection liveness/health model and how it feeds routing decisions.
- Whether node↔node connections are symmetric (either side may initiate sync/query) — working assumption: yes, capabilities permitting.
