# Inseam Docs

Inseam documentation lives here. Docs are to describe what is and act as concise serialization of the codebase in natural language. Keep files authored as markdown with well named descriptive filenames. They should be concise and stay on topic to the filename, breaking larger docs into multiple smaller topics. Nested folders are encouraged for grouping and follow the same descriptive naming of folders.

Docs are written for **incremental discovery** so that agents, and people, can scan at a high level, and drive themselves to the right documentation through the filesystem, without bloating the context window, or their brain's, with large files spanning multiple topics.

## Map

Start here:

- [get-started.md](get-started.md) — install a node, index something, query it, modify it.
- [cli.md](cli.md) — the `inseam` binary, command by command.
- [configuration.md](configuration.md) — composition: the node's one config surface.
- [glossary.md](glossary.md) — the vocabulary, one picture.

Then by area:

- [architecture/](architecture/README.md) — workspace layout, the kernel, the macOS app, doc generation.
- [indexing/](indexing/README.md) — filesystem host, transforms, storage, index maintenance.
- [finder/](finder/README.md) — query-time discovery: the algorithm and its operations.
- [plugins/](plugins/README.md) — both plugin tiers, validation, the registry, custom distributions.
- [crates/](crates/inseam-kernel.md) — per-crate overviews, generated from each crate's doc block.
- [skills/](skills/inseam-loaded-plugin.md) — agent skills, generated from their `SKILL.md` sources.
- [brand/](brand/README.md) — the brand and its assets.

Architectural *intent* — the decisions behind these shapes — lives in [design/](../design/README.md), deliberately outside this tree.
