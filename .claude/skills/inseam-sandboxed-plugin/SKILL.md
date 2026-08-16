---
name: inseam-sandboxed-plugin
description: Author, build, and validate a sandboxed (WASM component) inseam plugin against the transform seam contract. Use when creating or modifying a community/AI-authored plugin under plugins/.
---

# Authoring a sandboxed inseam plugin

You are writing a **sandboxed transform plugin**: a WASM component that the
inseam kernel mounts through the plugin-host bridge. It registers into the
same `transforms` seam native transforms use — tier is provenance, not
shape. Read `design/plugins.md` and `design/kernel.md` if you need the
architecture; this skill is the mechanical contract.

## The contract (source of truth)

- WIT world: `crates/inseam-wasm-host/wit/transform.wit` — READ IT FIRST.
  Your component exports the `transform` interface (`claims`, `apply`) and
  may import the `host` interface (log, llm-complete, llm-describe-image,
  source-bytes).
- Bridge behavior: `crates/inseam-wasm-host/src/lib.rs` (doc comments at
  the top). Key rules you must design around:
  - **Capabilities are manifest-gated.** Calling a host function your
    manifest didn't request returns `Err` — degrade gracefully (emit
    nothing), never panic.
  - **Effective claims = manifest claims ∩ exported claims.** Keep the two
    lists consistent or your plugin will never run.
  - **`apply` must be infallible in spirit**: return `Ok(empty output)` when
    you cannot do useful work (capability withheld, unreadable input). An
    `Err` is logged and treated as empty — it never gates the source.
  - **Fragments are a flattened tree**: `parent` must index an EARLIER
    fragment in your output list; `None` hangs the fragment off the claimed
    fragment. Never emit `text/x-inseam-*` mimetypes (the bridge drops
    them).
  - Relations: `contains`, `derived-from`, `transcribes`, `links-to`,
    `mentions`. A transcript-like output of media uses `transcribes`.
  - Per-call instantiation: no state survives between applications. Don't
    cache; don't count; the host meters your LLM budget mechanically.

## Project layout

Create the plugin as a **standalone crate** (NOT a member of the root
workspace) at `plugins/<name>/`:

```
plugins/<name>/
  Cargo.toml
  src/lib.rs
  <name>.manifest.toml     # reviewed by owners; enforced by the bridge
  README.md                # one paragraph: what it does, what it needs
```

`Cargo.toml` template:

```toml
[package]
name = "<name>"
version = "0.1.0"
edition = "2021"

# Standalone: the root workspace must not adopt this crate.
[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.60"

[profile.release]
opt-level = "s"
lto = true
strip = true
```

`src/lib.rs` skeleton:

```rust
wit_bindgen::generate!({
    path: "../../crates/inseam-wasm-host/wit",
    world: "transform-plugin",
});

use exports::inseam::plugin::transform::{ClaimSpec, Envelope, Fragment, Guest, Output};
use inseam::plugin::host;

struct Plugin;

impl Guest for Plugin {
    fn claims() -> ClaimSpec {
        ClaimSpec {
            mimetypes: vec!["image/png".into(), "image/jpeg".into()],
            roots_only: true,
        }
    }

    fn apply(
        _env: Envelope,
        mimetype: String,
        _is_root: bool,
        _text: Option<String>,
    ) -> Result<Output, String> {
        // ... use host::source_bytes(), host::llm_describe_image(...),
        // host::log(...) as granted; on Err from a host call, return
        // Ok(Output { fragments: vec![] }) — degrade, don't gate.
        let _ = mimetype;
        Ok(Output { fragments: vec![] })
    }
}

export!(Plugin);
```

`<name>.manifest.toml` schema (must sit next to the built `.wasm`):

```toml
name = "<name>"
version = "0.1.0"
seam = "transform"
claims = ["image/png", "image/jpeg"]   # or ["image/*"]; must overlap claims()
roots_only = true
kind = "enrichment"                     # or "structural"

[capabilities]
llm = true              # request ONLY what you use
source_bytes = true
llm_call_budget = 25    # per index run
```

Request the **minimum** capabilities: every extra grant is attack surface an
owner has to approve, and capability widening between versions triggers an
explicit approval gate.

## Build

```sh
cd plugins/<name>
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/<name>.wasm ./<name>.wasm
```

(`wasm32-wasip2` produces a component directly; the target is installed via
`rustup target add wasm32-wasip2` if missing. If the crate name has hyphens
the artifact uses underscores — rename the copy to match the manifest.)

## Validate (do not skip)

1. `cargo build --release --target wasm32-wasip2` — must be warning-free.
2. From the repo root:
   ```sh
   cargo run -p inseam-wasm-host --example inspect -- plugins/<name>/<name>.wasm
   ```
   This mounts the component through the real bridge and prints the
   effective claims. It must end with `OK`, and the claim list must match
   your intent — an empty claim list means manifest and `claims()` disagree.
3. If validation fails, fix and repeat. Do not hand off a plugin whose
   inspect run fails.

Reading inspect failures:
- The first invocation cold-compiles wasmtime and can take a few minutes —
  that is a build, not a hang.
- A failure like `component imports instance wasi:...` (an unsatisfied
  `wasi:*` import) is a **host-linker gap in the bridge**, not a plugin
  authoring error; report it against `inseam-wasm-host` instead of
  reworking the plugin.

## Mounting (how users will run it)

Users add an entry to their node's `composition.toml`:

```toml
[[entry]]
id = "<name>"
plugin = "wasm:plugins/<name>/<name>.wasm"
[entry.config]
# cooldown_days = 7    # release cooldown for newly observed versions
# allow_new = true     # explicit consent to skip the cooldown
```

Document that snippet in the plugin's README.

## Style

- Keep the component dependency-light: every dependency compiles into the
  artifact and into the audit burden.
- Prompts sent through `llm-complete`/`llm-describe-image` should say
  exactly what to return and forbid preamble — the output lands verbatim in
  a search index.
- Bound your output: cap fragment counts and text sizes yourself; the host
  prunes, but a tight plugin doesn't rely on it.
