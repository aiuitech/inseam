# Plugin Validation

One validation harness (`crates/inseam-wasm-host/src/check.rs` for transforms, `check_connection.rs` for connections) is the quality gate for loaded plugins everywhere it matters ([design/registry.md](../../design/registry.md)): the author's edit loop, the registry's CI, and every node's install-time admission all run the identical code — so a plugin that passes once passes everywhere.

```sh
inseam plugin check plugins/ocr/ocr.wasm       # exits nonzero on failure
inseam plugin check plugins/github/github.wasm
```

## The four phases

Both seams run the same four phases; the manifest's `seam` decides the details.

1. **static** — `<name>.manifest.toml` parses, the seam is supported, the `hosts` allow list is well-formed, and the capabilities make sense together (a budget without `llm = true`, or `grant = true` with no `hosts`, is inert and gets a warning). A transform's claims must be well-formed (`type/sub` or `type/*`); a connection must declare a valid `host_kind`, and is warned when it names no hosts (it can then reach nothing).
2. **mount** — the component compiles and instantiates against the *real* bridge. A transform's `claims()` is callable and returns the same thing every time, and the declared and exported claims overlap. A connection is configured with the checks file's `[config]`, must describe a host whose kind is the manifest's, must describe it the same way twice, and exports its capabilities.
3. **contract** — the hostile-input battery, built in and plugin-agnostic. For a transform: apply with no text and no capabilities, with an LLM that refuses (the shape of a spent budget), with a network that refuses, and with garbage bytes; a full-capability run with a canned LLM then feeds **output hygiene** (no inseam-defined mimetypes, no forward parent references, known relations only) and **determinism** (the same input twice; a difference warns, because it means the index churns on every re-sweep). For a connection: enumerate, read, and describe offline, enumerate against a host answering garbage with a 500, and configure with unparseable TOML. In all cases the plugin must degrade gracefully — a crash (which is what a guest panic becomes) fails hard; a returned `Err` earns a warning on the transform seam (`Ok` with empty output is its contract) and is the *expected* offline answer on the connection seam.
4. **golden** — the plugin's own checks (below), run through the real bridge. **Mandatory**: no checks file, an unparseable one, or one that fails the coverage rule is a `FAIL`, not a warning.

## Golden checks: `<name>.checks.toml`

The plugin's own tests, written **as data, not code** — an AI author can't game them, and any node can re-run them at install without trusting anything. Every capability the bridge grants is canned in a check: the LLM's reply, and the network's replies.

### Transform checks

```toml
[[check]]
name = "transcribes image text through the granted vision llm"
mimetype = "image/png"          # what the input arrives as; must be inside the effective claims
# is_root = true                # default
# text = "..."                  # fragment text; absent = content not read
bytes_file = "fixtures/pixel.png"   # handed to source-bytes; path relative to this file
llm_returns = "GARAGE SALE"     # canned LLM reply; ABSENT = the llm refuses (tests the fallback)
# [[check.fetch]]               # canned network reply, for a transform whose manifest names hosts
# url = "https://api.example.com/x"
# body = "..."                  # or body_file = "fixtures/x.json"; status = 200; content_type = "…"

[check.expect]
min_fragments = 1               # default 1; 0 + max 0 asserts clean degradation
# max_fragments = 10
fragment_contains = "GARAGE"    # some fragment's text contains this
relation = "transcribes"        # some fragment carries this relation
mimetype = "text/plain"         # some fragment's mimetype starts with this
# keyed_contains = "Ada Lovelace"  # some keyed sprout's key or text contains this (linked transforms only)
# max_keyed = 2
```

### Connection checks

```toml
[config]                        # the plugin's own config every check configures the instance with
repository = "octo/hello"

[[check]]
name = "enumerates every blob of the repository tree"
call = "enumerate"              # enumerate | read | describe
root = ""                       # enumerate: the scope
# locator = "README.md"         # read / describe: the locator
[[check.fetch]]                 # the canned network; ABSENT = every fetch refuses (the offline node)
url = "https://api.github.com/repos/octo/hello/git/trees/main?recursive=1"   # must be on a host the manifest names
body_file = "fixtures/tree.json"   # or body = "…"
# status = 200
# content_type = "application/json"
# authorized = true             # the reply needs the grant bearer; an unauthorized request gets 401

[check.expect]
min_sources = 4                 # enumerate: default 1
max_sources = 4
locator_contains = "README.md"  # enumerate: some source's locator contains this
content_type = "text/markdown"  # enumerate / describe: some source's (the described) content type starts with this
hint_contains = "guide.md"      # enumerate / describe: some hint contains this
# text_contains = "…"           # read: the bytes, as text, contain this
# error = true                  # the call must return an error (never a trap)
```

