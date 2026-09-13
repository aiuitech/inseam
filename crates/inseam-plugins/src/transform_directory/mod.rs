//! The `transform-directory` plugin: the structural transform for folder
//! sources (`inode/directory` roots — `design/indexing.md`, folders). A
//! folder's content is the listing the sweep composed from its landed
//! children ([`inseam_seams::listing`]); this transform parses it back and
//! plants one entry fragment per child, each carrying a content reference
//! to the child's address, so `expand` on a folder walks to what it holds.
//! The folder's summary is the summarizer's, reading the same listing as
//! prose. Its golden checks live beside it in `directory.checks.toml`.

use std::sync::Arc;

use inseam_kernel::address::{Address, Locator};
use inseam_kernel::fragment::{Extent, Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{
    ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory, parse_config,
};
use inseam_seams::listing::{self, ENTRIES_MAX, ListingEntry};
use inseam_seams::llm::LlmLane;
use inseam_seams::transforms::{
    Registration, Transform, TransformCtx, TransformKind, TransformOutput, register_as_effect,
};

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DirectoryConfig {}

pub struct DirectoryPlugin {
    #[allow(dead_code)]
    config: DirectoryConfig,
}

pub struct DirectoryFactory;

impl PluginFactory for DirectoryFactory {
    fn name(&self) -> &str {
        "transform-directory"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(DirectoryPlugin {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for DirectoryPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("transforms")];
        Manifest {
            name: "transform-directory",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "directory".to_string(),
                transform: Arc::new(DirectoryTransform),
                llm_call_budget: 0,
                llm_lane: LlmLane::Interactive,
                shape_fingerprint: "directory-v1".to_string(),
            },
        )
    }
}

pub(crate) struct DirectoryTransform;

#[async_trait::async_trait]
impl Transform for DirectoryTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Structural
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && mimetype.is_directory()
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let Some(text) = ctx.text else {
            return TransformOutput::default();
        };
        let entries = listing::parse(text);
        let entries_max = usize::try_from(ENTRIES_MAX).unwrap_or(usize::MAX);
        let sprouts: Vec<Sprout> = entries
            .iter()
            .take(entries_max)
            .enumerate()
            .filter_map(|(index, entry)| entry_sprout(ctx.address, index, entry))
            .collect();
        assert!(sprouts.len() <= entries_max);
        TransformOutput::sprouts(sprouts)
    }
}

/// The entry fragment for one listed child: the folder `contains` it. The
/// listing's header is line 1, so entry `index` sits on line `index + 2`.
/// A name the address grammar refuses yields no fragment rather than a
/// wrong one.
fn entry_sprout(folder: &Address, index: usize, entry: &ListingEntry) -> Option<Sprout> {
    let locator = Locator::new(format!("{}/{}", folder.locator.as_str(), entry.name)).ok()?;
    let line = u64::try_from(index).ok()?.checked_add(2)?;
    Some(Sprout::leaf(
        NewFragment {
            mimetype: Mimetype::directory_entry(),
            text: Some(entry.fragment_text()),
            extent: Some(Extent::Lines {
                start: line,
                end: line,
            }),
            content_address: Some(Address::new(folder.host.clone(), locator)),
        },
        RelationKind::contains(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{ContentLength, Envelope, Timestamp};

    fn folder_envelope() -> Envelope {
        Envelope {
            source_type: "directory".into(),
            content_type: Mimetype::directory(),
            length: ContentLength::Lines(3),
            created: None,
            modified: Some(Timestamp(1_700_000_000)),
            observed: Timestamp(1_700_000_100),
            properties: Vec::new(),
            facets: Vec::new(),
            hint: Some("notes".into()),
            content_digest: None,
        }
    }

    async fn apply(text: Option<&str>) -> TransformOutput {
        let envelope = folder_envelope();
        let address: Address = "inseam://fs-test/Users/greg/notes".parse().expect("valid");
        let mimetype = Mimetype::directory();
        DirectoryTransform
            .apply(TransformCtx {
                address: &address,
                envelope: &envelope,
                mimetype: &mimetype,
                is_root: true,
                text,
                bytes: None,
                reference_hops_left: 1,
                llm: None,
            })
            .await
    }

    #[test]
    fn claims_directory_roots_only() {
        assert!(DirectoryTransform.claims(&Mimetype::directory(), true));
        assert!(!DirectoryTransform.claims(&Mimetype::directory(), false));
        assert!(!DirectoryTransform.claims(&Mimetype::markdown(), true));
        assert!(!DirectoryTransform.claims(&Mimetype::directory_entry(), true));
    }

    #[tokio::test]
    async fn plants_one_referencing_entry_per_listed_child() {
        let listing = "Folder notes: 1 folder, 1 file\n\
                       photos\tinode/directory\tTrip photos.\n\
                       reno.md\ttext/markdown\tBudget notes.\n";
        let out = apply(Some(listing)).await;
        assert_eq!(out.sprouts.len(), 2);
        let entry = &out.sprouts[1];
        assert_eq!(entry.relation, RelationKind::contains());
        assert!(entry.fragment.mimetype.is_directory_entry());
        assert_eq!(
            entry.fragment.text.as_deref(),
            Some("reno.md (text/markdown)")
        );
        assert_eq!(
            entry
                .fragment
                .content_address
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("inseam://fs-test/Users/greg/notes/reno.md")
        );
        assert_eq!(
            entry.fragment.extent,
            Some(Extent::Lines { start: 3, end: 3 })
        );
        assert!(out.keyed.is_empty());
    }

    #[tokio::test]
    async fn withheld_or_unlisting_text_plants_nothing() {
        assert!(apply(None).await.sprouts.is_empty());
        assert!(apply(Some("")).await.sprouts.is_empty());
        assert!(
            apply(Some("# just a document\n\nno entries here\n"))
                .await
                .sprouts
                .is_empty()
        );
    }
}
