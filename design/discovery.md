# Discovery

Discovery is how anything — an agent, an app, a person — finds the addresses worth fetching. It is the second half of inseam's job: [addressing](addressing.md) makes everything reachable; discovery makes it findable.

## The index

Every node maintains a **local search index** over its catalog (and over content it is entitled to fetch). The index is a hybrid full-text + vector **semantic graph**, built by decomposing sources into related fragments — [indexing](indexing.md) covers how it's built, and the [Finder](finder.md) covers how it's queried.

The index is derived data: rebuildable at any time from the catalog + fetches, never a source of truth.

## Per-node configuration

Index quality is a *local* decision, configured per node via its [index profile](indexing.md):

- A phone builds a small, cheap index over envelope hints only — enough to work offline.
- A home server or our hosted service builds a large, high-quality index, fetching full content (through the normal access-controlled fetch path) and running better models.

This asymmetry is the point: underpowered devices stay functional alone, and borrow quality when connected.

## Query path

1. Query the local index. Offline, this is the whole story.
2. If online and configured to, fan the query out to connected nodes that advertise stronger indexes (typically the big home/hosted node).
3. Merge and rank; return **addresses + envelope metadata**, never content.
4. The caller fetches full sources progressively, only when a result earns it.

Queries between nodes are unfiltered — the network is a single trust domain. Queries made on behalf of an **external requester** are filtered by the requester's verified properties ([access-control](access-control.md)) before results are returned: boundary discovery must never reveal addresses the requester couldn't fetch.

## Indexing and the boundary

Within the network, a node may fetch and deep-index anything — indexing is an ordinary consumer of the fetch path. Authorization stays at the source level: index fragments carry no access rules of their own, and boundary-time filtering resolves each result against its source's trust properties ([access-control](access-control.md)).

## Paths not taken

- **One shared/global index.** Rejected: contradicts per-device configurability and offline operation, and concentrates content-derived data in one place.
- **Index shipping** (big node builds, small nodes download shards). Not a core concept; could appear later as a plugin-level optimization if envelope-only local indexes prove too weak.

## Open questions

- How nodes advertise index capability/quality to peers.
- Embedding model choice and versioning across heterogeneous nodes.
