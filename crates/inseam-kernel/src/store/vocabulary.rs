//! The vocabulary as the store holds it (`design/vocabulary.md`): every
//! vocabulary row is a keyed fragment the catalog already knows how to
//! dedupe, anchor, search, and collect; this module adds the row's
//! **kind**, **origin**, normalized spelling, document frequency, gloss, and
//! cluster beside it, and the **clusters** themselves — sets of rows that
//! co-occur across sources, each with one vector for grounding a query by
//! paraphrase. Clusters are not fragments and never enter the relation
//! graph: a cluster of sixty rows over thousands of sources would be the
//! super-hub the walk must never see.
//!
//! Everything here is derived and rebuildable by the sweep's vocabulary
//! pass, so the tables are created `IF NOT EXISTS` beside the catalog and
//! need no schema-version bump: a node from before the vocabulary opens
//! with empty tables and fills them on its next sweep.

use std::collections::HashMap;
use std::fmt;

use libsql::params;

use super::{
    ID_LIST_CHUNK, IndexStore, SourceId, StoreError, corrupt, id_list, keyed_fragment_in,
    search_tables_exist, vector_blob, vector_from_blob,
};
use crate::address::{Facet, HostId};
use crate::fragment::{FragmentId, FragmentKey, NewFragment, Relation, RelationKind};

pub const VOCABULARY_SCHEMA_SQL: &str = "CREATE TABLE IF NOT EXISTS vocabulary_rows (
       fragment INTEGER PRIMARY KEY REFERENCES fragments(id) ON DELETE CASCADE,
       kind TEXT NOT NULL,
       origin TEXT NOT NULL,
       normalized TEXT NOT NULL,
       document_frequency INTEGER NOT NULL DEFAULT 0,
       cluster INTEGER,
       gloss TEXT
     );
     CREATE INDEX IF NOT EXISTS vocabulary_rows_by_normalized ON vocabulary_rows(normalized);
     CREATE INDEX IF NOT EXISTS vocabulary_rows_by_cluster ON vocabulary_rows(cluster);
     CREATE TABLE IF NOT EXISTS clusters (
       id INTEGER PRIMARY KEY,
       label TEXT NOT NULL,
       text_digest TEXT NOT NULL,
       vector BLOB,
       member_count INTEGER NOT NULL DEFAULT 0,
       document_frequency INTEGER NOT NULL DEFAULT 0,
       changed_sweep INTEGER NOT NULL DEFAULT 0
     );";

pub const VOCABULARY_SCHEMA_DROP_SQL: &str = "DROP TABLE IF EXISTS vocabulary_rows;
     DROP TABLE IF EXISTS clusters;";

/// Most rows one listing returns; the CLI pages past it.
pub const VOCABULARY_LIST_MAX: u32 = 10_000;
/// Most clusters an index holds; the pass refuses to found more.
pub const CLUSTERS_MAX: u32 = 100_000;

/// What kind of thing a vocabulary row names. The kind decides how strongly
/// the row conducts in the walk (`design/vocabulary.md`, retrieval) and
/// which line of the ledger it lands on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum VocabularyKind {
    /// A word or phrase local to the corpus, mined or confirmed from text.
    Term,
    /// A ticket, PR, version, metric, or config name, exactly as written.
    Identifier,
    /// A person, customer, project, place: the entity plugins' rows.
    Entity,
    /// What a searcher would say instead of a row: a paraphrase or variant.
    Alias,
    /// What the host says about a source: its host kind, its container.
    Facet,
}

impl VocabularyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Term => "term",
            Self::Identifier => "identifier",
            Self::Entity => "entity",
            Self::Alias => "alias",
            Self::Facet => "facet",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "term" => Some(Self::Term),
            "identifier" => Some(Self::Identifier),
            "entity" => Some(Self::Entity),
            "alias" => Some(Self::Alias),
            "facet" => Some(Self::Facet),
            _ => None,
        }
    }

    pub const ALL: [Self; 5] = [
        Self::Term,
        Self::Identifier,
        Self::Entity,
        Self::Alias,
        Self::Facet,
    ];
}

impl fmt::Display for VocabularyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a vocabulary row came from — what may retract it. A mined row is
/// the pass's own and leaves when the statistics stop naming it; an
/// extracted row belongs to the transform that emitted it; a grounded row
/// (an alias, a gloss) is the cluster pass's; an envelope row is a facet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VocabularyOrigin {
    Mined,
    Extracted,
    Grounded,
    Envelope,
}

impl VocabularyOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mined => "mined",
            Self::Extracted => "extracted",
            Self::Grounded => "grounded",
            Self::Envelope => "envelope",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mined" => Some(Self::Mined),
            "extracted" => Some(Self::Extracted),
            "grounded" => Some(Self::Grounded),
            "envelope" => Some(Self::Envelope),
            _ => None,
        }
    }
}

/// Identifier of a stored cluster, local to one node's index.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ClusterId(pub i64);

impl fmt::Display for ClusterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A vocabulary row as the store holds it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct VocabularyRow {
    pub fragment: FragmentId,
    pub key: FragmentKey,
    pub kind: VocabularyKind,
    pub origin: VocabularyOrigin,
    /// The row's text as it is shown and searched.
    pub spelling: String,
    /// The lowercase, whitespace-collapsed spelling every exact match uses.
    pub normalized: String,
    /// Sources whose text spells the row (matched rows) or that anchor to
    /// it by relation (envelope rows), as of the last pass.
    pub document_frequency: u32,
    pub cluster: Option<ClusterId>,
    pub gloss: Option<String>,
}

