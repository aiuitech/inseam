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

## Plugins provide the long tail

The core ships the inseam-native protocols. Every service-specific connection type (Gmail, Slack, a filesystem watcher, a CalDAV server) is a plugin. Adding a new kind of host to the network means writing a connection plugin, not touching the core.

## Open questions

- Node↔node identity and authentication scheme (keypairs per node is the working assumption).
- Connection liveness/health model and how it feeds routing decisions.
- Whether node↔node connections are symmetric (either side may initiate sync/query) — working assumption: yes, capabilities permitting.
