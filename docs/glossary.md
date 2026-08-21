# Glossary

The inseam vocabulary, one page. Definitions here are canonical; the linked design docs explain the reasoning behind each.

## Data

- **Source** — one piece of data: an email, a file, a chat thread, a database row. The unit everything is addressed and found by. ([design/addressing.md](../design/addressing.md))
- **Address** — a source's global name: host identity + locator. Addresses sync everywhere; content never does.
- **Locator** — the part of an address that's specific to its host. Only that host's connection plugin knows how to read it.
- **Envelope** — a small, size-limited metadata record that travels with an address: types, content length, timestamps, trust properties, discovery hints. The only content-derived thing that ever leaves a host.

## Topology

- **Host** — where sources live: a filesystem, a Gmail account, a SaaS API. It just holds data; it has no inseam machinery of its own. ([design/nodes-and-hosts.md](../design/nodes-and-hosts.md))
- **Node** — a running inseam instance; a participant in the network. Distinct from a host even on the same machine: the host holds the sources, the node does the networking.
- **Steward** — the node that speaks for a host: publishes its addresses, indexes it, serves fetches from it. Every host joins the network through a steward.
- **Local host / remote host** — a host its steward reaches through the local machine (filesystem, OS APIs) vs. through a service's own protocol (OAuth + REST, IMAP). The only difference is which connection plugin is used.
- **Connection** — an edge owned by a node: node↔node (the inseam protocol: sync, query, fetch routing) or node→host (how a steward reaches a host). Carries protocol, credentials, and capabilities. A node holds many, one per host, registered by connection plugins into the `connections` seam. ([design/connections.md](../design/connections.md))
- **Grant** — one configured OAuth authorization against a provider account (`google`, `slack-work`): endpoints, scopes, and which environment variables hold the client's own credentials. Authorized once by the owner; host connections consume it by id for live access tokens. ([plugins/oauth.md](plugins/oauth.md))
- **Network** — the graph of nodes: a private network forming a single trust domain with no central authority. ([design/network.md](../design/network.md))

## Sync

- **Catalog** — a node's local copy of every known address, envelope, and host record: its complete, offline-available picture of the network. ([design/address-sync.md](../design/address-sync.md))
- **Address sync** — full, unfiltered copying of the catalog across node↔node connections; eventually consistent, and the origin node wins conflicts.
- **Tombstone** — the synced record of a deleted source, so deletions spread instead of lingering.

## Discovery

- **Index** — a node's local semantic graph over the catalog and fetchable content, searchable by full-text and by vector similarity. Derived, rebuildable, and configured per node. ([design/indexing.md](../design/indexing.md))
- **Fragment** — the unit of the index: a piece of understanding derived from a source (a markdown section, a transcript line, a summary, an entity), with a mimetype, an embedding, a position, and typed relations. Local to the index; never syncs; carries no access rules of its own — access is decided at the source level only.
- **Extent** — a fragment's recorded position and length within its parent (lines for text, bytes or timestamps otherwise); what makes `scan` ranges possible.
- **Relation** — a typed edge between fragments, read input → output. The kernel defines `contains` and `derives`; plugins name the rest (`links-to`, `mentions`, `transcribes`, …). The relation kind affects ranking.
- **Keyed fragment** — a fragment that belongs to no source and is deduplicated index-wide under a plugin-namespaced key; emitted by transforms as a *keyed sprout* with an anchor rule. Entities are the first vocabulary built on it.
- **Transform** — a plugin-registered handler that takes a fragment of a mimetype it claims and emits child fragments. Indexing is just transforms applied over and over.
- **Entity** — the entity plugin's keyed fragment for a person, place, organization, project, or date, connected by `mentions` edges to every fragment that references it — the glue of the graph. It belongs to no single source, so it carries relevance between results but never appears as a result itself.
- **Ignore rule** — an owner's statement that a source is not theirs to index: host-native exclusions in a connection's config (gitignore patterns for the filesystem) and host-agnostic globs over addresses and envelopes on the sweep. Ignored sources are never cataloged, and ones indexed earlier are removed. ([design/ignore.md](../design/ignore.md))
- **Shape stamp / mimetype inventory** — two records kept per source that make re-indexing precise: a fingerprint of the transforms that built its index, and the list of mimetypes found in it. When plugins change, only sources those changes actually touch get re-indexed. ([design/index-maintenance.md](../design/index-maintenance.md))
- **Finder** — the retrieval algorithm: combined full-text + vector search to find starting points, then relevance spread along the graph's relations, rolled up into ranked sources with scores, summaries, and hints. ([design/finder.md](../design/finder.md))
- **Incremental discovery** — the client loop the Finder serves: query → expand or scan the promising results → fetch only what earns it, each step costing more than the last.
- **Expand** — an operation returning one source's fragments and relations from the serving node's index, so a client can navigate a result's structure instead of searching again.
- **Scan** — an operation reading a range of a source (lines for text; media redirects to a text descendant such as a transcript), so a client can peek into a large source without fetching all of it.
- **Discovery** — querying indexes (local first, then better-placed connected nodes) to get ranked addresses and envelopes, then fetching sources step by step. ([design/discovery.md](../design/discovery.md))

