When making large decisions or features, ground yourself in our @design/README.md files first.

Always start new features by reading @docs/README.md and finish your work by documenting.

The `inseam` binary is installed on PATH via `cargo install`. After changing code, finish by running `cargo install --path .` so the local binary stays current with the source.

Finish work by committing your changes.

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

Tests:

- Test observable behavior through the public API, not private internals — if it needs `pub(crate)` access to test, the design is telling you something.
- Unit tests inline in `#[cfg(test)] mod tests`; integration tests in `tests/`; doctests on public items double as documentation that can't rot.
- Property-based tests (`proptest`) for parsers, codecs, and invariants; snapshot tests (`insta`) for complex output. Example-based tests for everything else.
- One behavior per test, named for that behavior (`rejects_empty_input`, not `test_parse_2`). A failing name should tell you what broke without reading the body.
- Table-driven cases over copy-pasted test fns; builders/fixtures over hand-rolled setup repeated in every test.
- No sleeps, no wall-clock time, no network — inject clocks and use in-memory fakes. Flaky tests get fixed or deleted, never retried.
- Test error paths as thoroughly as happy paths; assert on the error variant, not the message string.
