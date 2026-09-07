# Discovery

Discovery is how anything — an agent, an app, a person — finds the addresses worth fetching. It is the second half of inseam's job: [addressing](addressing.md) makes everything reachable; discovery makes it findable.

## The index

Every node maintains a **local search index** over its catalog (and over content it is entitled to fetch). The index is a hybrid full-text + vector **semantic graph**, built by decomposing sources into related fragments — [indexing](indexing.md) covers how it's built, and the [Finder](finder.md) covers how it's queried.

The index is derived data: rebuildable at any time from the catalog + fetches, never a source of truth.

## Per-node configuration

Index quality is a *local* decision, configured per node via its [composition](composition.md) ([indexing](indexing.md)):

- A phone builds a small, cheap index over envelope hints only — enough to work offline.
- A home server or our hosted service builds a large, high-quality index, fetching full content (through the normal access-controlled fetch path) and running better models.

This asymmetry is the point: underpowered devices stay functional alone, and borrow quality when connected.

## Query path

1. Query the local index. Offline, this is the whole story.
2. If online and configured to, fan the query out to nodes whose [roster](roster.md) node records advertise stronger indexes (typically the big home/hosted node).
3. Merge and rank; return **addresses + envelope metadata**, never content.
4. The caller fetches full sources progressively, only when a result earns it.

Queries between nodes are unfiltered — the network is a single trust domain. Queries made on behalf of an **external requester** are filtered by the requester's verified properties ([access-control](access-control.md)) before results are returned: boundary discovery must never reveal addresses the requester couldn't fetch.

## Indexing and the boundary

Within the network, a node may fetch and deep-index anything — indexing is an ordinary consumer of the fetch path. Authorization stays at the source level: index fragments carry no access rules of their own, and boundary-time filtering resolves each result against its source's trust properties ([access-control](access-control.md)).

## Paths not taken

- **One shared/global index.** Rejected: contradicts per-device configurability and offline operation, and concentrates content-derived data in one place.
- **Index shipping** (big node builds, small nodes download shards). Not a core concept; could appear later as a plugin-level optimization if envelope-only local indexes prove too weak.

## Settled since

- **Fan-out targets are the `deep_index` nodes, nearest first.** A query goes, in parallel, to every other roster node advertising `deep_index` — nodes with a live session first (known reachable right now), then `always_on`, then roster order — cut to a configured count with a hard ceiling of eight, each under its own timeout enforced twice. A query is never relayed: a relay would answer from its own index and be counted twice. A node that times out or refuses is reported beside the results (`meta.remote`) and never fails the query; with fan-out off, nobody is dialed. *Rejected:* fanning out to every node (a phone's envelope-only index adds latency and no signal).
- **Cross-node merge is rank fusion; `via` names the index.** Scores from differently profiled indexes do not compare, so the local list and each remote list fuse by reciprocal rank with the finder's own `k`; one copy per address survives (the local copy when this node ranked it, else the best-ranked remote copy, with its summary and hints); digest-equal copies collapse into replicas exactly as one node's results do; and the top result renormalizes to 1.0. With no remote results the local list stands untouched, so a node with no network answers as it always has. Every remote result carries `via`, the node whose index produced it, because the next rung — expand — is served from that index, and an owner reading a listing should see which node answered ([finder](finder.md)).
- **The index-quality vocabulary is one bit.** `deep_index` is what "stronger index" means today: a node either indexes content or holds envelopes only. Nothing in a personal network compares two deep indexes, so a graded score would be a guess nobody consumes; the bit is a fact a node can be honest about. A finer vocabulary waits for a network with two deep nodes worth telling apart.

## Open questions

- **Remote rows are not in the local index.** A catalog row learned from a peer's log is envelope-only and outside this node's search index, so a query finds remote content only through fan-out, and an offline node finds nothing beyond what it indexed itself even though its catalog lists everything. An envelope-only local index over remote rows — title and hints, no fetch — is the next step; the `deep_index` bit already names the distinction it would sit under.
- Embedding model choice and versioning across heterogeneous nodes.
