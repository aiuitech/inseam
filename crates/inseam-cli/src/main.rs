//! The stock `inseam` binary: the first-party distribution, whole. A custom
//! distribution (private linked plugins compiled in from source) is this
//! same three-liner in its own crate, with `.with_factories(...)` and
//! `.with_base_entries(...)` on the distribution — see
//! `docs/plugins/distributions.md`.

fn main() -> anyhow::Result<()> {
    inseam_cli::run(inseam_cli::Distribution::first_party())
}
