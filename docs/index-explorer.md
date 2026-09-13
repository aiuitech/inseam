# Index explorer

Run the local app from the repository root:

```sh
cargo install --path crates/inseam-cli
npm ci --prefix apps/explorer
npm start --prefix apps/explorer
```

Open <http://127.0.0.1:7340>. Node 22 or later and Python 3 with SQLite support must be on PATH. Use the numeric loopback address, not `localhost`; the server checks the exact host and origin.

## Choose an index

Select **Open or create index** and choose a benchmark run, an existing index directory with its original composition, or a new empty index. Benchmark discovery reads `benchmarks/runs/<suite>/<run>/manifest.json`. Runs whose data directories are missing stay visible but cannot be opened.

Imports create consistent SQLite snapshots in `apps/explorer/.indexes/<id>/`. They need disk space equal to the database size. Searches and configuration changes affect the working copy. The original benchmark data and composition remain untouched. The current CLI may rebuild an incompatible catalog in the copy, so use a matching binary for historical indexes when preserving their shape matters.

Custom indexes need the composition used to build them. Imported copies do not include node identities, OAuth tokens or plugin files. Transport, routing, sync and Google access stay disabled. A custom plugin or host may need local resources restored before retrieval can run.

## Index sources

The Indexing view accepts a filesystem folder or host scope, optional host ID, maximum deep-indexed source count, catalog-only mode, rebuild and provider batch mode. Start a run and watch its log. Cancel stops the child process; a later sweep resumes from durable completion marks.

New indexes use full-text retrieval and extractive summaries without model calls. The composition editor exposes the actual TOML for transforms, embedding provider and dimensions, sweep concurrency, summary targets and model call budgets. Syntax is checked on save; the installed CLI validates plugin settings on use. Saved changes apply to subsequent operations. Change index-shape settings before indexing, or run indexing again to apply them. An endpoint-backed configuration uses environment credentials inherited when starting the explorer and can make paid model calls.

## Inspect and search

- **Index map** shows total source, fragment and relation counts, a MIME-type inventory, and a graph of relations between types. Select a type to read exact grouped relation counts.
- **Search lab** runs hybrid, full-text or vector seeds with editable result count, seeds per list, graph hops and damping. These overrides are temporary. A vector-only search on an index without vectors returns no results.
- Select a result to see seed ranks, fused seed scores, graph contributions and the three rollup weights. The formula reconstructs its score. Summaries and keywords can contribute even when they do not appear in scan hints. Individual PageRank paths are not recorded.
- **Source catalog** filters paths and exact content types across the whole index, 100 rows per page. Indexed and pending sources are both visible.
- Open a source to explore its fragments, typed relations and neighbors. Select a fragment to read its indexed text. Read a line range to inspect source content, up to 2,000 lines per request.

The map shows at most 128 types and 256 grouped relations. Source graphs show at most 200 fragments and 400 relations. Captions state those bounds. Large raw operation responses exceeding 16 MiB fail visibly instead of exhausting server memory.

## Runtime controls

`INSEAM_EXPLORER_PORT` changes port 7340. `INSEAM_EXPLORER_DATA` chooses the working library directory. `INSEAM_BINARY` chooses the executable, default `inseam`. The server always binds to `127.0.0.1`.

Only one operation runs at a time to avoid conflicting CLI store writers. Indexing and imports run for at most six hours; other subprocesses have a ten-minute limit. Jobs remain available while the server is running, including across browser reloads. Restarting the server clears the job record, but working indexes remain on disk.

Run the app tests with `npm test --prefix apps/explorer`. Retrieval evidence is covered by `cargo test -p inseam-plugins --test finder_graph`.
