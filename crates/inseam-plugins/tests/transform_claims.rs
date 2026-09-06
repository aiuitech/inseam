//! The first-party transforms, seen together through the seam: which of
//! them claim which root mimetypes. Each plugin's own tests cover its apply;
//! this is the cross-plugin routing fact the sweep relies on — that a
//! markdown or plain-text root goes to the markdown transform and not the
//! chunker, that
//! every root gets the mandatory summarizer, and that nothing claims
//! inseam-defined types or non-roots.

mod common;

use inseam_kernel::fragment::Mimetype;
use inseam_seams::transforms::TRANSFORMS;

#[tokio::test]
async fn claims_route_each_root_mimetype_to_the_right_transforms() {
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(
        data.path(),
        "[[entry]]\nid = \"entities\"\nplugin = \"transform-entities\"\n",
    )
    .await;
    let registrations = kernel.service(&TRANSFORMS).expect("transforms bound").snapshot();

    // Expected in snapshot order: structural transforms first, then
    // enrichment, each by entry id — the order the sweep applies them in.
    let cases: &[(&str, &[&str])] = &[
        ("text/markdown", &["markdown", "entity-extractor", "summarizer"]),
        ("text/plain", &["markdown", "entity-extractor", "summarizer"]),
        ("application/json", &["chunker", "entity-extractor", "summarizer"]),
        ("image/jpeg", &["entity-extractor", "summarizer"]),
    ];
    for (mimetype, expected) in cases {
        let m = Mimetype::parse(mimetype).expect("valid");
        let names: Vec<&str> = registrations
            .iter()
            .filter(|r| r.transform.claims(&m, true))
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(&names, expected, "claims for {mimetype}");
    }

    for r in &registrations {
        assert!(!r.transform.claims(&Mimetype::markdown(), false), "{} claims a non-root", r.name);
        assert!(!r.transform.claims(&Mimetype::summary(), true), "{} claims summaries", r.name);
        let entity = Mimetype::parse("text/x-inseam-entity").expect("valid");
        assert!(!r.transform.claims(&entity, true), "{} claims entities", r.name);
    }
}