/// A vocabulary row to plant: get-or-create the keyed fragment, then file
/// the row beside it. Planting an existing key leaves the fragment and
/// its kind alone; the row is filed when none is filed yet, and its
/// document frequency is set either way — the pass measures it in memory
/// and the store never counts it (`design/vocabulary.md`, storage).
#[derive(Debug, Clone, PartialEq)]
pub struct NewVocabularyRow {
    pub key: FragmentKey,
    pub fragment: NewFragment,
    pub kind: VocabularyKind,
    pub origin: VocabularyOrigin,
    pub normalized: String,
    /// Sources whose text spells the row, as the pass matched them.
    pub document_frequency: u32,
}

/// One planted row: its fragment id and whether this planting created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlantedRow {
    pub fragment: FragmentId,
    pub created: bool,
}

/// A cluster as the store holds it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StoredCluster {
    pub id: ClusterId,
    /// The cluster's most frequent member spelling: a name for listings.
    pub label: String,
    /// Digest of the text the vector was embedded from.
    pub text_digest: String,
    #[serde(skip)]
    pub vector: Option<Vec<f32>>,
    pub member_count: u32,
    /// The sum of the members' document frequencies.
    pub document_frequency: u32,
    /// The pass generation that last changed membership.
    pub changed_sweep: u64,
}

/// A source with facets and a landed root: what facet rows anchor from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetedSource {
    pub source: SourceId,
    pub host: HostId,
    pub root: FragmentId,
    pub facets: Vec<Facet>,
}

/// A source-content text fragment: what the vocabulary pass matches
/// candidates against and anchors rows to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentText {
    pub fragment: FragmentId,
    pub source: SourceId,
    pub text: String,
}

/// A fragment's row kind for the walk: a vocabulary row's kind, or one of
/// the graph's other shapes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum RowKind {
    Prose,
    Summary,
    Entry,
    Term,
    Identifier,
    Entity,
    Alias,
    Facet,
    Other,
}

impl RowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prose => "prose",
            Self::Summary => "summary",
            Self::Entry => "entry",
            Self::Term => "term",
            Self::Identifier => "identifier",
            Self::Entity => "entity",
            Self::Alias => "alias",
            Self::Facet => "facet",
            Self::Other => "other",
        }
    }

    pub const ALL: [Self; 9] = [
        Self::Prose,
        Self::Summary,
        Self::Entry,
        Self::Term,
        Self::Identifier,
        Self::Entity,
        Self::Alias,
        Self::Facet,
        Self::Other,
    ];

    fn of_vocabulary(kind: VocabularyKind) -> Self {
        match kind {
            VocabularyKind::Term => Self::Term,
            VocabularyKind::Identifier => Self::Identifier,
            VocabularyKind::Entity => Self::Entity,
            VocabularyKind::Alias => Self::Alias,
            VocabularyKind::Facet => Self::Facet,
        }
    }

    /// The kind of a fragment that is no vocabulary row, from its mimetype
    /// and whether it belongs to a source.
    fn of_mimetype(essence: &str, keyed: bool) -> Self {
        if essence == "text/x-inseam-summary" || essence == "text/x-inseam-hint" {
            Self::Summary
        } else if essence == "text/x-inseam-entry" {
            Self::Entry
        } else if essence == "text/x-inseam-entity" {
            Self::Entity
        } else if essence == "text/x-inseam-term" {
            Self::Term
        } else if essence == "text/x-inseam-identifier" {
            Self::Identifier
        } else if keyed || essence.starts_with("text/x-inseam-") {
            Self::Other
        } else {
            Self::Prose
        }
    }
}

impl fmt::Display for RowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which text-bearing fragments are source content for the pass: anything
/// not inseam-defined, plus a verbatim summary (the text itself, kept
/// whole). Qualified on the `fragments` alias `g`.
const CONTENT_TEXT_PREDICATE: &str = "g.mimetype NOT LIKE 'text/x-inseam-%' \
     OR g.mimetype LIKE 'text/x-inseam-summary;via=verbatim%'";
/// Which sources hold content: a folder is a container whose text is its
/// entries' hints, written after the pass and never a document — it is
/// neither mined nor a row's anchor. Joins `sources` as `s` on `g`.
const CONTENT_SOURCE_JOIN: &str =
    "JOIN sources s ON s.id = g.source AND s.source_type != 'directory'";

/// The store's own name for the vocabulary generation counter: bumped by
/// every pass that changed a row or a cluster, so query-time caches know
/// when to reload.
const GENERATION_KEY: &str = "vocabulary_generation";
/// The digest of the pass configuration the vocabulary was last built
/// under; a change re-runs the pass in full.
const CONFIG_DIGEST_KEY: &str = "vocabulary_config_digest";

impl IndexStore {
    // ------------------------------------------------------------------
    // Text to mine and match
    // ------------------------------------------------------------------

    /// Source-content text fragments with ids past `after`, in id order, at
    /// most `limit` of them: one page of the text the pass walks. Derived
    /// understanding (`text/x-inseam-*`) and keyed fragments are excluded —
    /// they are candidates, never anchors (`design/vocabulary.md`) — with
    /// one exception: a **verbatim** summary is the source's own text, kept
    /// whole because it fit the target, and under the lean shape (no
    /// structural transform) it is the only text the source has.
    pub async fn content_texts(
        &self,
        after: FragmentId,
        limit: u32,
    ) -> Result<Vec<ContentText>, StoreError> {
        assert!(limit >= 1);
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "SELECT g.id, g.source, g.text FROM fragments g
                     {CONTENT_SOURCE_JOIN}
                     WHERE g.id > ?1 AND g.text IS NOT NULL
                       AND ({CONTENT_TEXT_PREDICATE})
                     ORDER BY g.id LIMIT ?2"
                ),
                params![after.0, i64::from(limit)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let fragment = FragmentId(row.get(0)?);
            let source = SourceId(row.get(1)?);
            let text: String = row.get(2)?;
            out.push(ContentText {
                fragment,
                source,
                text,
            });
        }
        assert!(out.len() <= usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(out)
    }

