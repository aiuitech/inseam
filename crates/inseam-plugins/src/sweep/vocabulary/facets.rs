//! Facets (`design/vocabulary.md`): what the host says about a source —
//! its container, its author, its labels — planted as vocabulary rows
//! anchored from the source's **root**, never from text. An author lands
//! on the entity row, so "wrote it" and "is mentioned in it" meet on one
//! row; every other facet is its own row under `facet:<key>:<value>`.
//! These are the only vocabulary rows with stored anchors: a facet is not
//! in the text, so the full-text index cannot stand in for its edges, and
//! a facet is selective by construction — a label, a channel, an author —
//! so the edges are few. The host is a column on every source and a
//! query filter already; it is no row.

use inseam_kernel::fragment::{FragmentId, FragmentKey, Mimetype, NewFragment, RelationKind};
use inseam_kernel::store::{
    IndexStore, NewVocabularyRow, SearchRole, SearchRow, SourceId, VocabularyKind,
    VocabularyOrigin, normalize_spelling,
};
use inseam_seams::SeamError;

/// Sources read per page.
const PAGE_SOURCES: u32 = 1_000;
/// Pages one pass may read.
const PAGES_MAX: u32 = 100_000;
/// The facet key that names an author.
pub const AUTHOR_KEY: &str = "author";

/// What planting produced.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Planted {
    pub rows_created: usize,
    pub anchors_written: usize,
}

/// Plant every faceted source's rows (whatever its host — a facet belongs
/// to the source that carries it), anchored from the root.
pub async fn plant_facets(store: &IndexStore) -> Result<Planted, SeamError> {
    let mut totals = Planted::default();
    let mut after = SourceId(0);
    for _ in 0..PAGES_MAX {
        let page = store.sources_with_facets(after, PAGE_SOURCES).await?;
        let mut batch: Vec<(FragmentId, NewVocabularyRow, bool)> = Vec::new();
        for source in &page {
            for facet in &source.facets {
                if let Some((row, is_author)) = facet_row(&facet.key, &facet.value) {
                    batch.push((source.root, row, is_author));
                }
            }
        }
        totals = totals.plus(plant_batch(store, &batch).await?);
        match page.last() {
            Some(last) if page.len() >= usize::try_from(PAGE_SOURCES).unwrap_or(0) => {
                after = last.source;
            }
            _ => break,
        }
    }
    Ok(totals)
}

impl Planted {
    fn plus(self, other: Self) -> Self {
        Self {
            rows_created: self.rows_created + other.rows_created,
            anchors_written: self.anchors_written + other.anchors_written,
        }
    }
}

/// Plant one batch of `(root, row, is_author)`: rows get-or-created, each
/// anchored from its root with `authored` or `faceted`, created rows given
/// a lexical search row.
async fn plant_batch(
    store: &IndexStore,
    batch: &[(FragmentId, NewVocabularyRow, bool)],
) -> Result<Planted, SeamError> {
    if batch.is_empty() {
        return Ok(Planted::default());
    }
    let faceted = RelationKind::new("faceted").expect("literal kind is valid");
    let authored = RelationKind::new("authored").expect("literal kind is valid");
    let rows: Vec<NewVocabularyRow> = batch.iter().map(|(_, row, _)| row.clone()).collect();
    let planted = store.plant_vocabulary_rows(&rows).await?;
    let mut faceted_anchors = Vec::new();
    let mut authored_anchors = Vec::new();
    for ((root, _, is_author), result) in batch.iter().zip(&planted) {
        if *is_author {
            authored_anchors.push((*root, result.fragment));
        } else {
            faceted_anchors.push((*root, result.fragment));
        }
    }
    store.anchor_vocabulary(&faceted, &faceted_anchors).await?;
    store
        .anchor_vocabulary(&authored, &authored_anchors)
        .await?;
    if store.has_search_surface().await? {
        let search_rows: Vec<SearchRow> = rows
            .iter()
            .zip(&planted)
            .filter(|(_, p)| p.created)
            .filter_map(|(row, p)| {
                row.fragment.text.as_ref().map(|text| SearchRow {
                    fragment: p.fragment,
                    source: None,
                    text: text.clone(),
                    vector: None,
                    role: SearchRole::Lexical,
                })
            })
            .collect();
        store.add_search_rows(&search_rows).await?;
    }
    Ok(Planted {
        rows_created: planted.iter().filter(|p| p.created).count(),
        anchors_written: faceted_anchors.len() + authored_anchors.len(),
    })
}

/// A facet as a row: an author on the entity row (`entity:person:`), any
/// other key under `facet:<key>:<value>`. `None` for an empty value. The
/// document frequency is recounted from the anchors after planting.
fn facet_row(key: &str, value: &str) -> Option<(NewVocabularyRow, bool)> {
    let normalized = normalize_spelling(value);
    let key_normalized = normalize_spelling(key);
    if normalized.is_empty() || key_normalized.is_empty() {
        return None;
    }
    if key_normalized == AUTHOR_KEY {
        let row = NewVocabularyRow {
            key: FragmentKey::new(format!("entity:person:{normalized}")).ok()?,
            fragment: NewFragment {
                mimetype: Mimetype::parse("text/x-inseam-entity")
                    .expect("literal mimetype is valid")
                    .with_param("kind", "person"),
                text: Some(value.trim().to_string()),
                extent: None,
                content_address: None,
            },
            kind: VocabularyKind::Entity,
            origin: VocabularyOrigin::Envelope,
            normalized,
            document_frequency: 0,
        };
        return Some((row, true));
    }
    let row = NewVocabularyRow {
        key: FragmentKey::new(format!("facet:{key_normalized}:{normalized}")).ok()?,
        fragment: NewFragment {
            mimetype: Mimetype::parse("text/x-inseam-facet")
                .expect("literal mimetype is valid")
                .with_param("key", &key_normalized),
            text: Some(value.trim().to_string()),
            extent: None,
            content_address: None,
        },
        kind: VocabularyKind::Facet,
        origin: VocabularyOrigin::Envelope,
        normalized: format!("{key_normalized}:{normalized}"),
        document_frequency: 0,
    };
    Some((row, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authors_land_on_the_entity_row_and_facets_on_their_own() {
        let (author, is_author) = facet_row("author", "Dana Reyes").expect("a row");
        assert!(is_author);
        assert_eq!(author.key.as_str(), "entity:person:dana reyes");
        assert_eq!(author.kind, VocabularyKind::Entity);
        assert_eq!(author.normalized, "dana reyes");
        let (label, is_author) = facet_row("label", "INBOX").expect("a row");
        assert!(!is_author);
        assert_eq!(label.key.as_str(), "facet:label:inbox");
        assert_eq!(label.kind, VocabularyKind::Facet);
        assert_eq!(label.normalized, "label:inbox");
        assert_eq!(label.fragment.mimetype.param("key"), Some("label"));
        assert!(facet_row("label", "  ").is_none());
    }
}
