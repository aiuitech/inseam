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
//!
//! `golden` is the checks-file schema both tiers share; `golden_transforms`
//! runs a linked transform's own checks through the seam, the mirror of the
//! golden phase of `inseam plugin check`.

pub mod golden;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use inseam_kernel::address::{Address, ContentLength, Envelope, Timestamp};
use inseam_kernel::fragment::{Mimetype, Sprout};
use inseam_kernel::substrate::{Kernel, PluginFactory};
use inseam_seams::SeamError;
use inseam_seams::transforms::{
    GrantedLlm, Registration, TRANSFORMS, TransformCtx, TransformOutput,
};

pub use golden::{ChecksFile, Emitted, EmittedFragment, EmittedKeyed, Expect, GoldenCheck};

/// Sweep a distribution's factories: every plugin must build from its
/// enrolled minimal config and declare a coherent manifest. `config_for`
/// maps a factory name to that config — panic inside it for an unenrolled
/// name, so adding a plugin without enrolling it fails the suite loudly.
pub fn check_factories(
    factories: &[Arc<dyn PluginFactory>],
    config_for: &dyn Fn(&str) -> toml::Table,
) {
    assert!(
        !factories.is_empty(),
        "a distribution links at least one plugin"
    );
    for factory in factories {
        let plugin = factory
            .build(&config_for(factory.name()))
            .unwrap_or_else(|e| {
                panic!(
                    "`{}` does not build from its conformance config: {e}",
                    factory.name()
                )
            });
        let manifest = plugin.manifest();
        assert_eq!(
            manifest.name,
            factory.name(),
            "factory and plugin manifest must agree on the name"
        );
        for inject in manifest.inject {
            assert!(
                !inject.key.is_empty(),
                "`{}` declares an empty inject",
                manifest.name
            );
        }
        for provided in manifest.provides {
            assert!(
                !provided.is_empty(),
                "`{}` declares an empty provide",
                manifest.name
            );
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
        content_digest: None,
    }
}

/// The address a check's source pretends to have: a fixed one, so a
/// transform that builds references from it (the directory transform's
/// entries) emits the same output on every run.
fn synthetic_address() -> Address {
    "inseam://conformance/check"
        .parse()
        .expect("the conformance address is valid")
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
                address: &synthetic_address(),
                envelope: &envelope,
                mimetype,
                is_root: true,
                text,
                bytes: None,
                reference_hops_left: 1,
                llm: None,
            })
            .await;
    }
}

/// Mimetypes every transform is offered; a transform claiming none of them
/// escapes the battery, which the sweep treats as a failure — extend the
/// list via `extra_samples` for exotic claims rather than skipping.
const SAMPLES: [&str; 5] = [
    "text/markdown",
    "text/plain",
    "text/x-rust",
    "image/png",
    "application/pdf",
];