    /// The text of derived fragments of one mimetype essence with ids past
    /// `after`, in id order: the summarizer's keywords and the extractors'
    /// cues are the pass's phrase candidates.
    pub async fn derived_texts(
        &self,
        essence: &str,
        after: FragmentId,
        limit: u32,
    ) -> Result<Vec<(FragmentId, String)>, StoreError> {
        assert!(limit >= 1);
        assert!(essence.starts_with("text/x-inseam-"));
        let mut rows = self
            .catalog
            .query(
                "SELECT id, text FROM fragments
                 WHERE id > ?1 AND text IS NOT NULL
                   AND (mimetype = ?2 OR mimetype LIKE ?2 || ';%')
                 ORDER BY id LIMIT ?3",
                params![after.0, essence, i64::from(limit)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((FragmentId(row.get(0)?), row.get::<String>(1)?));
        }
        Ok(out)
    }

    /// Distinct deep-indexed sources holding content text: the corpus size
    /// the document-frequency band is a fraction of.
    pub async fn content_source_count(&self) -> Result<u64, StoreError> {
        self.count_of(&format!(
            "SELECT COUNT(DISTINCT g.source) FROM fragments g
             {CONTENT_SOURCE_JOIN}
             WHERE g.text IS NOT NULL
               AND ({CONTENT_TEXT_PREDICATE})"
        ))
        .await
    }

    /// Sources carrying facets with ids past `after`, in id order, at most
    /// `limit`: what the pass turns into facet rows anchored from the root.
    pub async fn sources_with_facets(
        &self,
        after: SourceId,
        limit: u32,
    ) -> Result<Vec<FacetedSource>, StoreError> {
        assert!(limit >= 1);
        let mut rows = self
            .catalog
            .query(
                "SELECT id, host, root_fragment, facets FROM sources
                 WHERE id > ?1 AND facets != '[]' AND root_fragment IS NOT NULL
                 ORDER BY id LIMIT ?2",
                params![after.0, i64::from(limit)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            let host: String = row.get(1)?;
            let root: i64 = row.get(2)?;
            let facets: String = row.get(3)?;
            out.push(FacetedSource {
                source: SourceId(id),
                host: HostId::new(host).map_err(|e| corrupt(id, e))?,
                root: FragmentId(root),
                facets: serde_json::from_str::<Vec<Facet>>(&facets).map_err(|e| corrupt(id, e))?,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Rows
    // ------------------------------------------------------------------

    /// Plant rows in one transaction: get-or-create each keyed fragment,
    /// file its vocabulary row when none is filed. Returns one entry per
    /// input, in order.
    pub async fn plant_vocabulary_rows(
        &self,
        rows: &[NewVocabularyRow],
    ) -> Result<Vec<PlantedRow>, StoreError> {
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        let mut planted = Vec::with_capacity(rows.len());
        for row in rows {
            let resolved = keyed_fragment_in(&tx, &row.key, &row.fragment).await?;
            let fragment = resolved.id();
            tx.execute(
                "INSERT INTO vocabulary_rows (fragment, kind, origin, normalized, document_frequency)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(fragment) DO UPDATE SET document_frequency = excluded.document_frequency",
                params![
                    fragment.0,
                    row.kind.as_str(),
                    row.origin.as_str(),
                    row.normalized.as_str(),
                    i64::from(row.document_frequency)
                ],
            )
            .await?;
            planted.push(PlantedRow {
                fragment,
                created: matches!(resolved, super::KeyedFragment::Created(_)),
            });
        }
        tx.commit().await?;
        assert_eq!(planted.len(), rows.len());
        Ok(planted)
    }

    /// File vocabulary rows for keyed fragments that exist without one —
    /// the entity, term, and identifier fragments transforms planted
    /// before the vocabulary existed, or plant on every sweep. Returns how
    /// many were filed.
    pub async fn adopt_keyed_fragments(&self) -> Result<u64, StoreError> {
        let _write = self.write().await;
        let mut rows = self
            .catalog
            .query(
                "SELECT k.key, f.id, f.mimetype, f.text FROM keyed_fragments k
                 JOIN fragments f ON f.id = k.fragment
                 WHERE NOT EXISTS (SELECT 1 FROM vocabulary_rows v WHERE v.fragment = f.id)
                 LIMIT ?1",
                params![i64::from(VOCABULARY_LIST_MAX) * 100],
            )
            .await?;
        let mut adopted: Vec<(i64, VocabularyKind, String)> = Vec::new();
        while let Some(row) = rows.next().await? {
            let key: String = row.get(0)?;
            let id: i64 = row.get(1)?;
            let mimetype: String = row.get(2)?;
            let text: Option<String> = row.get(3)?;
            let Some(kind) = kind_of_key(&key, &mimetype) else {
                continue;
            };
            let normalized = normalize_spelling(text.as_deref().unwrap_or_default());
            if normalized.is_empty() {
                continue;
            }
            adopted.push((id, kind, normalized));
        }
        let tx = self.catalog.transaction().await?;
        for (id, kind, normalized) in &adopted {
            tx.execute(
                "INSERT OR IGNORE INTO vocabulary_rows (fragment, kind, origin, normalized)
                 VALUES (?1, ?2, 'extracted', ?3)",
                params![*id, kind.as_str(), normalized.as_str()],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(u64::try_from(adopted.len()).expect("a row count fits u64"))
    }

    /// Anchor rows: one `relation` per `(from, to)`, each `INSERT OR
    /// IGNORE`, in one transaction per call. Callers batch.
    pub async fn anchor_vocabulary(
        &self,
        kind: &RelationKind,
        anchors: &[(FragmentId, FragmentId)],
    ) -> Result<(), StoreError> {
        if anchors.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        for (from, to) in anchors {
            super::insert_relation_in(&tx, &Relation::new(*from, kind.clone(), *to)).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Record the document frequency the pass measured for rows it has,
    /// in one transaction. Callers batch.
    pub async fn set_document_frequencies(
        &self,
        frequencies: &[(FragmentId, u32)],
    ) -> Result<(), StoreError> {
        if frequencies.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        for (fragment, frequency) in frequencies {
            tx.execute(
                "UPDATE vocabulary_rows SET document_frequency = ?2 WHERE fragment = ?1",
                params![fragment.0, i64::from(*frequency)],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Retract rows outright: their vocabulary rows, relations, search
    /// rows, and fragments go in one transaction. A mined row the band no
    /// longer names is retracted this way, since it has no relations for
    /// the keyed-fragment collection to notice.
    pub async fn retract_vocabulary_rows(&self, rows: &[FragmentId]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        let purge_search = search_tables_exist(&tx).await?;
        for chunk in rows.chunks(ID_LIST_CHUNK) {
            let list = id_list(chunk);
            tx.execute(
                &format!(
                    "DELETE FROM relations WHERE to_fragment IN ({list}) OR from_fragment IN ({list})"
                ),
                (),
            )
            .await?;
            tx.execute(
                &format!("DELETE FROM vocabulary_rows WHERE fragment IN ({list})"),
                (),
            )
            .await?;
            if purge_search {
                tx.execute(&format!("DELETE FROM search_rows WHERE id IN ({list})"), ())
                    .await?;
            }
            tx.execute(&format!("DELETE FROM fragments WHERE id IN ({list})"), ())
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Drop the `mentions` relations an earlier pass stored for mined rows.
    /// The full-text index is a mined row's anchor now
    /// (`design/vocabulary.md`, storage); an index built before that
    /// carries millions of edges that say nothing the index does not. A
    /// no-op after the first pass that finds none.
    pub async fn drop_mined_anchors(&self) -> Result<u64, StoreError> {
        let _write = self.write().await;
        let dropped = self
            .catalog
            .execute(
                "DELETE FROM relations WHERE kind = 'mentions' AND to_fragment IN
                   (SELECT fragment FROM vocabulary_rows WHERE origin = 'mined')",
                (),
            )
            .await?;
        Ok(dropped)
    }

    /// Recount the document frequency of the rows whose anchors are
    /// relations — the envelope rows (facets, authors) — from those
    /// relations. Matched rows carry the frequency the pass measured.
    pub async fn recount_envelope_document_frequencies(&self) -> Result<(), StoreError> {
        let _write = self.write().await;
        self.catalog
            .execute(
                "UPDATE vocabulary_rows SET document_frequency =
                   (SELECT COUNT(DISTINCT f.source) FROM relations r
                      JOIN fragments f ON f.id = r.from_fragment
                     WHERE r.to_fragment = vocabulary_rows.fragment AND f.source IS NOT NULL)
                 WHERE origin = 'envelope'",
                (),
            )
            .await?;
        Ok(())
    }

    /// Rows by exact normalized spelling — the exact-grounding lookup. A
    /// spelling names at most a handful of rows (one per kind that spells
    /// it), so this is one index probe per query n-gram.
    pub async fn vocabulary_rows_spelled(
        &self,
        normalized: &str,
    ) -> Result<Vec<VocabularyRow>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "{VOCABULARY_ROW_SQL} WHERE v.normalized = ?1 ORDER BY v.fragment LIMIT 16"
                ),
                params![normalized],
            )
            .await?;
        collect_rows(&mut rows).await
    }

    /// Rows by fragment id, in the chunks the id lists allow.
    pub async fn vocabulary_rows_of(
        &self,
        ids: &[FragmentId],
    ) -> Result<Vec<VocabularyRow>, StoreError> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(ID_LIST_CHUNK) {
            let mut rows = self
                .catalog
                .query(
                    &format!(
                        "{VOCABULARY_ROW_SQL} WHERE v.fragment IN ({}) ORDER BY v.fragment",
                        id_list(chunk)
                    ),
                    (),
                )
                .await?;
            out.extend(collect_rows(&mut rows).await?);
        }
        Ok(out)
    }

    /// Rows of one origin, ids only, unbounded in count but bounded by
    /// the vocabulary's own size: what a retraction compares against.
    pub async fn vocabulary_rows_of_origin(
        &self,
        origin: VocabularyOrigin,
    ) -> Result<Vec<(FragmentId, String)>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT fragment, normalized FROM vocabulary_rows WHERE origin = ?1 ORDER BY fragment",
                params![origin.as_str()],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((FragmentId(row.get(0)?), row.get::<String>(1)?));
        }
        Ok(out)
    }

    /// A page of rows, most frequent first, for listings. `kind` narrows.
    pub async fn vocabulary_rows_page(
        &self,
        kind: Option<VocabularyKind>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<VocabularyRow>, StoreError> {
        let limit = limit.min(VOCABULARY_LIST_MAX);
        let kind_filter = kind.map(|k| k.as_str()).unwrap_or("");
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "{VOCABULARY_ROW_SQL} WHERE (?1 = '' OR v.kind = ?1)
                     ORDER BY v.document_frequency DESC, v.normalized LIMIT ?2 OFFSET ?3"
                ),
                params![kind_filter, i64::from(limit), i64::from(offset)],
            )
            .await?;
        collect_rows(&mut rows).await
    }

    /// Counts per kind and the number of rows over a degree bound: the
    /// pass report's and `status`'s numbers.
    pub async fn vocabulary_counts(&self) -> Result<VocabularyCounts, StoreError> {
        let mut by_kind = Vec::new();
        for kind in VocabularyKind::ALL {
            let count = self
                .count_of_with(
                    "SELECT COUNT(*) FROM vocabulary_rows WHERE kind = ?1",
                    params![kind.as_str()],
                )
                .await?;
            by_kind.push((kind, count));
        }
        let clusters = self.count_of("SELECT COUNT(*) FROM clusters").await?;
        let clustered = self
            .count_of("SELECT COUNT(*) FROM vocabulary_rows WHERE cluster IS NOT NULL")
            .await?;
        let glossed = self
            .count_of("SELECT COUNT(*) FROM vocabulary_rows WHERE gloss IS NOT NULL")
            .await?;
        Ok(VocabularyCounts {
            by_kind,
            clusters,
            clustered,
            glossed,
            generation: self.vocabulary_generation().await?,
        })
    }

    /// Set a row's gloss (the cluster pass writes it once).
    pub async fn set_vocabulary_gloss(
        &self,
        fragment: FragmentId,
        gloss: &str,
    ) -> Result<(), StoreError> {
        let _write = self.write().await;
        self.catalog
            .execute(
                "UPDATE vocabulary_rows SET gloss = ?2 WHERE fragment = ?1",
                params![fragment.0, gloss],
            )
            .await?;
        Ok(())
    }

    /// Merge one row into another: every anchor of `loser` re-keys to
    /// `survivor`, and the loser stays as an **alias** of the survivor —
    /// its spelling still grounds a question, and the next pass sees the
    /// spelling as a row it has and does not plant it again. Returns how
    /// many anchors moved.
    pub async fn merge_vocabulary_rows(
        &self,
        survivor: FragmentId,
        loser: FragmentId,
    ) -> Result<u64, StoreError> {
        assert_ne!(survivor, loser);
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        let moved = tx
            .execute(
                "INSERT OR IGNORE INTO relations (from_fragment, kind, to_fragment)
                 SELECT from_fragment, kind, ?1 FROM relations WHERE to_fragment = ?2",
                params![survivor.0, loser.0],
            )
            .await?;
        tx.execute(
            "DELETE FROM relations WHERE to_fragment = ?1 OR from_fragment = ?1",
            params![loser.0],
        )
        .await?;
        tx.execute(
            "UPDATE vocabulary_rows SET kind = 'alias', origin = 'grounded', cluster = NULL, gloss = NULL
             WHERE fragment = ?1",
            params![loser.0],
        )
        .await?;
        let aliases = RelationKind::new("aliases").expect("literal kind is valid");
        super::insert_relation_in(&tx, &Relation::new(loser, aliases, survivor)).await?;
        tx.commit().await?;
        Ok(moved)
    }

    // ------------------------------------------------------------------
    // Anchors and co-occurrence
    // ------------------------------------------------------------------

    /// The content fragments whose text spells a phrase, in no order, at
    /// most `limit`: a matched row's anchors, read from the prose
    /// full-text index instead of stored relations
    /// (`design/vocabulary.md`, storage). Only the text the pass mines
    /// counts — a derived summary that spells the term is not a source
    /// that does. The phrase is the spelling's alphanumeric runs in
    /// sequence, which is how the index tokenized the text. Empty without
    /// a search surface.
    pub async fn fragments_spelling(
        &self,
        spelling: &str,
        limit: u32,
    ) -> Result<Vec<FragmentId>, StoreError> {
        assert!(limit >= 1);
        let Some(phrase) = fts_phrase_expression(spelling) else {
            return Ok(Vec::new());
        };
        if !self.has_search_surface().await? {
            return Ok(Vec::new());
        }
        self.refuse_while_reembed_pending()?;
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "SELECT f.rowid FROM search_fts f
                     JOIN fragments g ON g.id = f.rowid
                     {CONTENT_SOURCE_JOIN}
                     WHERE search_fts MATCH ?1
                       AND ({CONTENT_TEXT_PREDICATE})
                     LIMIT ?2"
                ),
                params![phrase, i64::from(limit)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(FragmentId(row.get(0)?));
        }
        Ok(out)
    }

    /// The distinct sources whose text spells a phrase, bounded — the same
    /// lookup as [`Self::fragments_spelling`] rolled up to sources.
    pub async fn sources_spelling(
        &self,
        spelling: &str,
        limit: u32,
    ) -> Result<Vec<SourceId>, StoreError> {
        assert!(limit >= 1);
        let Some(phrase) = fts_phrase_expression(spelling) else {
            return Ok(Vec::new());
        };
        if !self.has_search_surface().await? {
            return Ok(Vec::new());
        }
        self.refuse_while_reembed_pending()?;
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "SELECT DISTINCT g.source FROM search_fts f
                     JOIN fragments g ON g.id = f.rowid
                     {CONTENT_SOURCE_JOIN}
                     WHERE search_fts MATCH ?1
                       AND ({CONTENT_TEXT_PREDICATE})
                     ORDER BY g.source LIMIT ?2"
                ),
                params![phrase, i64::from(limit)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(SourceId(row.get(0)?));
        }
        Ok(out)
    }

    /// The distinct sources anchored to a row, bounded.
    pub async fn sources_anchored_to(
        &self,
        row: FragmentId,
        limit: u32,
    ) -> Result<Vec<SourceId>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT DISTINCT f.source FROM relations r
                 JOIN fragments f ON f.id = r.from_fragment
                 WHERE r.to_fragment = ?1 AND f.source IS NOT NULL
                 ORDER BY f.source LIMIT ?2",
                params![row.0, i64::from(limit)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(SourceId(row.get(0)?));
        }
        Ok(out)
    }

    /// Vertex degrees: how many relations touch each id. A vocabulary
    /// row's degree is its anchor count, a folder root's its entry count;
    /// the walk's hub bound reads this (`design/vocabulary.md`).
    pub async fn degrees_of(
        &self,
        ids: &[FragmentId],
    ) -> Result<HashMap<FragmentId, u32>, StoreError> {
        let mut out: HashMap<FragmentId, u32> = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(ID_LIST_CHUNK) {
            let list = id_list(chunk);
            let mut rows = self
                .catalog
                .query(
                    &format!(
                        "SELECT id, SUM(n) FROM (
                           SELECT from_fragment AS id, COUNT(*) AS n FROM relations
                             WHERE from_fragment IN ({list}) GROUP BY from_fragment
                           UNION ALL
                           SELECT to_fragment AS id, COUNT(*) AS n FROM relations
                             WHERE to_fragment IN ({list}) GROUP BY to_fragment
                         ) GROUP BY id"
                    ),
                    (),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let id = FragmentId(row.get(0)?);
                let degree: i64 = row.get(1)?;
                out.insert(id, u32::try_from(degree).unwrap_or(u32::MAX));
            }
        }
        Ok(out)
    }

    /// The row kind of each fragment — vocabulary rows by their filed kind,
    /// every other fragment by its mimetype — without reading any text.
    pub async fn row_kinds_of(
        &self,
        ids: &[FragmentId],
    ) -> Result<HashMap<FragmentId, RowKind>, StoreError> {
        let mut out: HashMap<FragmentId, RowKind> = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(ID_LIST_CHUNK) {
            let list = id_list(chunk);
            let mut rows = self
                .catalog
                .query(
                    &format!(
                        "SELECT f.id, f.mimetype, f.source IS NULL, v.kind FROM fragments f
                         LEFT JOIN vocabulary_rows v ON v.fragment = f.id
                         WHERE f.id IN ({list})"
                    ),
                    (),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let id = FragmentId(row.get(0)?);
                let mimetype: String = row.get(1)?;
                let keyed: i64 = row.get(2)?;
                let kind: Option<String> = row.get(3)?;
                let essence = mimetype.split(';').next().unwrap_or("").trim();
                let row_kind = match kind.as_deref().and_then(VocabularyKind::parse) {
                    Some(kind) => RowKind::of_vocabulary(kind),
                    None => RowKind::of_mimetype(essence, keyed == 1),
                };
                out.insert(id, row_kind);
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Clusters
    // ------------------------------------------------------------------

    /// Every cluster, vectors included: the query-time grounding set and
    /// the pass's working set. Bounded by [`CLUSTERS_MAX`].
    pub async fn clusters(&self) -> Result<Vec<StoredCluster>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT id, label, text_digest, vector, member_count, document_frequency, changed_sweep
                 FROM clusters ORDER BY id LIMIT ?1",
                params![i64::from(CLUSTERS_MAX)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_cluster(&row)?);
        }
        Ok(out)
    }

    /// One cluster, if it exists.
    pub async fn cluster(&self, id: ClusterId) -> Result<Option<StoredCluster>, StoreError> {
        let row = self
            .first_row(
                "SELECT id, label, text_digest, vector, member_count, document_frequency, changed_sweep
                 FROM clusters WHERE id = ?1",
                params![id.0],
            )
            .await?;
        row.map(|r| row_to_cluster(&r)).transpose()
    }

    /// The members of a cluster, most frequent first.
    pub async fn cluster_members(
        &self,
        cluster: ClusterId,
    ) -> Result<Vec<VocabularyRow>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "{VOCABULARY_ROW_SQL} WHERE v.cluster = ?1
                     ORDER BY v.document_frequency DESC, v.fragment LIMIT ?2"
                ),
                params![cluster.0, i64::from(VOCABULARY_LIST_MAX)],
            )
            .await?;
        collect_rows(&mut rows).await
    }

    /// Found clusters, each with its members, in one transaction; returns
    /// one id per input, in order. Refused past [`CLUSTERS_MAX`]: the
    /// clusters that would cross it are not founded and get no id.
    pub async fn found_clusters(
        &self,
        clusters: &[(String, Vec<FragmentId>)],
        sweep: u64,
    ) -> Result<Vec<Option<ClusterId>>, StoreError> {
        if clusters.is_empty() {
            return Ok(Vec::new());
        }
        let _write = self.write().await;
        let mut count = self.count_of("SELECT COUNT(*) FROM clusters").await?;
        let tx = self.catalog.transaction().await?;
        let mut ids = Vec::with_capacity(clusters.len());
        for (label, members) in clusters {
            assert!(!members.is_empty());
            if count >= u64::from(CLUSTERS_MAX) {
                ids.push(None);
                continue;
            }
            let id = super::drain_single_i64(
                tx.query(
                    "INSERT INTO clusters (label, text_digest, member_count, changed_sweep)
                     VALUES (?1, '', ?2, ?3) RETURNING id",
                    params![
                        label.as_str(),
                        i64::try_from(members.len()).expect("member count fits i64"),
                        i64::try_from(sweep).unwrap_or(i64::MAX)
                    ],
                )
                .await?,
            )
            .await?
            .ok_or_else(|| corrupt(0, "cluster insert returned no id"))?;
            set_cluster_in(&tx, ClusterId(id), members).await?;
            ids.push(Some(ClusterId(id)));
            count += 1;
        }
        tx.commit().await?;
        assert_eq!(ids.len(), clusters.len());
        Ok(ids)
    }

    /// Add members to a cluster.
    pub async fn join_cluster(
        &self,
        cluster: ClusterId,
        members: &[FragmentId],
        sweep: u64,
    ) -> Result<(), StoreError> {
        if members.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        set_cluster_in(&tx, cluster, members).await?;
        tx.execute(
            "UPDATE clusters SET changed_sweep = ?2 WHERE id = ?1",
            params![cluster.0, i64::try_from(sweep).unwrap_or(i64::MAX)],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fold `loser` into `survivor`: members move, the loser row goes.
    pub async fn merge_clusters(
        &self,
        survivor: ClusterId,
        loser: ClusterId,
        sweep: u64,
    ) -> Result<(), StoreError> {
        assert_ne!(survivor, loser);
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        tx.execute(
            "UPDATE vocabulary_rows SET cluster = ?1 WHERE cluster = ?2",
            params![survivor.0, loser.0],
        )
        .await?;
        tx.execute("DELETE FROM clusters WHERE id = ?1", params![loser.0])
            .await?;
        tx.execute(
            "UPDATE clusters SET changed_sweep = ?2 WHERE id = ?1",
            params![survivor.0, i64::try_from(sweep).unwrap_or(i64::MAX)],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Recount every cluster's member count and document frequency from
    /// its members; clusters left with no member are dropped. Returns how
    /// many were dropped. The document frequency is the **sum** of the
    /// members' — an upper bound on the distinct sources, which only a
    /// walk over every member's matches could count exactly; listings
    /// order by it and nothing ranks by it.
    pub async fn recount_clusters(&self) -> Result<u64, StoreError> {
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        tx.execute(
            "UPDATE clusters SET
               member_count = (SELECT COUNT(*) FROM vocabulary_rows v WHERE v.cluster = clusters.id),
               document_frequency = (
                 SELECT COALESCE(SUM(v.document_frequency), 0) FROM vocabulary_rows v
                  WHERE v.cluster = clusters.id),
               label = COALESCE((SELECT f.text FROM vocabulary_rows v JOIN fragments f ON f.id = v.fragment
                          WHERE v.cluster = clusters.id
                          ORDER BY v.document_frequency DESC, v.fragment LIMIT 1), label)",
            (),
        )
        .await?;
        let dropped = tx
            .execute("DELETE FROM clusters WHERE member_count = 0", ())
            .await?;
        tx.commit().await?;
        Ok(dropped)
    }

    /// Record a cluster's vector and the digest of the text it embeds.
    pub async fn set_cluster_vector(
        &self,
        cluster: ClusterId,
        text_digest: &str,
        vector: &[f32],
    ) -> Result<(), StoreError> {
        let _write = self.write().await;
        self.catalog
            .execute(
                "UPDATE clusters SET text_digest = ?2, vector = ?3 WHERE id = ?1",
                params![
                    cluster.0,
                    text_digest,
                    libsql::Value::Blob(vector_blob(vector))
                ],
            )
            .await?;
        Ok(())
    }

    /// Clusters whose membership changed at or after `sweep`: the ones the
    /// pass re-embeds and re-grounds.
    pub async fn clusters_changed_since(
        &self,
        sweep: u64,
    ) -> Result<Vec<StoredCluster>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT id, label, text_digest, vector, member_count, document_frequency, changed_sweep
                 FROM clusters WHERE changed_sweep >= ?1 ORDER BY id LIMIT ?2",
                params![
                    i64::try_from(sweep).unwrap_or(i64::MAX),
                    i64::from(CLUSTERS_MAX)
                ],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_cluster(&row)?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Generation and configuration
    // ------------------------------------------------------------------

    /// The vocabulary generation: bumped by every pass that changed a row
    /// or a cluster. Query-time caches reload when it moves.
    pub async fn vocabulary_generation(&self) -> Result<u64, StoreError> {
        let row = self
            .first_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![GENERATION_KEY],
            )
            .await?;
        Ok(match row {
            None => 0,
            Some(row) => row.get::<String>(0)?.parse().unwrap_or(0),
        })
    }

    /// Advance the generation; returns the new value.
    pub async fn bump_vocabulary_generation(&self) -> Result<u64, StoreError> {
        let _write = self.write().await;
        let next = self.vocabulary_generation().await?.saturating_add(1);
        self.catalog
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![GENERATION_KEY, next.to_string()],
            )
            .await?;
        Ok(next)
    }

    /// The configuration digest the vocabulary was last built under.
    pub async fn vocabulary_config_digest(&self) -> Result<Option<String>, StoreError> {
        let row = self
            .first_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![CONFIG_DIGEST_KEY],
            )
            .await?;
        row.map(|r| r.get::<String>(0))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn set_vocabulary_config_digest(&self, digest: &str) -> Result<(), StoreError> {
        let _write = self.write().await;
        self.catalog
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![CONFIG_DIGEST_KEY, digest],
            )
            .await?;
        Ok(())
    }

    /// Whether the derived search tables exist, so a caller adding lexical
    /// rows for planted vocabulary knows there is a surface to add to.
    pub async fn has_search_surface(&self) -> Result<bool, StoreError> {
        search_tables_exist(&self.catalog).await
    }

    async fn count_of_with(
        &self,
        sql: &str,
        params: impl libsql::params::IntoParams,
    ) -> Result<u64, StoreError> {
        let row = self.first_row(sql, params).await?;
        Ok(match row {
            None => 0,
            Some(row) => u64::try_from(row.get::<i64>(0)?).unwrap_or(0),
        })
    }
}

/// Counts for the report and `status`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VocabularyCounts {
    pub by_kind: Vec<(VocabularyKind, u64)>,
    pub clusters: u64,
    pub clustered: u64,
    pub glossed: u64,
    pub generation: u64,
}

const VOCABULARY_ROW_SQL: &str = "SELECT v.fragment, k.key, v.kind, v.origin, f.text, v.normalized,
        v.document_frequency, v.cluster, v.gloss
 FROM vocabulary_rows v
 JOIN fragments f ON f.id = v.fragment
 JOIN keyed_fragments k ON k.fragment = v.fragment";

async fn collect_rows(rows: &mut libsql::Rows) -> Result<Vec<VocabularyRow>, StoreError> {
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(row_to_vocabulary(&row)?);
    }
    Ok(out)
}

fn row_to_vocabulary(row: &libsql::Row) -> Result<VocabularyRow, StoreError> {
    let id: i64 = row.get(0)?;
    let key: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let origin: String = row.get(3)?;
    let text: Option<String> = row.get(4)?;
    let normalized: String = row.get(5)?;
    let document_frequency: i64 = row.get(6)?;
    let cluster: Option<i64> = row.get(7)?;
    let gloss: Option<String> = row.get(8)?;
    Ok(VocabularyRow {
        fragment: FragmentId(id),
        key: FragmentKey::new(key).map_err(|e| corrupt(id, e))?,
        kind: VocabularyKind::parse(&kind).ok_or_else(|| corrupt(id, "unknown vocabulary kind"))?,
        origin: VocabularyOrigin::parse(&origin)
            .ok_or_else(|| corrupt(id, "unknown vocabulary origin"))?,
        spelling: text.unwrap_or_default(),
        normalized,
        document_frequency: u32::try_from(document_frequency).unwrap_or(u32::MAX),
        cluster: cluster.map(ClusterId),
        gloss,
    })
}

fn row_to_cluster(row: &libsql::Row) -> Result<StoredCluster, StoreError> {
    let id: i64 = row.get(0)?;
    let label: String = row.get(1)?;
    let text_digest: String = row.get(2)?;
    let vector: Option<Vec<u8>> = row.get(3)?;
    let member_count: i64 = row.get(4)?;
    let document_frequency: i64 = row.get(5)?;
    let changed_sweep: i64 = row.get(6)?;
    Ok(StoredCluster {
        id: ClusterId(id),
        label,
        text_digest,
        vector: vector
            .filter(|blob| !blob.is_empty())
            .map(|blob| vector_from_blob(&blob)),
        member_count: u32::try_from(member_count).unwrap_or(u32::MAX),
        document_frequency: u32::try_from(document_frequency).unwrap_or(u32::MAX),
        changed_sweep: u64::try_from(changed_sweep).unwrap_or(0),
    })
}

async fn set_cluster_in(
    tx: &libsql::Transaction,
    cluster: ClusterId,
    members: &[FragmentId],
) -> Result<(), StoreError> {
    for chunk in members.chunks(ID_LIST_CHUNK) {
        let list = id_list(chunk);
        tx.execute(
            &format!("UPDATE vocabulary_rows SET cluster = ?1 WHERE fragment IN ({list})"),
            params![cluster.0],
        )
        .await?;
    }
    Ok(())
}

/// The kind a transform-planted keyed fragment has, from its key namespace
/// and mimetype. Keys are plugin conventions, so this is best effort: an
/// unrecognised namespace is not adopted.
fn kind_of_key(key: &str, mimetype: &str) -> Option<VocabularyKind> {
    let namespace = key.split(':').next().unwrap_or("");
    let essence = mimetype.split(';').next().unwrap_or("").trim();
    match (namespace, essence) {
        ("entity", _) => Some(VocabularyKind::Entity),
        ("term", _) | (_, "text/x-inseam-term") => Some(VocabularyKind::Term),
        ("identifier", _) | (_, "text/x-inseam-identifier") => Some(VocabularyKind::Identifier),
        ("alias", _) | (_, "text/x-inseam-alias") => Some(VocabularyKind::Alias),
        ("facet", _) | (_, "text/x-inseam-facet") => Some(VocabularyKind::Facet),
        _ => None,
    }
}

/// The one normalization every exact match uses: lowercase, inner
/// whitespace collapsed to one space, edges trimmed. A row's key is built
/// A spelling as an FTS5 phrase: its alphanumeric runs, each quoted, in
/// one quoted sequence, so `eu-central-1` asks for the tokens `eu`,
/// `central`, `1` adjacent and in order — the way the index tokenized the
/// text. `None` when nothing alphanumeric remains.
fn fts_phrase_expression(spelling: &str) -> Option<String> {
    let cleaned: String = spelling
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    Some(format!("\"{}\"", tokens.join(" ")))
}

/// from it, so a spelling matches by construction.
pub fn normalize_spelling(spelling: &str) -> String {
    spelling
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_normalize_case_and_whitespace() {
        assert_eq!(normalize_spelling("  Greg   HUNT "), "greg hunt");
        assert_eq!(normalize_spelling("eu-central-1"), "eu-central-1");
        assert_eq!(normalize_spelling(""), "");
    }

    #[test]
    fn kinds_and_origins_roundtrip() {
        for kind in VocabularyKind::ALL {
            assert_eq!(VocabularyKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(VocabularyKind::parse("nope"), None);
        for origin in [
            VocabularyOrigin::Mined,
            VocabularyOrigin::Extracted,
            VocabularyOrigin::Grounded,
            VocabularyOrigin::Envelope,
        ] {
            assert_eq!(VocabularyOrigin::parse(origin.as_str()), Some(origin));
        }
    }

    #[test]
    fn transform_keys_adopt_by_namespace_then_mimetype() {
        assert_eq!(
            kind_of_key("entity:person:greg", "text/x-inseam-entity;kind=person"),
            Some(VocabularyKind::Entity)
        );
        assert_eq!(
            kind_of_key("term:eu-central-1", "text/x-inseam-term"),
            Some(VocabularyKind::Term)
        );
        assert_eq!(
            kind_of_key("identifier:sup-1", "text/x-inseam-identifier"),
            Some(VocabularyKind::Identifier)
        );
        assert_eq!(kind_of_key("citation:x", "text/x-inseam-citation"), None);
    }

    #[test]
    fn row_kinds_come_from_vocabulary_kind_first_then_mimetype() {
        assert_eq!(
            RowKind::of_vocabulary(VocabularyKind::Alias),
            RowKind::Alias
        );
        assert_eq!(
            RowKind::of_mimetype("text/x-inseam-summary", false),
            RowKind::Summary
        );
        assert_eq!(
            RowKind::of_mimetype("text/x-inseam-hint", false),
            RowKind::Summary
        );
        assert_eq!(
            RowKind::of_mimetype("text/x-inseam-entry", false),
            RowKind::Entry
        );
        assert_eq!(RowKind::of_mimetype("text/markdown", false), RowKind::Prose);
        assert_eq!(
            RowKind::of_mimetype("text/x-inseam-citation", true),
            RowKind::Other
        );
    }
}
