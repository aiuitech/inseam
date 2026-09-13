//! Folders in the sweep (`design/indexing.md`, folders). A folder source has
//! no bytes: its content is the listing of its direct children — name,
//! type, summary — composed from the catalog after those children have
//! landed. So folders are indexed after every file, deepest level first,
//! each level planned concurrently like files and landed in locator order,
//! and a folder is dirty when the listing's digest differs from the one its
//! last run recorded — a directory's own timestamp says nothing about what
//! its children now summarize to.

use std::collections::HashSet;
use std::sync::Arc;

use inseam_kernel::address::ContentDigest;
use inseam_kernel::store::{FolderChild, IndexStore, SourceIndexMeta};
use inseam_seams::SeamError;
use inseam_seams::connection::EnumeratedSource;
use inseam_seams::listing::{self, ENTRIES_MAX, ListingEntry};
use inseam_seams::transforms::Registration;

use super::plan::expected_stamp;

/// Levels a folder tree may have: the ancestor climb's bound, restated here
/// so the level loop is provably finite.
const LEVELS_MAX: usize = 4_096;

/// Whether an enumerated source is a folder — the sweep's one test for it.
pub(super) fn is_folder(source: &EnumeratedSource) -> bool {
    source.envelope.content_type.is_directory()
}

/// The folders that still hold an admitted file after the sweep's ignore
/// rules ran: a folder exists in the index exactly when something under it
/// does, and the host's walk cannot know what the sweep's rules keep out.
/// The climb per file is bounded by the locator's segment count.
pub(super) fn retain_holding(
    folders: Vec<EnumeratedSource>,
    files: &[EnumeratedSource],
) -> Vec<EnumeratedSource> {
    let mut holding: HashSet<&str> = HashSet::new();
    for file in files {
        let locator = file.address.locator.as_str();
        let mut climbed: usize = 0;
        for (index, _) in locator.match_indices('/') {
            climbed += 1;
            assert!(climbed <= LEVELS_MAX, "a locator's segments are bounded");
            holding.insert(&locator[..index]);
        }
    }
    folders
        .into_iter()
        .filter(|folder| holding.contains(folder.address.locator.as_str()))
        .collect()
}

/// Folders grouped by depth, deepest first, each level in locator order.
/// A folder's children are one level deeper, so processing levels in this
/// order lands every child before the folder that lists it.
pub(super) fn levels(folders: Vec<EnumeratedSource>) -> Vec<Vec<EnumeratedSource>> {
    let mut sorted = folders;
    sorted.sort_by(|a, b| {
        depth_of(b)
            .cmp(&depth_of(a))
            .then_with(|| a.address.locator.as_str().cmp(b.address.locator.as_str()))
    });
    let mut levels: Vec<Vec<EnumeratedSource>> = Vec::new();
    for folder in sorted {
        let depth = depth_of(&folder);
        match levels.last_mut() {
            Some(level) if depth_of(&level[0]) == depth => level.push(folder),
            _ => levels.push(vec![folder]),
        }
        assert!(
            levels.len() <= LEVELS_MAX,
            "folder depth is bounded by the walk"
        );
    }
    levels
}

fn depth_of(source: &EnumeratedSource) -> usize {
    source.address.locator.as_str().matches('/').count()
}

/// A folder's composed content and what it was composed from.
pub(super) struct FolderContent {
    /// The listing text the transforms will see.
    pub(super) text: String,
    /// The listing's digest: the folder's change detector.
    pub(super) digest: ContentDigest,
    /// Direct children carrying a summary — deep-indexed ones. A folder
    /// with none is not worth a summary yet (its children are past the
    /// cutoff or the budget), so it waits, cataloged, for the run that
    /// indexes one.
    pub(super) indexed_children: usize,
}

/// The folder's content: the listing composed from its cataloged children.
/// Bounded by [`ENTRIES_MAX`]; one child past it marks the listing
/// truncated.
pub(super) async fn compose_content(
    store: &IndexStore,
    folder: &EnumeratedSource,
) -> Result<FolderContent, SeamError> {
    let children = store
        .folder_children(&folder.address, ENTRIES_MAX + 1)
        .await?;
    let entries_max = usize::try_from(ENTRIES_MAX).unwrap_or(usize::MAX);
    let truncated = children.len() > entries_max;
    let entries: Vec<ListingEntry> = children
        .into_iter()
        .take(entries_max)
        .map(listing_entry)
        .collect();
    assert!(entries.len() <= entries_max);
    let indexed_children = entries.iter().filter(|e| e.summary.is_some()).count();
    let name = folder
        .envelope
        .hint
        .clone()
        .unwrap_or_else(|| last_segment(folder.address.locator.as_str()).to_string());
    let text = listing::compose(&name, &entries, truncated);
    let digest = ContentDigest::of_bytes(text.as_bytes());
    Ok(FolderContent {
        text,
        digest,
        indexed_children,
    })
}

