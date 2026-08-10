# CLI

The `inseam` binary invoking operations directly against the local node — the first transport adapter ([design/node-api.md](../design/node-api.md)). Global flags: `--data-dir` (`INSEAM_DATA_DIR`), `--profile` (`INSEAM_PROFILE`); see [configuration.md](configuration.md).

```sh
inseam index ~/Data                 # reconciling sweep (read-only): new/changed/vanished sources,
                                    # profile changes, pending re-embeds; --rebuild forces
inseam query "kitchen renovation"   # ranked addresses + summaries + hints; --json for the raw operation response
inseam expand <address>             # fragments, relations, connected entities
inseam scan <address> --start 120 --end 160
inseam fetch <address>
inseam agent "when did I ...?"      # live LLM driving query/expand/scan/fetch as tools
inseam models [--embeddings]        # OpenRouter catalog, cheapest first, for profile choices
inseam status                       # source/fragment/relation/entity counts, embedding config
```

`query`, `expand`, and `scan` take `--json` to emit the exact operation-layer response — what any other adapter (HTTP, MCP) would carry.

Typical loop, same ladder the agent climbs:

1. `inseam query "…"` — read scores, summaries, hints.
2. `inseam expand` a promising address, or `inseam scan` the lines a hint's extent points at.
3. `inseam fetch` only when a slice isn't enough.
