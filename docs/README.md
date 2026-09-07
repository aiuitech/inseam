# Inseam Docs

This is the documentation for inseam. Docs describe what the code does today, in plain language. Keep files short, on the topic their filename names, and split big topics into smaller files. Folders group related topics and follow the same descriptive naming.

Docs are written for **incremental discovery**: a person or an agent should be able to scan the tree at a high level and click down to exactly the page they need, without loading one giant file that covers everything.

## Map

Start here:

- [get-started.md](get-started.md) — install a node, index something, query it, modify it.
- [cli.md](cli.md) — the `inseam` binary, command by command.
- [configuration.md](configuration.md) — the composition file, the node's only config.
- [releases.md](releases.md) — how a binary updates itself, and how a release is built, signed, and promoted.
- [benchmarking.md](benchmarking.md) — set up EnterpriseRAG-Bench, run it through the CLI, and record comparable results.
- [glossary.md](glossary.md) — the vocabulary, one page.

Then by area:

- [architecture/](architecture/README.md) — how the repo is laid out, the kernel, the macOS app, the hosted node, the MCP server, and doc generation.
- [indexing/](indexing/README.md) — how files become an index: the filesystem host, transforms, storage, and index upkeep.
- [finder/](finder/README.md) — how queries work: the ranking algorithm and the operations built on it.
- [network/](network/README.md) — how nodes find and reach each other: identity, the transport, the roster, sync, routing across nodes, and query fan-out.
- [plugins/](plugins/README.md) — the two plugin kinds, validation, the registry, and custom builds.
- [crates/](crates/inseam-kernel.md) — per-crate overviews, generated from each crate's doc block.
- [skills/](skills/inseam-loaded-plugin.md) — agent skills, generated from their `SKILL.md` sources.
- [brand/](brand/README.md) — the brand and its assets.

The *why* behind these shapes — the design decisions — lives in [design/](../design/README.md), deliberately outside this tree.
