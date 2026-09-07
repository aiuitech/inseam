# plugins/ — the loaded plugin registry (v0)

This directory is both the **home of first-party loaded plugins** and the
**registry** nodes install from (`design/registry.md`): a reviewed git tree
whose integrity anchors are the committed `registry.toml` hashes and each
plugin's publisher-signed release record, with CI gates on every merge.
`inseam plugin install <name>` fetches from this tree (GitHub raw by
default, any checkout via `--registry <path>`), verifies the publisher's
signature and every file's sha256 against the signed record *and* the
index, runs the conformance harness, and mounts.

## Layout

```
plugins/
  registry.toml        # the index: name, version, publisher, artifact path, sha256
  publishers.toml      # the publisher roster: ids and minisign public keys — a trust root
  advisories.toml      # yanked/flagged versions; consulted by install and cooldown
  <name>/              # one directory per plugin
    Cargo.toml         # standalone crate (NOT a workspace member)
    src/lib.rs         # the component source
    <name>.wasm        # the built artifact, committed
    <name>.manifest.toml   # seam, declarations, requested capabilities (owner-reviewed)
    <name>.checks.toml     # golden checks: the plugin's promised behavior, as data
    <name>.release.toml    # the signed release record: sha256 of every file above and every fixture
    <name>.release.toml.minisig   # the publisher's signature over the record
    fixtures/          # byte fixtures the golden checks reference
      README.md        # what each fixture is and why — REQUIRED, see below
    README.md          # what the plugin does, what it needs
```

Two seams ship here: `ocr` is the reference **transform** (image OCR
through the granted vision LLM), `github` the reference **connection**
(one repository as a host, reached through the guarded `fetch` under its
manifest's host allow list). Authoring is skill-driven:
`skills/inseam-loaded-plugin` walks an agent (or a person) through the
whole loop with the CLI as companion (`inseam plugin new` → `try` →
`check` → `mount`; `docs/plugins/authoring-cli.md`), ending in a green
`inseam plugin check`.

## The test dataset: checks, fixtures, and the harness's built-ins

Three layers of data exercise every plugin, from generic to specific
(`docs/plugins/validation.md` has the full harness contract):

1. **The harness's built-in battery** (in `inseam-wasm-host/src/check.rs`
   and `check_connection.rs`, not on disk): synthetic hostile inputs every
   plugin faces regardless of what it claims — no text, refused
   capabilities, a refusing network, garbage bytes
   (`[0x00, 0xFF, 0x13, 0x37]`), a host answering garbage with a 500, an
   unparseable config, a canned-LLM sentinel string, plus a built-in 1×1
   PNG for byte-wanting image plugins. These exist to prove the *contract*
   (degrade, never trap) with zero knowledge of the plugin.
2. **Per-plugin golden checks** (`<name>.checks.toml`): declarative
   input → expected-output-shape cases, executed through the real bridge
   with a canned LLM and a canned network (`[[check.fetch]]` replies keyed
   by URL — on hosts the manifest names, never contacted). They are the
   plugin's enforced tests — **mandatory**: a plugin with none, or with
   checks that neither prove its claim nor pin its degrade path, fails the
   harness (`docs/plugins/validation.md` has the coverage rule). Written
   first during authoring, run by `inseam plugin check`, re-run by CI on
   merge, and re-run by every installing node at admission. Because they
   are data, not code, nothing in them needs to be trusted.
3. **Fixture files** (`<name>/fixtures/`): the raw bytes golden checks hand
   to `source-bytes`, or the canned reply bodies they serve through
   `fetch`. Every fixture directory carries its own `README.md`
   explaining each file and why it exists — fixtures accumulate fast as
   the registry grows, and an undocumented fixture is unreviewable. Keep
   fixtures minimal (the checks assert plumbing and shape, not model or
   host quality — the LLM and the network are always canned), and
   remember they are signed in the release record and downloaded by every
   `inseam plugin install`, so size is a cost multiplied across nodes.

Linked-tier plugins are tested by the shared conformance harness
(`crates/inseam-conformance`), whose workspace suite
(`crates/inseam-plugins/tests/conformance.rs`) sweeps every registered
factory automatically — the same hostile-input battery, and each
transform's own golden checks (`crates/inseam-plugins/src/<plugin>/
<name>.checks.toml`, same schema and coverage rule as here). A linked
plugin cannot be added without inheriting both.

## Publishing and CI

A version enters the registry only through a PR touching this tree. The
publisher signs first, on their own machine — the only place their key
exists (`docs/plugins/registry.md`):

```sh
cargo xtask plugin keygen --publisher <id>   # once; enrolls the public key in publishers.toml
cargo xtask plugin sign <name>               # <name>.release.toml + .minisig; updates registry.toml
```

The PR then runs (`.github/workflows/plugins.yml`):

- rebuild every plugin from source and require the committed `.wasm` to
  match byte-for-byte (pinned toolchain, committed `Cargo.lock`);
- `inseam plugin verify --registry plugins`: every publisher signature,
  every signed file's sha256, and the index agreeing with each record;
- run `inseam plugin check` — the same harness nodes run at admission;
- an AI security review of the diff (capability requests vs. what the code
  does — `hosts` above all — exfiltration through LLM prompts, fetch
  requests, or emitted fragments, obfuscation), plus a scheduled periodic
  sweep of the whole tree.

The AI review is **advisory** — the merge gate is human CODEOWNERS review
(`.github/CODEOWNERS`), which covers `registry.toml`, `publishers.toml`,
and `advisories.toml` as trust roots. Workflows are SHA-pinned, run
untrusted plugin code only in a secret-free read-only job, hold no signing
key, and never use `pull_request_target`; the full supply-chain posture is
in `design/registry.md`.

Yanking a version is a PR adding an `advisories.toml` entry.
