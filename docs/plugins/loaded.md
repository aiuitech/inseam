# Loaded Plugins

The untrusted tier ([design/plugins.md](../../design/plugins.md)), named for how the code arrives: **loaded** from an artifact at runtime, where [linked plugins](linked.md) are compiled in at build. They are WASM components mounted by the bridge in `crates/inseam-wasm-host`, sandboxed by construction. The tier is about where the code came from, not what it does — a WASM transform registers into the same `transforms` seam as a linked one, and the sweep can't tell them apart.

## Mounting

A composition entry whose plugin ref uses the `wasm:` scheme:

```toml
[[entry]]
id = "ocr"
plugin = "wasm:plugins/ocr/ocr.wasm"
[entry.config]
cooldown_days = 7    # wait period for new versions (see below); 0 by default for local dev
# allow_new = true   # explicit consent for a version still inside its wait period
# fuel = 2000000000  # per-run execution budget
```

Next to the artifact sits its manifest (`<name>.manifest.toml`): identity, version, seam, declared claims, and requested capabilities (`llm`, `source_bytes`, `llm_call_budget`). The manifest is what an owner reviews; the bridge guarantees the component gets nothing beyond it.

Three ways the entry gets there: by hand, as above; `inseam plugin mount` / `plugin install` ([authoring-cli.md](authoring-cli.md), [registry.md](registry.md)), which append it before the node boots; and, on a **running** node, the `install_plugin` owner operation — the web console's Plugins panel, `POST /api/v1/owner/plugins/install` ([../architecture/hosted-node.md](../architecture/hosted-node.md)) — which uploads the plugin directory, writes it under `<data-dir>/plugins/<id>/`, appends the entry, and reconciles the kernel in place, no restart; the macOS app does the same through `inseam_node_install_plugin` ([../architecture/macos-app.md](../architecture/macos-app.md)). A mount that fails is rolled back and named; the gates below apply to all three identically.

## The contract

`crates/inseam-wasm-host/wit/transform.wit` — the `transforms` seam expressed in WIT. Exports: `claims()` and `apply(envelope, mimetype, is-root, text)`. Imports (the only ones besides core WASI, which is given an **empty** environment — no files, env vars, args, or sockets):

- `log` — into the node's tracing output
- `llm-complete`, `llm-describe-image` — the same metered, guarded LLM handle linked transforms get; if the manifest didn't request it, every call errors
- `source-bytes` — the claimed source's raw bytes, root-only

## What the bridge enforces

- **Effective claims = manifest ∩ exported** — a component can't quietly claim more than its manifest promised; zero overlap refuses the mount.
- **A fresh instance per call** — component state is thrown away between runs; nothing leaks between sources.
- **Fuel limits** — a component stuck in a loop is stopped instead of hanging the sweep; a trap or error is logged and produces nothing (indexing is enrichment — a broken plugin never blocks a source).
- **Output hygiene** — emitted fragments are checked: inseam-defined mimetypes are dropped, unknown relations become `contains`, malformed parent references are treated as roots.
- **Release cooldown** — a newly seen artifact hash waits `cooldown_days` before activating, timed from when this node first saw it (recorded in the node's own plugin state, so it can't be forged). Requesting different capabilities than the approved version is a separate gate, regardless of the wait. `allow_new = true` on the entry is the explicit consent.
- **Install-time admission** — the first time a node sees an artifact it runs the full validation harness ([validation.md](validation.md)); a plugin that crashes on hostile input, ships no golden checks, or fails its own refuses to mount, naming the failing check. `admission = "enforce" | "warn" | "off"` per entry; verdicts are cached by content hash.
- **Shape stamps carry the artifact** — the name, version, and content hash go into the registration's shape fingerprint, so upgrading a plugin re-indexes exactly the sources it built ([indexing/maintenance.md](../indexing/maintenance.md)).

## Authoring and validating

The agent skill at `skills/inseam-loaded-plugin/SKILL.md` is the authoring guide (scaffold, golden checks first, manifest, build to `wasm32-wasip2`, validate). The validation gate:

```sh
inseam plugin check plugins/<name>/<name>.wasm
```

the same harness CI runs at publish and every node runs at admission ([validation.md](validation.md)). `plugins/ocr` — image OCR through the vision-capable LLM handle — is the reference plugin, and was written by an agent following that skill; `crates/inseam-wasm-host/tests/ocr_e2e.rs` exercises it end to end (with a fake `llm` provider, proving the component only ever sees the granted capability). Distribution and installation: [registry.md](registry.md).
