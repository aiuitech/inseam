---
name: inseam-linked-plugin
description: Author a linked inseam plugin — trusted Rust compiled into the inseam binary, working from the source tree. Use for new seams, providers, or anything the sandboxed WASM contract can't express. For a sandboxed runtime plugin (the default for AI- and community-authored work) use inseam-loaded-plugin instead.
---

# Authoring a linked inseam plugin

Linked plugins are Rust, statically compiled into a distribution, turned
on by the composition. You are working in the inseam source tree — read
the neighbors, don't guess.

**Tests are mandatory here too.** A linked transform cannot be registered
without golden checks: the conformance sweep (`cargo test -p
inseam-plugins --test conformance`) panics, naming the transform, until it
ships a `<name>.checks.toml` that proves its claim and pins its degrade
path — the same schema and the same judge as a loaded plugin's checks.
Write them first and build to them.

## Read first

- `design/plugins.md` — the intent, and the paths deliberately not taken.
- `docs/plugins/linked.md` — the tier and the first-party set;
  `docs/architecture/kernel.md` — the five declarations every plugin is
  (name, config, inject, provide, apply); `docs/plugins/validation.md` —
  the golden-check schema and the mandatory coverage.
- `crates/inseam-seams` — the seam definitions consumers depend on.
- `crates/inseam-plugins/src/` — the first-party set. Pick the closest
  neighbor to what you're building as your template, and read its
  `.checks.toml` beside it.
- The running node: `inseam claims <mimetype|path>` shows which transforms
  already claim an input (complement, don't duplicate); `inseam
  capabilities` shows what the node grants (an LLM? which model?), i.e.
  which path of your transform runs here.

## The TDD loop

1. **State the claim as checks (red).** For a transform, write
   `crates/inseam-plugins/src/transforms/<registration-name>.checks.toml`
   before the code (the neighbors — `markdown`, `chunker`, `summarizer`,
   `entity-extractor` — are the templates). Minimum coverage, enforced:
   one check with a substantive expectation (`fragment_contains`,
   `relation`, `mimetype`, or `entity` — linked transforms can assert
   entities, loaded ones cannot), and one *starved* check (no `text`, no
   `llm_returns`) with `max_fragments` set. Add a check per distinct
   behavior; each check's `mimetype` must be one the transform claims, or
   the sweep fails it. The LLM is canned (`llm_returns` verbatim, absent =
   refuses); checks prove plumbing and shape, never model quality.
   Fixtures (`bytes_file`) sit beside the checks file.
2. **Write the plugin** in `crates/inseam-plugins` (or in your own crate
   for a custom distribution — `docs/plugins/distributions.md`), following
   the styleguide in `AGENTS.md`. Start from the smallest apply that
   degrades to empty output. Declare exactly what you touch: inject only
   the seams you use — reaching for an undeclared service fails the fiber.
   Transforms never do I/O or touch the `llm` seam directly; they use the
   granted, metered handle the sweep passes in and fall back gracefully
   when it refuses. If your config names a secret environment variable
   (`api_key_env`-style), implement `Plugin::secrets()` with the variable
   and an owner-facing sentence on why it's needed (`llm_endpoint.rs` is
   the template).
3. **Register and enroll.** Add the factory to `factories()` in
   `crates/inseam-plugins/src/lib.rs` (or via
   `Distribution::with_factories` in a custom distribution). Then in
   `crates/inseam-plugins/tests/conformance.rs`: add a minimal config arm
   to `conformance_config` (the build/manifest sweep) and, for a
   transform, a path arm to `golden_checks_for` (the golden sweep). Both
   panic with instructions for anything unenrolled — that is the gate.
4. **Run the gate and watch it go red, then green.**

   ```sh
   cargo test -p inseam-plugins --test conformance
   ```

   `linked_transforms_pass_their_own_golden_checks` fails first, naming
   your transform, the check, what it expected and what actually came out
   (`got 0 fragment(s)`). Implement the smallest code that turns it green;
   rerun; repeat. `linked_transforms_claim_deterministically_and_survive_hostile_inputs`
   is the hostile-input battery — the same one loaded plugins face — and
   must stay green throughout; a failure there is a panic on withheld or
   garbage input. Add unit tests beside the code for the pieces golden
   checks can't reach (parsers, helpers), and an integration test in
   `crates/inseam-plugins/tests/` when the behavior only shows through the
   index.
5. **Prove it live.** Activate it with a composition entry, rebuild
   (`cargo install --path crates/inseam-cli`), then:

   ```sh
   inseam config --resolved      # your entry is in the layered composition
   inseam plugins                # your fiber is Active, with its effects listed
   inseam index <dir>            # the sweep applies it to each root it claims
   inseam status                 # fragment counts moved
   inseam query "<words it should surface>"
   inseam expand <address>       # the fragments it hung off the source
   ```

6. **Document:** update `docs/plugins/linked.md` (the first-party table
   if you added to it) and the crate `//!` doc block, then `cargo xtask
   docs`. Commit with the checks file, the enrollment arms, and the docs
   in the same change.
