# Index explorer

The explorer is a local developer app for comparing index shapes and inspecting retrieval. It lives beside the owner console because selecting arbitrary node directories and benchmark manifests is a workstation operation. The hosted owner API deliberately accepts only approved root IDs.

The browser talks to one loopback Node server. Every API request requires the exact loopback Host and Origin and a random token embedded in the same-origin page. The server invokes the installed CLI with argument arrays, never a shell. One operation owns the runner at a time; indexing and snapshot copies expose a bounded background job with logs and cancellation. The app keeps its own small npm install because it needs no console build pipeline.

Existing indexes become working copies through SQLite's online backup API. Copying the database files directly could miss WAL transactions. Opening the originals with the current CLI could converge search tables or rebuild an older catalog schema, so only copies are booted. The app copies the supplied composition but disables transport, routing, sync and Google access. It does not copy node identities, OAuth tokens or local plugin directories. Custom compositions may need adjustment when they depend on those resources. Source reads and explicitly configured model calls still use the workstation's filesystem and inherited environment.

The catalog database is read through a small Python helper opened in read-only mode. It owns fixed SQL for type counts, grouped relation counts and paginated source browsing. Mutations and retrieval go through the CLI. This read adapter depends on the documented catalog schema and fails visibly on incompatible layouts. It never migrates an original database.

## Graph and retrieval evidence

An index-wide fragment graph becomes unreadable at benchmark scale. The first view groups fragments by MIME type and relations by input type, kind and output type. Counts are exact, with 128 types and 256 grouped edges displayed. Source expansion is a separate bounded graph, with 200 nodes and 400 edges, plus text and relation lists. The graph shows stored direction; Finder's relevance walk uses undirected weighted edges.

The Finder emits the actual three contributing fragments for each returned local source. Each contribution includes one-based seed-list ranks, the fused seed score, the PageRank contribution and the source-rollup weight. Raw score and normalization accompany them. Hints cannot substitute for this evidence because they intentionally omit summary and keyword fragments. Evidence is attached to the local trace, so a remote merge must not interpret these numbers as its own final score. The explorer disables fan-out.

The evidence records contributions, not individual PageRank paths. Stored graph edges are not presented as proven query traversal paths. Search mode and graph controls are query-only composition overrides and never overwrite the saved composition.

## Performance sketch

Disk dominates imports: a 5 GB index requires about 5 GB of additional storage and one complete read and write. A local SSD at 500 MB/s needs roughly 20 seconds of aggregate transfer before SQLite and filesystem overhead. Snapshot work has a six-hour deadline and bounded page batches.

Overview aggregation scans fragments and relations once per explicit refresh, returning at most 384 grouped rows. Source browsing returns 100 entries per page. Path filtering and grouped counts can scan the catalog, so the helper has a VM instruction budget and a ten-minute subprocess deadline. It avoids sending millions of fragments to the browser. No background index-wide polling runs.

Query evidence adds at most three fragment records per returned source, normally 150 records for the CLI's 50-result limit. It reuses resident seed and graph scores and performs no extra database or network calls. The browser renders SVG directly, with no layout simulation or unbounded animation loop. Index jobs have a six-hour deadline, 16 MiB total process output, and a 64 KiB live log tail. Polling stops at job completion or after six hours.
