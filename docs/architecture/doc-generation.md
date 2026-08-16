# Doc Generation

Parts of this docs tree are exported from source rather than written by hand, so they can't drift from the code. `cargo xtask docs` (the `xtask/` crate) regenerates them. Every generated page opens with a `<!-- GENERATED … -->` comment naming its source — edit the source and regenerate, never the page.

Three exports:

- **Crate overviews** → `docs/crates/<name>.md`. The leading `//!` doc block of each workspace crate's `lib.rs` (or `main.rs`), copied verbatim. Write those blocks as the crate's doc page.
- **The WIT contract** → `docs/plugins/<world>-wit.md`. `crates/inseam-wasm-host/wit` rendered by wit-bindgen's markdown backend — the detailed reference for loaded-plugin authors. The backend puts a raw HTML anchor in its page title (`# <a id="…"></a>World <name>`), which the site would copy into the page title as-is, so the generator replaces it with a plain `# WIT world: <name>`; anchors further down stay, because the type cross-links point at them.
- **Skills** → `docs/skills/<name>.md`. Each `skills/*/SKILL.md` (agent-facing authoring guides, symlinked into `.claude/skills`), with its frontmatter folded into the title and byline.

Everything else in `docs/` is written by hand. Generated pages are committed like any other doc; rerun `cargo xtask docs` whenever a `//!` block, the WIT, or a skill changes.

The doc website (`apps/docs-website`, Astro Starlight, deployed to docs.inseam.io) uses `docs/` as its content source — generated and hand-written pages alike. Its `scripts/sync-content.mjs` copies this tree into the site before every dev/build, lifting each page's `# ` heading into the title Starlight needs — so files here stay plain markdown with no frontmatter and must open with a single `# ` title. Links between docs (`[x](kernel.md)`) are rewritten to site paths; links leaving `docs/` (e.g. into `design/`, which is deliberately not on the site) are rewritten to GitHub. `llms.txt`, `llms-full.txt`, and `llms-small.txt` are generated from the same tree by the `starlight-llms-txt` plugin.

The sidebar follows the folder tree with a thin layer of ordering: the site's `astro.config.mjs` pins the top-level sequence and gives each `docs/` folder its group label, while each group's contents come straight from the folder — adding a page to an existing folder needs no config change; adding a new top-level folder is one line in the config. Each folder's `README.md` is its section landing page, pinned first in the group as "Overview".
