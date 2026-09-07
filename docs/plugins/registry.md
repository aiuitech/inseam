# Plugin Registry

Registry v0 is the repository's `plugins/` tree ([design/registry.md](../../design/registry.md)): `registry.toml` (the index: name, version, publisher, artifact, sha256), `publishers.toml` (the publisher roster: ids and minisign public keys), `advisories.toml` (yanked versions), and one directory per plugin carrying its **signed release record** beside the artifact — layout in `plugins/README.md`.

## Installing

```sh
inseam plugin install ocr                      # from the repo's GitHub tree
inseam plugin install github --registry ~/ws/inseam/plugins   # any checkout/dir
INSEAM_REGISTRY=https://…/plugins inseam plugin install ocr
inseam plugin verify [name] --registry …       # prove without installing; every plugin when no name
```

What install does, in order, refusing at the first disagreement:

1. fetch `registry.toml`, find the entry, and refuse anything `advisories.toml` yanks;
2. fetch `publishers.toml` and look up the entry's `publisher` — an id the roster does not list is a refusal;
3. fetch `<name>.release.toml` and `<name>.release.toml.minisig`, and verify the signature against the publisher's key **before parsing** the record;
4. cross-check the record against the index: same name, same version, same publisher, same artifact sha256;
5. fetch every file the record lists — the artifact, the manifest, the golden checks, each fixture — and check each against its signed sha256; every fixture the checks reference must be in the record;
6. run the validation harness ([validation.md](validation.md));
7. write the files, the record, and the signature under `<data-dir>/plugins/<name>/` and add the entry to the node's `composition.toml` (a connection's entry gets a commented `[entry.config.plugin]` stub to fill in).

Any failure stops before the composition is touched. The mount-time gates (admission, release cooldown, capability widening) still apply afterward — install is a convenience, not a grant of trust. `verify` runs steps 1 through 5 and prints one line per plugin; it exits nonzero on any failure, which is what the registry's CI runs.

There is no registry server: integrity comes from two anchors that must agree — the sha256 committed in the reviewed index, and the sha256 the publisher signed — verified locally against whatever channel actually served the bytes. Either anchor catches a substitution the other missed.

## Publishing

Publishing is a PR touching `plugins/`. Before opening it, a publisher signs the release from their own machine, where the only copy of their key lives:

```sh
cargo xtask plugin keygen --publisher inseam   # once: an encrypted minisign key at ~/.config/inseam/publisher.key,
                                               # its public half enrolled in plugins/publishers.toml
cargo xtask plugin sign github                 # <name>.release.toml + .minisig, and the registry.toml entry
                                               # (version from the manifest, artifact sha256, publisher)
```

`sign` hashes every file the release is made of — the artifact, the manifest, the checks, and each fixture the checks name — so a fixture or a check cannot change under a signature. The passphrase is prompted for, or read from `INSEAM_PUBLISHER_PASSWORD` in a maintainer's shell; CI never holds a key. A new publisher is a PR adding their key to `publishers.toml`, which is a trust root under CODEOWNERS review like the index and the advisory feed.

`.github/workflows/plugins.yml` is the merge gate: a locked, reproducible rebuild must match the committed artifact, `inseam plugin verify --registry plugins` must prove every signature and every hash, `inseam plugin check` must pass, and an AI security review examines the diff (asking for more capability than needed — hosts above all — leaking data through LLM prompts, fetch requests, or emitted fragments, staged prompt injection, obfuscation). A weekly scheduled sweep re-audits the whole tree and files issues that become `advisories.toml` entries. Yanking a version is a PR adding an advisory.