## Trust

- **Trust domain** — the network itself. Everything inside is trusted by construction: setting up a node and its connections *is* the access declaration. Access control applies only at the boundary. ([design/access-control.md](../design/access-control.md))
- **Boundary** — the line between the network and everything outside it. Crossed only through a node's API, never through sync.
- **External requester** — an app, service, or session outside the network making requests at the boundary. Not a node; never syncs; sees only what its properties unlock.
- **Trust property** — a `key:value` claim (`email:greg@aiui.tech`) attached to a host or a source's envelope; the unit of access control. There are no user accounts.
- **Verified / claimed** — a property's two trust levels: proven by a defined method (with a verifier and an expiry) vs. simply asserted.
- **Verifier** — whoever performed a verification. A registered external caller may hold verifier trust for property namespaces (e.g. `email:*`), letting it vouch for its own users.
- **Public** — an explicit, owner-declared marking that makes a host or source visible to any external requester, no property match required. Nothing is public by default.

## Interfaces

- **Operation** — a typed request/response the core defines, independent of any transport: boundary operations (`query`, `expand`, `scan`, `fetch`, `verify`) and owner operations (managing connections, hosts, plugins). ([design/node-api.md](../design/node-api.md))
- **Adapter** — a thin transport wrapper over the operations layer: HTTP/JSON, MCP, and the CLI in core; gRPC and others are mechanical additions.

## The kernel and plugins

- **Kernel** — the smallest thing that makes "everything is a plugin" true: it runs plugins (the substrate) and owns persistent data (the store). It knows nothing about hosts, formats, ranking, or transports. ([design/kernel.md](../design/kernel.md))
- **Seam** — a typed service interface bound to a well-known key (`connections`, `transforms`, `llm`, `oauth`, …); together the seams are the system's real API. Definitions live in `inseam-seams`, separate from both providers and consumers. ([design/services.md](../design/services.md))
- **Capability fact** — a declared property of whatever provider is mounted at a key ("offers a change feed", "works offline"). Consumers check facts, never which provider it is.
- **Plugin** — the unit of everything: name + typed config + inject (its capability manifest) + provide + apply. Two trust tiers, one model, named for when the code enters the node: **linked** (Rust, compiled into the binary at build, turned on by the composition) and **loaded** (WASM components mounted while the node runs, sandboxed by construction). ([design/plugins.md](../design/plugins.md))
- **Fiber** — one running plugin instance, with a reactive lifecycle: it starts when the services it needs exist, restarts when one of them changes, and fails alone.
- **Effect** — a change to shared state recorded together with its undo at the moment it's made; unloading a plugin replays its undos in reverse, so removal never has to be hand-written.
- **Composition** — the declarative TOML tree saying what runs with what config; layered (built-in base + node file). However you edit it, the node ends up in the same state a fresh boot of the final file would produce. ([design/composition.md](../design/composition.md))
- **Distribution** — an app crate that compiles in a set of linked plugins and ships a base composition: the CLI, the FFI library, the macOS app. Custom distributions compile private linked plugins in from source ([plugins/distributions.md](plugins/distributions.md)).
- **Capability** — something a plugin is handed, never something it grabs: the metered LLM handle, source bytes, host allowlists. For loaded plugins the manifest declares what's requested and the bridge narrows it; plugins get no raw network access.
- **Release cooldown** — the wait period a newly seen loaded-plugin version sits through before activating, counted from when *this node* first saw it. Asking for wider capabilities than the approved version needs explicit consent regardless of the wait. ([design/plugins.md](../design/plugins.md))
- **Conformance harness** — the one validation gate for loaded plugins (`inseam plugin check`): static checks, a mount check, a hostile-input battery, and the plugin's own golden checks. Identical when authoring, in registry CI, and at install. ([plugins/validation.md](plugins/validation.md))
- **Admission** — the validation run a node performs the first time it sees an artifact; failure refuses the plugin with the failing check named. Cached by content hash. ([plugins/validation.md](plugins/validation.md))
- **Golden checks** — a plugin's own tests, written as data (`<name>.checks.toml`): input → expected output shape, run through the real bridge (loaded) or the seam (linked) with canned capabilities. Mandatory: at least one check that proves the claim and one that pins the degrade path. Any node can re-run them without trusting anything. ([plugins/validation.md](plugins/validation.md))
- **Registry (v0)** — the repository's `plugins/` tree: a sha256 index, an advisory feed, and CI gates (reproducible build, harness, AI review); `inseam plugin install` verifies everything locally. ([design/registry.md](../design/registry.md))
