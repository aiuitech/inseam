# Composition

A node's configuration is its **composition**: a declarative tree of plugin entries the [kernel](kernel.md) reconciles against. There is no imperative setup and no boot order — the composition says *what should be running with what config*, and reactive lifecycle plus confluence guarantee the running system converges to exactly that, whatever state it started from.

## Entries

The composition is a TOML tree of entries. Each entry:

- **id** — stable identity; the reconciler's diffing key, so edits restart exactly the entries they touch.
- **plugin** — which plugin: a linked plugin's name from the distribution, or a loaded artifact ref.
- **config** — the plugin's typed config, validated against its schema before the plugin runs.
- **disabled** — mount toggle.
- (later, with realms) **isolate** — scope a service key to this entry's subtree.

Entries nest into groups; a group is itself an ordinary entry, so subtrees can be toggled, shipped, and patched as units.

## Layers

A running node's composition is layered, later layers patching earlier ones by entry id:

1. **Distribution base** — each app crate ships the composition that makes it that product ([plugins](plugins.md)): the CLI's base mounts the filesystem connection, core transforms, finder, CLI transport.
2. **Node config** — the user's file: enable entries, override configs, add loaded plugins.
3. **Invocation overlays** — flag-level overrides for one run.

The same pure layering function answers `inseam config --resolved`, so what prints is what boots, by construction.

## Node profiles are compositions

The old standalone "index profile" dissolves: what made a phone a phone and a cloud node a cloud node was always plugin selection and plugin config — summarizer-only vs. every transform, short vs. long summaries, hashed vs. endpoint embedder, tight vs. no cutoff. Those dials live in the entries' configs now; a *profile* is just a named base composition a distribution ships. The per-node asymmetry [discovery](discovery.md) promises is expressed entirely in composition, and the four invalidation tiers of [index maintenance](index-maintenance.md) become properties of *which entry's config changed*: finder config is query-time, sweep budgets are run-metering, transform configs are shape, embedder config is embedding.

The **shape stamp** generalizes accordingly: each source records a digest of the transform entries that built its subtree, plus the subtree's mimetype inventory — so removing or editing a transform dirties the sources it touched, and adding one dirties only sources it could claim ([index-maintenance](index-maintenance.md) has the mechanics). Plugin ecosystem churn is absorbed by the same sweep as any config edit, with no new invalidation machinery.

## Reconciliation

Composition edits apply transactionally per entry: config-only changes update the fiber in place (or restart it, per the plugin's declaration); plugin/structure changes dispose-then-mount with rollback to the previous entry on failure. A failed entry is contained — the rest of the tree keeps running, and the error names the entry. Hot reload of the composition file is the ordinary path, not a special mode.

## Paths not taken

- **A monolithic profile struct in core** (the previous design). Every field was secretly some module's config plus hand-written invalidation-tier bookkeeping; the composition gives each plugin its own schema-validated config and derives the tiers from entry identity.
- **Row order as load order.** Activation is service-availability-driven ([kernel](kernel.md)); order in the file carries no semantics, which is what lets layers insert entries freely.
- **YAML with embedded expressions.** `dsh` interpolates JS in config values; powerful, but a config file that executes is a capability we don't want to hand the composition layer of a security-sensitive node. Dynamic values come from the environment through explicit, declared config fields (`key_env`-style), which is already the pattern.

## Settled since

- **Patch semantics: whole-config replacement per id.** A patch entry's `config` replaces the target's wholesale; `plugin` and `disabled` override when present; unknown ids append (and a patch that targets nothing *and* names no plugin is a loud error). `extends` sugar can come later without breaking files.
- **Loaded-plugin installation state**: the composition entry holds the artifact ref; the trust bookkeeping (first-seen clock per content hash, approved capability summary) lives in the wasm host's versioned state namespace — discardable, re-derivable, never synced.
- **A missing secret parks, never crashes and never degrades silently.** The entry that needs the key fails contained, its dependents stay pending, and transports open the node anyway, surfacing per-entry health (state + error + missing keys) so a UI can say "enter the key to enable". We considered tying secrets to individual seams so a keyless plugin could keep serving its other seams — rejected: optional injection plus capability facts already express partial degradation (the summarizer falls back to extractive without `llm`), and the one place that can't degrade that way, the embedder, *shouldn't*: falling back from `endpoint` to `hashed` embeddings is an embedding-identity change, an index-invalidation decision the owner must make explicitly in the composition, not a fallback taken silently at boot.
- **GUI secret storage: the platform keychain feeds the same env indirection.** A GUI app has no shell environment, so the `key_env` pattern would strand it. Rather than adding a second secret channel to core, the app stores name→value pairs in the OS keychain and exports them into its own process environment before node open — core keeps exactly one rule (secrets arrive as environment variables the composition names), and each transport supplies the environment its platform makes natural: the shell for the CLI, the keychain for the app.

## Open questions

- Owner operations that edit the composition at runtime (enable/disable a connection from the macOS app) and how they write back through the layering.
