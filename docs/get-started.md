# Get Started

Install a node, index something, query it — then make the node yours by modifying it. This page is written so a person *or an agent* can follow it top to bottom: give an agent this link and it can install the CLI, run it, extend the node with plugins, and start itself.

Prerequisite for every path below: a Rust toolchain (`curl https://sh.rustup.rs -sSf | sh` or [rustup.rs](https://rustup.rs)).

## Install

Two default paths. Pick by whether you want to modify the node.

### Just run it

Install the stock `inseam` binary straight from the repo, no source tree kept:

```sh
cargo install --git https://github.com/aiuitech/inseam inseam-cli
```

Or run an app that bundles the core — the [macOS app](macos-app.md) embeds the same node through the FFI and shares its data dir with the CLI.

Either way you can still extend the node with **loaded plugins** (`inseam plugin install <name>`, [plugins/registry.md](plugins/registry.md)) — installing plugins never requires the source tree.

### From source — the self-modifying node

Clone the repo and install the CLI from your checkout:

```sh
git clone https://github.com/aiuitech/inseam
cd inseam
cargo install --path crates/inseam-cli
```

The checkout is now your node's workshop: the CLI on your PATH is built from it, plugins are authored in it, and every layer — loaded plugins, linked plugins, the kernel itself — is editable and reinstalled with the same `cargo install --path crates/inseam-cli`. This is the path for anyone (human or agent) who intends the node to modify itself.

## First run

The CLI is self-documenting — `inseam --help` and `inseam <command> --help` are the reference; [cli.md](cli.md) is the tour. The essential loop:

```sh
export OPENROUTER_API_KEY=...   # default llm/embedder endpoint (configuration.md)
inseam index ~/Documents        # reconciling sweep over a directory
inseam query "that thing"       # ranked addresses + summaries
inseam status                   # store stats, embedding identity
inseam agent "when did I ...?"  # a live LLM driving the discovery ladder
```

No API key? The node boots anyway, warns about the waiting entries, and everything not needing them works. For a fully offline node, switch the embedder to `hashed` in `composition.toml` — [configuration.md](configuration.md) has the exact file.

## Self-modify

A node's behavior is its **composition**: which plugins run with what config ([configuration.md](configuration.md)). Modifying the node means adding or changing plugins, and there is a preferred order:

1. **Loaded plugins — the default, and the preferred method for AI-written plugins.** Sandboxed WASM components mounted at runtime with manifest-attenuated capabilities ([plugins/loaded.md](plugins/loaded.md)) — a plugin an agent wrote minutes ago can be mounted safely because the bridge, not the author, bounds what it can do. Install community plugins from the registry (`inseam plugin install`), or author your own: the agent skill at `skills/inseam-loaded-plugin/SKILL.md` ([docs mirror](skills/inseam-loaded-plugin.md)) is the complete authoring procedure, and `inseam plugin check` is the gate.
2. **Linked plugins / a custom distribution** — trusted Rust compiled into the binary, for first-party-grade or proprietary plugins ([plugins/linked.md](plugins/linked.md), [plugins/distributions.md](plugins/distributions.md)).
3. **Core changes** — edit the kernel or seams in the checkout ([crate.md](crate.md) maps the workspace, `design/` holds intent), then `cargo install --path crates/inseam-cli` to make the running binary current.

## For agents: set up and self-modify, start to finish

Follow these verbatim from a fresh machine with Rust installed:

1. `git clone https://github.com/aiuitech/inseam && cd inseam`
2. `cargo install --path crates/inseam-cli` — the CLI is now on PATH and current with this source tree. Re-run this after any source change.
3. `inseam --help` — the CLI documents itself; prefer it over guessing flags. Repo skills (`skills/`) are extra guidance for what the CLI can't tell you (authoring contracts, conventions), not a substitute for `--help`.
4. Configure: set `OPENROUTER_API_KEY`, or write the offline `composition.toml` from [configuration.md](configuration.md). Verify with `inseam config --resolved`.
5. Run: `inseam index <dir>`, then `inseam query "..."`. `inseam plugins` shows the live fiber tree if something isn't settling.
6. To write a new plugin, **default to a loaded plugin** — read `skills/inseam-loaded-plugin/SKILL.md` and follow it (golden checks first, build to `wasm32-wasip2`, validate with `inseam plugin check`). Only reach for linked plugins or core edits when the transform seam genuinely can't express the change.
7. Mount your plugin by adding its entry to `<data-dir>/composition.toml` (the skill has the snippet), then re-run `inseam index` — the sweep dirties exactly the sources the new plugin claims.
