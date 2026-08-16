# Plugin Registry

Registry v0 is the repository's `plugins/` tree
([design/registry.md](../../design/registry.md)): `registry.toml` (the
sha256 index), `advisories.toml` (yanked versions), and one directory per
plugin — layout in `plugins/README.md`.

## Installing

```sh
inseam plugin install ocr                      # from the repo's GitHub tree
inseam plugin install ocr --registry ~/ws/inseam/plugins   # any checkout/dir
INSEAM_REGISTRY=https://…/plugins inseam plugin install ocr
```

Install: fetch `registry.toml` → refuse anything in `advisories.toml` →
fetch the artifact and verify its sha256 against the index → fetch the
manifest, golden checks, and their fixtures → run the conformance harness
([validation.md](validation.md)) → append the entry to the node's
`composition.toml` under `<data-dir>/plugins/<name>/`. Any failure aborts
before the composition is touched. Mount-time gates (admission, release
cooldown, capability widening) still apply — install is convenience, not
trust.

There is no proxy and no registry server: integrity is the committed hash
in a reviewed tree, verified locally against whatever channel served the
bytes.

## Publishing

A PR touching `plugins/` — `.github/workflows/plugins.yml` is the merge
gate: locked reproducible rebuild must match the committed artifact,
`registry.toml` hashes must match, `inseam plugin check` must pass, and an
AI security review examines the diff (capability overreach, exfiltration
through LLM prompts or emitted fragments, prompt-injection staging,
obfuscation). A weekly scheduled sweep re-audits the whole tree and files
issues that become `advisories.toml` entries. Yanking is a PR appending an
advisory.
