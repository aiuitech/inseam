//! The linked tier's enforced conformance (`docs/plugins/validation.md`):
//! this suite sweeps `inseam_plugins::factories()` itself through the
//! shared harness (`crates/inseam-conformance`), so a linked plugin cannot
//! be built into a distribution without inheriting it — the exhaustive
//! match in `conformance_config` fails the suite the moment an unenrolled
//! factory appears. A private distribution runs the same harness over its
//! own factories (`docs/plugins/distributions.md`).

mod common;

use std::path::{Path, PathBuf};

use inseam_seams::transforms::TRANSFORMS;

/// Every linked plugin's minimal boot config. Adding a plugin to
/// `factories()` without an arm here panics the suite with instructions —
/// that is the enrollment gate, not an oversight.
fn conformance_config(name: &str) -> toml::Table {
    let raw = match name {
        "connections" | "connection-fs" | "connection-google" | "oauth" | "llm-endpoint"
        | "transforms" | "transform-markdown" | "transform-chunker" | "transform-summarizer"
        | "transform-entities" | "finder" | "sweep" | "operations" => "",
        "embedder" => "provider = \"hashed\"\nmodel = \"hashed\"\ndimensions = 8",
        other => panic!(
            "linked plugin `{other}` is not enrolled in the conformance suite; add a minimal \
             config arm for it in conformance_config (tests/conformance.rs)"
        ),
    };
    toml::from_str(raw).expect("static conformance config parses")
}

/// Every linked transform's golden checks, keyed by registration name and
/// kept beside the transform's source in its plugin directory
/// (`src/<plugin>/<registration>.checks.toml`). A transform registered
/// without an arm here panics the golden sweep with instructions — the
/// linked tier's mirror of "no checks file, no admission".
fn golden_checks_for(name: &str) -> Option<PathBuf> {
    let file = match name {
        "markdown" => "transform_markdown/markdown.checks.toml",
        "chunker" => "transform_chunker/chunker.checks.toml",
        "summarizer" => "transform_summarizer/summarizer.checks.toml",
        "entity-extractor" => "transform_entities/entity-extractor.checks.toml",
        _ => return None,
    };
    Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file))
}

#[test]
fn every_linked_plugin_builds_and_declares_a_sane_manifest() {
    inseam_conformance::check_factories(&inseam_plugins::factories(), &conformance_config);
}

#[tokio::test]
async fn linked_transforms_claim_deterministically_and_survive_hostile_inputs() {
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(
        data.path(),
        // The offline base omits the entity extractor only because it is
        // useless without an LLM; conformance still covers it.
        "[[entry]]\nid = \"entities\"\nplugin = \"transform-entities\"\n",
    )
    .await;
    let registrations = kernel.service(&TRANSFORMS).expect("transforms bound").snapshot();
    assert!(
        registrations.len() >= 4,
        "markdown, chunker, summarizer, entities all registered: {}",
        registrations.len()
    );
    inseam_conformance::batter_transforms(&kernel, &[]).await;
}

#[tokio::test]
async fn linked_transforms_pass_their_own_golden_checks() {
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(
        data.path(),
        "[[entry]]\nid = \"entities\"\nplugin = \"transform-entities\"\n",
    )
    .await;
    inseam_conformance::golden_transforms(&kernel, &golden_checks_for).await;
}
