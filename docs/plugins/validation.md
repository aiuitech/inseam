# Plugin Validation

One validation harness (`crates/inseam-wasm-host/src/check.rs`) is the quality gate for loaded plugins everywhere it matters ([design/registry.md](../../design/registry.md)): the author's edit loop, the registry's CI, and every node's install-time admission all run the identical code — so a plugin that passes once passes everywhere.

```sh
inseam plugin check plugins/ocr/ocr.wasm   # exits nonzero on failure
```

## The four phases

1. **static** — `<name>.manifest.toml` parses, the seam is supported, claims are well-formed (`type/sub` or `type/*`), and the capabilities make sense together (a budget without `llm = true` does nothing, and gets a warning).
2. **mount** — the component compiles and instantiates against the *real* bridge; `claims()` is callable and returns the same thing every time; the declared and exported claims overlap.
3. **contract** — the hostile-input battery, built in and plugin-agnostic: apply with no text and no capabilities, with an LLM that refuses (the shape of a spent budget), and with garbage bytes. In all cases the plugin must degrade gracefully — a crash (which is what a guest panic becomes) fails hard, a returned `Err` earns a warning (`Ok` with empty output is the contract). A full-capability run with a canned LLM then feeds two more checks: **output hygiene** (no inseam-defined mimetypes, no forward parent references, known relations only) and **determinism** (the same input twice; a difference warns, because it means the index churns on every re-sweep).
4. **golden** — the plugin's own checks (below), run through the real bridge. **Mandatory**: no checks file, an unparseable one, or one that fails the coverage rule is a `FAIL`, not a warning.

## Golden checks: `<name>.checks.toml`

The plugin's own tests, written **as data, not code** — an AI author can't game them, and any node can re-run them at install without trusting anything:

```toml
[[check]]
name = "transcribes image text through the granted vision llm"
mimetype = "image/png"          # what the input arrives as; must be inside the effective claims
# is_root = true                # default
# text = "..."                  # fragment text; absent = content not read
bytes_file = "fixtures/pixel.png"   # handed to source-bytes; path relative to this file
llm_returns = "GARAGE SALE"     # canned LLM reply; ABSENT = the llm refuses (tests the fallback)

[check.expect]
min_fragments = 1               # default 1; 0 + max 0 asserts clean degradation
# max_fragments = 10
fragment_contains = "GARAGE"    # some fragment's text contains this
relation = "transcribes"        # some fragment carries this relation
mimetype = "text/plain"         # some fragment's mimetype starts with this
# keyed_contains = "Ada Lovelace"  # some keyed sprout's key or text contains this (linked transforms only)
# max_keyed = 2
```

The schema, the matcher, and the coverage rule live once, in `inseam_conformance::golden`, and both tiers use them.

### The coverage rule

A checks file must contain, at minimum:

- **one check that proves the claim** — an expectation about *what* came out (`fragment_contains`, `relation`, `mimetype`, or `keyed_contains`), not merely that something did;
- **one check that pins the degrade path** — a *starved* check (no `text`, no `llm_returns`) with `max_fragments` set: `0` for "emits nothing", or the fallback's shape (the summarizer's starved check asserts exactly one `via=envelope` summary).

The harness reports each unmet requirement as its own `mandatory coverage` failure, worded as the fix. The rule is deliberately small: the contract battery already covers "doesn't crash" generically, so a plugin's own checks are for "does what it says" and "knows what it does without" — padding beyond that buys nothing.

Two more things the golden phase refuses: a check whose `mimetype` falls outside the effective claims (it would never run in production — usually a forgotten claim), and a `bytes_file` path that escapes the plugin directory.

### Reading a failure

A failing check names every missed expectation and then what actually came out, so the red→green loop needs no extra logging:

```
golden    transcribes image text through the granted vision llm FAIL
          no fragment carries relation "transcribes"; got 1 fragment(s) [text/plain contains "GARAGE SALE"]
```

Capabilities in golden checks mirror the bridge: `llm_returns`/`bytes_file` do nothing (with a warning) unless the manifest requests `llm`/`source_bytes`. Fixtures live beside the checks file and are documented in each plugin's `fixtures/README.md` (`plugins/README.md` has the dataset conventions); `inseam plugin install` downloads them so the installing node re-runs the same checks. The agent skill (`skills/inseam-loaded-plugin/SKILL.md`) is the test-first authoring loop built on this harness.

## Install-time admission

The bridge runs the harness the first time a node sees an artifact — keyed by a hash of artifact + manifest + checks, cached in the node's plugin state. A failing plugin refuses to activate, naming the failing check, instead of mounting and silently underperforming forever. Per-entry config: `admission = "enforce"` (default) `| "warn" | "off"`. The mount gates run in order: release cooldown, then admission, then the claims overlap check.

## The linked tier's equivalent

Linked plugins are gated at build time instead, through the shared harness crate `inseam-conformance`: `crates/inseam-plugins/tests/conformance.rs` sweeps `inseam_plugins::factories()` itself.

- `check_factories` — every factory must build from an enrolled config and declare a sensible manifest.
- `batter_transforms` — every registered transform faces the same hostile-input battery (text withheld, empty, garbage, no LLM) through the seam.
- `golden_transforms` — every registered transform must ship its own `<registration-name>.checks.toml` (beside its source, `crates/inseam-plugins/src/<plugin>/`), meeting the same coverage rule, and pass every check through the seam with the same canned LLM. Linked checks may additionally assert `keyed_contains`/`max_keyed`, since linked transforms emit keyed sprouts and the WIT seam does not.

Adding a linked plugin without enrolling it — a config arm, and for a transform a checks-file arm — panics the suite with instructions: there is no way to link one into a distribution without inheriting the tests. A custom distribution runs the same three sweeps over its own factories ([distributions.md](distributions.md)).
