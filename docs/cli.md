# CLI

The `inseam` binary is a distribution: the native plugin set + the wasm plugin host + a base composition, with every command a thin call on the `operations` seam ([design/node-api.md](../design/node-api.md)). Global flags: `--data-dir` (`INSEAM_DATA_DIR`), `--composition` (`INSEAM_COMPOSITION`); see [configuration.md](configuration.md).

```sh
inseam index ~/Data                 # reconciling sweep (read-only): new/changed/vanished sources,
                                    # composition changes, pending re-embeds; --rebuild forces
inseam query "kitchen renovation"   # ranked addresses + summaries + hints; --json for the raw operation response
inseam expand <address>             # fragments, relations, connected entities
inseam scan <address> --start 120 --end 160
inseam fetch <address>
inseam agent "when did I ...?"      # live LLM driving query/expand/scan/fetch as tools
inseam models [--embeddings]        # endpoint model catalog, cheapest first
inseam status                       # store stats + embedding identity + re-embed pending
inseam plugins                      # the fiber tree: each entry, its state, its live effects
inseam config [--resolved]          # the composition; --resolved = the layered result that boots
```

A composition that cannot fully settle (e.g. the default `endpoint` embedder with no API key set) prints a warning naming each waiting entry and its missing services; commands that need those seams fail with the same message, everything else keeps working. `inseam plugins` is the diagnostic view. The quickest offline setup is a `composition.toml` switching the embedder to `hashed` ([configuration.md](configuration.md) has the example).
