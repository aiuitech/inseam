# Glossary

The inseam vocabulary, one picture. Definitions here are canonical; the linked design docs carry the intent behind each.

## Data

- **Source** — a piece of data: an email, a file, a chat thread, a database row. The unit of addressing and discovery. ([design/addressing.md](../design/addressing.md))
- **Address** — the global name of a source: host identity + locator. Addresses sync everywhere; content never does.
- **Locator** — the within-host part of an address. Opaque to everything except the steward's connection plugin.
- **Envelope** — the small, size-bounded metadata record that syncs alongside an address: types, timestamps, trust properties, discovery hints. The only content-derived thing that leaves a host.

## Topology

- **Host** — where sources live: a filesystem, a Gmail account, a SaaS API. Holds data and nothing more; has no inseam machinery. ([design/nodes-and-hosts.md](../design/nodes-and-hosts.md))
- **Node** — a running inseam instance; a participant in the network. Distinct from a host even when they share a machine: the host holds sources, the node does the networking.
- **Steward** — the node that speaks for a host: publishes its addresses, indexes it, serves fetches from it. Every host joins the network through a steward.
- **Local host / remote host** — a host reached by its steward over local protocols (filesystem, OS APIs) vs. over a service's own protocol (OAuth + REST, IMAP). The only difference is the connection plugin.
- **Connection** — an edge owned by a node: node↔node (inseam protocol: sync, query, fetch routing) or node→host (how a steward reaches a host). Carries protocol, credentials, capabilities. ([design/connections.md](../design/connections.md))
- **Network** — the graph of nodes; an intranet forming a single trust domain with no central authority. ([design/network.md](../design/network.md))

## Sync

- **Catalog** — a node's local copy of every known address + envelope + host record; the node's complete, offline-available picture of the network. ([design/address-sync.md](../design/address-sync.md))
- **Address sync** — full, unfiltered replication of the catalog across node↔node connections; eventually consistent, origin-wins.
- **Tombstone** — the synced record of a deleted source, so removals propagate rather than linger.

## Discovery

- **Index** — a node's local hybrid full-text + vector search index over the catalog and fetchable content. Derived, rebuildable, configured per node (small on a phone, deep on a server). ([design/discovery.md](../design/discovery.md))
- **Discovery** — querying indexes (local first, then stronger connected nodes) to get ranked addresses + envelopes, then fetching sources progressively.

## Trust

- **Trust domain** — the network itself. All data inside is trusted by construction: setting up a node and its connections is the access declaration. Access control applies only at the boundary. ([design/access-control.md](../design/access-control.md))
- **Boundary** — the line between the network and everything outside it. Crossed via a node's API, never via sync.
- **External requester** — an app, service, or session outside the network making boundary requests. Not a node; never syncs; sees only what its properties unlock.
- **Trust property** — a `key:value` claim (`email:greg@aiui.tech`) attached to a host or a source's envelope; the unit of access control. There are no user accounts.
- **Verified / claimed** — the two trust levels of a property: proven by a defined method (with verifier and expiry) vs. asserted without proof.
- **Verifier** — whoever performed a verification. A registered external caller may hold verifier trust for property namespaces (e.g. `email:*`), letting it assert properties on behalf of its own users.
- **Public** — an explicit, owner-declared exposure marking a host or source as visible to any external requester, no property match required. Nothing is public by default.

## Interfaces

- **Operation** — a typed, transport-neutral request/response the core defines: boundary operations (`query`, `fetch`, `verify`) and owner operations (managing connections, hosts, plugins). ([design/node-api.md](../design/node-api.md))
- **Adapter** — a thin transport skin over the operations layer: HTTP/JSON, MCP, CLI in core; gRPC and others are mechanical additions.

## Extensibility

- **Plugin** — a sandboxed WASM component supplying the service-specific parts: connection types, source handling, verification methods. Any language; distributed through existing package ecosystems. ([design/plugins.md](../design/plugins.md))
- **Capability** — an I/O grant a plugin's manifest declares and the core mediates (e.g. which external hosts it may call). Plugins get no raw network access.
