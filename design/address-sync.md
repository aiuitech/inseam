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

The mechanism is **deliberately owned code, not an off-the-shelf sync engine**. Automerge, yrs, and iroh-docs are multi-writer CRDTs solving concurrent-edit merge — a problem origin-wins designed out of existence — and adopting one would embed its merge semantics inside the catalog while moving the log out of the kernel's own store ([kernel](kernel.md)). What remains is a small, proven pattern (per-author signed logs à la Secure Scuttlebutt; Kafka-style log shipping), cheaper to implement directly than to adapt. The [transport underneath is adopted, not owned](connections.md) — iroh carries the bytes; this layer decides what they mean.

## Deletion

Removals sync as tombstones; a source that disappears from a host must disappear from every catalog, not linger as a dead address.

## Settled since

- **The log lives in the kernel store beside the catalog, and the store never learns the node's identity.** One libSQL file holds the catalog and its log, so a catalog write and its log entry commit in one transaction and a peer's batch applies in one; a second store would have been two ways to lose an entry. The local log is filed under an empty origin, because identity — the keypair — is minted and kept above the kernel by the `node` plugin, beside the credential files, and the kernel's rule that credentials and identity never enter the store holds. Every read and apply takes the local id as a parameter, and the sync seam maps the empty origin to it at the boundary; a remote origin is always 64 hex characters, so the two never collide.
- **A log is named by its origin and an epoch.** A rebuilt store — a schema-version bump, a replaced data directory — starts a fresh log from sequence one, and without an epoch every peer holding the old log's higher sequences would ignore the new one forever. So the store mints an epoch when it creates its log and again on every rebuild (the clock in nanoseconds, or the superseded epoch plus one if the clock went backwards), and a higher epoch from one origin supersedes the lower wholesale: the peer purges what it held from that origin and takes the new log from the start. *Rejected:* keeping the sequence counter outside the store to survive a rebuild (the very files a rebuild replaces).
- **Compaction happens on append, and tombstones stay until overwritten.** Appending an entry deletes every earlier entry under the same key from the same origin and epoch — locally, and when a peer's entry is applied — so a log holds one live entry per key and never grows with churn. A tombstone is such an entry: it stays until a later entry under its key replaces it, so a removal keeps reaching nodes that have not seen it, and a resurrected source overwrites the tombstone rather than racing it. This is the answer to the compaction question: nothing is ever squashed, because there is nothing to squash, and a node holding an older vector still receives exactly the entries that superseded what it missed, since sequence numbers survive compaction and a vector is a high-water mark, not a list of what was seen. *Rejected:* a separate compaction pass (a second writer over the log, and a window in which a squashed log could not answer an older vector).
- **Entries are not signed.** The transport authenticates both ends of every exchange with the peers' keys, and the network is one trust domain, so a node re-sharing another's log is trusted to relay it unchanged — the same trust that lets it serve the other's content. What origin-wins needs is enforced at apply instead: a node record from anyone but the node it describes, or a stewardship from anyone but the steward, is held (so the vector stays honest) but never materialized. Signatures would buy protection against a member node that is trusted anyway, at the cost of a key ceremony in every entry; revisit only if federation — nodes with different owners — enters scope.
- **One round trip moves knowledge both ways.** The request carries the requester's vector and the entries it knows the responder lacks; the response carries the responder's vector afterwards and the entries the requester lacked. The first round ships nothing but a vector, every later round ships suffixes both ways, and an exchange ends when neither side has news, when the responder stops making progress, or at a fixed round bound — a peer far behind catches up over successive scheduled rounds rather than one exchange that never ends. Batches are bounded in entries and refused above the bound before anything applies.
- **The roster is materialized last-applied-wins, and a stewarded row is never overwritten from outside.** The roster tables hold the latest entry per key, each row stamped with the entry that produced it, so a purge by origin and a rebuild from the log both have what they need. The catalog gets one more rule: a peer's copy of a source updates only a row that is itself remote, and a remote tombstone deletes only its own origin's row, so what this node stewards is always what its own connection last saw; a local write over a remote row takes it over and logs it. Local writes log only on change — a new row, a takeover, a changed size, or an envelope that changed in anything but `observed` — because the sweep re-upserts every source it sees each run, and only real change may reach the peers.

## Open questions

- Catalog scale ceilings on small devices, and whether a low-power node may opt into a partial catalog (a local capacity choice, not an access rule).
- Minimal envelopes: whether a host or source can be marked to sync address + type only, withholding hints and property detail from replication at the cost of discovery quality elsewhere.
