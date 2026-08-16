# Sandboxed Plugins

The untrusted tier ([design/plugins.md](../../design/plugins.md)): WASM components mounted at runtime by the plugin-host bridge (`crates/inseam-wasm-host`). Tier is provenance, not shape — a component-backed transform registers into the same `transforms` seam as a native one and is indistinguishable to the sweep.

## Mounting

A composition entry whose plugin ref uses the `wasm:` scheme:

```toml
[[entry]]
id = "ocr"
plugin = "wasm:plugins/ocr/ocr.wasm"
[entry.config]
cooldown_days = 7    # release cooldown (see below); 0 by default for local dev
# allow_new = true   # explicit consent for a version inside its cooldown
# fuel = 2000000000  # per-application execution budget
```

Beside the artifact sits its manifest (`<name>.manifest.toml`): identity, version, seam, declared claims, and requested capabilities (`llm`, `source_bytes`, `llm_call_budget`). The manifest is what an owner reviews; the bridge enforces that the component gets nothing beyond it.

## The contract

`crates/inseam-wasm-host/wit/transform.wit` — the `transforms` seam projected into WIT. Exports: `claims()` and `apply(envelope, mimetype, is-root, text)`. Imports (the only ones besides core WASI, which is satisfied with an **empty** context — no preopens, env, args, or sockets):

- `log` — into the node's tracing output
- `llm-complete`, `llm-describe-image` — the same metered, guarded LLM grant native transforms get; absent from the manifest → every call errors
- `source-bytes` — the claimed source's raw bytes, root-only

## Enforcement at the bridge

- **Effective claims = manifest ∩ exported** — a component cannot quietly claim more than its manifest promised; zero overlap refuses the mount.
- **Per-call instantiation** — fresh component state each application; nothing leaks between sources.
- **Fuel limits** — a spinning component traps instead of wedging the sweep; a trap or error logs and emits nothing (indexing is enrichment, never gated).
- **Output hygiene** — emitted fragments are validated: inseam-defined mimetypes dropped, unknown relations coerced to `contains`, malformed parents treated as roots.
- **Release cooldown** — a newly observed artifact hash soaks for `cooldown_days` before activating, on a first-seen clock recorded in this node's plugin state (locally unforgeable). Requesting different capabilities than the approved version is its own gate regardless of soak. `allow_new = true` on the entry is the explicit consent moment.
- **Shape stamps carry the artifact** — name, version, and content hash are in the registration's shape fingerprint, so an upgraded plugin dirties exactly the sources it built ([index/maintenance.md](../index/maintenance.md)).

## Authoring and validating

The agent skill at `.claude/skills/inseam-sandboxed-plugin/SKILL.md` is the authoring procedure (scaffold, manifest, build to `wasm32-wasip2`, validate). The validation gate is:

```sh
cargo run -p inseam-wasm-host --example inspect -- plugins/<name>/<name>.wasm
```

which mounts the component through the real bridge and prints its effective claims. `plugins/ocr` — image OCR through the vision-capable LLM grant — is the reference plugin and was authored by an agent running that skill; `crates/inseam-wasm-host/tests/ocr_e2e.rs` exercises it end to end (with a fake `llm` provider, proving the component only ever sees the granted capability).
