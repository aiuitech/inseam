# Indexing

How a node turns sources into a searchable index: [connections](connections.md) are how the node reaches its hosts — the [filesystem host](filesystem-host.md) is the first — and list what exists, [transforms](transforms.md) break sources into fragments, [storage](storage.md) holds the catalog, graph, and search tables, [maintenance](maintenance.md) covers the sweep that keeps the index matching both reality and the configuration, and [ignore](ignore.md) covers keeping sources out of the node entirely.
