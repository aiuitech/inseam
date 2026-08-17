# The Kernel

`crates/inseam-kernel` implements the two kernel jobs from [design/kernel.md](../../design/kernel.md): run plugins, and own the stored data. Everything else in a running node is a plugin.

## The substrate (`substrate/`)

- **Services** — typed traits bound to well-known string keys (`ServiceKey<T>`). One provider per key; consumers get `Arc<T>` handles and check the provider's declared **capability facts** (`Facts`), never which provider it is. The interface definitions live in `inseam-seams`; the kernel itself only provides `store` and `state`.
- **Plugins and fibers** — a plugin is a name + typed config + `inject` (the list of services it may use — using an undeclared one is refused at runtime) + `provides` + `apply`. The kernel runs each composition entry as a **fiber** (`Pending → Active`, or `Failed` alone, without taking others down). Startup is reactive: a fiber activates once the services it needs exist, and restarts when one of them changes — the order of entries in the file means nothing. A composition that can't fully start fails loudly, naming each waiting entry and what it's missing.
- **Effects** — every change `apply` makes to shared state is recorded together with its undo, at the moment it's made (`provide`, `cx.effect`, `cx.keep`). Unloading runs the undos in reverse — removal is automatic, never hand-written; there is no uninstall code anywhere. Consumers of a withdrawn service tear down *before* the service disappears, and hold their `Arc`s through their own teardown. `inseam plugins` prints each fiber's live effects.
- **Events** — one typed bus with three dispatch modes, declared per event type: `Notify` (broadcast), `Guard` (a policy check where any listener's `Deny` wins — used by the `LlmCall` budget guard and the `OperationRequest` boundary guard), and `Waterfall` (middleware-style wrapping: each listener either passes its `Next` token along or returns its own decision).
- **Composition + reconciler** — `Composition` is a TOML entry tree layered by entry id ([configuration.md](../configuration.md)). `Kernel::reconcile` compares it to the running fibers — unchanged entries are left alone, changed ones are unloaded and remounted, removed ones are unwound — and settles. The guarantee: however you got here, the node ends up in the same state a fresh boot of the final composition would produce (`tests/substrate.rs` proves it).

## The store (`store.rs`, `state.rs`)

One libSQL database: catalog tables hold the graph and plugin state, derived search tables hold full-text and vectors ([indexing/storage.md](../indexing/storage.md)). Kernel-owned rules:

- **No migrations, ever.** A schema-version change drops and recreates the tables; everything in them is derived, and the next sweep rebuilds it. Plugins extend by vocabulary (mimetypes, relation kinds, properties), never by changing the schema.
- **The search surface is tied to the embedder.** The embedder declares its identity (`declare_embedding(model, dims)`) when it starts, and the search tables open under that identity. If it doesn't match what's recorded, an in-place re-embed is queued. No embedder mounted → searches refuse, with instructions.
- **Plugin state is namespaced and versioned** (the `state` service): a version mismatch discards the namespace. State must be rebuildable; credentials never live here.
- **Two records per source track what built it**: the `shape_stamp` (a fingerprint of the transforms that participated) and the `mimetypes` inventory. These are what let a plugin change re-index only the sources it actually touches ([indexing/maintenance.md](../indexing/maintenance.md)).
