When making large decisions or features, ground yourself in our @design/README.md files first.

Always start new features by reading @docs/README.md and finish your work by documenting.

The `inseam` binary is installed on PATH via `cargo install`. After changing code, finish by running `cargo install --path crates/inseam-cli` so the local binary stays current with the source.

Finish work by committing your changes and mention the hash in your final message.

## Rust Styleguide

Lean into:

- Make invalid states unrepresentable: newtypes over primitives, enums over bool/flag combos, typestate for lifecycles.
- Parse, don't validate — convert unstructured input into rich types once, at the boundary.
- `Result` everywhere fallible; `thiserror` for library errors, `anyhow` only at binary edges. Errors say what failed and why.
- Accept borrows (`&str`, `&[T]`, `impl Trait`), return owned. Let the caller decide allocation.
- Iterator chains over index loops; exhaustive `match` over `if let` cascades when variants matter.
- Small `pub` surface; `pub(crate)` by default. The module tree is the API design.
- Derive liberally (`Debug`, `Clone`, `PartialEq`); implement `From`/`TryFrom` instead of ad-hoc conversion fns.

Avoid:

- `unwrap`/`expect`/`panic!` outside tests and provably-infallible cases (document the proof).
- `.clone()` to satisfy the borrow checker — restructure ownership instead.
- `Arc<Mutex<T>>` as a reflex — prefer ownership transfer, channels, or scoped borrows.
- Premature generics and trait abstractions with one implementor; write the concrete version first.
- Stringly-typed data, `as` casts (use `From`/`try_into`), bool parameters, `unsafe` without a `// SAFETY:` justification.
- Deep module nesting and `mod.rs` re-export mazes; keep paths shallow and obvious.

TigerStyle (adapted from @TIGER_STYLE.md):

- Design goals in order: safety, performance, developer experience. Zero technical debt — do it right the first time; solve showstoppers in design, not production.
- Simple, explicit control flow. No recursion. Put a limit on everything: every loop and queue gets a fixed upper bound; assert loops that intentionally never terminate.
- Assert liberally: function pre/postconditions, invariants, and the relationships of constants. Assert both the positive space you expect and the negative space you don't. Assertions catch programmer errors — crashing is the correct response; operating errors get `Result`.
- Pair assertions: enforce the same property on at least two code paths (e.g. before write and after read).
- Split compound assertions and compound conditions — `assert!(a); assert!(b);` and nested `if/else` over `a && b`. State invariants positively; give every `if` a considered `else`.
- Hard limit of 70 lines per function. Push `if`s up and `for`s down: keep control flow and state changes in the parent, keep leaf helpers pure and branch-free.
- Use explicitly-sized integers (`u32`, `u64`); avoid `usize` except where APIs demand it.
- Declare variables at the smallest possible scope, closest to use. Don't duplicate variables or alias state — that's how it drifts out of sync.
- Treat `index`, `count`, and `size` as distinct concepts with explicit conversions; put units and qualifiers last in names (`latency_ms_max`, not `max_latency_ms`).
- Get nouns and verbs just right; no abbreviations. Prefix a helper with its caller's name (`read_sector` / `read_sector_callback`). Long-form flags in scripts (`--force`, not `-f`).
- Do a back-of-the-envelope performance sketch in design: network, disk, memory, CPU × bandwidth, latency. Optimize the slowest resource first, weighted by frequency. Batch to amortize costs; don't react to external events one at a time.
- Pass options explicitly at call sites instead of relying on library defaults.
- Comments and commit messages say *why*, in full sentences. Always motivate decisions.
- All warnings are errors at the compiler's strictest setting.

Tests:

- Test observable behavior through the public API, not private internals — if it needs `pub(crate)` access to test, the design is telling you something.
- Unit tests inline in `#[cfg(test)] mod tests`; integration tests in `tests/`; doctests on public items double as documentation that can't rot.
- Property-based tests (`proptest`) for parsers, codecs, and invariants; snapshot tests (`insta`) for complex output. Example-based tests for everything else.
- One behavior per test, named for that behavior (`rejects_empty_input`, not `test_parse_2`). A failing name should tell you what broke without reading the body.
- Table-driven cases over copy-pasted test fns; builders/fixtures over hand-rolled setup repeated in every test.
- No sleeps, no wall-clock time, no network — inject clocks and use in-memory fakes. Flaky tests get fixed or deleted, never retried.
- Test error paths as thoroughly as happy paths; assert on the error variant, not the message string.
