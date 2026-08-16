# Workspace Layout

The repo is a Cargo workspace mirroring the architecture ([design/kernel.md](../design/kernel.md), [design/runtime.md](../design/runtime.md)): a kernel, the seam definitions, plugins, and distributions.

- `crates/inseam-kernel` — the kernel: the plugin **substrate** (`substrate/`: services, plugins/fibers, effects, events, composition + reconciler) and the **store** (`store.rs`, `state.rs`), plus the vocabulary both speak (`address.rs`, `fragment.rs`, `dates.rs`, `text.rs`). Knows no host, format, ranking, or transport.
- `crates/inseam-seams` — the seam catalog as code ([design/services.md](../design/services.md)): one module per seam (`connection`, `transforms`, `embedder`, `llm`, `finder`, `sweep`, `operations`), each holding the trait, its well-known key, its vocabulary types, and its capability-fact names. Definitions live here, apart from every provider and consumer.
- `crates/inseam-plugins` — the first-party **native plugins** ([plugins/native.md](plugins/native.md)): one module per plugin, all registered through `factories()`. Also hosts the agent demo (a pure seam consumer).
- `crates/inseam-wasm-host` — the plugin-host bridge for **sandboxed plugins** ([plugins/sandboxed.md](plugins/sandboxed.md)): the WIT contract (`wit/transform.wit`), the wasmtime bridge, and the conformance harness ([plugins/validation.md](plugins/validation.md)) behind `inseam plugin check` and install-time admission.
- `crates/inseam-cli` — the `inseam` binary: a **distribution** — base composition + clap parsing + printing. No node logic; every command is a call on the `operations` seam.
- `crates/inseam-ffi` — C ABI staticlib distribution for native app shells; header at `include/inseam_ffi.h`. See [macos-app.md](macos-app.md).
- `plugins/` — sandboxed plugin projects (standalone crates, **not** workspace members; they build to `wasm32-wasip2`) **and registry v0** ([plugins/registry.md](plugins/registry.md)): `registry.toml` index, `advisories.toml`, per-plugin golden checks and fixtures (`plugins/README.md`). `plugins/ocr` is the reference example.
- `apps/macos` — the SwiftUI macOS app (SwiftPM, not a cargo member). See [macos-app.md](macos-app.md).
- `apps/docs-website` — the docs.inseam.io site (Astro Starlight on Cloudflare Workers, pnpm, not a cargo member); content synced from `docs/` ([doc-generation.md](doc-generation.md)).
- `xtask/` — repo automation, `cargo xtask docs` regenerates the source-exported doc pages ([doc-generation.md](doc-generation.md)).
- `skills/` — agent skills (symlinked into `.claude/skills`); exported into [docs/skills/](skills/) by the doc generator.

Shared dependency versions live in `[workspace.dependencies]` in the root `Cargo.toml`; member crates reference them with `dep.workspace = true`.
