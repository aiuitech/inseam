# Doc Generation

Parts of this docs tree are exported from source rather than authored by hand, so they cannot drift from the code. `cargo xtask docs` (the `xtask/` crate) regenerates them; every generated page opens with a `<!-- GENERATED … -->` comment naming its source — edit the source and regenerate, never the page.

Three exports:

- **Crate overviews** → `docs/crates/<name>.md`. The leading `//!` doc block of each workspace crate's `lib.rs` (or `main.rs`), verbatim. Write those blocks as the crate's doc page.
- **The WIT contract** → `docs/plugins/<world>-wit.md`. `crates/inseam-wasm-host/wit` rendered by wit-bindgen's markdown backend — the granular reference for loaded-plugin authors.
- **Skills** → `docs/skills/<name>.md`. Each `skills/*/SKILL.md` (agent-facing authoring guides, symlinked into `.claude/skills`), with its frontmatter folded into the title and byline.

Everything else in `docs/` is authored prose. Generated pages are committed like any other doc; rerun `cargo xtask docs` whenever a `//!` block, the WIT, or a skill changes.

The doc website (`apps/docs-website`, Astro Starlight, deployed to docs.inseam.io) consumes `docs/` as its content source — generated and authored pages alike. Its `scripts/sync-content.mjs` copies this tree into the site's content collection before every dev/build, lifting each page's `# ` heading into the frontmatter title Starlight needs — so files here stay plain markdown with no frontmatter and must open with a single `# ` title. Intra-docs links (`[x](kernel.md)`) are rewritten to site paths; links escaping `docs/` (e.g. into `design/`, which is intentionally not on the site) are rewritten to GitHub. `llms.txt`, `llms-full.txt`, and `llms-small.txt` are generated from the same tree by the `starlight-llms-txt` plugin.

The sidebar follows the tree with a thin layer of order: the site's `astro.config.mjs` pins the top-level sequence and gives each `docs/` folder its group label, while every group's contents autogenerate from the folder — adding a page to an existing folder needs no config change; adding a new top-level folder means one line in the config. Each folder's `README.md` is its section landing page, pinned first in the group as "Overview".
