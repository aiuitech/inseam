---
name: inseam-loaded-plugin
description: Author and validate a loaded inseam plugin — a sandboxed WASM component mounted into a node at runtime. The default kind for AI- and community-authored plugins; needs only the `inseam` CLI, never the inseam source tree. For trusted Rust compiled into the binary itself, use inseam-linked-plugin instead.
---

# Authoring a loaded inseam plugin

A loaded plugin is three files: a WASM component, a manifest, and golden
checks. It implements one of two seams: a **transform** (turns a claimed
source or fragment into fragments; per-call instances) or a **connection**
(stewards one host — a service reached through the node's guarded `fetch`;
one long-running instance per entry). You need the `inseam` CLI and a toolchain that emits WASM
components (Rust on `wasm32-wasip2` — `rustup target add wasm32-wasip2` —
or componentize-py, ComponentizeJS/jco, TinyGo). You do **not** need the
inseam source tree or the network: **the CLI is the authoring
companion** — the node you are extending describes its own contract,
capabilities, and current plugins, scaffolds the plugin, runs it against
real files, validates it, and mounts it. Every `inseam <command> --help`
is current for the binary you have. <https://docs.inseam.io> (and
<https://docs.inseam.io/llms.txt>) has depth beyond this page.

**The loop is test-driven and the tests are mandatory.** A plugin with no
golden checks, or with checks that prove nothing, fails `inseam plugin
check` — and therefore fails registry CI and every installing node's
admission. Write the checks first: they are the specification you build
to, the guardrail that tells you when you're done, and the proof that
ships with the plugin.

## The companion commands

| Question | Command |
| --- | --- |
| Which seams take loaded plugins, under what contract? | `inseam seams` — and `inseam seams --wit > wit/plugin.wit` for bindings (both worlds, one package) |
| What may my manifest request, and will *this* node grant it? | `inseam capabilities` — llm, source_bytes, `hosts`, `grant`; which hosts are already stewarded |
| Who already handles this input on this node? | `inseam claims <mimetype\|path>` (transforms); `inseam hosts` (connections) |
| Start a plugin that is red for the right reason | `inseam plugin new <name> --claims a/b,c/*` or `--seam connection --kind <kind>` |
| What does my artifact emit for this real file? | `inseam plugin try <name>.wasm <file> [--llm-returns "…"] [--as-check]` |
| What does my connection answer for this real host? | `inseam plugin try <name>.wasm --config key=value… --enumerate ""` (or `--read`, `--describe`) — live, under the manifest's hosts |
| Is it fit to ship? | `inseam plugin check <name>.wasm` |
| Put it in this node's composition | `inseam plugin mount $PWD/<name>.wasm` |
| Is it running, and doing what I meant? | `inseam plugins`, `inseam index <dir>`, `inseam status`, `inseam query "…"`, `inseam expand <address>` |

## The contract

