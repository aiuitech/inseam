# Product

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

Web surfaces (www.inseam.io marketing site, docs.inseam.io documentation site) plus a native macOS app (`apps/macos`). Each keeps its platform's design language; the brand carries across all of them.

## Users

Developers and tech-forward users, in two postures — ranked (confirmed 2026-08):

1. **Agent-wirers (primary):** developers connecting their own agents to their own context — email, files, chats, tickets, wherever the data already lives — wanting one permission-aware discovery surface instead of a bag of per-service integrations.
2. **Builders (secondary):** people building products on the network boundary — e.g. a support widget retrieving a visitor's own correspondence, an app reading a customer's records where they live.

## Product Purpose

inseam indexes data where it sits (a laptop's filesystem, a Gmail account, a service's API) and gives agents, apps, and people one way to find and fetch all of it. It stores addresses and a discovery index, never the content — the network syncs *where data is*; source material is fetched on demand from where it already lives. Success: an agent can climb the discovery ladder (`query` → `expand`/`scan` → `fetch`) across every source a user has, from any device, without content leaving its host.

## Positioning

- **Content never moves.** Only addresses and envelopes (small metadata records) replicate. Sync-everything competitors copy content into another silo you must trust and pay for; inseam cannot, by architecture.
- **Plugins all the way down.** The kernel is tiny — it runs plugins and owns the store. Every capability is a plugin on a typed service seam composed by `composition.toml`. Sandboxed WASM plugins are safe by construction (attenuated capabilities, fuel limits, conformance harness, release cooldown), so the connector long tail is written by the community and by users' own agents — "all your context" is not capped by a vendor's connector roadmap. The agent using inseam is the agent extending inseam.
- **Cross-source discovery, not per-service access.** One index that ranks an email against a file against a call transcript; the ladder returns progressively closer views instead of twenty chunks up front.
- **Yours, not hosted.** Open core; every node is the user's. The commercial offering is a hosted cloud node with no special *powers* — same kernel, same seams, nothing a self-hosted node cannot reach — selling index quality, uptime, and inseam-authored plugins we don't publish. Say both halves: no privileged platform, and yes, reserved plugins. Never imply the hosted node runs only published code.

## Operating Context

- Lives in terminals and agent harnesses: installed via `cargo install` / `install.sh`, driven as `inseam query` by a person or `inseam agent` by a live LLM.
- Local-first: offline a node discovers against its own index; connected, it fans out to nodes with stronger indexes (a phone keeps a cheap envelope-only index and borrows quality from a bigger node — the asymmetry is deliberate).
- Agents author plugins against the `skills/inseam-loaded-plugin` skill and validate with `inseam plugin check`; the first shipped plugin (`plugins/ocr`) was agent-written.
- Repo workflow: design intent in `design/`, docs in `docs/`, both kept current.

## Capabilities and Constraints

- **Real today:** kernel, both plugin tiers (compiled-in and sandboxed WASM), filesystem connection, transforms, embeddings, Finder, sweep, node API, the discovery ladder, CLI, plugin authoring + conformance harness, registry model with hash-verified installs.
- **Designed, not built:** multi-node sync; the external boundary (verified-property exposure, e.g. proving `email:user@example.com` to discover/fetch exactly the sources carrying that property — no user database anywhere); the hosted service (`design/hosted-service.md`) — one VM per tenant, a control plane that cannot read the nodes, metered by index volume.
- **Status is early and must be described as early.** Surfaces must not claim sync or the boundary as shipped.
- Terminology is fixed: source, host, node, envelope, seam, composition, the ladder (`query`/`expand`/`scan`/`fetch`), loaded vs compiled plugin tiers. Use it; don't invent parallel vocabulary.
- Stack (existing): Rust workspace for the product; Astro 7 + Cloudflare Workers (wrangler) for www.inseam.io; docs site at `apps/docs.inseam.io`; native macOS app at `apps/macos`.

## Brand Commitments

`docs/brand/README.md` is **binding** for all surfaces (confirmed 2026-08):

- Colors: ground `#0a0b0a` (near-black, faintly green), thread `#d8ff1c` (chartreuse) for dashes/accents/links, node `#f2f0e9` off-white for dots/body (pure `#ffffff` below ~32px). Dark-first; rare light surfaces invert with ground as ink, thread staying `#d8ff1c` on white.
- Mark: dash-dash-dot stitch (`●▬▬●▬▬●`), degrades gracefully to plain UTF-8. Mark and stitch-strip SVGs in `packages/brand/assets/`; canonical geometry in the brand doc. Always one horizontal line — a seam, never stacked.
- Type: monospace everywhere — headings, body, UI. Lowercase `inseam` wordmark, no custom lettering, no italics, regular/bold only.
- Voice: plain, lowercase-leaning, declarative. Say what it does. No exclamation points, no marketing superlatives. Should look at home in a terminal.

## Evidence on Hand

- Full README narrative and positioning (`README.md`).
- Brand assets: `packages/brand/assets/mark.svg`, `packages/brand/assets/stitch.svg`, favicons, macOS icon, PNG exports; build script `packages/brand/assets/build-icons.sh`.
- Working software: the CLI, kernel, plugins, and the agent-authored `plugins/ocr` as a live proof of the AI-authorship claim.
- Docs tree (`docs/`) and design-intent tree (`design/`).
- **Absent — do not fabricate:** testimonials, customers, case studies, benchmarks, pricing, press. No hosted node exists yet to point at.

## Product Principles

1. **Honesty is architectural.** Claims ("content never moves", "no user database") are backed by mechanism; surfaces should explain the mechanism, not assert the benefit.
2. **Early, and said so.** Never present designed-but-unbuilt capability as shipped.
3. **Terminal-native.** The product, brand, and voice belong to people who live in terminals; everything should feel at home there.
4. **The agent is the audience too.** Docs and surfaces are read by agents as much as people — incremental discovery, concise files, stable vocabulary.
5. **Plugins are the story.** Safety, flexibility, and AI-authorship of plugins back every value claim; keep that causal chain intact in messaging.
