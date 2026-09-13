# Inseam index explorer

A local web workspace for opening benchmark indexes, creating new indexes, inspecting typed relations, and tracing retrieval scores.

```sh
# From the repository root:
cargo install --path crates/inseam-cli
npm ci --prefix apps/explorer
npm start --prefix apps/explorer
```

Open http://127.0.0.1:7340. Requires Node 22+, Python 3 with SQLite, and the installed Inseam CLI.

See [the user guide](../../docs/index-explorer.md) for controls, limits and environment variables, and [the design](../../design/index-explorer.md) for isolation and performance decisions.

```sh
npm test --prefix apps/explorer
```
