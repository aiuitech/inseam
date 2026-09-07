# Workspace Layout

The repo is a Cargo workspace that mirrors the architecture ([design/kernel.md](../../design/kernel.md), [design/runtime.md](../../design/runtime.md)): a kernel, the seam definitions, plugins, and distributions.

- `crates/inseam-kernel` — the kernel: the plugin **substrate** (`substrate/`: services, plugins/fibers, effects, events, composition + reconciler) and the **store** (`store.rs`, `state.rs`), plus only the data-model types the store persists (`address.rs`, `fragment.rs`). Knows nothing about hosts, formats, ranking, transports, ignore rules, or dates.
- `crates/inseam-seams` — the seam catalog as code ([design/services.md](../../design/services.md)): one module per seam (`connection`, `transforms`, `embedder`, `llm`, `finder`, `sweep`, `operations`), each holding the trait, its well-known key, its types, and its capability-fact names. Definitions live here, separate from every provider and consumer. Beside them, the two conventions every plugin speaking the seams shares: `text.rs` (previews, the `scan` line arithmetic, which content types are read as text) and `dates.rs` (`YYYY-MM-DD` ↔ epoch).
- `crates/inseam-plugins` — the first-party **linked plugins** ([plugins/linked.md](../plugins/linked.md)): one directory per plugin, named for its composition name (`transform_markdown/` is `transform-markdown`), holding its `mod.rs`, any pure helpers, and — for transforms — its `<registration>.checks.toml`. `lib.rs` only lists the modules and `factories()`; nothing non-plugin lives here.
- `crates/inseam-conformance` — the linked tier's validation harness as a reusable crate ([plugins/validation.md](../plugins/validation.md)): the factory sweep and the hostile-input transform battery, shared by the workspace test suite and any custom distribution ([plugins/distributions.md](../plugins/distributions.md)).
- `crates/inseam-wasm-host` — the bridge that runs **loaded plugins** ([plugins/loaded.md](../plugins/loaded.md)): the WIT contract (`wit/transform.wit`), the wasmtime bridge, and the validation harness ([plugins/validation.md](../plugins/validation.md)) behind `inseam plugin check` and install-time admission.
- `crates/inseam-http` — the authenticated HTTP/JSON owner transport: bounded request handling, signed browser sessions, approved index-root resolution, and optional static Vite serving. It calls only the `operations` seam. See [hosted-node.md](hosted-node.md).
- `crates/inseam-cli` — the CLI as a library plus the stock `inseam` binary: a **distribution** — base composition + clap parsing + printing. No node logic; every command is a call on the `operations` seam, and the `inseam agent` demo loop (`agent.rs`) is a consumer of `operations` + `llm`. Custom distributions call `inseam_cli::run` with their own factories ([plugins/distributions.md](../plugins/distributions.md)).
- `crates/inseam-ffi` — a C ABI static library for native app shells; header at `include/inseam_ffi.h`. See [macos-app.md](macos-app.md).
- `plugins/` — loaded plugin projects (standalone crates, **not** workspace members; they build to `wasm32-wasip2`) **and registry v0** ([plugins/registry.md](../plugins/registry.md)): the `registry.toml` index, `advisories.toml`, and each plugin's golden checks and fixtures (`plugins/README.md`). `plugins/ocr` is the reference example.
- `apps/macos` — the SwiftUI macOS app (SwiftPM, not a cargo member). See [macos-app.md](macos-app.md).
- `apps/web` — the shared React, TypeScript, Vite, and shadcn owner console for local and hosted nodes. See [hosted-node.md](hosted-node.md).
- `packages/brand` — the `@inseam/brand` package: the brand's theme, assets, and components for `apps/web` and for the hosted console in the `inseam-console` repo, which fetches it from git. See [brand/package.md](../brand/package.md).
- `deploy/hosted` — the container, offline composition, and Compose example that package one node with the web console and a persistent data volume.
- `apps/docs-website` — the docs.inseam.io site (Astro Starlight on Cloudflare Workers, pnpm, not a cargo member); content synced from `docs/` ([doc-generation.md](doc-generation.md)).
- `xtask/` — repo automation; `cargo xtask docs` regenerates the doc pages exported from source ([doc-generation.md](doc-generation.md)).
- `skills/` — agent skills (symlinked into `.claude/skills`); exported into [docs/skills/](skills/) by the doc generator.

Shared dependency versions live in `[workspace.dependencies]` in the root `Cargo.toml`; member crates reference them with `dep.workspace = true`.

The root is also a pnpm workspace (`pnpm-workspace.yaml`) holding `apps/web` and `packages/brand`, with one lockfile at the root. The Astro sites under `apps/` keep their own `pnpm-workspace.yaml` and lockfile, so pnpm treats each as its own root.
