# The Kernel

`crates/inseam-kernel` implements the two kernel responsibilities from [design/kernel.md](../design/kernel.md). Everything else in a running node is a plugin.

## The substrate (`substrate/`)

- **Services** — typed traits bound to well-known string keys (`ServiceKey<T>`). One provider per key; consumers receive `Arc<T>` handles and branch on the provider's declared **capability facts** (`Facts`), never on its identity. Seam definitions live in `inseam-seams`; the kernel itself binds only `store` and `state`.
- **Plugins and fibers** — a plugin is name + typed config + `inject` (its capability manifest — undeclared access is refused at runtime) + `provides` + `apply`. The kernel instantiates each composition entry as a **fiber** (`Pending → Active`, or `Failed` alone, contained). Activation is reactive: a fiber activates when its required keys are bound, restarts when a key it injects changes provider or appears, and file order carries no semantics. Unsatisfiable compositions fail loudly, naming each waiting entry and its missing keys.
- **Effects** — every change `apply` makes to shared state is recorded with its undo at the moment it's made (`provide`, `cx.effect`, `cx.keep`). Unload runs the disposers in reverse — removal is derived, not authored; there is no uninstall path anywhere. Consumers of a withdrawn service tear down *before* the binding disappears and hold `Arc`s through their own teardown. `inseam plugins` prints each fiber's live effect labels.
- **Events** — one typed bus, three dispatch modes declared per event type: `Notify` (fan-out), `Guard` (policy check with **monotonic denial** — any listener's `Deny` wins; used by the `LlmCall` budget guard and the `OperationRequest` boundary guard), and `Waterfall` (middleware-style wrapping where a listener must either consume its `Next` token or return its own decision).
- **Composition + reconciler** — `Composition` is a TOML entry tree with layering by entry id ([configuration.md](configuration.md)); `Kernel::reconcile` diffs it against the running fibers (unchanged entries untouched, changed ones dispose-and-remount, removed ones unwind) and settles. The invariant is confluence: any history of edits ends in the same state as a fresh boot of the final composition (`tests/substrate.rs` proves it).

## The store (`store.rs`, `state.rs`)

SQLite catalog + graph + plugin state, LanceDB search surfaces ([index/storage.md](index/storage.md)). Kernel-owned rules:

- **No migrations, ever.** A schema-version change drops and recreates tables; everything is derived and rebuilt by the next sweep. Plugins extend by vocabulary (mimetypes, relation kinds, properties), never DDL.
- **The search surface binds to the embedder.** `declare_embedding(model, dims)` (called by the embedder provider on activation) opens Lance under that identity; a mismatch with the recorded identity pends an in-place re-embed. No embedder mounted → searches refuse with instructions.
- **Plugin state is namespaced and versioned** (`state` service): a version mismatch discards the namespace. State must be re-derivable; credentials never live here.
- **Claims-aware shape records** per source: the `shape_stamp` (digest of the transform registrations that participated in the subtree) and the `mimetypes` inventory, which is what lets plugin churn dirty only the sources a plugin's claims touch ([index/maintenance.md](index/maintenance.md)).