fn listing_entry(child: FolderChild) -> ListingEntry {
    ListingEntry {
        name: child.name,
        mimetype: child.content_type,
        summary: child.summary,
    }
}

fn last_segment(locator: &str) -> &str {
    locator.rsplit('/').next().unwrap_or(locator)
}

/// Whether a folder must be (re)planned: never indexed, interrupted, listing
/// changed, or shape-stale — the file rule with the listing digest in place
/// of the timestamp-and-size detector.
pub(super) fn is_dirty(
    meta: Option<&SourceIndexMeta>,
    digest: &ContentDigest,
    registrations: &[Arc<Registration>],
    sweep_shape: &str,
) -> bool {
    let Some(meta) = meta else {
        return true;
    };
    let content_changed = meta.content_digest.as_ref() != Some(digest);
    let shape_stale = match &meta.shape_stamp {
        None => true,
        Some(stored) => *stored != expected_stamp(registrations, &meta.mimetypes, sweep_shape),
    };
    !meta.indexed || content_changed || shape_stale
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{Address, ContentLength, Envelope, Timestamp};
    use inseam_kernel::fragment::Mimetype;

    fn folder(address: &str) -> EnumeratedSource {
        EnumeratedSource {
            address: address.parse::<Address>().expect("valid"),
            envelope: Envelope {
                source_type: "directory".into(),
                content_type: Mimetype::directory(),
                length: ContentLength::Bytes(0),
                created: None,
                modified: None,
                observed: Timestamp(0),
                properties: Vec::new(),
                facets: Vec::new(),
                hint: None,
                content_digest: None,
            },
            raw_bytes: 0,
        }
    }

    fn locators(level: &[EnumeratedSource]) -> Vec<&str> {
        level.iter().map(|s| s.address.locator.as_str()).collect()
    }

    #[test]
    fn levels_run_deepest_first_in_locator_order() {
        let levels = levels(vec![
            folder("inseam://fs/a"),
            folder("inseam://fs/a/b/c"),
            folder("inseam://fs/a/z"),
            folder("inseam://fs/a/b"),
            folder("inseam://fs/a/a"),
        ]);
        assert_eq!(levels.len(), 3);
        assert_eq!(locators(&levels[0]), vec!["a/b/c"]);
        assert_eq!(locators(&levels[1]), vec!["a/a", "a/b", "a/z"]);
        assert_eq!(locators(&levels[2]), vec!["a"]);
        assert!(super::levels(Vec::new()).is_empty());
    }

    #[test]
    fn only_folders_holding_an_admitted_file_are_retained() {
        let files = vec![folder("inseam://fs/a/b/kept.md")];
        let kept = retain_holding(
            vec![
                folder("inseam://fs/a"),
                folder("inseam://fs/a/b"),
                folder("inseam://fs/a/c"),
                folder("inseam://fs/a/b/kept.md/x"),
            ],
            &files,
        );
        assert_eq!(locators(&kept), vec!["a", "a/b"]);
        assert!(retain_holding(vec![folder("inseam://fs/a")], &[]).is_empty());
    }

    #[test]
    fn a_folder_is_dirty_until_its_recorded_digest_matches() {
        let digest = ContentDigest::of_bytes(b"listing");
        let mut meta = SourceIndexMeta {
            modified: None,
            raw_bytes: 0,
            content_digest: Some(digest),
            indexed: true,
            shape_stamp: Some(expected_stamp(&[], &[], "shape")),
            mimetypes: Vec::new(),
        };
        assert!(is_dirty(None, &digest, &[], "shape"));
        assert!(!is_dirty(Some(&meta), &digest, &[], "shape"));
        assert!(is_dirty(
            Some(&meta),
            &ContentDigest::of_bytes(b"other"),
            &[],
            "shape"
        ));
        meta.indexed = false;
        assert!(is_dirty(Some(&meta), &digest, &[], "shape"));
        meta.indexed = true;
        meta.shape_stamp = None;
        assert!(is_dirty(Some(&meta), &digest, &[], "shape"));
    }
}
