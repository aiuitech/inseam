# The Roster

The network's synchronized self-description: which nodes exist, which hosts exist, who stewards what, and how nodes can be reached. The [catalog](address-sync.md) answers *what data exists*; the roster answers *who is here and how do we reach them*. Both replicate to every node through the same [sync machinery](address-sync.md).

## Why it must sync

An [address](addressing.md) is pure identity — resolving one requires knowledge that lives outside the address: which node stewards the host, and how to dial that node. That knowledge changes out from under the network — IPs rotate, stewards come and go, machines are renamed — and a node holding a stale view can't reach data that is, in fact, available. So the network's self-knowledge is treated exactly like the address catalog: originated by the authoritative node, replicated everywhere, converged eventually.

## Three record kinds

**Node record** — authored by the node it describes; the node is origin and sole authority.

- **Node id: the node's public key** (fingerprint). Identity *is* the key — it survives IP rotation, re-homing, and reinstalls that keep the key. This resolves the node-identity question in [connections](connections.md); node↔node authentication is mutual proof of roster keys, and the id doubles as the dial target for the iroh transport.
- Display name.
- **Endpoints**: dialing hints for the transport — for iroh, the node's relay URL and last-known direct addresses ([connections](connections.md)). May be empty: an **outbound-only node** (laptop behind NAT, phone) participates by dialing others and is never dialed.
- **Capabilities**: always-on, index quality/depth (how [discovery](discovery.md) fan-out picks targets), willingness to relay fetches.

**Host record** — authored by any steward of the host.

- **Host id**: the opaque stable id ([addressing](addressing.md) defines the derivation).
- **Kind**: the locator-schema family (`fs`, `gmail`, …).
- Display name ("Greg's Mac mini") — presentation lives here, never in addresses.

Two stewards of the same host derive the same host id independently, so their records collide by design and merge by latest. This is what makes one Gmail account stewarded by two nodes *one* host.

**Stewardship record** — authored by the steward: a (node id, host id) claim plus the connection's published capabilities — change feed, writability, enumeration support. Credentials never appear; they stay local to the steward ([connections](connections.md)). A steward withdrawing publishes a tombstone; a host with no live steward is unreachable but still known.

## Durable facts, not liveness

The roster syncs slowly-changing truth. Who is online *right now* is not a record — it's a session, observed locally by whoever holds the connection. **Endpoints are global; sessions are local.** Routing dials candidates from roster endpoints and learns liveness by trying; it never consults a synced "online" bit, because such a bit is stale the moment it replicates.

## Rotation, and the always-on backbone

When a node's endpoint changes, it bumps its own node record and publishes it over whatever connection it can make — outbound dialing still works even when the node's inbound address just died. The update then spreads transitively like any catalog change.

The failure mode is a network of *only* unstable nodes: two laptops that both rotated can hold each other's stale endpoints forever. Hence the strong recommendation — a convention, not architecture: **every network should include at least one always-on node with a stable (DNS-named) endpoint.** Unstable nodes keep a standing outbound connection to it; roster updates flow through it; outbound-only nodes reach the network by dialing it; and it runs the network's own iroh relay ([connections](connections.md)), so no traffic ever depends on public relay infrastructure. Per [network](network.md), it remains an ordinary node — well-provisioned, never privileged.

## Paths not taken

- **Synced presence/heartbeats.** Replicating "last seen online" network-wide is chatty and inherently stale; liveness is a session property, kept local.
- **A separate sync channel for the roster.** One replication mechanism carries catalog and roster as different record kinds; two sync protocols would be two ways to be subtly inconsistent.
- **A central registry or rendezvous service.** The backbone node is a convention the owner adopts, not infrastructure the architecture requires; nothing breaks if it's absent, only convergence latency suffers.

## Open questions

- **Admission and revocation ceremony**: invite format (endpoint + key fingerprint pinning is the working assumption), and how a lost/compromised node's record is expelled.
- **Endpoint privacy**: the roster reveals every node's addresses to the whole network — fine inside one trust domain, but worth stating; possibly LAN endpoints sync scoped.

NAT traversal between two outbound-only nodes, previously open here, is resolved by the transport: both hole punching and relayed fallback are iroh's job, through the backbone-hosted relay ([connections](connections.md)).