Each loaded seam is a WIT world; `inseam seams` prints the summary and
`--wit` the world itself (the same file as
<https://github.com/aiuitech/inseam/tree/main/crates/inseam-wasm-host/wit>).
Rules that hold on every seam — design around them:

- Effective claims = manifest claims ∩ exported claims — keep the two
  lists consistent or the plugin never runs.
- Host imports are manifest-gated; an ungranted call returns `Err`.
  `inseam capabilities` lists the imports, the manifest key that grants
  each, and whether this node can honor it right now — a plugin whose
  capability the node cannot grant mounts fine and runs its degrade path.
- **The network is a described request.** `fetch` takes a method, URL,
  headers, body; the node performs it, only to hosts your manifest's
  `hosts` list names (exact or `*.domain`), through its SSRF guard, with
  a body cap, a timeout, and a per-call budget. A non-success status is an
  answer you read, not an error. Request the *minimum* hosts: adding one
  later is capability widening every node re-approves. With `authorize:
  true` the node attaches the entry's OAuth grant bearer (needs `grant =
  true` in the manifest and `grant = "<id>"` on the entry) — you never
  see the token.
- **Degrade, never gate** (transform): on withheld capability or unusable
  input, return `Ok` with empty output. Never panic; an `Err` from your
  apply entry point is logged and produces nothing.
- **An error is the offline answer** (connection): when the network
  refuses or the host answers something unusable, return `Err` naming what
  failed — the sweep reports it. Never an empty listing (it would
  reconcile every source away), never a panic (a trap discards your
  instance).
- Instances: a transform gets a fresh instance per call — no state, no
  caching, no counting; the host meters your LLM budget. A connection is
  one instance per entry, configured once (`configure` receives the
  entry's `[entry.config.plugin]` as TOML), so cursors and small caches
  may live in it; every call still gets its own fuel and fetch budget.
- Output is hygiene-checked: on the transform seam a fragment's `parent`
  must index an earlier fragment in your own output, relations are from a
  fixed set, and inseam-defined mimetypes are refused. On the connection
  seam the bridge derives the host id from the kind and principal you
  describe (the kind must equal the manifest's `host_kind`), drops sources
  with malformed locators, and stamps every envelope's `observed` itself.

## Shape

`inseam plugin new <name> --claims <mimetypes>` (or `--seam connection
--kind <kind>`) writes all of this:

```
<name>/
  <name>.manifest.toml   # what an owner reviews; the bridge enforces it
  <name>.checks.toml     # golden checks, pre-shaped to the mandatory coverage
  src/lib.rs             # a Rust stub that degrades everywhere (any language works)
  wit/plugin.wit         # the contract, embedded from your binary
  fixtures/README.md     # what each fixture is and why — required
  README.md              # what it does, what it needs, the mount snippet
```

Manifest (request the MINIMUM you use; widening later triggers a
re-approval gate):

```toml
name = "<name>"
version = "0.1.0"
seam = "transform"
claims = ["image/png"]   # must overlap claims()
roots_only = true
kind = "enrichment"      # or "structural"

[capabilities]
llm = true
source_bytes = true
llm_call_budget = 25
hosts = []               # hosts `fetch` may contact; empty = no network
```

or, for a connection:

```toml
name = "<name>"
version = "0.1.0"
seam = "connection"
host_kind = "github"     # identity: describe-host() must export exactly this

[connection]             # effective = this AND what capabilities() exports
enumerates = true
change_feed = false
writable = false

[capabilities]
hosts = ["api.github.com", "raw.githubusercontent.com"]
grant = true             # `authorize` may attach the entry's grant bearer
```

## The TDD loop

Work in this order. Each step names the command that tells you whether it
worked; run it, read the report, then move on.

### 0. Look before you build

```sh
inseam claims <a file you intend to handle>   # who claims it today; complement, don't duplicate
inseam hosts                                  # which hosts are stewarded today (a connection adds one)
inseam capabilities                           # what this node grants: an LLM? which model? which grants?
inseam plugin new <name> --claims <mimetype,…>              # a transform
inseam plugin new <name> --seam connection --kind <kind>    # a connection
```

### 1. State the claim as checks (red)

Open `<name>.checks.toml` and replace every `REPLACE`: what the plugin
promises, as input → expected output shape. Schema:
<https://docs.inseam.io/plugins/validation>. The harness enforces a
minimum coverage, already shaped for you:

- **One check that proves the claim** — a substantive expectation
  (`fragment_contains`, `relation`, or `mimetype` for a transform;
  `locator_contains`, `content_type`, `hint_contains`, or `text_contains`
  for a connection), not just "something came out". This is the plugin's
  reason to exist, in one example.
- **One check that pins the degrade path** — a starved check: for a
  transform no `text`, no `llm_returns`, and `max_fragments` set (`0` for
  "emits nothing", or the fallback's shape); for a connection no
  `[[check.fetch]]` replies and `error = true` (or `max_sources`). This is
  what the plugin does on an offline node or with a spent budget; it must
  be deliberate, never accidental.
- Add a check per distinct behavior you implement (a second mimetype, the
  LLM refusing while text is present, a malformed input you handle).
  Don't pad: the harness's own hostile-input battery already covers
  "doesn't crash"; your checks cover "does what it says".

Each transform check's `mimetype` must fall inside your manifest's claims,
or the harness fails it. The LLM is always canned during checks
(`llm_returns` is the verbatim reply; absent means it refuses), and so is
the network: a connection check (and a fetching transform's) carries
`[[check.fetch]]` replies keyed by the exact URL your code asks — on hosts
your manifest names, never contacted — with a `body` or a `body_file`
fixture, a `status`, and `authorized = true` when the reply needs the
grant bearer (an unauthorized request gets 401). A connection check also
names the `call` (`enumerate` with a `root`, `read` or `describe` with a
`locator`) and the `[config]` the instance is configured with. Checks
prove plumbing and shape, never model or host quality — write the canned
replies as the host really answers, and assert on how your code shapes
them.

Fixtures (`bytes_file`, `body_file`) live in `fixtures/`, relative to the checks file,
and must be **tiny and well-formed** — the smallest valid PNG, a
three-line CSV. Every fixture gets a row in `fixtures/README.md`: the
harness's LLM is fake and the component is sandboxed, so fixture *content*
never matters, only that bytes reach the plugin and come back shaped
right. Every installing node downloads them; size is a cost multiplied
across nodes.

### 2. Build the stub and watch it fail

Set the manifest's capabilities to what you will call, build, and run the
gate:

```sh
cargo build --release --target wasm32-wasip2 && cp target/wasm32-wasip2/release/<name>.wasm <name>.wasm
inseam plugin check <name>.wasm
```

Expect `FAIL`, and read why: `static`, `mount`, `contract`, and
`mandatory coverage` are `ok` (the scaffold degrades everywhere); your
positive check fails with what it wanted and what actually came out —
`…; got 0 fragment(s)`. That line is your to-do list. If `mount` fails
instead, fix toolchain and claims before behavior — the effective-claims
line must match your intent (empty means manifest and `claims()`
disagree). The first run cold-compiles wasmtime — minutes, not a hang.

### 3. Implement until green, watching real output as you go

Implement the smallest code that turns the failing check green; rerun;
repeat. Between runs, look at what the plugin actually does to a real
file — or, for a connection, what it answers for a real host (a live
call through the guarded network, under your manifest's hosts; no grant,
so `authorize` refuses):

```sh
inseam plugin try <name>.wasm <file> [--llm-returns "a reply like your prompt asks for"]
inseam plugin try <name>.wasm --config repository=octo/hello --enumerate ""   # or --read <locator>, --describe <locator>
```

It applies the artifact once through the same bridge the harness uses —
detected mimetype, text if the type is text, bytes on offer, canned LLM —
and prints every fragment (mimetype, relation, parent, text), plus notes
when the bridge withheld something (`bytes withheld: the manifest does
not request source_bytes`). When the output looks right, `--as-check`
prints it as a `[[check]]` to paste into the checks file and tighten: the
fastest route from "I saw it work" to "it is tested".

Every failing golden line prints the expectation it missed and a one-line
account of the output, so you never need to add logging. A `contract`
failure means a code path traps instead of degrading — hunt the
`unwrap`/`expect`/indexing. A `mandatory coverage` failure means the
checks don't yet prove the claim or pin the degrade path — back to step 1.
Warnings are worth reading: non-deterministic output or an `Err` return is
allowed but costs every node that mounts you.

Done when `inseam plugin check <name>.wasm` ends `PASS` with the claims
line as intended. This is the exact harness registry CI runs at publish
and every node runs at install-time admission; never hand off a failing
plugin — admission refuses the mount, naming the failing check.

### 4. Prove it live

```sh
export INSEAM_DATA_DIR=$(mktemp -d)          # a throwaway node (omit to use your real one)
inseam plugin mount $PWD/<name>.wasm         # appends the entry; --id to name it
# a connection: fill in [entry.config.plugin] on the entry (inseam config prints it)
inseam plugins                               # fiber `<name>` active, effects listed
inseam index <dir-with-files-you-claim>      # transform: the sweep applies you to each root
inseam hosts && inseam index --host <id> ""  # connection: your host, swept
inseam status                                # fragment counts moved
inseam query "<words your plugin should surface>"
inseam expand <address-from-the-query>       # the fragments you hung off the source
```

If `inseam plugins` shows the fiber `failed`, the reason names the gate
(admission with the failing check, a claims mismatch, a cooldown) — fix
and re-run from step 3. If it is `active` but `inseam expand` shows
nothing from you, your effective claims don't cover the files you indexed
(`inseam claims <file>` shows who does), or the node withholds a
capability and you degrade (`inseam capabilities`). To unmount, delete the
entry from `composition.toml` (`inseam config` prints it).

## Hand off

Finish with: `inseam plugin check` at `PASS`; `fixtures/README.md`
describing every fixture; a `README.md` saying what the plugin does, what
capabilities it needs and why, and the mount snippet:

```toml
[[entry]]
id = "<name>"
plugin = "wasm:<path>/<name>.wasm"
```

(`inseam plugin mount` writes exactly this; a connection's entry also
needs `[entry.config.plugin]`), or distribute through a registry and
`inseam plugin install` — a registry release is signed by its publisher
(`cargo xtask plugin sign <name>` in the inseam repository), so every
installing node can prove who shipped it (`docs/plugins/registry.md`).
