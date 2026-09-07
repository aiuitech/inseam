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

## Settled since

- **Capabilities are three explicit bits.** A node record advertises `always_on` (the backbone intent: reachable at a stable endpoint at all times), `deep_index` (it indexes the content of the hosts it stewards, so a fanned-out query answers from content), and `relays` (it forwards routed requests for hosts it does not steward). Every field is stated on the wire, never defaulted, so a node's whole contract is one record. Sync rounds and fan-outs order `always_on` nodes first; fan-out targets only `deep_index` nodes; a relay refuses to forward when `relays` is off. *Rejected:* a graded index-quality score — nothing compares two deep indexes yet, and a bit a node can be honest about beats a number it would guess ([discovery](discovery.md)).
- **Admission is roster membership.** The roster is the transport's admission policy: a peer is admitted when its node record is held and it is not expelled, refused otherwise, and the transport refuses everyone until the roster installs the policy. Membership needs no second list — being in the roster *is* being admitted — and the policy answers synchronously from a view refreshed on every roster change, because it runs on the accept path.
- **An invitation is a one-time token carried in the handshake, with the inviter's endpoints and key.** The join ceremony the open question asked for is one string: `inseam-invite:<base64url JSON>` holding the inviting node's id (the key pinning the working assumption wanted), its current endpoints, a 256-bit token, and an expiry a day out. The owner carries it to the new node by hand; the new node dials the inviter, presents the token as its `hello`, and syncs once, after which it is a roster member and the token is spent. Tokens live in memory only — an invitation is a short-lived ceremony, not a fact the roster should carry or replicate — so a restart forgets them, at most 32 are open at once, and a token is compared in constant time and never logged. *Rejected:* a shared network secret (one leak admits anyone forever), and pairing by fingerprint alone (it pins the inviter but gives the inviter no reason to admit the stranger).
- **Expulsion is a record any node may author.** Revocation is a log entry, `expulsion:<node>`, and it is the one record kind an origin publishes about another node, because the owner runs it on whichever node is at hand, never on the lost one. Every node that applies it purges the expelled node's log, the catalog rows it stewarded, and the roster rows it produced or that describe it, and refuses the key from then on — checked before any other admission rule, so a stale record can never readmit it. Expulsions the expelled node had itself authored survive the purge, or losing that node would readmit what the owner already expelled. There is no un-expel: an expelled device comes back as a new key through a fresh invitation. *Rejected:* a revocation signed by the expelled key (the lost device is exactly the one that cannot sign).
- **Rotation is a poll, not a hook.** The roster reconciler compares the transport's current endpoints with the last published node record on every pass — woken by connection changes and by a timer, thirty seconds by default — and republishes when they differ. The poll costs one comparison and reads the transport's own view, which is the truth about how the node can be reached; a change notification from the transport would have been a second path to the same answer. The relay coming up after boot, a laptop changing networks, and a forwarded port appearing all reach the network the same way, over the next sync exchange on whatever connection still works.

## Open questions

- **Endpoint privacy**: the roster reveals every node's addresses to the whole network, and every stewardship record carries the steward's configured folder paths — fine inside one trust domain, but worth stating; possibly LAN endpoints sync scoped.

NAT traversal between two outbound-only nodes, previously open here, is resolved by the transport: both hole punching and relayed fallback are iroh's job, through the backbone-hosted relay ([connections](connections.md)).