The schema, the matcher, and the coverage rule live once, in `inseam_conformance::golden`, and both tiers and both seams use them.

### The coverage rule

A checks file must contain, at minimum:

- **one check that proves the claim** — an expectation about *what* came out (`fragment_contains`, `relation`, `mimetype`, or `keyed_contains` for a transform; `locator_contains`, `content_type`, `hint_contains`, or `text_contains` for a connection), not merely that something did;
- **one check that pins the degrade path** — a *starved* check with the outcome fixed: for a transform, no `text` and no `llm_returns` with `max_fragments` set (`0` for "emits nothing", or the fallback's shape — the summarizer's starved check asserts exactly one `via=envelope` summary); for a connection, no `[[check.fetch]]` replies with `error = true` or `max_sources` set — what the plugin does on an offline node, deliberately.

The harness reports each unmet requirement as its own `mandatory coverage` failure, worded as the fix. The rule is deliberately small: the contract battery already covers "doesn't crash" generically, so a plugin's own checks are for "does what it says" and "knows what it does without" — padding beyond that buys nothing.

Things the golden phase refuses: a transform check whose `mimetype` falls outside the effective claims (it would never run in production — usually a forgotten claim); a canned reply on a host the manifest does not name (the bridge would refuse it too); a `bytes_file` or `body_file` path that escapes the plugin directory; an enumeration returning a locator the bridge would drop.

### Reading a failure

A failing check names every missed expectation and then what actually came out, so the red→green loop needs no extra logging:

```
golden    transcribes image text through the granted vision llm FAIL
          no fragment carries relation "transcribes"; got 1 fragment(s) [text/plain contains "GARAGE SALE"]
golden    enumerates every blob of the repository tree          FAIL
          no source locator contains "README.md"; got 0 source(s) []
```

Capabilities in golden checks mirror the bridge: `llm_returns`, `bytes_file`, and `[[check.fetch]]` do nothing (with a warning) unless the manifest requests `llm`, `source_bytes`, and `hosts`. Fixtures live beside the checks file and are documented in each plugin's `fixtures/README.md` (`plugins/README.md` has the dataset conventions); `inseam plugin install` downloads them — each one signed in the release record — so the installing node re-runs the same checks. The agent skill (`skills/inseam-loaded-plugin/SKILL.md`) is the test-first authoring loop built on this harness.

## Install-time admission

The bridge runs the harness the first time a node sees an artifact — keyed by a hash of artifact + manifest + checks, cached in the node's plugin state. A failing plugin refuses to activate, naming the failing check, instead of mounting and silently underperforming forever. Per-entry config: `admission = "enforce"` (default) `| "warn" | "off"`. The mount gates run in order: release cooldown and capability widening, then admission, then the claims overlap check (transform) or the host-kind check (connection).

## The linked tier's equivalent

Linked plugins are gated at build time instead, through the shared harness crate `inseam-conformance`: `crates/inseam-plugins/tests/conformance.rs` sweeps `inseam_plugins::factories()` itself.

- `check_factories` — every factory must build from an enrolled config and declare a sensible manifest.
- `batter_transforms` — every registered transform faces the same hostile-input battery (text withheld, empty, garbage, no LLM) through the seam.
- `golden_transforms` — every registered transform must ship its own `<registration-name>.checks.toml` (beside its source, `crates/inseam-plugins/src/<plugin>/`), meeting the same coverage rule, and pass every check through the seam with the same canned LLM. Linked checks may additionally assert `keyed_contains`/`max_keyed`, since linked transforms emit keyed sprouts and the WIT seam does not.

Adding a linked plugin without enrolling it — a config arm, and for a transform a checks-file arm — panics the suite with instructions: there is no way to link one into a distribution without inheriting the tests. A custom distribution runs the same three sweeps over its own factories ([distributions.md](distributions.md)).
