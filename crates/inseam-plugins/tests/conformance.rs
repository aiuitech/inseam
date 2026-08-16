//! The native tier's enforced conformance (`docs/plugins/validation.md`):
//! this suite sweeps `inseam_plugins::factories()` itself, so a native
//! plugin cannot be linked into a distribution without inheriting it — the
//! exhaustive match in `conformance_config` fails the build of this test
//! the moment an unenrolled factory appears.

mod common;

use std::sync::Arc;

use inseam_kernel::address::{ContentLength, Envelope, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_seams::transforms::{Registration, TransformCtx, TRANSFORMS};

/// Every native plugin's minimal boot config. Adding a plugin to
/// `factories()` without an arm here panics the suite with instructions —
/// that is the enrollment gate, not an oversight.
fn conformance_config(name: &str) -> toml::Table {
    let raw = match name {
        "connection-fs" | "llm-endpoint" | "transforms" | "transform-markdown"
        | "transform-chunker" | "transform-summarizer" | "transform-entities" | "finder"
        | "sweep" | "operations" => "",
        "embedder" => "provider = \"hashed\"\nmodel = \"hashed\"\ndimensions = 8",
        other => panic!(
            "native plugin `{other}` is not enrolled in the conformance suite; add a minimal \
             config arm for it in conformance_config (tests/conformance.rs)"
        ),
    };
    toml::from_str(raw).expect("static conformance config parses")
}

#[test]
fn every_native_plugin_builds_and_declares_a_sane_manifest() {
    let factories = inseam_plugins::factories();
    assert!(!factories.is_empty());
    for factory in factories {
        let plugin = factory
            .build(&conformance_config(factory.name()))
            .unwrap_or_else(|e| {
                panic!("`{}` does not build from its conformance config: {e}", factory.name())
            });
        let manifest = plugin.manifest();
        assert_eq!(
            manifest.name,
            factory.name(),
            "factory and plugin manifest must agree on the name"
        );
        for inject in manifest.inject {
            assert!(!inject.key.is_empty(), "`{}` declares an empty inject", manifest.name);
        }
        for provided in manifest.provides {
            assert!(!provided.is_empty(), "`{}` declares an empty provide", manifest.name);
        }
    }
}

fn synthetic_envelope(mimetype: &Mimetype) -> Envelope {
    Envelope {
        source_type: "conformance".into(),
        content_type: mimetype.clone(),
        length: ContentLength::Bytes(64),
        created: None,
        modified: Some(Timestamp(0)),
        observed: Timestamp(0),
        properties: Vec::new(),
        hint: Some("conformance".into()),
    }
}

/// The same hostile-input battery the wasm harness runs, aimed at every
/// native transform through the seam: text withheld, empty text, garbage
/// text, never a granted LLM. The contract is identical across tiers —
/// degrade to empty output, never panic.
async fn battery(registration: &Arc<Registration>, mimetype: &Mimetype) {
    let envelope = synthetic_envelope(mimetype);
    let hostile_texts: [Option<&str>; 4] = [
        None,
        Some(""),
        Some("\u{0}\u{FFFD} \u{202E}garbage\n\n\n\t\u{0}"),
        Some("plain conformance text with an entity like Ada Lovelace in it"),
    ];
    for text in hostile_texts {
        let _output = registration
            .transform
            .apply(TransformCtx {
                envelope: &envelope,
                mimetype,
                is_root: true,
                text,
                bytes: None,
                llm: None,
            })
            .await;
    }
}

#[tokio::test]
async fn native_transforms_claim_deterministically_and_survive_hostile_inputs() {
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(
        data.path(),
        // The offline base omits the entity extractor only because it is
        // useless without an LLM; conformance still covers it.
        "[[entry]]\nid = \"entities\"\nplugin = \"transform-entities\"\n",
    )
    .await;
    let registry = kernel.service(&TRANSFORMS).expect("transforms bound");
    let registrations = registry.snapshot();
    assert!(
        registrations.len() >= 4,
        "markdown, chunker, summarizer, entities all registered: {}",
        registrations.len()
    );

    let samples = ["text/markdown", "text/plain", "text/x-rust", "image/png", "application/pdf"];
    for registration in &registrations {
        let mut battered = false;
        for sample in samples {
            let mimetype = Mimetype::parse(sample).expect("sample parses");
            for is_root in [true, false] {
                assert_eq!(
                    registration.transform.claims(&mimetype, is_root),
                    registration.transform.claims(&mimetype, is_root),
                    "`{}` claims({sample}, {is_root}) must be deterministic",
                    registration.name
                );
            }
            if registration.transform.claims(&mimetype, true) {
                battery(registration, &mimetype).await;
                battered = true;
            }
        }
        assert!(
            battered,
            "`{}` claims none of the sample mimetypes; extend the sample list so it gets battered",
            registration.name
        );
    }
}
