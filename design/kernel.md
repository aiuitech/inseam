# Kernel

Everything in inseam is a [plugin](plugins.md) except what this file describes. The kernel is the smallest thing that can make that sentence true: it **runs plugins** and it **owns persistent structure**. It knows no service, no file format, no ranking algorithm, no transport, no protocol — those are all plugins. What it does own, it owns completely: plugins are added, removed, reconfigured, and replaced at runtime through one lifecycle, and the store's schema never migrates.

The substrate adopts the context model of the **Cordis** meta-framework (the cordiverse paper's revertible effects + reactive dependencies; proven at scale by Koishi's ~4000-plugin ecosystem and DeepSeek's `dsh` harness, both studied closely for this design), adapted to Rust and to inseam's trust model.

## Responsibility 1: the substrate

How plugins exist and compose. Five pieces.

### Services

A **service** is a typed interface (a Rust trait) bound to a well-known key — `host`, `transforms`, `embedder`, `llm`, `finder`, `operations`, … ([services](services.md) is the catalog). At most one provider binds a key at a time; binding a taken key is a loud error. Three roles, deliberately separable into different plugins:

- the **definition**: the trait plus its vocabulary types, owning the key;
- a **provider**: a plugin that binds an implementation to the key;
- **consumers**: plugins that declare the key and receive a typed handle.

Definitions expose **capability facts** — declared properties of whatever provider is mounted (does this connection offer a change feed? does this embedder work offline? what does this LLM's token cost look like?) — so consumers branch on facts, never on provider identity. Swapping a provider must never require touching a consumer.

### Plugins, fibers, reactivity

A plugin declares what it **injects** (service keys it requires), what it **provides**, and a typed, schema-validated config. The kernel instantiates it as a **fiber** — one running instance with a lifecycle: *pending → active → unloading → inactive*, with failure landing the fiber inactive-with-error, alone; siblings and dependents-of-siblings are untouched.

Lifecycle is **reactive, not ordered**. There is no boot sequence to arrange: a fiber activates when everything it injects is provided, and unloads when something it injects withdraws. A dependency changing *provider identity* — not just disappearing — restarts its consumers against the new implementation; that one rule makes provider hot-swap correct everywhere. Two hard-learned rules from the systems we studied are law here:

- **Missing required dependencies are loud.** When the composition settles, every fiber still pending names its missing keys as a boot error. Silent indefinite waiting is opt-in per entry, never the default.
- **A withdrawn service stays readable through its consumers' teardown.** Deactivation is ordered so a consumer can use the very capability it is losing to clean up after itself (hand connections back, flush through the store handle it holds).

### Effects

The kernel's one mutation rule: **every change a plugin makes to shared state is an effect, registered together with its undo at the moment it is made.** Registering a transform, subscribing to an event, binding a service, spawning a task — each returns a disposer the fiber accumulates. Removal is thereby *derived, not authored*: unloading runs the accumulated disposers in reverse, and there is no separate uninstall path to write, drift, or forget. Effects attempted during teardown are refused, so cleanup cannot leak new state past the unload. The kernel keeps a labelled tree of each fiber's live effects — "what does this plugin currently own right now" is a query, not archaeology.

### Events

A typed event bus where each event declares its **dispatch mode** as part of its contract: notify (fire-and-forget or awaited-parallel), first-answer-wins, or **waterfall** — listeners wrap each other and the built-in behavior middleware-style, and may veto or short-circuit. Waterfalls are the policy seam of the whole system: budget metering, boundary filtering, audit, retries are waterfall listeners on the seams they govern, not features of the things they govern. Listener registration is an effect like any other. In Rust the waterfall contract is enforced structurally — a listener receives a `Next` value it must either consume or return a decision from, so "forgot to call next" cannot compile.

### Composition and confluence

Which plugins run, with what config, in what nesting, is the node's [composition](composition.md) — declarative data the kernel reconciles against, at boot and on every edit. The design target, proven for this model in the cordiverse paper, is **confluence**: whatever history of loads, unloads, config edits, and failures a node has been through, its quiescent state equals a fresh boot of the final composition. Dynamic history leaves no trace. This is the same stance [index maintenance](index-maintenance.md) takes toward the index — a reconciling sweep converging on declared intent — applied to the plugin tree. One philosophy, two layers: **converge, never migrate**.

## Responsibility 2: the store

The kernel owns the data model — addresses, envelopes, sources, fragments, relations, properties — and **all persistent structure**: the catalog, the semantic graph, the search surfaces (FTS + vectors), and plugin state. No plugin ever issues DDL, and there are **no data migrations, anywhere, ever**. The rules that make that possible:

- **The catalog is source of truth and is tiny.** Address + envelope + properties, exactly what [addressing](addressing.md) specifies. It syncs network-wide, so its schema is the most stable surface in the system; envelopes grow by bounded key-value hints, not columns.
- **Everything else is derived.** Graph and search surfaces are rebuildable from catalog + fetches ([discovery](discovery.md)); a kernel schema-version bump is a rebuild trigger, not a migration script.
- **Plugins extend by vocabulary, not schema.** A new mimetype, relation kind, property namespace, or fragment shape is *data flowing through the fixed structure*. Teaching the index about videos adds no tables.
- **Plugin state is namespaced and versioned.** The kernel provides each plugin a keyed state namespace declared with a version; on mismatch the namespace is discarded and rebuilt. Plugin state must therefore be derived or re-obtainable — credentials and configuration live in the composition and credential files, never the store.
- **Search primitives are kernel; ranking is not.** The kernel exposes FTS search, vector search, and graph reads as primitives, and records the embedding identity the vectors were built with. What to do with seed matches — fusion, propagation, rollup — is the `finder` service, a plugin.

## Security posture

Two trust tiers, detailed in [plugins](plugins.md): **linked plugins** are trusted in-process Rust whose declared injections form an auditable capability manifest — the substrate makes undeclared access unrepresentable, which is discipline and reviewability, not a sandbox. **Loaded plugins** are WASM components behind a real execution boundary, bridged onto the same service seams with capabilities attenuated per their manifest. The kernel's contribution is that both tiers pass through one mediation point: services are the only way to reach anything, so attenuation (budgets, host allowlists, boundary property filters) is interception on a seam, not per-feature code.

## Paths not taken

- **The fat core (this project's own first architecture).** Indexer, transforms, finder, LLM client, embedder, host access, and transports as core modules, with plugins bolted on at the edges for connectors. Rejected: every one of those modules was already growing config knobs and swap points — provider seams by another name, each hand-rolled. The substrate does it once, uniformly, with lifecycle and teardown for free, and the core stops being a privileged author of features it should merely host.
- **A single all-WASM plugin tier.** Maximal uniformity, rejected: the sandbox tax (instantiation, copying across the boundary, WIT ceremony) on every transform application and finder call contradicts the performance budget, and first-party code gains nothing from sandboxing itself. Trust tiers are a property of provenance, not of the plugin model.
- **Dynamic native loading (`dlopen`).** No sandbox, no ABI stability, per-platform artifacts. Linked plugins are statically linked and *activated* by composition; dynamism at runtime is lifecycle, not code loading. Dynamic code arrival is exactly what the WASM tier is for.
- **A migrations framework.** The industry default, rejected on principle: migrations exist to preserve mutable authoritative state through schema change, and the design deliberately has almost no such state. Keeping the catalog minimal and everything else derived is cheaper than maintaining migration machinery forever.
- **Stringly-typed hooks and untyped service lookup.** The dynamic-language versions of this model pay for openness with runtime `undefined` and compensating CI gates. Rust lets declarations be the type system's problem; we take that trade everywhere it's available.

## Open questions

- **Realms**: Cordis supports isolating the same service key to different providers for different subtrees (two `llm` endpoints, an isolated loaded-plugin group). Almost certainly wanted eventually (per-requester attenuation may ride on it); deferred until a concrete composition needs it.
- **How much lifecycle to verify at compile time**: injection sets are static, so pending-forever cycles are statically detectable; how far to push (proc-macro derived handles, typestate on fiber phases) is an implementation question.
- **Loaded plugins and events**: the bus is host-side only; whether to project it into WIT or keep components request/response-only with bridges listening on their behalf.

## Settled since

- **Event representation**: a typed bus keyed by event type, with the dispatch mode declared per type — `Notify` (fan-out), `Guard` (monotonic deny: every listener asked, any deny wins, no force-allow), and `Waterfall` (a `Next` token the listener must consume or replace with its own decision). Budget metering and boundary filtering landed as guards; waterfall remains the wrapping seam.
- **Dependency appearance restarts consumers.** Not just provider-identity *change*: when a key a fiber injects (even optionally) becomes available, the fiber restarts against it. This is what makes composition order carry no semantics for optional dependencies too; a bounded restart count per reconcile turns genuine provide-cycles into a contained fiber failure instead of a livelock.
