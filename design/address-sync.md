# Address Sync

How every node converges on the network-wide catalog of addresses + envelopes. The same machinery carries the [roster](roster.md) — node, host, and stewardship records are just additional record kinds in the stream; one replication mechanism, one convergence story.

## The catalog

Each node keeps a local **catalog**: every address it knows about, with envelopes, host records, and attached trust properties. The catalog is the node's complete picture of what exists on the network — available offline, always local-first.

## Everyone gets everything

Sync is **full and unfiltered**: every node converges on the entire catalog. This is safe because the network is a single trust domain ([access-control](access-control.md)) — all nodes belong to the same owner, and joining the network is itself the grant. External requesters never participate in sync at all; they only see filtered query and fetch responses at the boundary.

This decision buys simplicity (no per-edge personalization of the sync stream) and full offline discovery on every node. It would need revisiting only if federated networks — nodes with different owners — ever enter scope.

One tension acknowledged: "content never moves" does not mean "nothing content-derived moves." Envelopes carry titles, discovery hints, and trust properties (correspondent emails, filenames), and full sync places all of that on every node — including a hosted one. Within a single trust domain this is by design, but the privacy claim should be stated precisely: content stays on hosts; envelope metadata replicates everywhere. Whether a host can opt sources down to minimal envelopes (address + type, no hints) is an open question below.

## Propagation

- Catalog changes replicate across node↔node [connections](connections.md); nodes re-share what they learn, so knowledge crosses the network transitively even between nodes that never connect directly.
- Consistency is **eventual**. An offline node serves discovery from its last-known catalog and reconciles on reconnect.
- **Stewards originate entries** for the hosts they serve: the steward's connection plugin enumerates the host, emits address + envelope records, and publishes updates (new, changed, deleted) into sync. Likewise each node originates its own [roster](roster.md) records.
- **Origin wins**: the authoritative record for any key is whatever its origin last published — the steward for an address, the node itself for its node record. Entries elsewhere are observations, not claims, so conflicts reduce to "latest from origin."

## Mechanism

**Per-origin append-only logs with version vectors.** Every record a node originates goes into its own log with a monotonic sequence number. Sync between two nodes is an exchange of version vectors (highest sequence seen per origin) followed by shipping the missing suffixes — including logs of origins neither end has met directly, which is what makes propagation transitive. Within one origin's log, later overwrites earlier per key; deletions are tombstone entries like any other. This shape was chosen over pure gossip (no convergence proof per key) and CRDT merge (origin-wins makes general merge unnecessary — there is exactly one writer per key).

## Deletion

Removals sync as tombstones; a source that disappears from a host must disappear from every catalog, not linger as a dead address.

## Open questions

- Log compaction: how long tombstones and overwritten entries live before a log is squashed, and how a squashed log resyncs to a node holding an older vector.
- Catalog scale ceilings on small devices, and whether a low-power node may opt into a partial catalog (a local capacity choice, not an access rule).
- Minimal envelopes: whether a host or source can be marked to sync address + type only, withholding hints and property detail from replication at the cost of discovery quality elsewhere.
