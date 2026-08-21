# Connections

A **connection** is an edge owned by a node: the concrete, configured means of reaching one other party. Connections are where protocols, credentials, and capabilities live.

## Two kinds

- **Node ↔ node.** Carries the inseam protocol: address sync, discovery queries, and fetch routing. Both ends are inseam instances, inside the trust domain.
- **Node → host.** How a steward reaches a host it serves. For a local host this is a local protocol (filesystem, OS APIs); for a remote host it is the service's own protocol (Gmail API, IMAP, a REST endpoint…). Used to enumerate sources, extract envelopes, and fetch content on demand. The host is unaware of inseam either way.

External requesters ([access-control](access-control.md)) are neither: they don't hold connections into the network — they arrive through a node's [API](node-api.md) and stay outside it.

## Anatomy

Every connection specifies:

- **Protocol** — how to speak: for node↔node, the inseam protocol over the iroh transport (below); for node→host, a service protocol supplied by a [plugin](plugins.md).
- **Credentials** — OAuth grants, API keys, mutual keys between nodes. Held locally by the owning node, never synced.
- **Capabilities** — what the edge supports: read-only vs. read-write, sync-capable (node↔node only) vs. fetch-only, enumeration support, and whether it offers a **change feed** (FSEvents, Gmail history API, …) that lets [index maintenance](index-maintenance.md) run targeted sweeps instead of full ones.
- **Host-native exclusions** — the host's own vocabulary for what not to enumerate (`.gitignore` and path patterns for a filesystem; labels and queries for a mailbox), applied during enumeration so an excluded source never becomes an address ([ignore](ignore.md)).

Capabilities of node→host connections are published network-wide as stewardship records in the [roster](roster.md), so routing knows which stewards can serve (and write to) which hosts. Credentials never leave the owning node; the roster carries the *fact* of the connection, never the means.

## Every connection type is a plugin

Connection types are providers on the `connection` [seam](services.md): the filesystem connection and the iroh node↔node transport ship as linked plugins in the distributions, and every service-specific type (Gmail, Slack, a CalDAV server) arrives as a loaded [plugin](plugins.md). Adding a new kind of host to the network means writing a connection plugin, not touching anything. A connection's capabilities — change feed, writability — surface as capability facts consumers branch on ([kernel](kernel.md)).

Node↔node connections authenticate by mutual proof of the peers' keypairs — a node's identity *is* its public key, distributed via its [roster](roster.md) node record.

## Node↔node transport: iroh

The flagship node↔node connection plugin is built on **iroh**: QUIC dialed by the peer's public key, with NAT hole punching and encrypted, stateless relay fallback. Its model matches the roster's assumptions one-to-one — iroh's NodeId *is* the roster's node id, and its node-address info (relay URL + direct-address hints) is what a node record's endpoints field carries. This buys the miserable, undifferentiated problems — path finding, NAT traversal, relaying — from a wire-stable library, instead of spending our novelty budget on plumbing.

The network runs its **own relay**, typically on the always-on backbone node ([network](network.md)), rather than depending on public relay infrastructure. The bet is contained by the seam: iroh is one connection plugin, and swapping transports can never touch a consumer.

HTTP remains at the node [API](node-api.md) boundary for external requesters — that's a different door, not a node↔node transport.

## Paths not taken

- **libp2p.** Built for open networks of anonymous millions: transport negotiation, DHTs, gossip peer-scoring — modularity we'd pay for forever, solving problems a closed trust domain of tens of nodes doesn't have, with weaker NAT traversal than iroh in practice.
- **A mesh VPN (Tailscale/WireGuard) as prerequisite.** Solves reachability well but as an environmental requirement; inseam must work as self-contained node software. Users may of course run nodes over one.
- **A message broker (NATS/MQTT).** A central rendezvous contradicts the no-central-authority stance outright.
- **Hand-rolled transport.** Dial-by-key with traversal and relays is years of work orthogonal to inseam's actual contribution; the connection seam means adopting it costs no architectural freedom.

## Settled since

- **The seam is a registry, and it is plural.** A node stewards many hosts at once, so `connections` (the seam key; the singular `connection` was a single binding and therefore a single host per node — the first architecture's hidden assumption) is a registry provider in the spirit of `transforms`: every connection plugin, linked or loaded, registers one `Registration` per host it reaches as a fiber effect — host description (id, kind, display name), declared `Capabilities` (enumerates, change feed, writable), and the `Connection` handle — and unmounting the plugin unwinds it. Consumers resolve by host: the sweep by the host a `SweepRequest` names, operations by the host in an address. A registration is exactly the stewardship record the [roster](roster.md) will publish, minus credentials, so the roster work is a sync of what the registry already holds. One node holds **one connection per host**; a second registration for a stewarded host fails that fiber alone, by name. *Rejected:* realms (one `connection` key isolated per subtree) — they would answer "several hosts" but not "which host does this address belong to", which the registry answers directly.
- **Scopes name their host.** `IndexRequest`/`SweepRequest` carry a host id; `inseam index` accepts `--host` or an address (`inseam://<host>/<root>`), and a bare scope is accepted only while exactly one host is mounted — the moment there are two, naming is required, never guessed. The filesystem connection accepts a locator as a root (rooted at `/`), so the address form and the path form name the same scope.
- **OAuth is a seam, not a connection.** "A generic OAuth connection" cannot enumerate anything — Gmail's sources are messages, Drive's are files — so the reusable part is the *credential*, and it is the `oauth` seam ([services](services.md)): grants are configured once in the composition (authorization and token endpoints, scopes, which environment variables hold the client id and secret), the linked `oauth` provider runs the authorization-code + PKCE flow over a loopback redirect (RFC 6749/7636/8252), keeps tokens in owner-private credential files under the node's data directory, and refreshes them ahead of expiry; a host connection consumes a grant by id and asks the handle for a live access token. This is also the attenuation point for loaded connections: a bridge can hand a component a handle that authorizes requests without ever revealing the token. *Rejected:* a per-plugin OAuth implementation (every service connection would carry the same flow and its own token file format), and keeping refresh tokens in the kernel's `state` namespace (credentials never enter the store — [kernel](kernel.md)); the data directory gained a plugin-visible path (`ApplyCx::data_dir`) for exactly this class of file. A grant whose client variables are unset is mounted `MissingSecret` rather than failing the whole `oauth` fiber, so one unconfigured provider never parks the others — a deliberate, per-grant reading of the "missing secret parks" rule ([composition](composition.md)).

## Open questions

- The change-feed API behind the `change_feed` capability: the registry declares it today; the scheduling hook that turns a feed's hints into targeted sweeps is unbuilt ([index-maintenance](index-maintenance.md)).
- Projecting `connections` and `oauth` into WIT for loaded connection plugins (the transform seam crossed first; connections force long-running instances and streaming reads).
- Connection liveness/health model and how it feeds routing decisions.
- Whether node↔node connections are symmetric (either side may initiate sync/query) — working assumption: yes, capabilities permitting.
