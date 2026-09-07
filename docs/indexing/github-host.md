# The GitHub Host

`plugins/github` is the reference **loaded connection** ([../plugins/loaded.md](../plugins/loaded.md)): a WASM component that stewards one GitHub repository as a host of kind `github` ([connections.md](connections.md)), registered into the same registry as the filesystem and Google hosts. It holds no socket and no token — every request is a `fetch` the node performs under the manifest's host allow list (`api.github.com`, `raw.githubusercontent.com`), through the same guard the [web host](web-host.md) uses.

Install it from the registry, or mount a local build, and give it a repository:

```sh
inseam plugin install github
```

```toml
[[entry]]
id = "github"
plugin = "wasm:<data-dir>/plugins/github/github.wasm"   # what install wrote
[entry.config]
roots = [""]                    # the whole repository is one scope; "docs/" is another
# grant = "github"              # an oauth grant (../plugins/oauth.md) for private repositories, with authorize = true
[entry.config.plugin]
repository = "octo/hello"
ref = "main"                    # branch, tag, or commit; HEAD for the default branch
# authorize = true              # send the entry's grant bearer with every request
# files_max = 5000              # blobs listed per enumeration
# file_bytes_max = 2000000      # larger blobs are listed, never read
```

Then:

```sh
inseam hosts                                # github-…  github  …  enumerates read-only  GitHub · octo/hello
inseam index --host github-<id> ""          # the whole repository; "docs/" for a subdirectory
inseam query "how to paint a shed"
inseam fetch inseam://github-<id>/README.md
```

## What it does

- **Identity.** The host id is derived by the bridge from the kind and the repository name (`derive_host_id("github", "octo/hello")`), so two nodes stewarding one repository mint one host and the component cannot forge another's.
- **Enumeration** is one tree call (`git/trees/<ref>?recursive=1`): every blob under the named scope becomes a source whose locator is its path, with a content type from its extension and its size as the envelope length; tree entries are not sources. The scope's locator prefix is the directory, so vanished files reconcile.
- **Reads** come from the raw content host at the configured ref, capped at `file_bytes_max`; **describe** asks the contents API for one path.
- **Private repositories.** Name an OAuth grant on the entry and set `authorize = true`: the bridge attaches the grant's bearer to every request. The token never crosses into the component, and never rides a redirect to another host.
- **Offline.** With no network, enumeration is an error the sweep reports — never a trap, and never an empty listing that would reconcile every source away.

Its golden checks (`plugins/github/github.checks.toml`) are the network as data: canned GitHub replies keyed by the real URLs, so `inseam plugin check` proves the plumbing without contacting anything ([../plugins/validation.md](../plugins/validation.md)).