/// Batter every transform registered in the booted kernel: claims must be
/// deterministic, and every claimed sample mimetype faces the hostile-input
/// battery. Boot the kernel with the distribution's own factories and a
/// composition activating the transforms under test, then hand it here.
pub async fn batter_transforms(kernel: &Kernel, extra_samples: &[&str]) {
    let registry = kernel.service(&TRANSFORMS).expect("transforms seam bound");
    let registrations = registry.snapshot();
    assert!(
        !registrations.is_empty(),
        "no transforms registered; nothing to batter"
    );

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

// ---------------------------------------------------------------------------
// Golden checks for linked transforms
// ---------------------------------------------------------------------------

/// The harness's granted LLM: a canned reply, or a refusal — the two shapes
/// a real grant has (the metered handle refuses once budget is spent).
struct CannedLlm(Option<String>);

#[async_trait::async_trait]
impl GrantedLlm for CannedLlm {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String, SeamError> {
        self.0
            .clone()
            .ok_or_else(|| SeamError::Unavailable("conformance check: llm refused".into()))
    }

    async fn describe_image(
        &self,
        _prompt: &str,
        _mimetype: &str,
        _image: &[u8],
    ) -> Result<String, SeamError> {
        self.0
            .clone()
            .ok_or_else(|| SeamError::Unavailable("conformance check: llm refused".into()))
    }
}

/// Flatten a transform's output into the tier-neutral shape the golden
/// matcher judges. Sprout trees are walked iteratively with a hard cap —
/// a check's expectations are about shape, and a plugin emitting more than
/// `FRAGMENTS_PER_CHECK_MAX` has already failed it.
pub fn emitted_from_output(output: &TransformOutput) -> Emitted {
    let mut fragments = Vec::new();
    let mut pending: Vec<&Sprout> = output.sprouts.iter().rev().collect();
    while let Some(sprout) = pending.pop() {
        if fragments.len() >= golden::FRAGMENTS_PER_CHECK_MAX {
            break;
        }
        fragments.push(EmittedFragment {
            mimetype: sprout.fragment.mimetype.to_string(),
            relation: sprout.relation.as_str().to_string(),
            text: sprout.fragment.text.clone(),
        });
        pending.extend(sprout.children.iter().rev());
    }
    Emitted {
        fragments,
        keyed: output
            .keyed
            .iter()
            .map(|k| EmittedKeyed {
                key: k.key.to_string(),
                relation: k.relation.as_str().to_string(),
                text: k.fragment.text.clone(),
            })
            .collect(),
    }
}

/// Read and gate one transform's checks file, panicking with the fix.
fn load_checks(name: &str, path: &Path) -> ChecksFile {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "`{name}`: cannot read golden checks {}: {e}",
            path.display()
        )
    });
    let checks = ChecksFile::parse(&raw)
        .unwrap_or_else(|e| panic!("`{name}`: {} does not parse: {e}", path.display()));
    if let Err(unmet) = checks.required_coverage() {
        panic!(
            "`{name}`: {} lacks the mandatory coverage:\n  - {}",
            path.display(),
            unmet.join("\n  - ")
        );
    }
    checks
}

/// Run one golden check through the seam.
async fn run_golden(
    registration: &Arc<Registration>,
    checks_path: &Path,
    check: &GoldenCheck,
) -> Result<(), String> {
    let mimetype = Mimetype::parse(&check.mimetype)
        .map_err(|e| format!("check mimetype `{}` is invalid: {e}", check.mimetype))?;
    if !registration.transform.claims(&mimetype, check.is_root) {
        return Err(format!(
            "the transform does not claim `{}` (is_root = {}); this check would never run in \
             production",
            check.mimetype, check.is_root
        ));
    }
    let bytes = match check.fixture_path(checks_path)? {
        None => None,
        Some(path) => {
            Some(std::fs::read(&path).map_err(|e| format!("fixture {}: {e}", path.display()))?)
        }
    };
    let envelope = synthetic_envelope(&mimetype);
    let llm: Arc<dyn GrantedLlm> = Arc::new(CannedLlm(check.llm_returns.clone()));
    let output = registration
        .transform
        .apply(TransformCtx {
            address: &synthetic_address(),
            envelope: &envelope,
            mimetype: &mimetype,
            is_root: check.is_root,
            text: check.text.as_deref(),
            bytes: bytes.as_deref(),
            reference_hops_left: 1,
            llm: Some(llm),
        })
        .await;
    check.verdict(&emitted_from_output(&output))
}

/// The linked tier's golden phase: every transform registered in the booted
/// kernel must ship a checks file (`checks_for` maps the registration name
/// to its path — return `None` and the sweep panics with instructions, the
/// enrollment gate), the file must meet the mandatory coverage, and every
/// check must pass through the seam. Identical schema and judge to the
/// loaded tier's `inseam plugin check`.
pub async fn golden_transforms(kernel: &Kernel, checks_for: &dyn Fn(&str) -> Option<PathBuf>) {
    let registry = kernel.service(&TRANSFORMS).expect("transforms seam bound");
    let registrations = registry.snapshot();
    assert!(
        !registrations.is_empty(),
        "no transforms registered; nothing to check"
    );

    for registration in &registrations {
        let name = registration.name.as_str();
        let Some(path) = checks_for(name) else {
            panic!(
                "linked transform `{name}` ships no golden checks; write `{name}.checks.toml` \
                 beside its source (docs/plugins/validation.md) and map it in checks_for"
            );
        };
        let checks = load_checks(name, &path);
        for check in &checks.check {
            if let Err(reason) = run_golden(registration, &path, check).await {
                panic!("`{name}` golden check \"{}\" failed: {reason}", check.name);
            }
        }
    }
}
