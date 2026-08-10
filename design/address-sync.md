# Address Sync

How every node converges on the network-wide catalog of addresses + envelopes.

## The catalog

Each node keeps a local **catalog**: every address it knows about, with envelopes, host records, and attached trust properties. The catalog is the node's complete picture of what exists on the network — available offline, always local-first.

## Everyone gets everything

Sync is **full and unfiltered**: every node converges on the entire catalog. This is safe because the network is a single trust domain ([access-control](access-control.md)) — all nodes belong to the same owner, and joining the network is itself the grant. External requesters never participate in sync at all; they only see filtered query and fetch responses at the boundary.

This decision buys simplicity (no per-edge personalization of the sync stream) and full offline discovery on every node. It would need revisiting only if federated networks — nodes with different owners — ever enter scope.

One tension acknowledged: "content never moves" does not mean "nothing content-derived moves." Envelopes carry titles, discovery hints, and trust properties (correspondent emails, filenames), and full sync places all of that on every node — including a hosted one. Within a single trust domain this is by design, but the privacy claim should be stated precisely: content stays on hosts; envelope metadata replicates everywhere. Whether a host can opt sources down to minimal envelopes (address + type, no hints) is an open question below.

## Propagation

- Catalog changes replicate across node↔node [connections](connections.md); nodes re-share what they learn, so knowledge crosses the network transitively even between nodes that never connect directly.
- Consistency is **eventual**. An offline node serves discovery from its last-known catalog and reconciles on reconnect.
- **Stewards originate entries** for the hosts they serve: the steward's connection plugin enumerates the host, emits address + envelope records, and publishes updates (new, changed, deleted) into sync.
- **Origin wins**: the authoritative record for an address is whatever its steward last published. Catalog entries elsewhere are observations, not claims, so conflicts reduce to "latest from origin."

## Deletion

Removals sync as tombstones; a source that disappears from a host must disappear from every catalog, not linger as a dead address.

## Open questions

- Sync mechanism: gossip vs. log replication vs. CRDT-style merge (deletion + partial-order needs point toward a per-origin log with vector clocks).
- Catalog scale ceilings on small devices, and whether a low-power node may opt into a partial catalog (a local capacity choice, not an access rule).
- Minimal envelopes: whether a host or source can be marked to sync address + type only, withholding hints and property detail from replication at the cost of discovery quality elsewhere.
