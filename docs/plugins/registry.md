# Plugin Registry

Registry v0 is the repository's `plugins/` tree ([design/registry.md](../../design/registry.md)): `registry.toml` (the sha256 index), `advisories.toml` (yanked versions), and one directory per plugin — layout in `plugins/README.md`.

## Installing

```sh
inseam plugin install ocr                      # from the repo's GitHub tree
inseam plugin install ocr --registry ~/ws/inseam/plugins   # any checkout/dir
INSEAM_REGISTRY=https://…/plugins inseam plugin install ocr
```

What install does: fetch `registry.toml` → refuse anything listed in `advisories.toml` → fetch the artifact and check its sha256 against the index → fetch the manifest, golden checks, and their fixtures → run the validation harness ([validation.md](validation.md)) → add the entry to the node's `composition.toml` under `<data-dir>/plugins/<name>/`. Any failure stops before the composition is touched. The mount-time gates (admission, release cooldown, capability widening) still apply afterward — install is a convenience, not a grant of trust.

There is no registry server: integrity comes from the committed hash in a reviewed tree, verified locally against whatever channel actually served the bytes.

## Publishing

Publishing is a PR touching `plugins/`. `.github/workflows/plugins.yml` is the merge gate: a locked, reproducible rebuild must match the committed artifact, the `registry.toml` hashes must match, `inseam plugin check` must pass, and an AI security review examines the diff (asking for more capability than needed, leaking data through LLM prompts or emitted fragments, staged prompt injection, obfuscation). A weekly scheduled sweep re-audits the whole tree and files issues that become `advisories.toml` entries. Yanking a version is a PR adding an advisory.
