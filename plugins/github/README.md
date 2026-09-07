# github

Install: `inseam plugin install github` (verifies, checks, and mounts — see
`docs/plugins/registry.md`), or mount a local build as below. Golden checks
live in `github.checks.toml` (fixtures documented in `fixtures/README.md`);
validate any change with `inseam plugin check plugins/github/github.wasm`.

The reference **loaded connection**: one GitHub repository as a host of
kind `github`, reached through the node's guarded `fetch` under the
manifest's host allow list (`api.github.com`, `raw.githubusercontent.com`)
— the component holds no socket and no token. Enumeration is one tree call
(every blob under the named scope becomes a source whose locator is its
path, content type from its extension); reads come from the raw content
host at the configured ref; `describe` asks the contents API for one path.
Public repositories need nothing else. For a private repository, or higher
rate limits, name an OAuth grant on the entry and set `authorize = true`:
the node attaches the grant's bearer to every request.

## Mounting

```toml
[[entry]]
id = "github-hello"
plugin = "wasm:plugins/github/github.wasm"
[entry.config]
roots = [""]                    # the whole repository is one scope; "docs/" is another
# grant = "github"              # an oauth grant (docs/plugins/oauth.md), with authorize = true below
# cooldown_days = 7
[entry.config.plugin]
repository = "octo/hello"
ref = "main"                    # branch, tag, or commit; HEAD for the default branch
# authorize = true              # send the entry's grant bearer with every request
# files_max = 5000              # blobs listed per enumeration
# file_bytes_max = 2000000      # larger blobs are listed, never read
```

Then `inseam hosts` shows `github-…`, and `inseam index --host github-… ""`
(or a subdirectory as the root) sweeps it.

## Develop

```sh
cargo build --release --target wasm32-wasip2 && cp target/wasm32-wasip2/release/github.wasm github.wasm
inseam plugin check github.wasm
inseam plugin try github.wasm --config repository=octo/hello --enumerate ""   # live, under the allow list
```
