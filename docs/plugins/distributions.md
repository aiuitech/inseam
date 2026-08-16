# Custom Distributions

The linked tier's path for professional setups ([design/plugins.md](../../design/plugins.md)): a node operator — a commercial cloud node, a fleet with private high-performance plugins — ships private trusted plugins by **compiling them in from source**, not through the registry. The registry ([registry.md](registry.md)) distributes loaded artifacts; trusted code is a build input.

A distribution is an app crate that compiles in a set of linked plugins and ships a base composition. `inseam-cli` is a library plus a three-line binary, so a custom distribution is a thin crate:

```toml
# Cargo.toml of your private distribution repo
[package]
name = "acme-inseam"

[[bin]]
name = "inseam"
path = "src/main.rs"

[dependencies]
inseam-cli = { git = "ssh://git@github.com/aiuitech/inseam", package = "inseam-cli" }
acme-plugins = { path = "../acme-plugins" }   # your PluginFactory impls
```

```rust
// src/main.rs
fn main() -> anyhow::Result<()> {
    inseam_cli::run(
        inseam_cli::Distribution::first_party()
            .with_factories(acme_plugins::factories())
            .with_base_entries(
                "[[entry]]\nid = \"acme-search\"\nplugin = \"acme-search\"\n",
            ),
    )
}
```

Provisioning a node is `git clone` + `cargo install --path .` — the same shape as installing the stock CLI. Your plugins are addressable from any `composition.toml` by bare name, exactly like the first-party set; `with_base_entries` turns them on by default, and a node's composition file overrides those entries by id like any base entry.

## The validation guarantee travels with you

The linked tier's validation gate ([validation.md](validation.md)) is the reusable `inseam-conformance` crate, not something private to this workspace. A custom distribution's test suite enrolls its own factories:

```rust
#[test]
fn every_linked_plugin_builds_and_declares_a_sane_manifest() {
    inseam_conformance::check_factories(&acme_plugins::factories(), &minimal_config);
}

#[tokio::test]
async fn linked_transforms_survive_hostile_inputs() {
    let kernel = /* boot with your factories + a composition activating them */;
    inseam_conformance::batter_transforms(&kernel, &[]).await;
}
```

`check_factories` is the build/manifest sweep with the enrollment gate (an unenrolled factory panics with instructions); `batter_transforms` is the same hostile-input battery loaded plugins face in `inseam plugin check` — consistent claims, text withheld/empty/garbage, never a granted LLM, degrade gracefully and never crash. The registry-specific steps (sha256 index, advisories, cooldown) don't apply here: those defend against untrusted download channels, and a source build has none.

## What this path is not

- **Not dynamic loading.** Native dynamic libraries stay rejected ([design/plugins.md](../../design/plugins.md) — paths not taken); private trusted code links statically and rides deliberate binary updates.
- **Not a private registry.** The registry is the loaded tier's channel; pointing `inseam plugin install --registry` at a private tree works for private *loaded* plugins, but trusted linked code never flows through it.
