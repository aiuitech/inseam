# CLI

The `inseam` binary is a distribution: the first-party linked plugins, the WASM plugin host, and a base composition. Every command is a thin call on the node's `operations` interface ([design/node-api.md](../design/node-api.md)). Global flags: `--data-dir` (`INSEAM_DATA_DIR`), `--composition` (`INSEAM_COMPOSITION`); see [configuration.md](configuration.md).

```sh
inseam index ~/Data                 # index a directory (read-only): picks up new, changed,
                                    # and deleted files, plus config changes; --rebuild forces
inseam query "kitchen renovation"   # ranked results with summaries and hints; --json for raw output
inseam expand <address>             # one source's fragments, relations, connected entities
inseam scan <address> --start 120 --end 160   # read a line range of a source
inseam fetch <address>              # the whole source
inseam agent "when did I ...?"      # a live LLM using query/expand/scan/fetch as tools
inseam models [--embeddings]        # the endpoint's model catalog, cheapest first
inseam status                       # store stats, embedding info, pending re-embeds
inseam plugins                      # every running plugin, its state, and its live effects
inseam plugin check <artifact.wasm> # validate a plugin (plugins/validation.md)
inseam plugin install <name>        # fetch from the registry, verify, check, mount (plugins/registry.md)
inseam config [--resolved]          # the composition; --resolved = exactly what the node boots with
```

If part of the configuration can't start (say the default `endpoint` embedder with no API key set), the node prints a warning naming each waiting entry and what it's missing. Commands that need those parts fail with the same message; everything else keeps working. `inseam plugins` is the diagnostic view. The quickest offline setup is a `composition.toml` that switches the embedder to `hashed` ([configuration.md](configuration.md) has the example).
