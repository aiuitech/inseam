---
name: inseam-loaded-plugin
description: Author and validate a loaded inseam plugin — a sandboxed WASM component mounted into a node at runtime. The default kind for AI- and community-authored plugins; needs only the `inseam` CLI, never the inseam source tree. For trusted Rust compiled into the binary itself, use inseam-linked-plugin instead.
---

# Authoring a loaded inseam plugin

A loaded plugin is three files: a WASM component, a manifest, and golden
checks. You need the `inseam` CLI (self-documenting — `inseam plugin
check --help`) and any toolchain that emits WASM components (Rust on
`wasm32-wasip2`, componentize-py, ComponentizeJS/jco, TinyGo). Do not
assume the inseam source tree exists; everything else is online at
<https://docs.inseam.io> — fetch <https://docs.inseam.io/llms.txt> for the
map, and the Plugins section for depth beyond this page.

**The loop is test-driven and the tests are mandatory.** A plugin with no
golden checks, or with checks that prove nothing, fails `inseam plugin
check` — and therefore fails registry CI and every installing node's
admission. Write the checks first: they are the specification you build
to, the guardrail that tells you when you're done, and the proof that
ships with the plugin.

## The contract

Each loaded seam is a WIT world. <https://docs.inseam.io/plugins/loaded>
names the seams the bridge mounts and everything it enforces; each seam's
rendered WIT reference sits alongside it in the Plugins section, and the
raw `.wit` files to generate bindings from live at
<https://github.com/aiuitech/inseam/tree/main/crates/inseam-wasm-host/wit>.

Rules that hold on every seam — design around them:

- Effective claims = manifest claims ∩ exported claims — keep the two
  lists consistent or the plugin never runs.
- Host imports are manifest-gated; an ungranted call returns `Err`. Your
  seam's WIT reference lists what it may import.
- **Degrade, never gate**: on withheld capability or unusable input,
  return `Ok` with empty output. Never panic; an `Err` from your apply
  entry point is logged and produces nothing.
- A fresh instance per call: no state, no caching, no counting — the host
  meters your LLM budget.
- Output is hygiene-checked; the seam's docs say what the bridge drops or
  rewrites (e.g. on the transform seam, a fragment's `parent` must index
  an earlier fragment in your own output, and inseam-defined mimetypes
  are refused).

## Shape

```
<name>/
  <name>.wasm            # the component
  <name>.manifest.toml   # what an owner reviews; the bridge enforces it
  <name>.checks.toml     # golden checks — write these FIRST
  fixtures/              # tiny byte fixtures the checks reference
    README.md            # what each fixture is and why — required
  README.md              # what it does, what it needs, the mount snippet
```

Manifest:

```toml
name = "<name>"
version = "0.1.0"
seam = "transform"       # the seam this plugin binds
claims = ["image/png"]   # must overlap claims()
roots_only = true
kind = "enrichment"      # or "structural"

[capabilities]           # request the MINIMUM you use — widening
llm = true               # capabilities later triggers a re-approval gate
source_bytes = true
llm_call_budget = 25
```

## The TDD loop

Work in this order. Each step names the command that tells you whether it
worked; run it, read the report, then move on.

### 1. State the claim as checks (red)

Before any code, write `<name>.checks.toml`: what the plugin promises, as
input → expected output shape. Full schema:
<https://docs.inseam.io/plugins/validation>. The harness enforces a
minimum coverage, and so should you:

- **One check that proves the claim** — a substantive expectation
  (`fragment_contains`, `relation`, or `mimetype`), not just "something
  came out". This is the plugin's reason to exist, in one example.
- **One check that pins the degrade path** — no `text`, no `llm_returns`,
  and `max_fragments` set (`0` for "emits nothing", or the fallback's
  shape). This is what the plugin does on an offline node or with a spent
  budget; it must be deliberate, never accidental.
- Add a check per distinct behavior you implement (a second mimetype, the
  LLM refusing while text is present, a malformed input you handle).
  Don't pad: the harness's own hostile-input battery already covers
  "doesn't crash"; your checks cover "does what it says".

Each check's `mimetype` must fall inside your manifest's claims, or the
harness fails it — a check for a mimetype you never claimed would never run
in production. The LLM is always canned during checks (`llm_returns` is
the verbatim reply; absent means it refuses), so checks prove plumbing and
shape, never model quality — write `llm_returns` as the kind of reply your
prompt asks for, and assert on how your code shapes it.

