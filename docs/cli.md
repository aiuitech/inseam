# CLI

The `inseam` binary is a distribution: the first-party linked plugins, the WASM plugin host, and a base composition. Every command is a thin call on the node's `operations` interface ([design/node-api.md](../design/node-api.md)). Global flags: `--data-dir` (`INSEAM_DATA_DIR`), `--composition` (`INSEAM_COMPOSITION`); see [configuration.md](configuration.md).

```sh
inseam serve                       # authenticated owner HTTP API; --web-dir also serves the Vite build
inseam index ~/Data                 # index a scope of a host (read-only): picks up new, changed,
                                    # and deleted sources, plus config changes; --rebuild forces.
                                    # --host <id> names the host once several are mounted;
                                    # an address (inseam://<host>/<root>) names it too
inseam hosts                        # the hosts this node stewards and what each connection supports
inseam grants                       # the OAuth grants this node holds and where each stands
inseam authorize <grant>            # sign in to a provider: prints the URL, waits for the browser
inseam revoke <grant>               # forget a grant's tokens; its hosts withdraw
inseam query "kitchen renovation"   # ranked results with summaries and hints; --json for raw output
inseam expand <address>             # one source's fragments, relations, connected keyed fragments (entities)
inseam scan <address> --start 120 --end 160   # read a line range of a source
inseam fetch <address>              # the whole source
inseam agent "when did I ...?"      # a live LLM using query/expand/scan/fetch as tools
inseam models [--embeddings]        # the endpoint's model catalog, cheapest first
inseam status                       # store stats, sizes (on disk; content cataloged), embedding info, pending re-embeds
inseam plugins                      # every running plugin, its state, and its live effects
inseam seams [--wit]                # seams that take loaded plugins; --wit prints the contract (plugins/authoring-cli.md)
inseam capabilities                 # what a manifest may request, and what this node grants right now
inseam claims <mimetype|path>       # which transforms on this node claim an input
inseam plugin new <name> --claims … # scaffold a plugin whose first check is red for the right reason
inseam plugin try <artifact> <file> # apply a plugin to one real file and print what it emits (--as-check)
inseam plugin check <artifact.wasm> # validate a plugin (plugins/validation.md)
inseam plugin mount <artifact.wasm> # append a local artifact to the composition
inseam plugin install <name>        # fetch from the registry, verify, check, mount (plugins/registry.md)
inseam config [--resolved]          # the composition; --resolved = exactly what the node boots with
```

`inseam serve` requires `INSEAM_OWNER_TOKEN` or `--owner-token` with at least
32 bytes. It listens on `127.0.0.1:7337` by default. A browser may index only
roots the operator names with repeated `--index-root id=/absolute/path` flags
or comma-separated `INSEAM_INDEX_ROOTS`. `--cookie local-http` or
`INSEAM_COOKIE_SECURITY=local-http` permits the session cookie on an HTTP
development server; hosted nodes keep the secure default and terminate TLS in
front of the listener. `--public-url` (`INSEAM_PUBLIC_URL`) is the origin
owners reach the node at — what OAuth providers redirect back to when a grant
is authorized from the web console; it is derived from `--bind` when unset.
A serving node also applies composition edits submitted through the owner
API — installing a loaded plugin from the console mounts it into the running
node, while `plugin mount` / `plugin install` from a shell take effect on
the next boot. See [architecture/hosted-node.md](architecture/hosted-node.md).

If part of the configuration can't start (say the default `endpoint` embedder with no API key set), the node prints a warning naming each waiting entry and what it's missing. Commands that need those parts fail with the same message; everything else keeps working. `inseam plugins` is the diagnostic view. The quickest offline setup is a `composition.toml` that switches the embedder to `hashed` ([configuration.md](configuration.md) has the example).
