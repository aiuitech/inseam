# Glossary

The inseam vocabulary, one picture. Definitions here are canonical; the linked design docs carry the intent behind each.

## Data

- **Source** — a piece of data: an email, a file, a chat thread, a database row. The unit of addressing and discovery. ([design/addressing.md](../design/addressing.md))
- **Address** — the global name of a source: host identity + locator. Addresses sync everywhere; content never does.
- **Locator** — the within-host part of an address. Opaque to everything except the steward's connection plugin.
- **Envelope** — the small, size-bounded metadata record that syncs alongside an address: types, content length, timestamps, trust properties, discovery hints. The only content-derived thing that leaves a host.

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

- **Index** — a node's local semantic graph over the catalog and fetchable content, searchable by full-text and vector similarity. Derived, rebuildable, configured per node. ([design/indexing.md](../design/indexing.md))
- **Fragment** — the unit of the index: a piece of understanding derived from a source (a markdown section, a transcript line, a summary, an entity), carrying a mimetype, an embedding, an extent, and typed relations. Index-local; never syncs; carries no access rules of its own — authorization is source-level only.
- **Extent** — a fragment's recorded position and length within its parent (lines for text, bytes or timestamps otherwise); what makes `scan` ranges possible.
- **Relation** — a typed edge between fragments: `contains`, `links-to`, `derived-from`, `mentions`, `transcribes`… Relation kinds drive retrieval boosting.
- **Transform** — a plugin-registered handler that takes a fragment of a mimetype it claims and emits child fragments. Indexing is recursive transform application.
- **Entity** — a deduplicated fragment for a person, place, organization, project, or date, related by `mentions` edges to every fragment that references it; the graph's connective tissue. Belongs to no single source, so it conducts relevance but never ranks as a result.
- **Shape stamp / mimetype inventory** — the two per-source records that make invalidation claims-aware: a digest of the transform registrations that built the subtree, and the mimetypes present in it. Plugin churn dirties only sources whose inventory intersects the changed claims. ([design/index-maintenance.md](../design/index-maintenance.md))
- **Finder** — the retrieval algorithm: hybrid full-text + vector seed search, then relevance propagation along relations (spreading activation / Personalized PageRank), rolled up to ranked sources with scores, summaries, and hints. ([design/finder.md](../design/finder.md))
- **Incremental discovery** — the client loop the Finder serves: query → expand or scan the promising results → fetch only what earns it, each step costing more context than the last.
- **Expand** — boundary operation returning one source's fragments and relations from the serving node's index, so a client navigates a result's structure instead of re-searching.
- **Scan** — boundary operation reading a range of a source (lines for text; on media, redirects to descendant text fragments such as a transcript), so a client peeks into a large source without fetching it all.
- **Discovery** — querying indexes (local first, then stronger connected nodes) to get ranked addresses + envelopes, then fetching sources progressively. ([design/discovery.md](../design/discovery.md))

## Trust

- **Trust domain** — the network itself. All data inside is trusted by construction: setting up a node and its connections is the access declaration. Access control applies only at the boundary. ([design/access-control.md](../design/access-control.md))
- **Boundary** — the line between the network and everything outside it. Crossed via a node's API, never via sync.
- **External requester** — an app, service, or session outside the network making boundary requests. Not a node; never syncs; sees only what its properties unlock.
- **Trust property** — a `key:value` claim (`email:greg@aiui.tech`) attached to a host or a source's envelope; the unit of access control. There are no user accounts.
- **Verified / claimed** — the two trust levels of a property: proven by a defined method (with verifier and expiry) vs. asserted without proof.
- **Verifier** — whoever performed a verification. A registered external caller may hold verifier trust for property namespaces (e.g. `email:*`), letting it assert properties on behalf of its own users.
- **Public** — an explicit, owner-declared exposure marking a host or source as visible to any external requester, no property match required. Nothing is public by default.

## Interfaces

- **Operation** — a typed, transport-neutral request/response the core defines: boundary operations (`query`, `expand`, `scan`, `fetch`, `verify`) and owner operations (managing connections, hosts, plugins). ([design/node-api.md](../design/node-api.md))
- **Adapter** — a thin transport skin over the operations layer: HTTP/JSON, MCP, CLI in core; gRPC and others are mechanical additions.

## The kernel and plugins

- **Kernel** — the smallest thing that makes "everything is a plugin" true: it runs plugins (the substrate) and owns persistent structure (the store). Knows no host, format, ranking, or transport. ([design/kernel.md](../design/kernel.md))
- **Seam** — a typed service interface bound to a well-known key (`connection`, `transforms`, `llm`, …); the system's real API surface. Definitions live in `inseam-seams`, apart from providers and consumers. ([design/services.md](../design/services.md))
- **Capability fact** — a declared property of whatever provider is mounted at a key ("offers a change feed", "works offline"); consumers branch on facts, never provider identity.
- **Plugin** — the unit of everything: name + typed config + inject (its capability manifest) + provide + apply. Two trust tiers, one model: **native** (Rust, statically linked, activated by composition) and **sandboxed** (WASM components mounted at runtime through the plugin-host bridge). ([design/plugins.md](../design/plugins.md))
- **Fiber** — one running plugin instance with a reactive lifecycle: activates when its required seams are provided, restarts when a provider changes, fails alone.
- **Effect** — a change to shared state registered together with its undo at the moment it is made; unload unwinds a fiber's effects in reverse, so removal is derived, never authored.
- **Composition** — the declarative TOML entry tree saying what runs with what config; layered (distribution base ← node file) and reconciled with confluence: any edit history ends where a fresh boot of the final composition would. ([design/composition.md](../design/composition.md))
- **Distribution** — an app crate that links a set of native plugins and ships a base composition: the CLI, the FFI library, the macOS app.
- **Capability** — a grant handed to a plugin, never grabbed: the metered LLM handle, source bytes, host allowlists. For sandboxed plugins the manifest declares requests and the bridge attenuates; plugins get no raw network access.
- **Release cooldown** — the soak a newly observed sandboxed-plugin version waits before activating, on this node's own first-seen clock; capability widening needs explicit consent regardless of soak. ([design/plugins.md](../design/plugins.md))
