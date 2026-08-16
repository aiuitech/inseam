# Plugin Validation

One conformance harness (`crates/inseam-wasm-host/src/check.rs`) is the
fitness gate for sandboxed plugins everywhere it matters
([design/registry.md](../../design/registry.md)): the author's loop, the
registry's CI, and every node's install-time admission run the identical
code, so a plugin that passes once passes everywhere.

```sh
inseam plugin check plugins/ocr/ocr.wasm   # exits nonzero on failure
```

## The four phases

1. **static** — `<name>.manifest.toml` parses, the seam is supported,
   claims are well-formed (`type/sub` or `type/*`), capabilities are
   coherent (a budget without `llm = true` is inert, and warned).
2. **mount** — the component compiles and instantiates against the *real*
   bridge linker; `claims()` is callable and deterministic; declared ∩
   exported claims is non-empty.
3. **contract** — the hostile-input battery, built-in and plugin-agnostic:
   apply with no text and no capabilities, with a refusing LLM (the shape
   of a spent budget), and with garbage bytes; all must degrade — a trap
   (which is what a guest panic becomes) fails hard, a returned `Err` earns
   a warning (`Ok` + empty output is the contract). A full-capability run
   with a canned LLM then feeds two more checks: **output hygiene** (no
   inseam-defined mimetypes, no forward parent indices, known relations)
   and **determinism** (identical input twice; divergence warns, because it
   means index churn on every re-sweep).
4. **golden** — the plugin's own checks (below), executed through the real
   bridge.

## Golden checks: `<name>.checks.toml`

Enforced plugin tests **as data, not code** — an AI author can't game them
and a node can re-run them at install without trusting anything:

```toml
[[check]]
name = "transcribes image text through the granted vision llm"
mimetype = "image/png"          # what the application arrives as
# is_root = true                # default
# text = "..."                  # fragment text; absent = content not read
bytes_file = "fixtures/pixel.png"   # handed to source-bytes; path relative to this file
llm_returns = "GARAGE SALE"     # canned LLM reply; ABSENT = the llm refuses (degrade path)

[check.expect]
min_fragments = 1               # default 1; 0 + max 0 asserts clean degradation
# max_fragments = 10
fragment_contains = "GARAGE"    # some fragment's text contains this
relation = "transcribes"        # some fragment carries this relation
mimetype = "text/plain"         # some fragment's mimetype starts with this
```

Capabilities in golden checks mirror the bridge: `llm_returns`/`bytes_file`
are inert (warned) unless the manifest requests `llm`/`source_bytes`.
Fixtures live beside the checks file and are documented in each plugin's
`fixtures/README.md` (`plugins/README.md` has the dataset conventions);
`inseam plugin install` downloads them so the installing node re-runs the
same checks.

## Install-time admission

The bridge runs the harness the first time a node sees an artifact — keyed
by a hash of artifact + manifest + checks, cached in the node's plugin
state — and a failing plugin's fiber refuses to activate with the failing
check named, instead of mounting and silently degrading forever. Per-entry
config: `admission = "enforce"` (default) `| "warn" | "off"`. Order of
mount gates: release cooldown, then admission, then claims intersection.

## The native tier's equivalent

Native plugins are gated by CI instead:
`crates/inseam-plugins/tests/conformance.rs` sweeps
`inseam_plugins::factories()` itself — every factory must build from an
enrolled config and declare a coherent manifest, and every registered
transform faces the same hostile-input battery (text withheld, empty,
garbage, no LLM) through the seam. Adding a native plugin without enrolling
it panics the suite with instructions; there is no way to link one into a
distribution without inheriting the tests.
