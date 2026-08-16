//! The linked tier's conformance harness (`docs/plugins/validation.md`),
//! reusable by any distribution: the same sweep `crates/inseam-plugins`
//! runs over the first-party factories, exported so a private distribution
//! crate linking its own plugins inherits the identical battery with a
//! two-line test. The loaded tier's mirror image is `inseam plugin check`
//! (`crates/inseam-wasm-host`); this crate is the build-time gate for code
//! that is compiled in rather than mounted.
//!
//! The functions panic with instructive messages — they are test assertions,
//! meant to run inside `#[test]`/`#[tokio::test]`.

use std::sync::Arc;

use inseam_kernel::address::{ContentLength, Envelope, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::{Kernel, PluginFactory};
use inseam_seams::transforms::{Registration, TransformCtx, TRANSFORMS};

/// Sweep a distribution's factories: every plugin must build from its
/// enrolled minimal config and declare a coherent manifest. `config_for`
/// maps a factory name to that config — panic inside it for an unenrolled
/// name, so adding a plugin without enrolling it fails the suite loudly.
pub fn check_factories(
    factories: &[Arc<dyn PluginFactory>],
    config_for: &dyn Fn(&str) -> toml::Table,
) {
    assert!(!factories.is_empty(), "a distribution links at least one plugin");
    for factory in factories {
        let plugin = factory.build(&config_for(factory.name())).unwrap_or_else(|e| {
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

/// The same hostile-input battery the wasm harness runs, aimed at a linked
/// transform through the seam: text withheld, empty text, garbage text,
/// never a granted LLM. The contract is identical across tiers — degrade to
/// empty output, never panic.
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

/// Mimetypes every transform is offered; a transform claiming none of them
/// escapes the battery, which the sweep treats as a failure — extend the
/// list via `extra_samples` for exotic claims rather than skipping.
const SAMPLES: [&str; 5] =
    ["text/markdown", "text/plain", "text/x-rust", "image/png", "application/pdf"];

/// Batter every transform registered in the booted kernel: claims must be
/// deterministic, and every claimed sample mimetype faces the hostile-input
/// battery. Boot the kernel with the distribution's own factories and a
/// composition activating the transforms under test, then hand it here.
pub async fn batter_transforms(kernel: &Kernel, extra_samples: &[&str]) {
    let registry = kernel.service(&TRANSFORMS).expect("transforms seam bound");
    let registrations = registry.snapshot();
    assert!(!registrations.is_empty(), "no transforms registered; nothing to batter");

    let samples: Vec<&str> = SAMPLES.iter().chain(extra_samples).copied().collect();
    for registration in &registrations {
        let mut battered = false;
        for sample in &samples {
            let mimetype = Mimetype::parse(sample).expect("sample mimetype parses");
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
            "`{}` claims none of the sample mimetypes; pass its mimetype in extra_samples so it gets battered",
            registration.name
        );
    }
}
