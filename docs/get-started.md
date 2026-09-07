# Get Started

Install a node, index something, query it — then make the node yours by changing it. This page works for a person *or* an agent reading top to bottom: hand an agent this link and it can install the CLI, run it, add plugins, and get itself going.

## Install

Two paths, depending on whether you want to compile the node yourself — or skip both and hand the whole job to an agent.

### Just run it — nothing to compile

One line installs the prebuilt binary (macOS and Linux, arm64 and x86_64):

```sh
curl -fsSL https://docs.inseam.io/install.sh | sh
```

The script figures out your platform, downloads the latest [GitHub release](https://github.com/aiuitech/inseam/releases), checks the download against the release's sha256 checksums, and installs to `~/.local/bin`. Set `INSEAM_INSTALL_DIR` or `INSEAM_VERSION` to override, or grab the tarballs from the releases page yourself. You can also run an app that bundles the core — the [macOS app](architecture/macos-app.md) embeds the same node and shares its data with the CLI.

The prebuilt binary can still grow: it installs plugins from the registry (`inseam plugin install <name>`, [plugins/registry.md](plugins/registry.md)) and validates plugins you write yourself (`inseam plugin check`). Since loaded plugins can be written in any language that compiles to a WASM component, you can extend the node without ever installing Rust (see [Self-modify](#self-modify)).

### Hand it to an agent

You don't have to run any of this yourself. Paste this into an agent with shell access (Claude Code or similar) and let it drive:

```text
Fetch https://docs.inseam.io/llms.txt and follow its Get Started guide:
install the inseam CLI, configure it, index a directory I name, and verify
a query returns results. Prefer the from-source path if Rust is available
so the node can modify itself; otherwise use the one-line installer. If
something I need indexed isn't supported yet, author a loaded plugin per
the guide, validate it with `inseam plugin check`, and mount it.
```

`llms.txt` is the agent-readable index of this whole site, with this page at the top — one URL is enough for an agent to install, configure, extend, and start a node on its own.

### From source — the self-modifying node

This path compiles the node, so it needs Rust (`curl https://sh.rustup.rs -sSf | sh` or [rustup.rs](https://rustup.rs)). Clone the repo and install the CLI from your checkout:

```sh
git clone https://github.com/aiuitech/inseam
cd inseam
cargo install --path crates/inseam-cli
```

The checkout is now your node's workshop: the CLI on your PATH is built from it, plugins are written in it, and every layer — loaded plugins, linked plugins, even the kernel — can be edited and reinstalled with the same `cargo install --path crates/inseam-cli`. Pick this path if you (or your agent) want the node to be able to change itself.

## First run

The CLI documents itself — `inseam --help` and `inseam <command> --help` are the reference; [cli.md](cli.md) is the tour. The essential loop:

```sh
export OPENROUTER_API_KEY=...   # default llm/embedder endpoint (configuration.md)
inseam index ~/Documents        # index a directory
inseam query "that thing"       # ranked results with summaries
inseam status                   # store stats, embedding info
inseam agent "when did I ...?"  # a live LLM searching for you
```

No API key? The node still starts, tells you which parts are waiting on one, and everything else works. For a fully offline node, switch the embedder to `hashed` in `composition.toml` — [configuration.md](configuration.md) has the exact file — or point the `llm` entry at a local ollama and pick an installed embedding model ([indexing/embeddings.md](indexing/embeddings.md)).

## Connect an agent

Any MCP host — Claude Desktop, Claude Code, an IDE — can climb the same ladder the CLI does. Start the node's owner API, build the MCP server once, and point the host at it ([architecture/mcp-server.md](architecture/mcp-server.md)):

```sh
export INSEAM_OWNER_TOKEN=$(openssl rand -hex 32)
inseam serve                                   # 127.0.0.1:7337
cd apps/mcp && pnpm install && pnpm build      # once
claude mcp add inseam -e INSEAM_OWNER_TOKEN=$INSEAM_OWNER_TOKEN -- node "$PWD/build/main.js"
```

The host now has `query`, `expand`, `scan`, `fetch`, `fetch_bytes`, `hosts`, `status`, `catalog`, and `index` as tools. `--transport http` serves the same tools at an HTTP endpoint for hosts that connect instead of launching ([apps/mcp/README.md](../apps/mcp/README.md)).

## Self-modify

A node's behavior is its **composition**: which plugins run, with what config ([configuration.md](configuration.md)). Changing the node means adding or changing plugins, in this preferred order:

1. **Loaded plugins — the default, and the right choice for AI-written plugins.** These are sandboxed WASM components mounted while the node runs, limited to exactly what their manifest asks for ([plugins/loaded.md](plugins/loaded.md)). A plugin an agent wrote minutes ago can be mounted safely, because the sandbox — not the author — decides what it can touch. Install community plugins from the registry (`inseam plugin install`), or write your own: the agent skill at `skills/inseam-loaded-plugin/SKILL.md` ([docs mirror](skills/inseam-loaded-plugin.md)) walks through the whole process, and `inseam plugin check` is the gate. **This needs no Rust**: the contract is a language-neutral WIT interface ([plugins/transform-plugin-wit.md](plugins/transform-plugin-wit.md)), so any language that compiles to a WASM component works — the skill's walkthrough uses Rust, but componentize-py (Python), ComponentizeJS/jco (JavaScript), and TinyGo (Go) produce the same kind of artifact, and the prebuilt CLI validates all of them the same way.
2. **Linked plugins / a custom build** — trusted Rust compiled into the binary, for first-party-grade or private plugins ([plugins/linked.md](plugins/linked.md), [plugins/distributions.md](plugins/distributions.md)).
3. **Core changes** — edit the kernel or the seam interfaces in the checkout ([workspace-layout.md](architecture/workspace-layout.md) maps the repo, `design/` holds the reasoning), then `cargo install --path crates/inseam-cli` to update the running binary.

## For agents: set up and self-modify, start to finish

Follow these verbatim from a fresh machine with Rust installed. No Rust and no permission to install it? Replace steps 1–2 with the curl one-liner from [Just run it](#just-run-it--nothing-to-compile) and, at step 6, write the plugin in a WASM-capable language the machine does have — everything else is identical:

1. `git clone https://github.com/aiuitech/inseam && cd inseam`
2. `cargo install --path crates/inseam-cli` — the CLI is now on PATH and matches this source tree. Re-run this after any source change.
3. `inseam --help` — the CLI documents itself; prefer it over guessing flags. Repo skills (`skills/`) cover what `--help` can't (authoring contracts, conventions), not a substitute for it.
4. Configure: set `OPENROUTER_API_KEY`, or write the offline `composition.toml` from [configuration.md](configuration.md). Verify with `inseam config --resolved`.
5. Run: `inseam index <dir>`, then `inseam query "..."`. `inseam plugins` shows every running plugin and its state if something isn't working.
6. To write a new plugin, **default to a loaded plugin** — read `skills/inseam-loaded-plugin/SKILL.md` and follow it with the CLI as companion: `inseam claims`/`inseam capabilities` to see what the node has, `inseam plugin new` to scaffold with the golden checks first, `inseam plugin try` to watch real output, `inseam plugin check` to validate, `inseam plugin mount` to run it ([plugins/authoring-cli.md](plugins/authoring-cli.md)). Only reach for linked plugins or core edits when the transform interface genuinely can't express the change.
7. Mount your plugin by adding its entry to `<data-dir>/composition.toml` (the skill has the snippet), then re-run `inseam index` — only the sources your new plugin applies to get re-indexed.