Fixtures (`bytes_file`) live in `fixtures/`, relative to the checks file,
and must be **tiny and well-formed** — the smallest valid PNG, a
three-line CSV. Every fixture gets a row in `fixtures/README.md` saying
what it is and why it exists: the harness's LLM is fake and the component
is sandboxed, so fixture *content* never matters, only that the bytes
reach the plugin and come back shaped right. Fixtures are downloaded by
every installing node; size is a cost multiplied across nodes.

Example — an OCR plugin's complete minimum:

```toml
[[check]]
name = "transcribes image text through the granted vision llm"
mimetype = "image/png"
bytes_file = "fixtures/pixel.png"
llm_returns = "GARAGE SALE SATURDAY 9AM"

[check.expect]
fragment_contains = "GARAGE SALE"
relation = "transcribes"
mimetype = "text/plain"

[[check]]
name = "emits nothing when the llm is withheld"
mimetype = "image/png"
bytes_file = "fixtures/pixel.png"

[check.expect]
min_fragments = 0
max_fragments = 0
```

### 2. Write the manifest, scaffold the component, build, and watch it fail

Write the manifest. Scaffold a component whose apply entry point returns
`Ok` with empty output (Rust: `crate-type = ["cdylib"]` + `wit-bindgen`,
then `cargo build --release --target wasm32-wasip2` — that target emits a
component directly). Then run the gate:

```sh
inseam plugin check <name>/<name>.wasm
```

Expect `FAIL`. Read the report top to bottom: `static`, `mount`, and
`contract` should already be `ok` (the scaffold degrades everywhere); the
positive golden check fails with what it wanted and what actually came
out, e.g. `no fragment carries relation "transcribes"; got 0
fragment(s)`. That line is your to-do list. If `mount` fails instead, fix
the toolchain and claims before touching behavior — the effective-claims
line must match your intent (empty means manifest and `claims()`
disagree). The first run cold-compiles wasmtime — minutes, not a hang.

### 3. Implement until green

Implement the smallest code that turns the failing check green, rerun the
same command, repeat. Every failing golden line prints the expectation it
missed and a one-line account of the output (`got 2 fragment(s)
[text/plain contains "…"]`), so you can see the gap without adding
logging. When a new behavior needs code, add its check first and watch it
fail before writing the code.

A `contract` failure means a code path traps instead of degrading; hunt
the `unwrap`/`expect`/indexing. A `mandatory
coverage` failure means your checks don't yet prove the claim or pin the
degrade path — go back to step 1. Warnings are worth reading: a
non-deterministic output or an `Err` return is allowed but will cost
every node that mounts you.

Done when:

```sh
inseam plugin check <name>/<name>.wasm   # ends `PASS`, claims line as intended
```

This is the exact harness registry CI runs at publish and every node runs
at install-time admission. Never hand off a failing plugin — admission on
the user's node refuses the mount, naming the failing check.

### 4. Prove it live

Checks prove shape against canned capabilities; the last step is the real
node. Mount the artifact in a scratch node and index something it claims:

```sh
export INSEAM_DATA_DIR=$(mktemp -d)                 # a throwaway node
inseam config                                       # see the composition you are patching
cat >> "$INSEAM_DATA_DIR/composition.toml" <<EOF
[[entry]]
id = "<name>"
plugin = "wasm:$PWD/<name>/<name>.wasm"
EOF
inseam config --resolved                            # your entry appears, layered over the base
inseam plugins                                      # fiber `<name>` is Active, with its effects listed
inseam index <dir-with-files-you-claim>             # the sweep applies you to each root
inseam status                                       # fragment counts moved
inseam query "<words your plugin should surface>"   # your fragments rank
inseam expand <address-from-the-query>              # see the fragments you hung off the source
```

If `inseam plugins` shows the fiber `Failed`, the reason names the gate
(admission with the failing check, a claims mismatch, a cooldown) — fix
and re-run the loop from step 3. If the fiber is `Active` but `inseam
expand` shows nothing from you, your effective claims don't cover the
files you indexed, or the node's LLM is unconfigured and you degrade
(check `inseam status` and the entry's capabilities). `inseam
<command> --help` documents every flag.

## Hand off

Finish with: `inseam plugin check` at `PASS`; `fixtures/README.md`
describing every fixture; a `README.md` saying what the plugin does, what
capabilities it needs and why, and the mount snippet:

```toml
[[entry]]
id = "<name>"
plugin = "wasm:<path>/<name>.wasm"
```

in the node's composition (`inseam config` prints it), or distribute
through a registry and `inseam plugin install`.
