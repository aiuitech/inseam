//! The index store — the kernel's second responsibility (`design/kernel.md`).
//! One libSQL database, two layers, as `design/runtime.md` settled: the
//! catalog tables are the transactional source of truth (sources + envelopes,
//! the semantic graph of fragments + relations + the keyed-fragment registry, and
//! plugin state namespaces); the search tables — FTS5 full-text and native
//! vectors — are derived, rebuildable from the catalog tables at any time.
//!
//! There are no data migrations, anywhere, ever: a schema-version bump drops
//! and recreates the tables (everything here is derived and rebuilt by the
//! next sweep), and plugins extend by vocabulary — mimetypes, relation kinds,
//! properties — never by DDL. The search surface binds lazily to whatever
//! embedding identity the mounted embedder declares; a changed identity pends
//! an in-place re-embed instead of refusing to open.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use libsql::params;
use thiserror::Error;

use crate::address::{
    Address, ContentDigest, ContentLength, Envelope, HostId, Locator, Property, Timestamp,
};
use crate::fragment::{
    Extent, FragmentId, FragmentKey, Mimetype, NewFragment, Relation, RelationKind,
};
use crate::subtree::{PlanNode, SubtreePlan};

const SCHEMA_VERSION: &str = "7";
/// Ids per `IN (...)` predicate: every id-list query and delete is issued in
/// chunks of this many, so no caller can build unbounded SQL.
const ID_LIST_CHUNK: usize = 400;
const SEARCH_VECTOR_INDEX: &str = "search_rows_vector_idx";
/// The DiskANN graph's shape: one-bit neighbor vectors, 32 neighbors per
/// node, a 400-entry search list. Measured on BEIR NFCorpus at 384 dims
/// (vector-only nDCG@10, exact cosine 0.368): 8 one-bit neighbors scored
/// 0.299 at 1.2 KB per row; 32 with this search list 0.367 at 4.0 KB per
/// row; 16 float8 neighbors 0.368 at 8 KB per row. The graph is where the
/// recall lives, so this is the lean setting that still finds what exact
/// search finds (`design/runtime.md`).
const SEARCH_VECTOR_INDEX_PARAMETERS: &str =
    "'metric=cosine', 'compress_neighbors=float1bit', 'max_neighbors=32', 'search_l=400'";
const SEARCH_VECTOR_CANDIDATE_MULTIPLIER: usize = 4;
#[cfg(not(test))]
const SEARCH_VECTOR_MIGRATION_BATCH_ROWS: u64 = 4_096;
#[cfg(test)]
const SEARCH_VECTOR_MIGRATION_BATCH_ROWS: u64 = 1;
const SEARCH_VECTOR_MIGRATION_BATCHES_MAX: u64 = 1_000_000;
/// The full-text split reads the catalog's text in batches of this many
/// rows, each landed in its own transaction.
const SEARCH_FTS_MIGRATION_BATCH_ROWS: i64 = 4_096;
const SEARCH_FTS_MIGRATION_BATCHES_MAX: u64 = 1_000_000;
pub const RELATION_HOPS_MAX: u32 = 4;
pub const RELATION_LIMIT_MAX: u32 = 100_000;

/// Drops the derived search tables (and their sync triggers) — the inverse
/// of [`search_schema_sql`], used by re-embeds and the schema converge.
const SEARCH_SCHEMA_DROP_SQL: &str = "DROP TRIGGER IF EXISTS search_rows_after_insert;
     DROP TRIGGER IF EXISTS search_rows_after_delete;
     DROP TABLE IF EXISTS search_fts;
     DROP TABLE IF EXISTS search_fts_lexical;
     DROP TABLE IF EXISTS search_rows;";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store database error: {0}")]
    Database(#[from] libsql::Error),
    #[error("could not prepare index directory {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error(
        "index was embedded with {stored}, the mounted embedder declares {declared}; \
         run `inseam index <dir>` to re-embed the search index"
    )]
    ReembedRequired {
        stored: EmbeddingIdentity,
        declared: EmbeddingIdentity,
    },
    #[error("no embedder has declared a search surface; mount an embedder plugin first")]
    NoSearchSurface,
    #[error(
        "vector has {actual} dimensions, the declared search surface has {expected}; \
         the embedder's declaration and its output disagree"
    )]
    DimensionMismatch { expected: usize, actual: usize },
    #[error("stored row {0} is corrupt: {1}")]
    Corrupt(i64, String),
}

/// Identifier of a cataloged source, local to this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub i64);

/// A cataloged source: address + envelope, plus index bookkeeping.
#[derive(Debug, Clone)]
pub struct StoredSource {
    pub id: SourceId,
    pub address: Address,
    pub envelope: Envelope,
    pub root_fragment: Option<FragmentId>,
}

/// A fragment as the graph stores it. `source` is `None` only for keyed
/// fragments, which belong to no single source and are deduplicated across
/// the whole index under their key.
#[derive(Debug, Clone)]
pub struct StoredFragment {
    pub id: FragmentId,
    pub source: Option<SourceId>,
    pub mimetype: Mimetype,
    pub text: Option<String>,
    pub extent: Option<Extent>,
    /// Where the fragment's bytes live when the index holds a reference
    /// instead of text ([`NewFragment::content_address`]).
    pub content_address: Option<Address>,
}

/// The index bookkeeping the sweep reads to decide dirtiness: content-change
/// fields plus the two claims-aware shape records
/// (`design/index-maintenance.md`) — the **shape stamp** (digest of the
/// transform entries that participated in the subtree) and the **mimetype
/// inventory** (every mimetype present, root and emitted), so a newly mounted
/// transform dirties only sources whose inventory intersects its claims.
#[derive(Debug, Clone)]
pub struct SourceIndexMeta {
    pub modified: Option<Timestamp>,
    pub raw_bytes: u64,
    /// The digest the last run recorded for the content it read — a
    /// folder's change detector, since a directory's timestamp says nothing
    /// about what its children now summarize to (`design/indexing.md`).
    pub content_digest: Option<ContentDigest>,
    pub indexed: bool,
    pub shape_stamp: Option<String>,
    pub mimetypes: Vec<InventoryEntry>,
}

/// One direct child of a folder source as its listing sees it: the name
/// (the last locator segment), the child's type, and its summary when a
/// run has built one. Catalog-only children carry `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderChild {
    pub address: Address,
    pub name: String,
    pub content_type: Mimetype,
    pub summary: Option<String>,
}

/// One inventory record: a mimetype present in a source's subtree and
/// whether it appeared at the root.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InventoryEntry {
    pub mimetype: String,
    pub is_root: bool,
}

/// A row bound for the search tables: a text-bearing fragment and its
/// (optional) embedding. The role decides which full-text table the text
/// enters: prose rows and lexical rows keep separate statistics.
#[derive(Debug, Clone)]
pub struct SearchRow {
    pub fragment: FragmentId,
    pub source: Option<SourceId>,
    pub text: String,
    pub vector: Option<Vec<f32>>,
    pub role: SearchRole,
}

/// What the store assigned when it landed a [`SubtreePlan`]: the source's
/// id, its root, and the ids of the planned fragments and keyed fragments,
/// each by plan position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeWritten {
    pub source: SourceId,
    pub root: FragmentId,
    /// `fragments[i]` is the id of the plan's `i`th fragment.
    pub fragments: Vec<FragmentId>,
    /// `keyed[i]` resolves the plan's `i`th keyed sprout.
    pub keyed: Vec<KeyedFragment>,
}

impl SubtreeWritten {
    pub fn id_of(&self, node: PlanNode) -> FragmentId {
        match node {
            PlanNode::Root => self.root,
            PlanNode::Fragment(n) => {
                let index = usize::try_from(n).expect("plan positions fit usize");
                self.fragments[index]
            }
        }
    }
}

/// A source whose run has finished: its shape records, to be written with
/// the `indexed` mark once its search rows have landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCompletion {
    pub source: SourceId,
    pub stamp: String,
    pub inventory: Vec<InventoryEntry>,
}

/// What the digest-keyed artifact caches hold (`design/indexing.md`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheCounts {
    /// Vectors filed by text digest, model, and width.
    pub embeddings: u64,
    /// Transform outputs filed by input digest and transform identity.
    pub transforms: u64,
}

/// How a source enters the catalog without a subtree
/// (`design/index-maintenance.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogMark {
    /// Newly seen outside the cutoff: cataloged, not marked indexed.
    Seen,
    /// Past this run's deep-index budget: marked indexed without a shape,
    /// which keeps it dirty for a later run with budget.
    CatalogOnly,
}

/// One catalog-only write: a source the sweep saw but did not deep-index.
#[derive(Debug, Clone)]
pub struct CatalogEntry<'a> {
    pub address: &'a Address,
    pub envelope: &'a Envelope,
    pub raw_bytes: u64,
    pub mark: CatalogMark,
}

/// A source is **deep-indexed** when its `indexed` mark is set *and* it
/// carries a shape stamp; a catalog-only row is marked without a stamp
/// (`CatalogMark::CatalogOnly`), which records the run saw it while keeping
/// it dirty. Every count and listing of "indexed" sources uses this one
/// predicate.
const DEEP_INDEXED: &str = "indexed = 1 AND shape_stamp IS NOT NULL";

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct StoreStats {
    pub sources: u64,
    /// Sources with a landed fragment subtree (deep-indexed), not the
    /// catalog-only rows a run marked as seen.
    pub indexed_sources: u64,
    pub fragments: u64,
    pub relations: u64,
    pub keyed_fragments: u64,
    /// Bytes the store occupies on disk: the database file plus its
    /// write-ahead log, which holds commits not yet checkpointed.
    pub store_bytes: u64,
    /// Bytes of source content the catalog covers (the `raw_bytes` every
    /// cataloged row carries, summed) — the hosts' size, not the node's.
    pub content_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchIndexRepair {
    Ensure,
    Rebuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchIndexRepairOutcome {
    Empty,
    AlreadyReady,
    Built,
    Rebuilt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchIndexRepairReport {
    pub search_rows: u64,
    pub vectors_converted: u64,
    pub outcome: SearchIndexRepairOutcome,
}

/// Which cataloged sources a listing selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSelection {
    All,
    Indexed,
    /// Cataloged but with no landed subtree: catalog-only, past the cutoff,
    /// or interrupted.
    Pending,
}

/// One row of a catalog listing: what the catalog knows about a source
/// without loading its envelope.
#[derive(Debug, Clone)]
pub struct CatalogRow {
    pub address: Address,
    pub indexed: bool,
    pub content_type: Mimetype,
    pub raw_bytes: u64,
    pub modified: Option<Timestamp>,
}

/// Counts over one host selection of the catalog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatalogCounts {
    pub sources: u64,
    pub indexed: u64,
    pub pending: u64,
}

/// The outcome of asking for a keyed fragment: the id it already had, or the
/// id it was just created under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyedFragment {
    Existing(FragmentId),
    Created(FragmentId),
}

impl KeyedFragment {
    pub fn id(&self) -> FragmentId {
        match self {
            Self::Existing(id) | Self::Created(id) => *id,
        }
    }
}

/// What a search row is to the vector scope — the one fact the scope
/// branches on. Vectors are for prose: source content and the summary
/// derived from it. Names and terms — keywords, keyed fragments such as
/// entities, a folder's entries — are what full-text search matches
/// exactly, and a vector over a list of names buys nothing it does not
/// already have (`design/indexing.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchRole {
    /// Source prose: a section, a chunk, a transcript.
    Content,
    /// The mandatory summary (`text/x-inseam-summary`) and the hints
    /// plugin's prose written to be found (`text/x-inseam-hint`): the rows
    /// a vector is worth buying for.
    Summary,
    /// Names and terms, matched by the full-text index alone: keywords,
    /// keyed fragments, and every other derived type.
    Lexical,
}

impl SearchRole {
    /// The role of a fragment from its mimetype and whether it belongs to
    /// a source at all (keyed fragments belong to none).
    pub fn of(mimetype: &Mimetype, keyed: bool) -> Self {
        if mimetype.is_summary() || mimetype.is_hint() {
            Self::Summary
        } else if keyed || mimetype.is_inseam_defined() {
            Self::Lexical
        } else {
            Self::Content
        }
    }
}

/// Which text-bearing fragments get vectors. Every text fragment enters the
/// full-text index regardless; the scope decides which of them also pay for
/// a vector — summaries alone make a lean index whose vector bulk is one
/// bounded row per source (`design/indexing.md`). Rows in the
/// [`SearchRole::Lexical`] role never carry one under either scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorScope {
    /// Every prose fragment — source content and summaries — gets a vector.
    #[default]
    All,
    /// Only summary fragments (`text/x-inseam-summary`) get vectors.
    Summaries,
}

impl VectorScope {
    /// The stable spelling persisted in the store's meta table.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Summaries => "summaries",
        }
    }

    /// Parse the persisted spelling. An unknown spelling can only come from
    /// a newer store; it reads as the widest scope so the mismatch pends a
    /// re-embed instead of silently narrowing the search surface.
    fn parse(value: &str) -> Self {
        match value {
            "summaries" => Self::Summaries,
            _ => Self::All,
        }
    }

    /// Whether a row in this role gets a vector under this scope.
    pub fn covers(self, role: SearchRole) -> bool {
        match (self, role) {
            (_, SearchRole::Lexical) => false,
            (Self::All, SearchRole::Content) => true,
            (Self::All, SearchRole::Summary) => true,
            (Self::Summaries, SearchRole::Summary) => true,
            (Self::Summaries, SearchRole::Content) => false,
        }
    }
}

/// The embedding identity the search tables are bound to: the model, the
/// vector width, and which fragments carry vectors. The store compares the
/// identity it was built under with the one the mounted embedder declares;
/// any difference pends an in-place re-embed (`design/index-maintenance.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingIdentity {
    pub model: String,
    pub dimensions: usize,
    pub vectors: VectorScope,
}

impl std::fmt::Display for EmbeddingIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` ({} dims", self.model, self.dimensions)?;
        match self.vectors {
            VectorScope::All => f.write_str(")"),
            VectorScope::Summaries => f.write_str(", summaries only)"),
        }
    }
}

/// One text-bearing fragment to re-populate the search table with: what the
/// indexer would have buffered when it built the subtree, plus the one fact
/// the vector scope branches on.
#[derive(Debug, Clone)]
pub struct ReembedTarget {
    pub fragment: FragmentId,
    pub source: Option<SourceId>,
    pub text: String,
    pub role: SearchRole,
}

/// What the store knows about its search surface, under one lock so the two
/// facts are never observed out of step.
#[derive(Default)]
struct SearchState {
    /// `None` until an embedder plugin declares the embedding identity; the
    /// search surface belongs to that identity, not to the store's opening.
    surface: Option<EmbeddingIdentity>,
    /// The identity the index was embedded with when that differs from the
    /// declared identity: search refuses until an index run re-embeds
    /// (`design/index-maintenance.md`).
    reembed_from: Option<EmbeddingIdentity>,
}

/// How long a connection waits for another process's write to land before
/// reporting the database locked. A subtree lands in one short transaction,
/// so a status probe beside an indexing run waits milliseconds, not this.
const BUSY_TIMEOUT_MS: u32 = 5_000;

pub struct IndexStore {
    #[expect(dead_code, reason = "keeps the database handle alive for its connections")]
    db: libsql::Database,
    /// The database file; its size (with the WAL beside it) is the store's
    /// footprint on disk.
    database_path: PathBuf,
    catalog: libsql::Connection,
    search: Mutex<SearchState>,
    /// Serializes every write. One libSQL connection carries one open
    /// transaction at a time, so two tasks writing concurrently — the sweep's
    /// subtree landing and its embedding landing, say — would interleave
    /// their statements into each other's transactions. Holding this across
    /// each write keeps every write atomic on its own; reads stay free.
    write_lock: tokio::sync::Mutex<()>,
}

impl IndexStore {
    /// Open (or create) the store in `dir`. The catalog and graph are usable
    /// immediately; search surfaces come up when [`Self::declare_embedding`]
    /// binds an embedding identity.
    pub async fn open(dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let database_path = dir.join("catalog.sqlite3");
        let db = libsql::Builder::new_local(&database_path).build().await?;
        let catalog = db.connect()?;
        catalog.query("PRAGMA journal_mode = WAL", ()).await?;
        // WAL + NORMAL: a commit appends to the log without an fsync; the
        // log is synced at checkpoints. A power cut can lose the last
        // commits but can never corrupt the file — and every table here is
        // rebuildable (`design/kernel.md`), with the sweep re-indexing any
        // source whose `indexed` mark did not survive. FULL would fsync per
        // commit, which dominated index time before subtrees landed in one
        // transaction each.
        catalog.query("PRAGMA synchronous = NORMAL", ()).await?;
        catalog.query("PRAGMA foreign_keys = ON", ()).await?;
        // A second process — `inseam status` beside a running `inseam
        // index` — opens the same file, and its schema convergence takes
        // the write lock. Without a busy timeout SQLite answers "database is
        // locked" the instant a subtree transaction holds it; with one it
        // waits, bounded, for that transaction to land.
        catalog
            .query(&format!("PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}"), ())
            .await?;
        converge_schema(&catalog).await?;
        Ok(Self {
            db,
            database_path,
            catalog,
            search: Mutex::new(SearchState::default()),
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Take the write turn. Every mutating method holds the guard for its
    /// whole statement or transaction.
    async fn write(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.write_lock.lock().await
    }

    /// The search state, for a short synchronous read or update. A poisoned
    /// lock is recovered: the state is plain data, never left half-written.
    fn search(&self) -> MutexGuard<'_, SearchState> {
        self.search.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bind the search surface to the mounted embedder's identity. Called by
    /// the embedder provider when it activates. A mismatch with the identity
    /// the index was built under pends an in-place re-embed rather than
    /// refusing; searches refuse until an index run performs it.
    pub async fn declare_embedding(&self, identity: EmbeddingIdentity) -> Result<(), StoreError> {
        let _write = self.write().await;
        let stored = read_embedding_meta(&self.catalog).await?;
        let pending = match stored {
            None => {
                set_embedding_meta(&self.catalog, &identity).await?;
                None
            }
            Some(stored) if stored == identity => None,
            Some(mismatch) => Some(mismatch),
        };

        // A surface built with one full-text table is recognised before the
        // schema statements run, since those create the lexical table
        // (empty) and would hide the difference.
        let split_pending = search_tables_exist(&self.catalog).await?
            && !search_fts_lexical_exists(&self.catalog).await?;
        // `IF NOT EXISTS` deliberately leaves tables built under a different
        // identity in place: searches refuse while the re-embed is pending,
        // and `begin_reembed` recreates them under the new dimensions.
        self.catalog
            .execute_batch(&search_schema_sql(identity.dimensions))
            .await?;
        migrate_search_text_layout(&self.catalog).await?;
        if split_pending {
            migrate_search_role_layout(&self.catalog).await?;
        }
        ensure_search_vector_schema(&self.catalog, identity.dimensions).await?;
        let mut search = self.search();
        search.surface = Some(identity);
        search.reembed_from = pending;
        Ok(())
    }

    /// Withdraw the search surface (the embedder unmounted). Catalog and
    /// graph stay serviceable; searches refuse until a new declaration.
    pub fn withdraw_embedding(&self) {
        self.search().surface = None;
    }

    fn surface(&self) -> Result<EmbeddingIdentity, StoreError> {
        self.search().surface.clone().ok_or(StoreError::NoSearchSurface)
    }

    // ------------------------------------------------------------------
    // Catalog
    // ------------------------------------------------------------------

    /// What the index recorded about a source, for the sweep's dirtiness
    /// decision. `None` means the source has never been cataloged. The shape
    /// verdict is the sweep's to make: it intersects the stored inventory
    /// with the currently mounted transforms' claims and compares stamps.
    pub async fn index_meta(
        &self,
        address: &Address,
    ) -> Result<Option<SourceIndexMeta>, StoreError> {
        let row = self
            .first_row(
                "SELECT modified, raw_bytes, indexed, shape_stamp, mimetypes, digest
                 FROM sources WHERE host = ?1 AND locator = ?2",
                params![address.host.as_str(), address.locator.as_str()],
            )
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let modified: Option<i64> = row.get(0)?;
        let raw_bytes: i64 = row.get(1)?;
        let indexed: i64 = row.get(2)?;
        let shape_stamp: Option<String> = row.get(3)?;
        let mimetypes: Option<String> = row.get(4)?;
        let digest: Option<String> = row.get(5)?;
        Ok(Some(SourceIndexMeta {
            modified: modified.map(Timestamp),
            raw_bytes: u64::try_from(raw_bytes).unwrap_or(0),
            content_digest: digest.as_deref().and_then(|d| d.parse().ok()),
            indexed: indexed == 1,
            shape_stamp,
            mimetypes: mimetypes
                .as_deref()
                .and_then(|m| serde_json::from_str(m).ok())
                .unwrap_or_default(),
        }))
    }

    /// First row of a catalog query, if any — the optional-single-row shape
    /// the catalog reads use throughout.
    async fn first_row(
        &self,
        sql: &str,
        params: impl libsql::params::IntoParams,
    ) -> Result<Option<libsql::Row>, StoreError> {
        let mut rows = self.catalog.query(sql, params).await?;
        Ok(rows.next().await?)
    }

    /// Write a source's envelope into the catalog, clearing its indexed mark
    /// until `mark_indexed` confirms the fragments are in place.
    pub async fn upsert_source(
        &self,
        address: &Address,
        envelope: &Envelope,
        raw_bytes: u64,
    ) -> Result<SourceId, StoreError> {
        let _write = self.write().await;
        upsert_source_in(&self.catalog, address, envelope, raw_bytes).await
    }

    /// Confirm a source's index run completed. A deep-indexed source records
    /// the shape stamp its subtree was built under plus the subtree's
    /// mimetype inventory; a catalog-only source records neither, which is
    /// exactly what makes it dirty again the moment budget or cutoff would
    /// let it be deep-indexed.
    pub async fn mark_indexed(
        &self,
        source: SourceId,
        shape: Option<(&str, &[InventoryEntry])>,
    ) -> Result<(), StoreError> {
        let _write = self.write().await;
        mark_indexed_in(&self.catalog, source, shape).await
    }

    /// Land a batch of sources that enter the catalog without a subtree —
    /// seen past the cutoff, or past this run's deep-index budget — in one
    /// transaction.
    pub async fn catalog_sources(&self, entries: &[CatalogEntry<'_>]) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        for entry in entries {
            let sid = upsert_source_in(&tx, entry.address, entry.envelope, entry.raw_bytes).await?;
            match entry.mark {
                CatalogMark::Seen => {}
                CatalogMark::CatalogOnly => mark_indexed_in(&tx, sid, None).await?,
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Land one source's planned subtree atomically: upsert the catalog row,
    /// drop whatever subtree it had (and its search rows), insert the root
    /// and every planned fragment and relation, resolve the keyed sprouts
    /// (get-or-create under their keys, then anchor), all in one
    /// transaction. The source is **not** marked indexed here — that waits
    /// for its search rows ([`Self::land_search_rows`]), so a crash between
    /// the two leaves it dirty rather than half-searchable.
    pub async fn write_subtree(&self, plan: &SubtreePlan) -> Result<SubtreeWritten, StoreError> {
        assert!(plan.is_well_ordered(), "a subtree plan names only earlier positions");
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        let source = upsert_source_in(&tx, &plan.address, &plan.envelope, plan.raw_bytes).await?;
        delete_fragments_of_in(&tx, source).await?;
        let root = insert_fragment_in(&tx, Some(source), &plan.root).await?;
        tx.execute(
            "UPDATE sources SET root_fragment = ?1 WHERE id = ?2",
            params![root.0, source.0],
        )
        .await?;
        let mut written = SubtreeWritten {
            source,
            root,
            fragments: Vec::with_capacity(plan.fragments.len()),
            keyed: Vec::with_capacity(plan.keyed.len()),
        };
        for planned in &plan.fragments {
            let parent = written.id_of(planned.parent);
            let id = insert_fragment_in(&tx, Some(source), &planned.fragment).await?;
            insert_relation_in(&tx, &Relation::new(parent, planned.relation.clone(), id)).await?;
            written.fragments.push(id);
        }
        for planned in &plan.keyed {
            let resolved = keyed_fragment_in(&tx, &planned.key, &planned.fragment).await?;
            for anchor in &planned.anchors {
                let from = written.id_of(*anchor);
                insert_relation_in(&tx, &Relation::new(from, planned.relation.clone(), resolved.id()))
                    .await?;
            }
            written.keyed.push(resolved);
        }
        tx.commit().await?;
        assert_eq!(written.fragments.len(), plan.fragments.len());
        assert_eq!(written.keyed.len(), plan.keyed.len());
        Ok(written)
    }

    /// Land embedded search rows and, in the same transaction, mark the
    /// sources whose every row is now searchable as indexed with their shape
    /// records. Rows and marks commit together, so `indexed` is never true
    /// for a source whose rows are missing.
    pub async fn land_search_rows(
        &self,
        rows: &[SearchRow],
        completed: &[SourceCompletion],
    ) -> Result<(), StoreError> {
        if rows.is_empty() && completed.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        if !rows.is_empty() {
            let identity = self.surface()?;
            insert_search_rows_in(&tx, rows, &identity).await?;
        }
        for completion in completed {
            mark_indexed_in(
                &tx,
                completion.source,
                Some((&completion.stamp, &completion.inventory)),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Every cataloged source of a host, as `(id, locator)` — the sweep's
    /// deletion reconciliation diffs this against what enumeration saw.
    /// The direct children of a folder source — cataloged sources exactly one
    /// locator segment below `folder` on the same host — in locator order,
    /// each with its summary when one has landed. This is what a folder's
    /// own content is composed from (`design/indexing.md`, folders), so the
    /// sweep reads it after the level below has landed. The locator range
    /// `<folder>/` ..< `<folder>0` walks the `(host, locator)` index rather
    /// than scanning the catalog, and the segment test keeps grandchildren
    /// out. Bounded: at most `limit` children are returned.
    pub async fn folder_children(
        &self,
        folder: &Address,
        limit: u32,
    ) -> Result<Vec<FolderChild>, StoreError> {
        assert!(limit >= 1);
        let prefix = format!("{}/", folder.locator.as_str());
        // '0' is the character after '/' in ASCII, so every locator under
        // the folder sorts strictly between the two bounds.
        let upper = format!("{}0", folder.locator.as_str());
        let mut rows = self
            .catalog
            .query(
                "SELECT s.id, s.locator, s.content_type,
                        (SELECT f.text FROM relations r
                           JOIN fragments f ON f.id = r.to_fragment
                          WHERE r.from_fragment = s.root_fragment AND r.kind = ?4
                            AND (f.mimetype = ?5 OR f.mimetype LIKE ?5 || ';%')
                          LIMIT 1) AS summary
                 FROM sources s
                 WHERE s.host = ?1 AND s.locator >= ?2 AND s.locator < ?3
                   AND instr(substr(s.locator, length(?2) + 1), '/') = 0
                 ORDER BY s.locator
                 LIMIT ?6",
                params![
                    folder.host.as_str(),
                    prefix.as_str(),
                    upper.as_str(),
                    RelationKind::derives().as_str(),
                    Mimetype::summary().to_string(),
                    i64::from(limit)
                ],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            let locator: String = row.get(1)?;
            let content_type: String = row.get(2)?;
            let summary: Option<String> = row.get(3)?;
            let name = locator
                .strip_prefix(&prefix)
                .ok_or_else(|| corrupt(id, "locator outside the folder's range"))?
                .to_string();
            assert!(!name.is_empty());
            assert!(!name.contains('/'), "a direct child has one more segment");
            out.push(FolderChild {
                address: Address::new(
                    folder.host.clone(),
                    Locator::new(locator).map_err(|e| corrupt(id, e))?,
                ),
                name,
                content_type: Mimetype::parse(&content_type).map_err(|e| corrupt(id, e))?,
                summary,
            });
        }
        assert!(out.len() <= usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(out)
    }

    pub async fn sources_of_host(
        &self,
        host: &HostId,
    ) -> Result<Vec<(SourceId, String)>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT id, locator FROM sources WHERE host = ?1",
                params![host.as_str()],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((SourceId(row.get(0)?), row.get::<String>(1)?));
        }
        Ok(out)
    }
}

/// Write a source's envelope into the catalog, clearing its indexed mark
/// until it is confirmed again.
async fn upsert_source_in(
    conn: &libsql::Connection,
    address: &Address,
    envelope: &Envelope,
    raw_bytes: u64,
) -> Result<SourceId, StoreError> {
    let properties = serde_json::to_string(&envelope.properties)
        .expect("envelope properties serialize to JSON");
    let id = drain_single_i64(
        conn.query(
                "INSERT INTO sources
                   (host, locator, source_type, content_type, len_unit, len,
                    created, modified, observed, hint, properties, digest, raw_bytes, indexed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0)
                 ON CONFLICT (host, locator) DO UPDATE SET
                   source_type = excluded.source_type,
                   content_type = excluded.content_type,
                   len_unit = excluded.len_unit,
                   len = excluded.len,
                   created = excluded.created,
                   modified = excluded.modified,
                   observed = excluded.observed,
                   hint = excluded.hint,
                   properties = excluded.properties,
                   digest = excluded.digest,
                   raw_bytes = excluded.raw_bytes,
                   indexed = 0
                 RETURNING id",
                params![
                    address.host.as_str(),
                    address.locator.as_str(),
                    envelope.source_type.as_str(),
                    envelope.content_type.to_string(),
                    envelope.length.unit(),
                    i64::try_from(envelope.length.value()).unwrap_or(i64::MAX),
                    envelope.created.map(|t| t.0),
                    envelope.modified.map(|t| t.0),
                    envelope.observed.0,
                    envelope.hint.as_deref(),
                    properties,
                    envelope.content_digest.map(|d| d.to_hex()),
                    i64::try_from(raw_bytes).unwrap_or(i64::MAX),
                ],
            )
            .await?,
    )
    .await?
    .ok_or_else(|| StoreError::Corrupt(0, "source upsert returned no id".into()))?;
    Ok(SourceId(id))
}

/// Set a source's `indexed` mark, with its shape records (deep-indexed) or
/// without (catalog-only).
async fn mark_indexed_in(
    conn: &libsql::Connection,
    source: SourceId,
    shape: Option<(&str, &[InventoryEntry])>,
) -> Result<(), StoreError> {
    let (stamp, inventory) = match shape {
        Some((stamp, inventory)) => (
            Some(stamp),
            Some(serde_json::to_string(inventory).expect("inventory entries serialize to JSON")),
        ),
        None => (None, None),
    };
    conn.execute(
        "UPDATE sources SET indexed = 1, shape_stamp = ?2, mimetypes = ?3 WHERE id = ?1",
        params![source.0, stamp, inventory],
    )
    .await?;
    Ok(())
}

/// Drop a source's fragments and their derived search rows (relations
/// cascade). Keyed fragments survive — only their edges into this source go.
async fn delete_fragments_of_in(conn: &libsql::Connection, source: SourceId) -> Result<(), StoreError> {
    if search_tables_exist(conn).await? {
        conn.execute("DELETE FROM search_rows WHERE source = ?1", params![source.0])
            .await?;
    }
    conn.execute("DELETE FROM fragments WHERE source = ?1", params![source.0])
        .await?;
    Ok(())
}

/// Insert one fragment; `source` is `None` only for keyed fragments.
async fn insert_fragment_in(
    conn: &libsql::Connection,
    source: Option<SourceId>,
    fragment: &NewFragment,
) -> Result<FragmentId, StoreError> {
    // Single-row reads are drained, not peeked, so a surrounding
    // transaction can commit afterwards.
    let id = drain_single_i64(
        conn.query(INSERT_FRAGMENT_SQL, fragment_params(source, fragment))
            .await?,
    )
    .await?
    .ok_or_else(|| StoreError::Corrupt(0, "fragment insert returned no id".into()))?;
    Ok(FragmentId(id))
}

async fn insert_relation_in(conn: &libsql::Connection, relation: &Relation) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO relations (from_fragment, kind, to_fragment) VALUES (?1, ?2, ?3)",
        params![relation.from.0, relation.kind.as_str(), relation.to.0],
    )
    .await?;
    Ok(())
}

/// Get-or-create the fragment stored under `key`: the first emitter's
/// `fragment` is what the index keeps, later emitters get the existing id.
async fn keyed_fragment_in(
    conn: &libsql::Connection,
    key: &FragmentKey,
    fragment: &NewFragment,
) -> Result<KeyedFragment, StoreError> {
    let existing = drain_single_i64(
        conn.query(
            "SELECT fragment FROM keyed_fragments WHERE key = ?1",
            params![key.as_str()],
        )
        .await?,
    )
    .await?;
    if let Some(id) = existing {
        return Ok(KeyedFragment::Existing(FragmentId(id)));
    }
    let id = insert_fragment_in(conn, None, fragment).await?;
    conn.execute(
        "INSERT INTO keyed_fragments (key, fragment) VALUES (?1, ?2)",
        params![key.as_str(), id.0],
    )
    .await?;
    Ok(KeyedFragment::Created(id))
}

/// Insert search rows under a surface of `dims` dimensions.
async fn insert_search_rows_in(
    conn: &libsql::Connection,
    rows: &[SearchRow],
    identity: &EmbeddingIdentity,
) -> Result<(), StoreError> {
    let dims = identity.dimensions;
    for row in rows {
        // An embedder whose output disagrees with its declaration is a
        // plugin fault, reported to the caller — never a crash of the node.
        if let Some(vector) = &row.vector {
            check_dimensions(dims, vector)?;
        }
        let source = match row.source {
            Some(s) => libsql::Value::Integer(s.0),
            None => libsql::Value::Null,
        };
        // At zero dimensions the surface is full-text only and the table
        // has no vector column to name.
        if dims == 0 {
            conn.execute(
                "INSERT INTO search_rows (id, source) VALUES (?1, ?2)",
                libsql::params![row.fragment.0, source],
            )
            .await?;
        } else {
            let vector = match &row.vector {
                Some(v) => libsql::Value::Blob(vector_blob(v)),
                None => libsql::Value::Null,
            };
            conn.execute(
                "INSERT INTO search_rows (id, source, ann_vector)
                 VALUES (?1, ?2, CASE WHEN ?3 IS NULL THEN NULL ELSE vector8(?3) END)",
                libsql::params![row.fragment.0, source, vector],
            )
            .await?;
        }
        conn.execute(
            &format!(
                "INSERT INTO {} (rowid, text) VALUES (?1, ?2)",
                search_fts_table(row.role)
            ),
            libsql::params![row.fragment.0, row.text.as_str()],
        )
        .await?;
    }
    remember_embeddings_in(conn, rows, identity).await
}

/// File every landed vector in the embedding cache under the text's digest
/// and the surface's model and width. A vector the cache already holds is
/// left alone (`OR IGNORE`): the cache is append-only until `vacuum`, and
/// a row landed from a cache hit re-files nothing.
async fn remember_embeddings_in(
    conn: &libsql::Connection,
    rows: &[SearchRow],
    identity: &EmbeddingIdentity,
) -> Result<(), StoreError> {
    if identity.dimensions == 0 {
        return Ok(());
    }
    let dimensions = i64::try_from(identity.dimensions).expect("a vector width fits i64");
    for row in rows {
        let Some(vector) = &row.vector else {
            continue;
        };
        let digest = ContentDigest::of_bytes(row.text.as_bytes()).to_hex();
        conn.execute(
            "INSERT OR IGNORE INTO embedding_cache (digest, model, dimensions, vector)
             VALUES (?1, ?2, ?3, ?4)",
            libsql::params![
                digest,
                identity.model.as_str(),
                dimensions,
                libsql::Value::Blob(vector_blob(vector))
            ],
        )
        .await?;
    }
    Ok(())
}

impl IndexStore {
    /// Remove a source and everything derived from it in one transaction:
    /// its search rows, its catalog row, and — through the foreign-key
    /// cascades — its fragments and their relations. A crash can never leave
    /// search rows pointing at fragments the catalog no longer has.
    pub async fn delete_source(&self, source: SourceId) -> Result<(), StoreError> {
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        if search_tables_exist(&tx).await? {
            tx.execute(
                "DELETE FROM search_rows WHERE source = ?1",
                params![source.0],
            )
            .await?;
        }
        tx.execute("DELETE FROM sources WHERE id = ?1", params![source.0])
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Drop keyed fragments no relation touches anymore — the consequence of
    /// source deletions, rebuilds that no longer anchor them, and unmounting
    /// the plugin that emitted them. Their search rows go in the same
    /// transaction; the registry rows cascade. Returns the dropped ids.
    pub async fn gc_keyed_fragments(&self) -> Result<Vec<FragmentId>, StoreError> {
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        let mut rows = tx
            .query(
                "SELECT id FROM fragments WHERE source IS NULL
                   AND NOT EXISTS (SELECT 1 FROM relations
                                   WHERE from_fragment = fragments.id OR to_fragment = fragments.id)",
                (),
            )
            .await?;
        let mut ids: Vec<FragmentId> = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(FragmentId(row.get(0)?));
        }
        let purge_search = search_tables_exist(&tx).await?;
        for chunk in ids.chunks(ID_LIST_CHUNK) {
            let list = id_list(chunk);
            if purge_search {
                tx.execute(&format!("DELETE FROM search_rows WHERE id IN ({list})"), ())
                    .await?;
            }
            tx.execute(&format!("DELETE FROM fragments WHERE id IN ({list})"), ())
                .await?;
        }
        tx.commit().await?;
        Ok(ids)
    }

    pub async fn set_root_fragment(
        &self,
        source: SourceId,
        fragment: FragmentId,
    ) -> Result<(), StoreError> {
        let _write = self.write().await;
        self.catalog
            .execute(
                "UPDATE sources SET root_fragment = ?1 WHERE id = ?2",
                params![fragment.0, source.0],
            )
            .await?;
        Ok(())
    }

    pub async fn source(&self, id: SourceId) -> Result<Option<StoredSource>, StoreError> {
        let row = self
            .first_row(
                &format!("SELECT {SOURCE_COLUMNS} FROM sources WHERE id = ?1"),
                params![id.0],
            )
            .await?;
        row.map(|r| row_to_source(&r)).transpose()
    }

    pub async fn source_by_address(
        &self,
        address: &Address,
    ) -> Result<Option<StoredSource>, StoreError> {
        let row = self
            .first_row(
                &format!("SELECT {SOURCE_COLUMNS} FROM sources WHERE host = ?1 AND locator = ?2"),
                params![address.host.as_str(), address.locator.as_str()],
            )
            .await?;
        row.map(|r| row_to_source(&r)).transpose()
    }

    // ------------------------------------------------------------------
    // Graph
    // ------------------------------------------------------------------

    /// Drop a source's fragments and their derived search rows in one
    /// transaction (relations cascade) — a crash can never leave search rows
    /// pointing at fragments the catalog no longer has. Keyed fragments
    /// survive — only their edges into this source go.
    pub async fn delete_fragments_of(&self, source: SourceId) -> Result<(), StoreError> {
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        delete_fragments_of_in(&tx, source).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Insert a fragment of a source's subtree. Source-less fragments exist
    /// only through [`keyed_fragment`](Self::keyed_fragment), so "no source"
    /// and "keyed" stay one state.
    pub async fn insert_fragment(
        &self,
        source: SourceId,
        fragment: &NewFragment,
    ) -> Result<FragmentId, StoreError> {
        let _write = self.write().await;
        insert_fragment_in(&self.catalog, Some(source), fragment).await
    }

    pub async fn insert_relation(&self, relation: &Relation) -> Result<(), StoreError> {
        let _write = self.write().await;
        insert_relation_in(&self.catalog, relation).await
    }

    pub async fn fragment(&self, id: FragmentId) -> Result<Option<StoredFragment>, StoreError> {
        let row = self
            .first_row(
                &format!("SELECT {FRAGMENT_COLUMNS} FROM fragments WHERE id = ?1"),
                params![id.0],
            )
            .await?;
        row.map(|r| row_to_fragment(&r)).transpose()
    }

    /// The lowest-id fragment whose content lives at `address` — how a
    /// fetch learns the content type of referenced content that is not a
    /// cataloged source of its own (an image a document links to).
    pub async fn fragment_referencing(
        &self,
        address: &Address,
    ) -> Result<Option<StoredFragment>, StoreError> {
        let row = self
            .first_row(
                &format!(
                    "SELECT {FRAGMENT_COLUMNS} FROM fragments WHERE content_address = ?1 ORDER BY id LIMIT 1"
                ),
                params![address.to_string()],
            )
            .await?;
        row.map(|r| row_to_fragment(&r)).transpose()
    }

    pub async fn fragments_of(&self, source: SourceId) -> Result<Vec<StoredFragment>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                &format!("SELECT {FRAGMENT_COLUMNS} FROM fragments WHERE source = ?1 ORDER BY id"),
                params![source.0],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_fragment(&row)?);
        }
        Ok(out)
    }

    /// The fragments with these ids — one per distinct id that exists, in id
    /// order within each chunk of [`ID_LIST_CHUNK`]. One query per chunk, not
    /// one per id.
    pub async fn fragments(&self, ids: &[FragmentId]) -> Result<Vec<StoredFragment>, StoreError> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(ID_LIST_CHUNK) {
            let mut rows = self
                .catalog
                .query(
                    &format!(
                        "SELECT {FRAGMENT_COLUMNS} FROM fragments WHERE id IN ({}) ORDER BY id",
                        id_list(chunk)
                    ),
                    (),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                out.push(row_to_fragment(&row)?);
            }
        }
        Ok(out)
    }

    /// The whole relation graph. Personal-scale indexes keep this cheap; the
    /// Finder's propagation wants all edges anyway.
    pub async fn all_relations(&self) -> Result<Vec<Relation>, StoreError> {
        let mut rows = self
            .catalog
            .query("SELECT from_fragment, kind, to_fragment FROM relations", ())
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_relation(&row)?);
        }
        Ok(out)
    }

    /// Every relation with either endpoint in `ids`, each once.
    pub async fn relations_touching(&self, ids: &[FragmentId]) -> Result<Vec<Relation>, StoreError> {
        let mut out: Vec<Relation> = Vec::new();
        // A relation touching ids in two different chunks arrives twice.
        let mut seen: HashSet<Relation> = HashSet::new();
        for chunk in ids.chunks(ID_LIST_CHUNK) {
            let list = id_list(chunk);
            let mut rows = self
                .catalog
                .query(
                    &format!(
                        "SELECT from_fragment, kind, to_fragment FROM relations
                         WHERE from_fragment IN ({list}) OR to_fragment IN ({list})"
                    ),
                    (),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let relation = row_to_relation(&row)?;
                if seen.insert(relation.clone()) {
                    out.push(relation);
                }
            }
        }
        Ok(out)
    }

    /// A bounded neighborhood around `ids`, breadth-first. Finder queries use
    /// this instead of materializing the entire graph for every request.
    pub async fn relations_near(
        &self,
        ids: &[FragmentId],
        hops: u32,
        limit: u32,
    ) -> Result<Vec<Relation>, StoreError> {
        let hops = hops.min(RELATION_HOPS_MAX);
        let limit = limit.min(RELATION_LIMIT_MAX);
        if ids.is_empty() || hops == 0 || limit == 0 {
            return Ok(Vec::new());
        }
        let mut frontier: Vec<FragmentId> = ids.to_vec();
        frontier.sort();
        frontier.dedup();
        let mut visited: HashSet<FragmentId> = frontier.iter().copied().collect();
        let mut seen: HashSet<Relation> = HashSet::new();
        let mut out = Vec::new();
        for _ in 0..hops {
            let remaining = usize::try_from(limit).expect("relation limit fits usize") - out.len();
            if remaining == 0 {
                break;
            }
            let level = self.relations_touching_limited(&frontier, remaining).await?;
            frontier = relation_frontier(&level, &mut visited, &mut seen, &mut out);
            if frontier.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    async fn relations_touching_limited(
        &self,
        ids: &[FragmentId],
        limit: usize,
    ) -> Result<Vec<Relation>, StoreError> {
        let mut out = Vec::new();
        for chunk in ids.chunks(ID_LIST_CHUNK) {
            let remaining = limit - out.len();
            if remaining == 0 {
                break;
            }
            let list = id_list(chunk);
            let mut rows = self.catalog.query(
                &format!(
                    "SELECT from_fragment, kind, to_fragment FROM relations
                     WHERE from_fragment IN ({list}) OR to_fragment IN ({list})
                     ORDER BY from_fragment, kind, to_fragment LIMIT ?1"
                ),
                params![bounded_limit(remaining)],
            ).await?;
            while let Some(row) = rows.next().await? {
                out.push(row_to_relation(&row)?);
            }
        }
        Ok(out)
    }

    /// Which source each fragment belongs to (keyed fragments absent).
    pub async fn sources_of_fragments(
        &self,
        ids: &[FragmentId],
    ) -> Result<HashMap<FragmentId, SourceId>, StoreError> {
        let mut map = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(ID_LIST_CHUNK) {
            let mut rows = self
                .catalog
                .query(
                    &format!(
                        "SELECT id, source FROM fragments WHERE id IN ({}) AND source IS NOT NULL",
                        id_list(chunk)
                    ),
                    (),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                map.insert(FragmentId(row.get(0)?), SourceId(row.get(1)?));
            }
        }
        Ok(map)
    }

    /// The mandatory summary for a source, if the index has built one.
    pub async fn summary_of(&self, source: SourceId) -> Result<Option<String>, StoreError> {
        let row = self
            .first_row(
                // The root `derives` its summary: input -> output, like
                // every relation.
                // The summary type exactly, or with parameters after `;` —
                // never a longer type that merely shares the prefix.
                "SELECT f.text FROM fragments f
                 JOIN relations r ON r.to_fragment = f.id AND r.kind = ?2
                 JOIN sources s ON r.from_fragment = s.root_fragment
                 WHERE s.id = ?1 AND (f.mimetype = ?3 OR f.mimetype LIKE ?3 || ';%')
                 LIMIT 1",
                params![
                    source.0,
                    RelationKind::derives().as_str(),
                    Mimetype::summary().to_string()
                ],
            )
            .await?;
        match row {
            None => Ok(None),
            Some(row) => Ok(row.get::<Option<String>>(0)?),
        }
    }

    // ------------------------------------------------------------------
    // Keyed fragments
    // ------------------------------------------------------------------

    /// The fragment stored under `key`, if any plugin has created it.
    pub async fn fragment_by_key(&self, key: &FragmentKey) -> Result<Option<FragmentId>, StoreError> {
        let row = self
            .first_row(
                "SELECT fragment FROM keyed_fragments WHERE key = ?1",
                params![key.as_str()],
            )
            .await?;
        match row {
            None => Ok(None),
            Some(row) => Ok(Some(FragmentId(row.get(0)?))),
        }
    }

    /// Get-or-create the fragment stored under `key`, in one transaction:
    /// the first emitter's `fragment` is what the index keeps, later
    /// emitters get the existing id. This is the only way a source-less
    /// fragment comes to exist.
    pub async fn keyed_fragment(
        &self,
        key: &FragmentKey,
        fragment: &NewFragment,
    ) -> Result<KeyedFragment, StoreError> {
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        let resolved = keyed_fragment_in(&tx, key, fragment).await?;
        tx.commit().await?;
        Ok(resolved)
    }

    pub async fn stats(&self) -> Result<StoreStats, StoreError> {
        let database_bytes = file_size_or_zero(&self.database_path).await;
        let wal_bytes = file_size_or_zero(&self.database_path.with_extension("sqlite3-wal")).await;
        Ok(StoreStats {
            sources: self.count_of("SELECT COUNT(*) FROM sources").await?,
            indexed_sources: self
                .count_of(&format!("SELECT COUNT(*) FROM sources WHERE {DEEP_INDEXED}"))
                .await?,
            fragments: self.count_of("SELECT COUNT(*) FROM fragments").await?,
            relations: self.count_of("SELECT COUNT(*) FROM relations").await?,
            keyed_fragments: self.count_of("SELECT COUNT(*) FROM keyed_fragments").await?,
            store_bytes: database_bytes.saturating_add(wal_bytes),
            content_bytes: self
                .count_of("SELECT COALESCE(SUM(raw_bytes), 0) FROM sources")
                .await?,
        })
    }

    /// Counts over the catalog, for one host or all of them.
    pub async fn catalog_counts(&self, host: Option<&HostId>) -> Result<CatalogCounts, StoreError> {
        let host_filter = host.map_or(String::new(), |h| h.as_str().to_string());
        // An empty host filter selects every host: `?1 = ''` short-circuits
        // the match, and a host id is never empty (`HostId::new` rejects it).
        let row = self
            .first_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM({DEEP_INDEXED}), 0)
                     FROM sources WHERE (?1 = '' OR host = ?1)"
                ),
                params![host_filter],
            )
            .await?
            .ok_or_else(|| StoreError::Corrupt(0, "COUNT(*) returned no row".into()))?;
        let sources: i64 = row.get(0)?;
        let indexed: i64 = row.get(1)?;
        let sources = u64::try_from(sources).expect("row counts are non-negative");
        let indexed = u64::try_from(indexed).expect("row counts are non-negative");
        assert!(indexed <= sources);
        Ok(CatalogCounts {
            sources,
            indexed,
            pending: sources - indexed,
        })
    }

    /// The first `limit` cataloged sources matching the selection, ordered
    /// by host then locator so a listing is stable across runs.
    pub async fn catalog_rows(
        &self,
        host: Option<&HostId>,
        selection: CatalogSelection,
        limit: u32,
    ) -> Result<Vec<CatalogRow>, StoreError> {
        let host_filter = host.map_or(String::new(), |h| h.as_str().to_string());
        let selected = match selection {
            CatalogSelection::All => "1".to_string(),
            CatalogSelection::Indexed => DEEP_INDEXED.to_string(),
            CatalogSelection::Pending => format!("NOT ({DEEP_INDEXED})"),
        };
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "SELECT id, host, locator, content_type, raw_bytes,
                            ({DEEP_INDEXED}), modified
                     FROM sources
                     WHERE (?1 = '' OR host = ?1) AND ({selected})
                     ORDER BY host, locator
                     LIMIT ?2"
                ),
                params![host_filter, i64::from(limit)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_catalog_row(&row)?);
        }
        assert!(out.len() <= usize::try_from(limit).expect("u32 fits in usize"));
        Ok(out)
    }

    async fn count_of(&self, sql: &str) -> Result<u64, StoreError> {
        let row = self
            .first_row(sql, ())
            .await?
            .ok_or_else(|| StoreError::Corrupt(0, "COUNT(*) returned no row".into()))?;
        let count: i64 = row.get(0)?;
        Ok(u64::try_from(count).expect("row counts are non-negative"))
    }

    // ------------------------------------------------------------------
    // Search surfaces (libSQL)
    // ------------------------------------------------------------------

    /// Vector width of the bound surface; 0 when no vectors (or none bound).
    pub fn dimensions(&self) -> usize {
        self.search().surface.as_ref().map_or(0, |s| s.dimensions)
    }

    /// The embedding identity the search surface is bound to, if any.
    pub fn embedding_identity(&self) -> Option<EmbeddingIdentity> {
        self.search().surface.clone()
    }

    pub async fn add_search_rows(&self, rows: &[SearchRow]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let identity = self.surface()?;
        let tx = self.catalog.transaction().await?;
        insert_search_rows_in(&tx, rows, &identity).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Compact the full-text index. FTS5 stays transactionally in sync with
    /// `search_rows` through triggers, so this is maintenance, not a rebuild:
    /// it merges the incremental b-trees appended since the last index run.
    pub async fn rebuild_fts(&self) -> Result<(), StoreError> {
        let _write = self.write().await;
        let surface = self.surface()?;
        self.catalog
            .execute("INSERT INTO search_fts (search_fts) VALUES ('optimize')", ())
            .await?;
        self.catalog
            .execute(
                "INSERT INTO search_fts_lexical (search_fts_lexical) VALUES ('optimize')",
                (),
            )
            .await?;
        repair_search_vector_index(&self.catalog, surface.dimensions, SearchIndexRepair::Ensure)
            .await?;
        Ok(())
    }

    /// Full-text seed search over prose rows (content and summaries):
    /// fragment ids with BM25 scores, best first.
    pub async fn search_fts(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<(FragmentId, f32)>, StoreError> {
        self.search_fts_in("search_fts", query, k).await
    }

    /// Full-text seed search over lexical rows (keywords, terms,
    /// identifiers, entities, entries): the same shape as [`Self::search_fts`],
    /// ranked by that table's own statistics.
    pub async fn search_fts_lexical(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<(FragmentId, f32)>, StoreError> {
        self.search_fts_in("search_fts_lexical", query, k).await
    }

    async fn search_fts_in(
        &self,
        table: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<(FragmentId, f32)>, StoreError> {
        assert!(table == "search_fts" || table == "search_fts_lexical");
        self.refuse_while_reembed_pending()?;
        self.surface()?;
        let matcher = fts_match_expression(query);
        if matcher.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .catalog
            .query(
                &format!(
                    "SELECT rowid, bm25({table}) FROM {table}
                     WHERE {table} MATCH ?1 ORDER BY bm25({table}) LIMIT ?2"
                ),
                libsql::params![matcher, bounded_limit(k)],
            )
            .await?;
        // FTS5's bm25() is lower-is-better and negative; negate so callers
        // get the same higher-is-better score shape BM25 seeds always had.
        collect_scored(rows, |raw| -raw).await
    }

    /// Vector seed search: fragment ids with cosine distances, best first.
    /// DiskANN retrieves a wider compact candidate set and orders it by
    /// cosine distance instead of scanning every vector.
    pub async fn search_vector(
        &self,
        vector: &[f32],
        k: usize,
    ) -> Result<Vec<(FragmentId, f32)>, StoreError> {
        self.refuse_while_reembed_pending()?;
        let surface = self.surface()?;
        if surface.dimensions == 0 {
            return Ok(Vec::new());
        }
        check_dimensions(surface.dimensions, vector)?;
        // A missing index is built here, and a stale one — built under an
        // earlier definition — is rebuilt here, so the first query after
        // either pays for the build and every later one finds what exact
        // search would.
        if !search_vector_index_current(&self.catalog).await? {
            self.repair_search_index(SearchIndexRepair::Ensure).await?;
        }
        if !search_vector_index_exists(&self.catalog).await? {
            return Ok(Vec::new());
        }
        let candidate_k = k.saturating_mul(SEARCH_VECTOR_CANDIDATE_MULTIPLIER);
        let query = libsql::Value::Blob(vector_blob(vector));
        let rows = self
            .catalog
            .query(
                "SELECT search_rows.id,
                        vector_distance_cos(search_rows.ann_vector, vector8(?1)) AS exact_distance
                 FROM vector_top_k('search_rows_vector_idx', vector8(?1), ?2) AS candidates
                 JOIN search_rows ON search_rows.id = candidates.id
                 WHERE search_rows.ann_vector IS NOT NULL
                 ORDER BY exact_distance LIMIT ?3",
                libsql::params![query, bounded_limit(candidate_k), bounded_limit(k)],
            )
            .await?;
        collect_scored(rows, |raw| raw).await
    }

    /// Drop the DiskANN index ahead of a bulk landing, so the rows go in at
    /// table speed and the index is rebuilt once over the whole table when
    /// the sweep ends ([`Self::rebuild_fts`]). Measured on a laptop: a row
    /// inserts ~36x slower through the index than without it, while a bulk
    /// build costs under half an indexed insert per row — so once a run
    /// lands a large share of the table, dropping first is the faster path.
    /// Until the rebuild, a vector search builds the index itself (as on a
    /// fresh node). Returns whether there was an index to drop.
    pub async fn defer_search_vector_index(&self) -> Result<bool, StoreError> {
        self.refuse_while_reembed_pending()?;
        let surface = self.surface()?;
        if surface.dimensions == 0 {
            return Ok(false);
        }
        let _write = self.write().await;
        if !search_vector_index_exists(&self.catalog).await? {
            return Ok(false);
        }
        self.catalog
            .execute(&format!("DROP INDEX IF EXISTS {SEARCH_VECTOR_INDEX}"), ())
            .await?;
        assert!(!search_vector_index_exists(&self.catalog).await?);
        tracing::info!(
            index = SEARCH_VECTOR_INDEX,
            "vector search index deferred until the bulk landing ends"
        );
        Ok(true)
    }

    pub async fn repair_search_index(
        &self,
        repair: SearchIndexRepair,
    ) -> Result<SearchIndexRepairReport, StoreError> {
        self.refuse_while_reembed_pending()?;
        let surface = self.surface()?;
        let _write = self.write().await;
        repair_search_vector_index(&self.catalog, surface.dimensions, repair).await
    }

    pub async fn search_rows_count(&self) -> Result<usize, StoreError> {
        self.surface()?;
        let mut rows = self
            .catalog
            .query("SELECT COUNT(*) FROM search_rows", ())
            .await?;
        let row = rows.next().await?.ok_or_else(|| {
            StoreError::Corrupt(0, "COUNT(*) returned no row".into())
        })?;
        let count: i64 = row.get(0)?;
        Ok(usize::try_from(count).expect("row counts are non-negative"))
    }

    /// Whether the vector index exists under the current definition. An
    /// index built under an earlier one reads as not ready: `inseam repair`
    /// or the next vector search rebuilds it.
    pub async fn search_vector_index_ready(&self) -> Result<bool, StoreError> {
        let surface = self.surface()?;
        if surface.dimensions == 0 {
            return Ok(false);
        }
        search_vector_index_current(&self.catalog).await
    }

    // ------------------------------------------------------------------
    // Digest-keyed artifact caches (design/indexing.md)
    // ------------------------------------------------------------------

    /// The cached vectors for these text digests under the bound surface's
    /// model and width, by digest. Digests the cache lacks are absent from
    /// the map; the caller embeds exactly those. The vector scope is not
    /// part of the key: it decides which rows get a vector, never what the
    /// vector is, so flipping it re-embeds from the cache for free.
    pub async fn cached_embeddings(
        &self,
        digests: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, StoreError> {
        let identity = self.surface()?;
        let mut out = HashMap::with_capacity(digests.len());
        if identity.dimensions == 0 {
            return Ok(out);
        }
        let dimensions = i64::try_from(identity.dimensions).expect("a vector width fits i64");
        for chunk in digests.chunks(ID_LIST_CHUNK) {
            let mut rows = self
                .catalog
                .query(
                    &format!(
                        "SELECT digest, vector FROM embedding_cache \
                         WHERE model = ?1 AND dimensions = ?2 AND digest IN ({})",
                        text_list(chunk)
                    ),
                    params![identity.model.as_str(), dimensions],
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let digest: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                let vector = vector_from_blob(&blob);
                check_dimensions(identity.dimensions, &vector)?;
                out.insert(digest, vector);
            }
        }
        assert!(out.len() <= digests.len());
        Ok(out)
    }

    /// The cached transform outputs for these keys, by key. A key names the
    /// input's content digest and the transform's shape identity
    /// (`design/indexing.md`); what the value is, is the sweep's business.
    pub async fn cached_transform_outputs(
        &self,
        keys: &[String],
    ) -> Result<HashMap<String, String>, StoreError> {
        let mut out = HashMap::with_capacity(keys.len());
        for chunk in keys.chunks(ID_LIST_CHUNK) {
            let mut rows = self
                .catalog
                .query(
                    &format!(
                        "SELECT key, output FROM transform_cache WHERE key IN ({})",
                        text_list(chunk)
                    ),
                    (),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                out.insert(row.get::<String>(0)?, row.get::<String>(1)?);
            }
        }
        assert!(out.len() <= keys.len());
        Ok(out)
    }

    /// File transform outputs under their keys, in one transaction. An
    /// existing entry is kept: the first output under a key stands until
    /// `vacuum`, so two planners racing on identical content agree.
    pub async fn remember_transform_outputs(
        &self,
        entries: &[(String, String)],
    ) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        for (key, output) in entries {
            tx.execute(
                "INSERT OR IGNORE INTO transform_cache (key, output) VALUES (?1, ?2)",
                params![key.as_str(), output.as_str()],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// How many artifacts each cache holds, for `status`.
    pub async fn cache_counts(&self) -> Result<CacheCounts, StoreError> {
        let embeddings = self
            .first_row("SELECT COUNT(*) FROM embedding_cache", ())
            .await?
            .map(|row| row.get::<i64>(0))
            .transpose()?
            .unwrap_or(0);
        let transforms = self
            .first_row("SELECT COUNT(*) FROM transform_cache", ())
            .await?
            .map(|row| row.get::<i64>(0))
            .transpose()?
            .unwrap_or(0);
        Ok(CacheCounts {
            embeddings: u64::try_from(embeddings).unwrap_or(0),
            transforms: u64::try_from(transforms).unwrap_or(0),
        })
    }

    // ------------------------------------------------------------------
    // Embedding migration (design/index-maintenance.md)
    // ------------------------------------------------------------------

    /// Whether the index was embedded under a different identity than the
    /// mounted embedder declares and awaits an index run to re-embed.
    pub fn reembed_pending(&self) -> bool {
        self.search().reembed_from.is_some()
    }

    fn refuse_while_reembed_pending(&self) -> Result<(), StoreError> {
        let search = self.search();
        match search.reembed_from.as_ref() {
            None => Ok(()),
            Some(stored) => {
                // A pending re-embed is only ever set alongside a declared
                // surface (`declare_embedding`), so the empty identity is
                // unreachable; it exists so this path needs no panic.
                let declared = search.surface.clone().unwrap_or(EmbeddingIdentity {
                    model: String::new(),
                    dimensions: 0,
                    vectors: VectorScope::All,
                });
                Err(StoreError::ReembedRequired {
                    stored: stored.clone(),
                    declared,
                })
            }
        }
    }

    /// Every text-bearing fragment, for re-populating the search table from
    /// the catalog: exactly the rows the indexer would have buffered when it
    /// built each subtree.
    pub async fn reembed_targets(&self) -> Result<Vec<ReembedTarget>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT id, source, text, mimetype FROM fragments \
                 WHERE text IS NOT NULL AND TRIM(text) != ''",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let mimetype: String = row.get(3)?;
            let source = row.get::<Option<i64>>(1)?.map(SourceId);
            let role = Mimetype::parse(&mimetype)
                .map(|m| SearchRole::of(&m, source.is_none()))
                .unwrap_or(SearchRole::Content);
            out.push(ReembedTarget {
                fragment: FragmentId(row.get(0)?),
                source,
                text: row.get::<String>(2)?,
                role,
            });
        }
        Ok(out)
    }

    /// Recreate the search table empty under the declared identity's
    /// dimensions. The rows are re-added by the caller; searches keep
    /// refusing until [`Self::finish_reembed`].
    pub async fn begin_reembed(&self) -> Result<(), StoreError> {
        let _write = self.write().await;
        let surface = self.surface()?;
        self.catalog.execute_batch(SEARCH_SCHEMA_DROP_SQL).await?;
        self.catalog
            .execute_batch(&search_schema_sql(surface.dimensions))
            .await?;
        ensure_search_vector_schema(&self.catalog, surface.dimensions).await?;
        Ok(())
    }

    /// Record the declared embedding identity as the one the index is built
    /// with and lift the search refusal. Interrupted migrations never reach
    /// this, so the next declaration detects the mismatch again and redoes
    /// the pass.
    pub async fn finish_reembed(&self) -> Result<(), StoreError> {
        let _write = self.write().await;
        let identity = self.embedding_identity().ok_or(StoreError::NoSearchSurface)?;
        set_embedding_meta(&self.catalog, &identity).await?;
        self.search().reembed_from = None;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugin state namespaces (design/kernel.md: versioned, discard on
    // mismatch, never migrated)
    // ------------------------------------------------------------------

    pub(crate) async fn state_open_namespace(
        &self,
        ns: &str,
        version: &str,
    ) -> Result<(), StoreError> {
        let _write = self.write().await;
        let stored = self
            .first_row(
                "SELECT version FROM plugin_state_meta WHERE namespace = ?1",
                params![ns],
            )
            .await?
            .map(|row| row.get::<String>(0))
            .transpose()?;
        if stored.as_deref() != Some(version) {
            if stored.is_some() {
                tracing::info!(namespace = ns, "plugin state version changed; discarding");
            }
            self.catalog
                .execute("DELETE FROM plugin_state WHERE namespace = ?1", params![ns])
                .await?;
            self.catalog
                .execute(
                    "INSERT OR REPLACE INTO plugin_state_meta (namespace, version) VALUES (?1, ?2)",
                    params![ns, version],
                )
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn state_get(&self, ns: &str, key: &str) -> Result<Option<String>, StoreError> {
        self.first_row(
            "SELECT value FROM plugin_state WHERE namespace = ?1 AND key = ?2",
            params![ns, key],
        )
        .await?
        .map(|row| row.get::<String>(0).map_err(StoreError::from))
        .transpose()
    }

    pub(crate) async fn state_put(&self, ns: &str, key: &str, value: &str) -> Result<(), StoreError> {
        let _write = self.write().await;
        self.catalog
            .execute(
                "INSERT OR REPLACE INTO plugin_state (namespace, key, value) VALUES (?1, ?2, ?3)",
                params![ns, key, value],
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn state_delete(&self, ns: &str, key: &str) -> Result<(), StoreError> {
        let _write = self.write().await;
        self.catalog
            .execute(
                "DELETE FROM plugin_state WHERE namespace = ?1 AND key = ?2",
                params![ns, key],
            )
            .await?;
        Ok(())
    }
}

impl std::fmt::Debug for IndexStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexStore")
            .field("dims", &self.dimensions())
            .finish_non_exhaustive()
    }
}

/// Converge the SQLite schema — never migrate. A stored schema version other
/// than the current one drops every table and recreates them: the catalog is
/// re-derived by the next sweep's enumeration and everything else is derived
/// data by design (`design/kernel.md`).
async fn converge_schema(conn: &libsql::Connection) -> Result<(), StoreError> {
    let stored: Option<String> = match conn
        .query("SELECT value FROM meta WHERE key = 'schema_version'", ())
        .await
    {
        Ok(mut rows) => match rows.next().await {
            Ok(Some(row)) => row.get(0).ok(),
            _ => None,
        },
        // A fresh database has no meta table yet; that is the None case.
        Err(_) => None,
    };
    if stored.as_deref().is_some_and(|v| v != SCHEMA_VERSION) {
        tracing::warn!(
            from = stored.as_deref().unwrap_or("?"),
            to = SCHEMA_VERSION,
            "store schema version changed; dropping derived tables for rebuild"
        );
        conn.execute_batch(SEARCH_SCHEMA_DROP_SQL).await?;
        conn.execute_batch(CATALOG_SCHEMA_DROP_SQL).await?;
    }
    conn.execute_batch(CATALOG_SCHEMA_SQL).await?;
    migrate_embedding_cache_layout(conn).await?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION],
    )
    .await?;
    Ok(())
}

/// Whether a table was created with the given fragment in its statement.
/// The catalog keeps the statement that created each table, so a layout
/// change is visible without a version to bump — the same test the vector
/// index uses for its parameters.
async fn table_sql_contains(
    conn: &libsql::Connection,
    table: &str,
    fragment: &str,
) -> Result<bool, StoreError> {
    let mut rows = conn
        .query(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(false);
    };
    let sql: String = row.get(0)?;
    Ok(sql.contains(fragment))
}

/// Move an embedding cache built as a WITHOUT ROWID table into the rowid
/// layout, in one transaction: the rows are the same, only their pages
/// change. Nothing is re-embedded.
async fn migrate_embedding_cache_layout(conn: &libsql::Connection) -> Result<(), StoreError> {
    if !table_sql_contains(conn, "embedding_cache", "WITHOUT ROWID").await? {
        return Ok(());
    }
    tracing::info!("moving the embedding cache into its rowid layout");
    conn.execute_batch(
        "BEGIN;
         ALTER TABLE embedding_cache RENAME TO embedding_cache_legacy;
         CREATE TABLE embedding_cache (
           id INTEGER PRIMARY KEY,
           digest TEXT NOT NULL,
           model TEXT NOT NULL,
           dimensions INTEGER NOT NULL,
           vector BLOB NOT NULL,
           UNIQUE (digest, model, dimensions)
         );
         INSERT INTO embedding_cache (digest, model, dimensions, vector)
           SELECT digest, model, dimensions, vector FROM embedding_cache_legacy;
         DROP TABLE embedding_cache_legacy;
         COMMIT;",
    )
    .await?;
    assert!(!table_sql_contains(conn, "embedding_cache", "WITHOUT ROWID").await?);
    Ok(())
}

/// The catalog tables: the source of truth the search tables derive from.
const CATALOG_SCHEMA_SQL: &str = "CREATE TABLE IF NOT EXISTS meta (
       key TEXT PRIMARY KEY,
       value TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS sources (
       id INTEGER PRIMARY KEY,
       host TEXT NOT NULL,
       locator TEXT NOT NULL,
       source_type TEXT NOT NULL,
       content_type TEXT NOT NULL,
       len_unit TEXT NOT NULL,
       len INTEGER NOT NULL,
       created INTEGER,
       modified INTEGER,
       observed INTEGER NOT NULL,
       hint TEXT,
       properties TEXT NOT NULL DEFAULT '[]',
       digest TEXT,
       raw_bytes INTEGER NOT NULL DEFAULT 0,
       root_fragment INTEGER,
       indexed INTEGER NOT NULL DEFAULT 0,
       shape_stamp TEXT,
       mimetypes TEXT,
       UNIQUE (host, locator)
     );
     CREATE TABLE IF NOT EXISTS fragments (
       id INTEGER PRIMARY KEY,
       source INTEGER REFERENCES sources(id) ON DELETE CASCADE,
       mimetype TEXT NOT NULL,
       text TEXT,
       extent_unit TEXT,
       extent_start INTEGER,
       extent_end INTEGER,
       content_address TEXT
     );
     CREATE INDEX IF NOT EXISTS fragments_by_source ON fragments(source);
     CREATE INDEX IF NOT EXISTS fragments_by_content_address ON fragments(content_address);
     CREATE TABLE IF NOT EXISTS relations (
       from_fragment INTEGER NOT NULL REFERENCES fragments(id) ON DELETE CASCADE,
       kind TEXT NOT NULL,
       to_fragment INTEGER NOT NULL REFERENCES fragments(id) ON DELETE CASCADE,
       PRIMARY KEY (from_fragment, kind, to_fragment)
     ) WITHOUT ROWID;
     CREATE INDEX IF NOT EXISTS relations_by_to ON relations(to_fragment);
     CREATE TABLE IF NOT EXISTS keyed_fragments (
       key TEXT PRIMARY KEY,
       fragment INTEGER NOT NULL REFERENCES fragments(id) ON DELETE CASCADE
     );
     -- The digest-keyed artifact caches (design/indexing.md): the expensive
     -- derived artifacts, stored once per content digest and identity so a
     -- rebuild re-pays only what actually changed. Catalog tables, not
     -- search tables: a re-embed drops the search surface and must find
     -- the vectors still here.
     -- A rowid table, not WITHOUT ROWID: an index b-tree keeps only a
     -- quarter of a page in-row, so a 1.5 KB vector spilled to an overflow
     -- page apiece and the cache cost 4.5 KB per 384-wide vector. In a table
     -- b-tree two vectors share a page (measured: 111 MB to 52 MB for
     -- 25,000 vectors).
     CREATE TABLE IF NOT EXISTS embedding_cache (
       id INTEGER PRIMARY KEY,
       digest TEXT NOT NULL,
       model TEXT NOT NULL,
       dimensions INTEGER NOT NULL,
       vector BLOB NOT NULL,
       UNIQUE (digest, model, dimensions)
     );
     CREATE TABLE IF NOT EXISTS transform_cache (
       key TEXT PRIMARY KEY,
       output TEXT NOT NULL
     ) WITHOUT ROWID;
     CREATE TABLE IF NOT EXISTS plugin_state_meta (
       namespace TEXT PRIMARY KEY,
       version TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS plugin_state (
       namespace TEXT NOT NULL,
       key TEXT NOT NULL,
       value TEXT NOT NULL,
       PRIMARY KEY (namespace, key)
     ) WITHOUT ROWID;";

/// The inverse of [`CATALOG_SCHEMA_SQL`], children before parents.
const CATALOG_SCHEMA_DROP_SQL: &str = "DROP TABLE IF EXISTS relations;
     DROP TABLE IF EXISTS keyed_fragments;
     DROP TABLE IF EXISTS fragments;
     DROP TABLE IF EXISTS sources;
     DROP TABLE IF EXISTS plugin_state;
     DROP TABLE IF EXISTS plugin_state_meta;
     DROP TABLE IF EXISTS embedding_cache;
     DROP TABLE IF EXISTS transform_cache;
     DROP TABLE IF EXISTS meta;";

/// The embedding identity the index was built with, if one is recorded. An
/// index built before the vector scope existed recorded none; it embedded
/// every fragment, so it reads as `all` and pends no re-embed for merely
/// predating the dial.
async fn read_embedding_meta(
    conn: &libsql::Connection,
) -> Result<Option<EmbeddingIdentity>, StoreError> {
    let mut rows = conn
        .query(
            "SELECT
               (SELECT value FROM meta WHERE key = 'embedding_model'),
               (SELECT value FROM meta WHERE key = 'embedding_dims'),
               (SELECT value FROM meta WHERE key = 'embedding_vectors')",
            (),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let model: Option<String> = row.get(0)?;
    let dims: Option<String> = row.get(1)?;
    let vectors: Option<String> = row.get(2)?;
    match (model, dims) {
        (Some(model), Some(dims)) => Ok(Some(EmbeddingIdentity {
            model,
            dimensions: dims.parse().unwrap_or(0),
            vectors: vectors
                .as_deref()
                .map_or(VectorScope::All, VectorScope::parse),
        })),
        _ => Ok(None),
    }
}

async fn set_embedding_meta(
    conn: &libsql::Connection,
    identity: &EmbeddingIdentity,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_model', ?1)",
        params![identity.model.as_str()],
    )
    .await?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_dims', ?1)",
        params![identity.dimensions.to_string()],
    )
    .await?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_vectors', ?1)",
        params![identity.vectors.as_str()],
    )
    .await?;
    Ok(())
}

/// The derived search tables' schema. A search row is a fragment id, its
/// source, and its vector; the text it was made from is not kept here — the
/// fragment holds it, and the FTS5 table is *contentless*: it keeps the
/// inverted index and nothing else, and `contentless_delete` lets a row be
/// deleted by id without handing the text back. (An external-content table
/// over `search_rows` stored every fragment's text a second time; nothing
/// ever read that copy, since results carry fragment ids and the Finder
/// reads text from the fragment.) A trigger keeps the two in step on
/// delete; inserts land in both in [`insert_search_rows_in`]. The vector
/// column exists only under an embedding identity with dims; at zero dims
/// the surface is FTS-only.
fn search_schema_sql(dims: usize) -> String {
    let vector_column = if dims > 0 {
        format!(",\n           ann_vector F8_BLOB({dims})")
    } else {
        String::new()
    };
    format!(
        "CREATE TABLE IF NOT EXISTS search_rows (
           id INTEGER PRIMARY KEY,
           source INTEGER{vector_column}
         );
         -- Subtree rebuilds and source deletions purge by source; without
         -- this index each purge scans every (vector-wide) row, and a full
         -- index run scans the table once per source.
         CREATE INDEX IF NOT EXISTS search_rows_by_source ON search_rows(source);
         -- Two inverted indexes, one per kind of text. BM25 normalises a
         -- row's score by the table's average row length, so a table that
         -- mixed whole documents with hundreds of thousands of one-line
         -- terms and names ranked documents by the wrong statistics; each
         -- kind now keeps its own (`design/finder.md`).
         CREATE VIRTUAL TABLE IF NOT EXISTS search_fts
           USING fts5(text, {SEARCH_FTS_CONTENTLESS});
         CREATE VIRTUAL TABLE IF NOT EXISTS search_fts_lexical
           USING fts5(text, {SEARCH_FTS_CONTENTLESS});
         CREATE TRIGGER IF NOT EXISTS search_rows_after_delete
           AFTER DELETE ON search_rows BEGIN
             DELETE FROM search_fts WHERE rowid = old.id;
             DELETE FROM search_fts_lexical WHERE rowid = old.id;
           END;"
    )
}

/// The full-text table a row's role sends its text to: prose (content and
/// summaries, hints included) in `search_fts`, names and terms in
/// `search_fts_lexical`.
fn search_fts_table(role: SearchRole) -> &'static str {
    match role {
        SearchRole::Content | SearchRole::Summary => "search_fts",
        SearchRole::Lexical => "search_fts_lexical",
    }
}

/// Bring a surface built with one full-text table into the two-table
/// layout: both inverted indexes are rebuilt from the catalog's text, in
/// bounded batches, each its own transaction, so an interruption leaves
/// the next open to continue. No re-embed, no re-index.
async fn migrate_search_role_layout(conn: &libsql::Connection) -> Result<(), StoreError> {
    assert!(search_tables_exist(conn).await?);
    tracing::info!("splitting the full-text index into prose and lexical tables");
    conn.execute_batch(&format!(
        "DROP TRIGGER IF EXISTS search_rows_after_delete;
         DROP TABLE IF EXISTS search_fts;
         DROP TABLE IF EXISTS search_fts_lexical;
         CREATE VIRTUAL TABLE search_fts USING fts5(text, {SEARCH_FTS_CONTENTLESS});
         CREATE VIRTUAL TABLE search_fts_lexical USING fts5(text, {SEARCH_FTS_CONTENTLESS});
         CREATE TRIGGER search_rows_after_delete
           AFTER DELETE ON search_rows BEGIN
             DELETE FROM search_fts WHERE rowid = old.id;
             DELETE FROM search_fts_lexical WHERE rowid = old.id;
           END;"
    ))
    .await?;
    let mut last_id: i64 = 0;
    for _ in 0..SEARCH_FTS_MIGRATION_BATCHES_MAX {
        let landed = migrate_search_role_batch(conn, last_id).await?;
        match landed {
            Some(id) => last_id = id,
            None => break,
        }
    }
    assert!(search_fts_lexical_exists(conn).await?);
    Ok(())
}

/// One batch of the split: the search rows after `last_id`, each filed by
/// its role. Returns the last id filed, or `None` when nothing was left.
async fn migrate_search_role_batch(
    conn: &libsql::Connection,
    last_id: i64,
) -> Result<Option<i64>, StoreError> {
    let mut rows = conn
        .query(
            "SELECT f.id, f.source, f.mimetype, f.text FROM fragments f
             JOIN search_rows s ON s.id = f.id
             WHERE f.id > ?1 AND f.text IS NOT NULL
             ORDER BY f.id LIMIT ?2",
            params![last_id, SEARCH_FTS_MIGRATION_BATCH_ROWS],
        )
        .await?;
    let mut batch: Vec<(i64, SearchRole, String)> = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: i64 = row.get(0)?;
        let source: Option<i64> = row.get(1)?;
        let mimetype: String = row.get(2)?;
        let text: String = row.get(3)?;
        let role = Mimetype::parse(&mimetype)
            .map(|m| SearchRole::of(&m, source.is_none()))
            .unwrap_or(SearchRole::Content);
        batch.push((id, role, text));
    }
    let Some(&(last, _, _)) = batch.last() else {
        return Ok(None);
    };
    let tx = conn.transaction().await?;
    for (id, role, text) in &batch {
        tx.execute(
            &format!(
                "INSERT INTO {} (rowid, text) VALUES (?1, ?2)",
                search_fts_table(*role)
            ),
            params![*id, text.as_str()],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Some(last))
}

async fn search_fts_lexical_exists(conn: &libsql::Connection) -> Result<bool, StoreError> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'search_fts_lexical'",
            (),
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

/// The FTS5 options that make `search_fts` contentless yet deletable; also
/// the mark by which a current table is told from the external-content
/// layout it replaced.
const SEARCH_FTS_CONTENTLESS: &str = "content='', contentless_delete=1";

/// Bring a search surface built with the external-content layout — its text
/// stored a second time in `search_rows` — into the contentless one, in
/// place: the inverted index is rebuilt from the copy about to be dropped,
/// then the column goes. One transaction, no re-embed, no re-index.
async fn migrate_search_text_layout(conn: &libsql::Connection) -> Result<(), StoreError> {
    if !table_sql_contains(conn, "search_fts", "content='search_rows'").await? {
        return Ok(());
    }
    tracing::info!("moving the full-text index into its contentless layout");
    conn.execute_batch(&format!(
        "BEGIN;
         DROP TRIGGER IF EXISTS search_rows_after_insert;
         DROP TRIGGER IF EXISTS search_rows_after_delete;
         DROP TABLE search_fts;
         CREATE VIRTUAL TABLE search_fts USING fts5(text, {SEARCH_FTS_CONTENTLESS});
         INSERT INTO search_fts (rowid, text) SELECT id, text FROM search_rows;
         ALTER TABLE search_rows DROP COLUMN text;
         CREATE TRIGGER search_rows_after_delete
           AFTER DELETE ON search_rows BEGIN
             DELETE FROM search_fts WHERE rowid = old.id;
           END;
         COMMIT;"
    ))
    .await?;
    assert!(table_sql_contains(conn, "search_fts", SEARCH_FTS_CONTENTLESS).await?);
    assert!(!search_column_exists(conn, "text").await?);
    Ok(())
}

async fn ensure_search_vector_schema(
    conn: &libsql::Connection,
    dims: usize,
) -> Result<(), StoreError> {
    if dims == 0 {
        return Ok(());
    }
    if !search_column_exists(conn, "ann_vector").await? {
        conn.execute(
            &format!("ALTER TABLE search_rows ADD COLUMN ann_vector F8_BLOB({dims})"),
            (),
        )
        .await?;
    }
    Ok(())
}

/// Bring an old float32 search surface forward without re-embedding, then
/// ensure its compact DiskANN index exists. Each SQL statement commits on its
/// own, so interruption leaves a state the next repair can continue.
async fn repair_search_vector_index(
    conn: &libsql::Connection,
    dims: usize,
    repair: SearchIndexRepair,
) -> Result<SearchIndexRepairReport, StoreError> {
    let started = Instant::now();
    ensure_search_vector_schema(conn, dims).await?;
    let search_rows = search_rows_count_in(conn).await?;
    if dims == 0 {
        return Ok(SearchIndexRepairReport {
            search_rows,
            vectors_converted: 0,
            outcome: SearchIndexRepairOutcome::Empty,
        });
    }
    let existed = search_vector_index_exists(conn).await?;
    // An index built under an earlier definition is stale, not ready: it
    // exists, but finds less than the current one would, so Ensure treats
    // it as a rebuild rather than mounting it and underperforming forever.
    let current = existed && search_vector_index_current(conn).await?;
    if current && repair == SearchIndexRepair::Ensure {
        return Ok(SearchIndexRepairReport {
            search_rows,
            vectors_converted: 0,
            outcome: SearchIndexRepairOutcome::AlreadyReady,
        });
    }
    let vectors_converted = if existed {
        0
    } else {
        migrate_legacy_vectors(conn).await?
    };
    if search_rows == 0 {
        return Ok(SearchIndexRepairReport {
            search_rows,
            vectors_converted,
            outcome: SearchIndexRepairOutcome::Empty,
        });
    }
    // A rebuild drops and recreates rather than REINDEXing: REINDEX keeps
    // the parameters the index was created with, and a rebuild is how a
    // node adopts the current definition.
    if existed {
        conn.execute(&format!("DROP INDEX IF EXISTS {SEARCH_VECTOR_INDEX}"), ())
            .await?;
    }
    conn.execute(&search_vector_index_definition(), ()).await?;
    assert!(search_vector_index_current(conn).await?);
    tracing::info!(
        index = SEARCH_VECTOR_INDEX,
        elapsed_ms = started.elapsed().as_millis(),
        "vector search index is ready"
    );
    let outcome = if existed {
        SearchIndexRepairOutcome::Rebuilt
    } else {
        SearchIndexRepairOutcome::Built
    };
    Ok(SearchIndexRepairReport {
        search_rows,
        vectors_converted,
        outcome,
    })
}

async fn migrate_legacy_vectors(conn: &libsql::Connection) -> Result<u64, StoreError> {
    if !search_column_exists(conn, "vector").await? {
        return Ok(0);
    }
    let converted = count_of_in(
        conn,
        "SELECT COUNT(*) FROM search_rows
         WHERE ann_vector IS NULL AND vector IS NOT NULL",
    )
    .await?;
    let batch_count = converted
        .checked_div(SEARCH_VECTOR_MIGRATION_BATCH_ROWS)
        .and_then(|count| count.checked_add(1))
        .expect("migration batch count fits u64");
    assert!(batch_count <= SEARCH_VECTOR_MIGRATION_BATCHES_MAX);
    let mut migrated = 0_u64;
    for _ in 0..batch_count {
        let changed = migrate_legacy_vectors_batch(conn).await?;
        migrated = migrated
            .checked_add(changed)
            .expect("migrated row count fits u64");
        if changed == 0 {
            break;
        }
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").await?;
    }
    assert_eq!(migrated, converted);
    Ok(converted)
}

async fn migrate_legacy_vectors_batch(conn: &libsql::Connection) -> Result<u64, StoreError> {
    let changed = conn
        .execute(
            "UPDATE search_rows
             SET ann_vector = vector8(vector), vector = NULL
             WHERE id IN (
               SELECT id FROM search_rows
               WHERE ann_vector IS NULL AND vector IS NOT NULL
               ORDER BY id LIMIT ?1
             )",
            params![i64::try_from(SEARCH_VECTOR_MIGRATION_BATCH_ROWS)
                .expect("migration batch size fits i64")],
        )
        .await?;
    Ok(changed)
}

async fn search_vector_index_exists(conn: &libsql::Connection) -> Result<bool, StoreError> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![SEARCH_VECTOR_INDEX],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

/// The statement that builds the vector index under the current parameters.
fn search_vector_index_definition() -> String {
    format!(
        "CREATE INDEX {SEARCH_VECTOR_INDEX} ON search_rows(libsql_vector_idx(ann_vector, {SEARCH_VECTOR_INDEX_PARAMETERS}))"
    )
}

/// Whether the existing index was built under the current parameters. The
/// catalog keeps the statement that created an index, so a definition
/// change is visible without a version to bump.
async fn search_vector_index_current(conn: &libsql::Connection) -> Result<bool, StoreError> {
    let mut rows = conn
        .query(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![SEARCH_VECTOR_INDEX],
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(false);
    };
    let sql: String = row.get(0)?;
    Ok(sql.contains(SEARCH_VECTOR_INDEX_PARAMETERS))
}

async fn search_rows_count_in(conn: &libsql::Connection) -> Result<u64, StoreError> {
    count_of_in(conn, "SELECT COUNT(*) FROM search_rows").await
}

async fn count_of_in(conn: &libsql::Connection, sql: &str) -> Result<u64, StoreError> {
    let mut rows = conn.query(sql, ()).await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| StoreError::Corrupt(0, "COUNT(*) returned no row".into()))?;
    let count: i64 = row.get(0)?;
    Ok(u64::try_from(count).expect("search row counts are non-negative"))
}

async fn search_column_exists(
    conn: &libsql::Connection,
    column: &str,
) -> Result<bool, StoreError> {
    let mut rows = conn.query("PRAGMA table_info(search_rows)", ()).await?;
    while let Some(row) = rows.next().await? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn relation_frontier(
    relations: &[Relation],
    visited: &mut HashSet<FragmentId>,
    seen: &mut HashSet<Relation>,
    out: &mut Vec<Relation>,
) -> Vec<FragmentId> {
    let mut next = Vec::new();
    for relation in relations {
        if !seen.insert(relation.clone()) {
            continue;
        }
        if visited.insert(relation.from) {
            next.push(relation.from);
        }
        if visited.insert(relation.to) {
            next.push(relation.to);
        }
        out.push(relation.clone());
    }
    next
}

/// Whether the derived search tables exist yet — they appear when an
/// embedder first declares an identity, so the transactional catalog deletes
/// purge search rows only once there are search tables to purge from.
async fn search_tables_exist(conn: &libsql::Connection) -> Result<bool, StoreError> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'search_rows'",
            (),
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

/// An embedding as SQLite stores it: the raw little-endian `f32` bytes an
/// `F32_BLOB` column holds and the vector functions read.
fn vector_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The inverse of [`vector_blob`]: little-endian `f32`s, a trailing partial
/// word (which `vector_blob` never writes) dropped.
fn vector_from_blob(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|word| f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect()
}

/// A quoted `IN (...)` list of text keys — digests and cache keys, which
/// are hex and pipe-joined identifiers with no quote in them; a quote is
/// still escaped so a stray one can never break the statement.
fn text_list(keys: &[String]) -> String {
    assert!(keys.len() <= ID_LIST_CHUNK, "key lists are issued in chunks");
    keys.iter()
        .map(|key| format!("'{}'", key.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",")
}

/// A vector's width against the declared surface — checked on the way in
/// (`insert_search_rows_in`) and on the way out (`search_vector`), so the
/// two ends of the surface agree with each other.
fn check_dimensions(expected: usize, vector: &[f32]) -> Result<(), StoreError> {
    if vector.len() == expected {
        Ok(())
    } else {
        Err(StoreError::DimensionMismatch {
            expected,
            actual: vector.len(),
        })
    }
}

/// Fragment ids as an `IN (...)` body. Numeric, so there is no injection
/// surface; callers pass at most [`ID_LIST_CHUNK`] at a time.
fn id_list(ids: &[FragmentId]) -> String {
    assert!(ids.len() <= ID_LIST_CHUNK, "id lists are issued in chunks");
    ids.iter()
        .map(|f| f.0.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// A caller's `k` as a LIMIT parameter. Seed searches ask for tens of rows;
/// anything that does not fit an `i64` is a programmer error.
fn bounded_limit(k: usize) -> i64 {
    i64::try_from(k).expect("search limits fit in i64")
}

/// Drain `(id, score)` rows, applying `shape` to the raw SQL double so both
/// search paths hand back the same higher-is-better / lower-is-better shapes
/// they always had.
async fn collect_scored(
    mut rows: libsql::Rows,
    shape: fn(f64) -> f64,
) -> Result<Vec<(FragmentId, f32)>, StoreError> {
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: i64 = row.get(0)?;
        let raw: f64 = row.get(1)?;
        // Scores narrow to f32 at the API boundary, as they always have;
        // callers only consume rank order and coarse magnitudes.
        #[expect(clippy::cast_possible_truncation, reason = "deliberate score narrowing")]
        out.push((FragmentId(id), shape(raw) as f32));
    }
    Ok(out)
}

/// User text into an FTS5 MATCH expression. Keep letters, digits and spaces
/// — FTS5 syntax characters in user text would otherwise error or skew the
/// query — then quote each token (so bare AND/OR/NOT stay terms) and join
/// with OR: seeds want recall, and RRF fusion downweights weak matches.
fn fts_match_expression(q: &str) -> String {
    let cleaned: String = q
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    cleaned
        .split_whitespace()
        .map(|token| format!("\"{token}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

const SOURCE_COLUMNS: &str =
    "id, host, locator, source_type, content_type, len_unit, len, created, modified, observed, \
     hint, properties, root_fragment, digest";

/// A missing file is a zero-byte footprint: the WAL is absent between
/// checkpoints, and the database itself only before the first open.
async fn file_size_or_zero(path: &Path) -> u64 {
    match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

fn row_to_catalog_row(r: &libsql::Row) -> Result<CatalogRow, StoreError> {
    let id: i64 = r.get(0)?;
    let host: String = r.get(1)?;
    let locator: String = r.get(2)?;
    let content_type: String = r.get(3)?;
    let raw_bytes: i64 = r.get(4)?;
    let indexed: i64 = r.get(5)?;
    let modified: Option<i64> = r.get(6)?;
    Ok(CatalogRow {
        address: Address::new(
            HostId::new(host).map_err(|e| corrupt(id, e))?,
            Locator::new(locator).map_err(|e| corrupt(id, e))?,
        ),
        indexed: indexed == 1,
        content_type: Mimetype::parse(&content_type).map_err(|e| corrupt(id, e))?,
        raw_bytes: u64::try_from(raw_bytes).unwrap_or(0),
        modified: modified.map(Timestamp),
    })
}

fn row_to_source(r: &libsql::Row) -> Result<StoredSource, StoreError> {
    let id: i64 = r.get(0)?;
    let host: String = r.get(1)?;
    let locator: String = r.get(2)?;
    let content_type: String = r.get(4)?;
    let len_unit: String = r.get(5)?;
    let len: i64 = r.get(6)?;
    let properties: String = r.get(11)?;
    let digest: Option<String> = r.get(13)?;
    let address = Address::new(
        HostId::new(host).map_err(|e| corrupt(id, e))?,
        Locator::new(locator).map_err(|e| corrupt(id, e))?,
    );
    let len = u64::try_from(len).unwrap_or(0);
    let length = match len_unit.as_str() {
        "lines" => ContentLength::Lines(len),
        _ => ContentLength::Bytes(len),
    };
    Ok(StoredSource {
        id: SourceId(id),
        address,
        envelope: Envelope {
            source_type: r.get(3)?,
            content_type: Mimetype::parse(&content_type).map_err(|e| corrupt(id, e))?,
            length,
            created: r.get::<Option<i64>>(7)?.map(Timestamp),
            modified: r.get::<Option<i64>>(8)?.map(Timestamp),
            observed: Timestamp(r.get(9)?),
            hint: r.get(10)?,
            properties: serde_json::from_str::<Vec<Property>>(&properties)
                .map_err(|e| corrupt(id, e))?,
            content_digest: digest
                .map(|d| d.parse().map_err(|e| corrupt(id, e)))
                .transpose()?,
        },
        root_fragment: r.get::<Option<i64>>(12)?.map(FragmentId),
    })
}

const FRAGMENT_COLUMNS: &str =
    "id, source, mimetype, text, extent_unit, extent_start, extent_end, content_address";

fn row_to_fragment(r: &libsql::Row) -> Result<StoredFragment, StoreError> {
    let id: i64 = r.get(0)?;
    let mimetype: String = r.get(2)?;
    let extent = match (
        r.get::<Option<String>>(4)?,
        r.get::<Option<i64>>(5)?,
        r.get::<Option<i64>>(6)?,
    ) {
        (Some(unit), Some(start), Some(end)) => {
            let start = u64::try_from(start).unwrap_or(0);
            let end = u64::try_from(end).unwrap_or(0);
            Some(match unit.as_str() {
                "lines" => Extent::Lines { start, end },
                "millis" => Extent::Millis { start, end },
                _ => Extent::Bytes { start, end },
            })
        }
        _ => None,
    };
    let content_address = r
        .get::<Option<String>>(7)?
        .map(|a| a.parse::<Address>().map_err(|e| corrupt(id, e)))
        .transpose()?;
    Ok(StoredFragment {
        id: FragmentId(id),
        source: r.get::<Option<i64>>(1)?.map(SourceId),
        mimetype: Mimetype::parse(&mimetype).map_err(|e| corrupt(id, e))?,
        text: r.get(3)?,
        extent,
        content_address,
    })
}

fn row_to_relation(r: &libsql::Row) -> Result<Relation, StoreError> {
    let from: i64 = r.get(0)?;
    let kind: String = r.get(1)?;
    let to: i64 = r.get(2)?;
    Ok(Relation {
        from: FragmentId(from),
        kind: RelationKind::new(kind).map_err(|e| corrupt(from, e))?,
        to: FragmentId(to),
    })
}

fn corrupt(id: i64, err: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(id, format!("{err}"))
}

const INSERT_FRAGMENT_SQL: &str =
    "INSERT INTO fragments (source, mimetype, text, extent_unit, extent_start, extent_end, content_address)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id";

/// Read the first column of an at-most-one-row result and run the statement
/// to completion, so a surrounding transaction can commit afterwards.
async fn drain_single_i64(mut rows: libsql::Rows) -> Result<Option<i64>, StoreError> {
    let mut value: Option<i64> = None;
    while let Some(row) = rows.next().await? {
        assert!(value.is_none(), "single-row query returned more than one row");
        value = Some(row.get(0)?);
    }
    Ok(value)
}

/// The bound parameters for [`INSERT_FRAGMENT_SQL`]; `source` is `None`
/// only for keyed fragments.
fn fragment_params(source: Option<SourceId>, fragment: &NewFragment) -> impl libsql::params::IntoParams {
    let (unit, start, end) = extent_columns(fragment.extent);
    params![
        source.map(|s| s.0),
        fragment.mimetype.to_string(),
        fragment.text.as_deref(),
        unit,
        start,
        end,
        fragment.content_address.as_ref().map(Address::to_string),
    ]
}

fn extent_columns(extent: Option<Extent>) -> (Option<&'static str>, Option<i64>, Option<i64>) {
    let (unit, start, end) = match extent {
        None => return (None, None, None),
        Some(Extent::Lines { start, end }) => ("lines", start, end),
        Some(Extent::Bytes { start, end }) => ("bytes", start, end),
        Some(Extent::Millis { start, end }) => ("millis", start, end),
    };
    (Some(unit), Some(extent_bound(start)), Some(extent_bound(end)))
}

/// An extent bound as the INTEGER column holds it; a bound past `i64::MAX`
/// is clamped, like `raw_bytes`, rather than wrapped.
fn extent_bound(bound: u64) -> i64 {
    i64::try_from(bound).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(modified: i64, bytes: u64) -> Envelope {
        Envelope {
            source_type: "file".into(),
            content_type: Mimetype::markdown(),
            length: ContentLength::Bytes(bytes),
            created: None,
            modified: Some(Timestamp(modified)),
            observed: Timestamp(1_700_000_000),
            properties: Vec::new(),
            hint: Some("note.md".into()),
            content_digest: None,
        }
    }

    fn addr(s: &str) -> Address {
        s.parse().expect("test address parses")
    }

    fn identity(model: &str, dimensions: usize) -> EmbeddingIdentity {
        EmbeddingIdentity {
            model: model.to_string(),
            dimensions,
            vectors: VectorScope::All,
        }
    }

    async fn store(dir: &Path) -> IndexStore {
        let s = IndexStore::open(dir).await.expect("opens");
        s.declare_embedding(identity("test-model", 8))
            .await
            .expect("declares");
        s
    }

    #[tokio::test]
    async fn source_lifecycle_records_index_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");
        let env = envelope(100, 10);

        assert!(
            s.index_meta(&a).await.expect("ok").is_none(),
            "unknown source"
        );
        let sid = s.upsert_source(&a, &env, 10).await.expect("upserts");
        // Not yet marked indexed: an interrupted run must read as dirty.
        let meta = s.index_meta(&a).await.expect("ok").expect("present");
        assert!(!meta.indexed);
        assert_eq!(meta.raw_bytes, 10);

        let inventory = [
            InventoryEntry {
                mimetype: "text/markdown".into(),
                is_root: true,
            },
            InventoryEntry {
                mimetype: "text/x-inseam-summary".into(),
                is_root: false,
            },
        ];
        s.mark_indexed(sid, Some(("stamp-a", &inventory)))
            .await
            .expect("marks");
        let meta = s.index_meta(&a).await.expect("ok").expect("present");
        assert!(meta.indexed);
        assert_eq!(meta.modified, env.modified);
        assert_eq!(meta.shape_stamp.as_deref(), Some("stamp-a"));
        assert_eq!(meta.mimetypes, inventory);

        // Catalog-only rows carry no stamp or inventory.
        s.mark_indexed(sid, None).await.expect("marks catalog-only");
        let meta = s.index_meta(&a).await.expect("ok").expect("present");
        assert_eq!(meta.shape_stamp, None);
        assert!(meta.mimetypes.is_empty());

        let stored = s.source_by_address(&a).await.expect("ok").expect("present");
        assert_eq!(stored.id, sid);
        assert_eq!(stored.envelope.hint.as_deref(), Some("note.md"));
    }

    #[tokio::test]
    async fn landed_vectors_are_filed_by_text_digest_under_the_surface_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let rows = vec![
            SearchRow {
                fragment: FragmentId(1),
                source: None,
                text: "espresso descaling".into(),
                vector: Some(vec![0.5; 8]),
                role: SearchRole::Content,
            },
            SearchRow {
                fragment: FragmentId(2),
                source: None,
                text: "text only".into(),
                vector: None,
                role: SearchRole::Content,
            },
        ];
        s.add_search_rows(&rows).await.expect("adds");

        let digest = ContentDigest::of_bytes(b"espresso descaling").to_hex();
        let missing = ContentDigest::of_bytes(b"text only").to_hex();
        let cached = s
            .cached_embeddings(&[digest.clone(), missing.clone()])
            .await
            .expect("reads");
        assert_eq!(cached.get(&digest), Some(&vec![0.5; 8]));
        assert!(!cached.contains_key(&missing), "rows without a vector file nothing");
        assert_eq!(s.cache_counts().await.expect("counts").embeddings, 1);

        // Another model's vectors are another identity's business.
        s.declare_embedding(identity("other-model", 8)).await.expect("declares");
        let other = s.cached_embeddings(&[digest]).await.expect("reads");
        assert!(other.is_empty());
    }

    #[tokio::test]
    async fn cached_vectors_survive_a_reembed_of_the_search_surface() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let rows = vec![SearchRow {
            fragment: FragmentId(1),
            source: None,
            text: "kitchen".into(),
            vector: Some(vec![0.25; 8]),
            role: SearchRole::Content,
        }];
        s.add_search_rows(&rows).await.expect("adds");
        s.begin_reembed().await.expect("begins");
        assert_eq!(s.search_rows_count().await.expect("counts"), 0);
        let digest = ContentDigest::of_bytes(b"kitchen").to_hex();
        let cached = s.cached_embeddings(std::slice::from_ref(&digest)).await.expect("reads");
        assert_eq!(cached.get(&digest), Some(&vec![0.25; 8]));
    }

    /// A search surface built as external-content FTS kept every text a
    /// second time in `search_rows`. Declaring an embedding over such a node
    /// moves it to the contentless layout in place: the same rows are still
    /// found, and the duplicate column is gone.
    #[tokio::test]
    async fn an_external_content_search_surface_migrates_to_contentless_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        s.catalog
            .execute_batch(
                "DROP TRIGGER search_rows_after_delete;
                 DROP TABLE search_fts;
                 DROP TABLE search_rows;
                 CREATE TABLE search_rows (
                   id INTEGER PRIMARY KEY, source INTEGER, text TEXT NOT NULL,
                   ann_vector F8_BLOB(8));
                 CREATE VIRTUAL TABLE search_fts
                   USING fts5(text, content='search_rows', content_rowid='id');
                 CREATE TRIGGER search_rows_after_insert AFTER INSERT ON search_rows BEGIN
                   INSERT INTO search_fts (rowid, text) VALUES (new.id, new.text); END;
                 CREATE TRIGGER search_rows_after_delete AFTER DELETE ON search_rows BEGIN
                   INSERT INTO search_fts (search_fts, rowid, text)
                     VALUES ('delete', old.id, old.text); END;
                 INSERT INTO search_rows (id, source, text) VALUES (7, NULL, 'legacy moodboard');
                 INSERT INTO fragments (id, source, mimetype, text)
                   VALUES (7, NULL, 'text/x-inseam-summary', 'legacy moodboard');",
            )
            .await
            .expect("builds the legacy layout");
        drop(s);
        let s = store(dir.path()).await;
        assert!(!search_column_exists(&s.catalog, "text").await.expect("reads columns"));
        assert!(search_fts_lexical_exists(&s.catalog).await.expect("reads tables"));
        let hits = s.search_fts("moodboard", 5).await.expect("searches");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(FragmentId(7)));
        s.add_search_rows(&[SearchRow {
            fragment: FragmentId(8),
            source: None,
            text: "new moodboard".into(),
            vector: None,
            role: SearchRole::Content,
        }])
        .await
        .expect("adds after migration");
        assert_eq!(s.search_fts("moodboard", 5).await.expect("searches").len(), 2);
        s.catalog
            .execute("DELETE FROM search_rows WHERE id = 7", ())
            .await
            .expect("deletes");
        let hits = s.search_fts("moodboard", 5).await.expect("searches");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(FragmentId(8)));
        assert_eq!(hits.len(), 1, "the trigger removed the deleted row's terms");
    }

    /// Prose and lexical rows are two inverted indexes with their own
    /// statistics: a term row is found by the lexical search and never by
    /// the prose one, and the other way round.
    #[tokio::test]
    async fn lexical_rows_land_in_their_own_full_text_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        s.add_search_rows(&[
            SearchRow {
                fragment: FragmentId(1),
                source: Some(SourceId(1)),
                text: "the kitchen renovation budget".into(),
                vector: None,
                role: SearchRole::Content,
            },
            SearchRow {
                fragment: FragmentId(2),
                source: None,
                text: "kitchen".into(),
                vector: None,
                role: SearchRole::Lexical,
            },
        ])
        .await
        .expect("adds");
        let prose: Vec<FragmentId> = s.search_fts("kitchen", 5).await.expect("searches")
            .into_iter().map(|(id, _)| id).collect();
        let lexical: Vec<FragmentId> = s.search_fts_lexical("kitchen", 5).await.expect("searches")
            .into_iter().map(|(id, _)| id).collect();
        assert_eq!(prose, vec![FragmentId(1)]);
        assert_eq!(lexical, vec![FragmentId(2)]);
        s.catalog
            .execute("DELETE FROM search_rows WHERE id = 2", ())
            .await
            .expect("deletes");
        assert!(s.search_fts_lexical("kitchen", 5).await.expect("searches").is_empty());
    }

    /// A surface built with one full-text table is split at the next
    /// declaration, both tables rebuilt from the catalog's text by role.
    #[tokio::test]
    async fn a_single_table_surface_is_split_by_role_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        s.catalog
            .execute_batch(
                "DROP TRIGGER search_rows_after_delete;
                 DROP TABLE search_fts_lexical;
                 INSERT INTO fragments (id, source, mimetype, text)
                   VALUES (11, NULL, 'text/x-inseam-summary', 'prose about the moodboard'),
                          (12, NULL, 'text/x-inseam-term', 'moodboard');
                 INSERT INTO search_rows (id, source) VALUES (11, NULL), (12, NULL);
                 INSERT INTO search_fts (rowid, text)
                   VALUES (11, 'prose about the moodboard'), (12, 'moodboard');",
            )
            .await
            .expect("builds the single-table layout");
        drop(s);
        let s = store(dir.path()).await;
        let prose: Vec<FragmentId> = s.search_fts("moodboard", 5).await.expect("searches")
            .into_iter().map(|(id, _)| id).collect();
        let lexical: Vec<FragmentId> = s.search_fts_lexical("moodboard", 5).await.expect("searches")
            .into_iter().map(|(id, _)| id).collect();
        assert_eq!(prose, vec![FragmentId(11)]);
        assert_eq!(lexical, vec![FragmentId(12)]);
    }

    /// An embedding cache built WITHOUT ROWID is moved to the rowid layout
    /// at open, keeping every vector.
    #[tokio::test]
    async fn a_without_rowid_embedding_cache_migrates_at_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        s.catalog
            .execute_batch(
                "DROP TABLE embedding_cache;
                 CREATE TABLE embedding_cache (
                   digest TEXT NOT NULL, model TEXT NOT NULL, dimensions INTEGER NOT NULL,
                   vector BLOB NOT NULL, PRIMARY KEY (digest, model, dimensions)
                 ) WITHOUT ROWID;",
            )
            .await
            .expect("builds the legacy layout");
        s.add_search_rows(&[SearchRow {
            fragment: FragmentId(1),
            source: None,
            text: "kitchen".into(),
            vector: Some(vec![0.25; 8]),
            role: SearchRole::Content,
        }])
        .await
        .expect("adds");
        drop(s);
        let s = store(dir.path()).await;
        assert!(!table_sql_contains(&s.catalog, "embedding_cache", "WITHOUT ROWID")
            .await
            .expect("reads sql"));
        let digest = ContentDigest::of_bytes(b"kitchen").to_hex();
        let cached = s.cached_embeddings(std::slice::from_ref(&digest)).await.expect("reads");
        assert_eq!(cached.get(&digest), Some(&vec![0.25; 8]));
        assert_eq!(s.cache_counts().await.expect("counts").embeddings, 1);
    }

    /// With no embedder (zero dimensions) the surface is full-text only:
    /// rows land without a vector column to name, and full-text search
    /// finds them.
    #[tokio::test]
    async fn an_fts_only_surface_lands_rows_and_searches_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = IndexStore::open(dir.path()).await.expect("opens");
        s.declare_embedding(identity("none", 0)).await.expect("declares");
        s.add_search_rows(&[SearchRow {
            fragment: FragmentId(3),
            source: None,
            text: "renovation budget".into(),
            vector: None,
            role: SearchRole::Content,
        }])
        .await
        .expect("lands without a vector column");
        let hits = s.search_fts("renovation", 5).await.expect("searches");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(FragmentId(3)));
        assert!(!search_column_exists(&s.catalog, "ann_vector").await.expect("reads"));
    }

    #[tokio::test]
    async fn transform_outputs_are_filed_first_writer_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        s.remember_transform_outputs(&[("k1".into(), "first".into()), ("k2".into(), "two".into())])
            .await
            .expect("remembers");
        s.remember_transform_outputs(&[("k1".into(), "second".into())])
            .await
            .expect("remembers");
        let cached = s
            .cached_transform_outputs(&["k1".into(), "k3".into()])
            .await
            .expect("reads");
        assert_eq!(cached.get("k1").map(String::as_str), Some("first"));
        assert!(!cached.contains_key("k3"));
        assert_eq!(s.cache_counts().await.expect("counts").transforms, 2);
    }

    #[test]
    fn vector_blob_roundtrips() {
        let vector = vec![0.0, -1.5, 3.25, f32::MAX];
        assert_eq!(vector_from_blob(&vector_blob(&vector)), vector);
        assert!(vector_from_blob(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn text_list_quotes_and_escapes() {
        assert_eq!(text_list(&["ab".into(), "c'd".into()]), "'ab','c''d'");
    }

    #[tokio::test]
    async fn content_digest_roundtrips_through_the_catalog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");

        // Pair assertion with the write below: absent stays absent.
        s.upsert_source(&a, &envelope(1, 10), 10).await.expect("upserts");
        let stored = s.source_by_address(&a).await.expect("ok").expect("present");
        assert_eq!(stored.envelope.content_digest, None);

        let digest = crate::address::ContentDigest::of_bytes(b"# hi\n");
        let mut env = envelope(2, 10);
        env.content_digest = Some(digest);
        s.upsert_source(&a, &env, 10).await.expect("upserts");
        let stored = s.source_by_address(&a).await.expect("ok").expect("present");
        assert_eq!(stored.envelope.content_digest, Some(digest));
    }

    /// Catalog a source and, when `summary` is given, land a root with a
    /// `derives` summary under it — the shape `folder_children` reads.
    async fn catalog_child(s: &IndexStore, address: &str, summary: Option<&str>) {
        let a = addr(address);
        let sid = s.upsert_source(&a, &envelope(1, 10), 10).await.expect("upserts");
        let Some(summary) = summary else {
            return;
        };
        let root = s
            .insert_fragment(
                sid,
                &NewFragment {
                    mimetype: Mimetype::markdown(),
                    text: None,
                    extent: None,
                    content_address: None,
                },
            )
            .await
            .expect("inserts");
        s.set_root_fragment(sid, root).await.expect("sets root");
        let fragment = s
            .insert_fragment(
                sid,
                &NewFragment {
                    mimetype: Mimetype::summary().with_param("via", "extractive"),
                    text: Some(summary.to_string()),
                    extent: None,
                    content_address: None,
                },
            )
            .await
            .expect("inserts");
        s.insert_relation(&Relation::new(root, RelationKind::derives(), fragment))
            .await
            .expect("relates");
    }

    #[tokio::test]
    async fn folder_children_lists_direct_children_with_their_summaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        catalog_child(&s, "inseam://fs-test/tmp/notes/b.md", Some("about b")).await;
        catalog_child(&s, "inseam://fs-test/tmp/notes/a.md", None).await;
        catalog_child(&s, "inseam://fs-test/tmp/notes/sub", Some("a subfolder")).await;
        // Negative space: a grandchild, a sibling of the folder, a locator
        // that merely shares the prefix characters, and another host.
        catalog_child(&s, "inseam://fs-test/tmp/notes/sub/deep.md", Some("deep")).await;
        catalog_child(&s, "inseam://fs-test/tmp/other.md", Some("other")).await;
        catalog_child(&s, "inseam://fs-test/tmp/notes-archive/x.md", Some("x")).await;
        catalog_child(&s, "inseam://fs-other/tmp/notes/c.md", Some("c")).await;

        let children = s
            .folder_children(&addr("inseam://fs-test/tmp/notes"), 100)
            .await
            .expect("lists");
        let seen: Vec<(&str, Option<&str>)> = children
            .iter()
            .map(|c| (c.name.as_str(), c.summary.as_deref()))
            .collect();
        assert_eq!(
            seen,
            vec![("a.md", None), ("b.md", Some("about b")), ("sub", Some("a subfolder"))]
        );
        assert_eq!(
            children[0].address,
            addr("inseam://fs-test/tmp/notes/a.md"),
            "the child's address is the folder's plus the name"
        );
    }

    #[tokio::test]
    async fn folder_children_is_bounded_by_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        for i in 0..5 {
            catalog_child(&s, &format!("inseam://fs-test/tmp/notes/{i}.md"), None).await;
        }
        let children = s
            .folder_children(&addr("inseam://fs-test/tmp/notes"), 3)
            .await
            .expect("lists");
        assert_eq!(children.len(), 3);
        assert!(s
            .folder_children(&addr("inseam://fs-test/tmp/empty"), 3)
            .await
            .expect("lists")
            .is_empty());
    }

    #[tokio::test]
    async fn index_meta_carries_the_recorded_content_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/notes");
        let digest = crate::address::ContentDigest::of_bytes(b"listing");
        let mut env = envelope(1, 0);
        env.content_digest = Some(digest);
        s.upsert_source(&a, &env, 0).await.expect("upserts");
        let meta = s.index_meta(&a).await.expect("ok").expect("present");
        assert_eq!(meta.content_digest, Some(digest));
        // Pair assertion: a source cataloged without one reads back none.
        let b = addr("inseam://fs-test/tmp/other");
        s.upsert_source(&b, &envelope(1, 0), 0).await.expect("upserts");
        let meta = s.index_meta(&b).await.expect("ok").expect("present");
        assert_eq!(meta.content_digest, None);
    }

    #[tokio::test]
    async fn fragments_relations_and_summary_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");
        let sid = s
            .upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");

        let root = s
            .insert_fragment(
                sid,
                &NewFragment {
                    mimetype: Mimetype::markdown(),
                    text: None,
                    extent: Some(Extent::lines(1, 10)),
                    content_address: None,
                },
            )
            .await
            .expect("inserts");
        s.set_root_fragment(sid, root).await.expect("sets root");
        let section = s
            .insert_fragment(
                sid,
                &NewFragment {
                    mimetype: Mimetype::markdown(),
                    text: Some("# Kitchen\nbudget notes".into()),
                    extent: Some(Extent::lines(1, 2)),
                    content_address: None,
                },
            )
            .await
            .expect("inserts");
        let summary = s
            .insert_fragment(
                sid,
                &NewFragment {
                    mimetype: Mimetype::summary(),
                    text: Some("Notes about the kitchen budget.".into()),
                    extent: None,
                    content_address: None,
                },
            )
            .await
            .expect("inserts");
        s.insert_relation(&Relation::new(root, RelationKind::contains(), section))
            .await
            .expect("relates");
        s.insert_relation(&Relation::new(root, RelationKind::derives(), summary))
            .await
            .expect("relates");

        assert_eq!(
            s.summary_of(sid).await.expect("ok").as_deref(),
            Some("Notes about the kitchen budget.")
        );
        let frags = s.fragments_of(sid).await.expect("ok");
        assert_eq!(frags.len(), 3);
        assert_eq!(frags[1].text.as_deref(), Some("# Kitchen\nbudget notes"));
        assert_eq!(frags[1].extent, Some(Extent::lines(1, 2)));
        // Pair assertion with the content-reference test: a text fragment
        // stores no reference.
        assert!(frags.iter().all(|f| f.content_address.is_none()));

        let rels = s.relations_touching(&[section]).await.expect("ok");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].kind, RelationKind::contains());

        // Deleting the source's fragments cascades relations.
        s.delete_fragments_of(sid).await.expect("deletes");
        assert!(s.fragments_of(sid).await.expect("ok").is_empty());
        assert!(s.all_relations().await.expect("ok").is_empty());
    }

    #[tokio::test]
    async fn a_content_reference_roundtrips_on_its_fragment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");
        let sid = s
            .upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let image = addr("inseam://fs-test/tmp/diagram.png");
        let id = s
            .insert_fragment(
                sid,
                &NewFragment {
                    mimetype: Mimetype::parse("image/png").expect("valid"),
                    text: None,
                    extent: None,
                    content_address: Some(image.clone()),
                },
            )
            .await
            .expect("inserts");
        let stored = s.fragment(id).await.expect("ok").expect("present");
        assert_eq!(stored.content_address, Some(image.clone()));
        assert_eq!(stored.text, None);
        let referencing = s.fragment_referencing(&image).await.expect("ok").expect("present");
        assert_eq!(referencing.id, id);
        let other = addr("inseam://fs-test/tmp/other.png");
        assert!(s.fragment_referencing(&other).await.expect("ok").is_none());
    }

    #[tokio::test]
    async fn search_finds_appended_rows_by_text_and_vector() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;

        let unit = |i: usize| -> Vec<f32> {
            let mut v = vec![0.0; 8];
            v[i] = 1.0;
            v
        };
        s.add_search_rows(&[
            SearchRow {
                fragment: FragmentId(1),
                source: Some(SourceId(1)),
                text: "kitchen renovation budget and demolition plan".into(),
                vector: Some(unit(0)),
                role: SearchRole::Content,
            },
            SearchRow {
                fragment: FragmentId(2),
                source: Some(SourceId(2)),
                text: "quarterly tax filing checklist".into(),
                vector: Some(unit(3)),
                role: SearchRole::Content,
            },
            SearchRow {
                fragment: FragmentId(3),
                source: None,
                text: "unembedded fragment about renovation permits".into(),
                vector: None,
                role: SearchRole::Content,
            },
        ])
        .await
        .expect("adds");
        s.rebuild_fts().await.expect("indexes");

        let hits = s.search_fts("renovation", 10).await.expect("searches");
        let ids: Vec<FragmentId> = hits.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&FragmentId(1)) && ids.contains(&FragmentId(3)), "got {ids:?}");
        assert!(!ids.contains(&FragmentId(2)));

        let near = s.search_vector(&unit(0), 2).await.expect("searches");
        assert_eq!(near[0].0, FragmentId(1));
        assert!(near[0].1 < near[1].1, "cosine distance orders results");

        // Appending after the FTS index exists must still be searchable.
        s.add_search_rows(&[SearchRow {
            fragment: FragmentId(9),
            source: Some(SourceId(3)),
            text: "renovation moodboard links".into(),
            vector: Some(unit(5)),
            role: SearchRole::Content,
        }])
        .await
        .expect("adds");
        s.rebuild_fts().await.expect("reindexes");
        let hits = s.search_fts("moodboard", 10).await.expect("searches");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(FragmentId(9)));

        // Deleting a source's fragments drops its search rows in the same
        // transaction; rows of other sources (and the source-less row 3)
        // stay searchable.
        s.delete_fragments_of(SourceId(1)).await.expect("deletes");
        let hits = s.search_fts("renovation", 10).await.expect("searches");
        let ids: Vec<FragmentId> = hits.iter().map(|(id, _)| *id).collect();
        assert!(!ids.contains(&FragmentId(1)), "got {ids:?}");
        assert!(ids.contains(&FragmentId(3)), "got {ids:?}");
    }

    #[tokio::test]
    async fn vector_index_converges_existing_float32_rows_without_reembedding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        s.catalog
            .execute("ALTER TABLE search_rows ADD COLUMN vector F32_BLOB(8)", ())
            .await
            .expect("adds legacy column");
        let mut vector = vec![0.0_f32; 8];
        vector[2] = 1.0;
        s.catalog
            .execute(
                "INSERT INTO search_rows (id, source, vector)
                 VALUES (41, NULL, ?1)",
                params![libsql::Value::Blob(vector_blob(&vector))],
            )
            .await
            .expect("inserts legacy row");

        drop(s);
        let s = store(dir.path()).await;
        assert!(!s.search_vector_index_ready().await.expect("reads readiness"));
        let legacy = s
            .first_row("SELECT vector FROM search_rows WHERE id = 41", ())
            .await
            .expect("reads")
            .expect("row exists");
        assert!(legacy.get::<Option<Vec<u8>>>(0).expect("reads vector").is_some());
        drop(legacy);
        let report = s
            .repair_search_index(SearchIndexRepair::Ensure)
            .await
            .expect("converges");
        assert_eq!(report.vectors_converted, 1);
        assert_eq!(report.outcome, SearchIndexRepairOutcome::Built);
        let hits = s.search_vector(&vector, 1).await.expect("searches");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(FragmentId(41)));
        let row = s
            .first_row("SELECT vector, ann_vector FROM search_rows WHERE id = 41", ())
            .await
            .expect("reads")
            .expect("row exists");
        assert!(row.get::<Option<Vec<u8>>>(0).expect("reads vector").is_none());
        assert!(row.get::<Option<Vec<u8>>>(1).expect("reads ANN vector").is_some());
        drop(row);
        let rebuilt = s
            .repair_search_index(SearchIndexRepair::Rebuild)
            .await
            .expect("rebuilds");
        assert_eq!(rebuilt.vectors_converted, 0);
        assert_eq!(rebuilt.outcome, SearchIndexRepairOutcome::Rebuilt);
    }

    /// The catalog keeps the statement that built the index, so an index
    /// built under an earlier definition (the eight-neighbor one, say) is
    /// visible as stale and rebuilt the first time a search asks for it,
    /// rather than mounted and underperforming forever.
    #[tokio::test]
    async fn an_index_built_under_an_earlier_definition_is_rebuilt_on_first_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let vector = vec![0.5; 8];
        s.add_search_rows(&[SearchRow {
            fragment: FragmentId(7),
            source: None,
            text: "row".into(),
            vector: Some(vector.clone()),
            role: SearchRole::Content,
        }])
        .await
        .expect("lands");
        // Rows land without an index (the sweep builds it at its end), so
        // the earlier definition can be created directly.
        s.catalog
            .execute(
                "CREATE INDEX search_rows_vector_idx ON search_rows(libsql_vector_idx(
                   ann_vector, 'metric=cosine', 'compress_neighbors=float1bit', 'max_neighbors=8'
                 ))",
                (),
            )
            .await
            .expect("builds the earlier definition");
        assert!(search_vector_index_exists(&s.catalog).await.expect("reads"));
        assert!(!s.search_vector_index_ready().await.expect("reads readiness"), "stale is not ready");

        let hits = s.search_vector(&vector, 1).await.expect("searches");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(FragmentId(7)));
        assert!(s.search_vector_index_ready().await.expect("reads readiness"));
        let again = s
            .repair_search_index(SearchIndexRepair::Ensure)
            .await
            .expect("ensures");
        assert_eq!(again.outcome, SearchIndexRepairOutcome::AlreadyReady);
    }

    #[tokio::test]
    async fn deferring_the_vector_index_drops_it_and_rebuild_restores_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        assert!(!s.defer_search_vector_index().await.expect("nothing to defer"));
        let vector = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        s.add_search_rows(&[SearchRow {
            fragment: FragmentId(7),
            source: Some(SourceId(1)),
            text: "deferred".into(),
            vector: Some(vector.clone()),
            role: SearchRole::Content,
        }])
        .await
        .expect("adds");
        s.repair_search_index(SearchIndexRepair::Ensure).await.expect("builds");
        assert!(s.search_vector_index_ready().await.expect("ready"));
        assert!(s.defer_search_vector_index().await.expect("defers"));
        assert!(!s.search_vector_index_ready().await.expect("dropped"));
        s.add_search_rows(&[SearchRow {
            fragment: FragmentId(8),
            source: Some(SourceId(1)),
            text: "landed without the index".into(),
            vector: Some(vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            role: SearchRole::Content,
        }])
        .await
        .expect("adds under no index");
        s.rebuild_fts().await.expect("rebuilds the index at the end");
        assert!(s.search_vector_index_ready().await.expect("ready again"));
        let hits = s.search_vector(&vector, 1).await.expect("searches");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(FragmentId(7)));
    }

    #[tokio::test]
    async fn embedding_change_pends_a_reembed_instead_of_refusing() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let s = store(dir.path()).await;
            s.add_search_rows(&[SearchRow {
                fragment: FragmentId(1),
                source: Some(SourceId(1)),
                text: "kitchen renovation".into(),
                vector: Some(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                role: SearchRole::Content,
            }])
            .await
            .expect("adds");
            s.rebuild_fts().await.expect("indexes");
        }

        let s = IndexStore::open(dir.path()).await.expect("opens");
        s.declare_embedding(identity("other-model", 16))
            .await
            .expect("declares despite the change");
        assert!(s.reembed_pending());
        // Both search paths refuse with the migration instruction.
        assert!(matches!(
            s.search_fts("kitchen", 5).await,
            Err(StoreError::ReembedRequired { .. })
        ));
        assert!(matches!(
            s.search_vector(&[0.0; 16], 5).await,
            Err(StoreError::ReembedRequired { .. })
        ));

        // The migration lifecycle: recreate, re-add, finish.
        s.begin_reembed().await.expect("recreates the table");
        s.add_search_rows(&[SearchRow {
            fragment: FragmentId(1),
            source: Some(SourceId(1)),
            text: "kitchen renovation".into(),
            vector: Some({
                let mut v = vec![0.0; 16];
                v[0] = 1.0;
                v
            }),
            role: SearchRole::Content,
        }])
        .await
        .expect("adds under new dims");
        s.finish_reembed().await.expect("records the new config");
        s.rebuild_fts().await.expect("indexes");
        assert!(!s.reembed_pending());
        let hits = s.search_fts("kitchen", 5).await.expect("searches again");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(FragmentId(1)));

        // A fresh open + declaration under the new identity is clean.
        drop(s);
        let s = IndexStore::open(dir.path()).await.expect("opens");
        s.declare_embedding(identity("other-model", 16))
            .await
            .expect("declares");
        assert!(!s.reembed_pending());
    }

    #[tokio::test]
    async fn narrowing_the_vector_scope_pends_a_reembed_and_widening_it_back_clears() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let _s = store(dir.path()).await;
        }
        let s = IndexStore::open(dir.path()).await.expect("opens");
        s.declare_embedding(EmbeddingIdentity {
            vectors: VectorScope::Summaries,
            ..identity("test-model", 8)
        })
        .await
        .expect("declares");
        assert!(s.reembed_pending());
        let Err(StoreError::ReembedRequired { stored, declared }) = s.search_fts("x", 1).await
        else {
            panic!("search must refuse while a re-embed is pending");
        };
        assert_eq!(stored.vectors, VectorScope::All);
        assert_eq!(declared.vectors, VectorScope::Summaries);
        assert!(declared.to_string().contains("summaries only"));
        s.begin_reembed().await.expect("recreates");
        s.finish_reembed().await.expect("records the scope");
        assert!(!s.reembed_pending());

        drop(s);
        let s = IndexStore::open(dir.path()).await.expect("opens");
        s.declare_embedding(identity("test-model", 8))
            .await
            .expect("declares");
        assert!(s.reembed_pending(), "widening back is a change too");
    }

    #[tokio::test]
    async fn an_index_recorded_without_a_vector_scope_reads_as_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let _s = store(dir.path()).await;
        }
        let s = IndexStore::open(dir.path()).await.expect("opens");
        s.catalog
            .execute("DELETE FROM meta WHERE key = 'embedding_vectors'", ())
            .await
            .expect("simulates a pre-scope index");
        s.declare_embedding(identity("test-model", 8))
            .await
            .expect("declares");
        assert!(!s.reembed_pending());
    }

    #[tokio::test]
    async fn reembed_targets_flag_summaries() {
        use crate::subtree::{PlanNode, PlannedFragment};
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let mut plan = plan_for("inseam://fs-test/tmp/a.md", &["body"]);
        plan.fragments.push(PlannedFragment {
            parent: PlanNode::Root,
            relation: RelationKind::derives(),
            fragment: NewFragment {
                mimetype: Mimetype::summary().with_param("via", "extractive"),
                text: Some("summary".into()),
                extent: None,
                content_address: None,
            },
        });
        s.write_subtree(&plan).await.expect("lands");
        let mut targets = s.reembed_targets().await.expect("lists");
        targets.sort_by(|a, b| a.text.cmp(&b.text));
        let roles: Vec<(&str, SearchRole)> = targets
            .iter()
            .map(|t| (t.text.as_str(), t.role))
            .collect();
        assert_eq!(
            roles,
            vec![("body", SearchRole::Content), ("summary", SearchRole::Summary)]
        );
    }

    #[tokio::test]
    async fn vector_width_disagreeing_with_the_declaration_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let row = SearchRow {
            fragment: FragmentId(1),
            source: None,
            text: "x".into(),
            vector: Some(vec![1.0; 3]),
            role: SearchRole::Content,
        };
        assert!(matches!(
            s.add_search_rows(&[row]).await,
            Err(StoreError::DimensionMismatch {
                expected: 8,
                actual: 3
            })
        ));
        assert_eq!(s.search_rows_count().await.expect("ok"), 0, "the batch rolled back");
        assert!(matches!(
            s.search_vector(&[0.0; 3], 5).await,
            Err(StoreError::DimensionMismatch {
                expected: 8,
                actual: 3
            })
        ));
    }

    #[tokio::test]
    async fn id_list_reads_span_chunks_without_duplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let sid = s
            .upsert_source(&addr("inseam://fs-test/tmp/note.md"), &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let plain = |t: &str| NewFragment {
            mimetype: Mimetype::text_plain(),
            text: Some(t.to_string()),
            extent: None,
            content_address: None,
        };
        let a = s.insert_fragment(sid, &plain("a")).await.expect("inserts");
        let b = s.insert_fragment(sid, &plain("b")).await.expect("inserts");
        s.insert_relation(&Relation::new(a, RelationKind::contains(), b))
            .await
            .expect("relates");
        // `a` lands in the first chunk and `b` in the second: the one
        // relation touches both and must come back exactly once.
        let mut ids = vec![a];
        ids.extend((0..ID_LIST_CHUNK).map(|n| FragmentId(1_000_000 + i64::try_from(n).expect("fits"))));
        ids.push(b);
        let relations = s.relations_touching(&ids).await.expect("ok");
        assert_eq!(relations, vec![Relation::new(a, RelationKind::contains(), b)]);
        let fragments = s.fragments(&ids).await.expect("ok");
        assert_eq!(fragments.iter().map(|f| f.id).collect::<Vec<_>>(), vec![a, b]);
        let owners = s.sources_of_fragments(&ids).await.expect("ok");
        assert_eq!(owners.len(), 2);
        assert_eq!(owners.get(&b), Some(&sid));
    }

    #[tokio::test]
    async fn relation_neighborhood_is_hop_and_size_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let sid = s
            .upsert_source(&addr("inseam://fs-test/tmp/note.md"), &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let fragment = || NewFragment {
            mimetype: Mimetype::text_plain(),
            text: Some("node".to_string()),
            extent: None,
            content_address: None,
        };
        let mut ids = Vec::new();
        for _ in 0..6_u32 {
            ids.push(s.insert_fragment(sid, &fragment()).await.expect("inserts"));
        }
        for pair in ids[..4].windows(2) {
            s.insert_relation(&Relation::new(pair[0], RelationKind::contains(), pair[1]))
                .await
                .expect("relates");
        }
        s.insert_relation(&Relation::new(ids[4], RelationKind::contains(), ids[5]))
            .await
            .expect("relates");

        let near = s.relations_near(&[ids[0]], 2, 10).await.expect("reads");
        assert_eq!(near.len(), 2);
        assert!(near.iter().all(|relation| relation.from != ids[4]));
        let capped = s.relations_near(&[ids[0]], 4, 1).await.expect("reads");
        assert_eq!(capped.len(), 1);
    }

    #[tokio::test]
    async fn search_refuses_until_an_embedder_declares() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = IndexStore::open(dir.path()).await.expect("opens");
        assert!(matches!(
            s.search_fts("anything", 5).await,
            Err(StoreError::NoSearchSurface)
        ));
        s.declare_embedding(identity("test-model", 8))
            .await
            .expect("declares");
        assert!(s.search_fts("anything", 5).await.is_ok());
        s.withdraw_embedding();
        assert!(matches!(
            s.search_fts("anything", 5).await,
            Err(StoreError::NoSearchSurface)
        ));
    }

    #[tokio::test]
    async fn gc_drops_only_unanchored_keyed_fragments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");
        let sid = s
            .upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let root = s
            .insert_fragment(
                sid,
                &NewFragment {
                    mimetype: Mimetype::markdown(),
                    text: Some("greg's note".into()),
                    extent: None,
                    content_address: None,
                },
            )
            .await
            .expect("inserts");
        async fn keyed(s: &IndexStore, key: &str, name: &str) -> FragmentId {
            s.keyed_fragment(
                &FragmentKey::new(key).expect("valid key"),
                &NewFragment {
                    mimetype: Mimetype::parse("text/x-test-entity;kind=person").expect("valid"),
                    text: Some(name.into()),
                    extent: None,
                    content_address: None,
                },
            )
            .await
            .expect("creates")
            .id()
        }
        let mentioned = keyed(&s, "person:greg", "Greg").await;
        let orphan = keyed(&s, "person:nobody", "Nobody").await;
        let mentions = RelationKind::new("mentions").expect("valid kind");
        s.insert_relation(&Relation::new(root, mentions, mentioned))
            .await
            .expect("relates");
        // Keyed fragments carry search rows too; GC must purge the orphan's
        // row in the same transaction it drops the fragment.
        s.add_search_rows(&[
            SearchRow {
                fragment: mentioned,
                source: None,
                text: "Greg".into(),
                vector: None,
                role: SearchRole::Content,
            },
            SearchRow {
                fragment: orphan,
                source: None,
                text: "Nobody".into(),
                vector: None,
                role: SearchRole::Content,
            },
        ])
        .await
        .expect("adds");
        s.rebuild_fts().await.expect("indexes");

        let dropped = s.gc_keyed_fragments().await.expect("gcs");
        assert_eq!(dropped, vec![orphan]);
        assert!(s.fragment(mentioned).await.expect("ok").is_some());
        assert!(s.fragment(orphan).await.expect("ok").is_none());
        let hits = s.search_fts("Nobody", 5).await.expect("searches");
        assert!(hits.is_empty(), "orphan search row purged, got {hits:?}");
        let hits = s.search_fts("Greg", 5).await.expect("searches");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(mentioned));
        // The registry row cascaded with the fragment.
        let nobody = FragmentKey::new("person:nobody").expect("valid key");
        let greg = FragmentKey::new("person:greg").expect("valid key");
        assert_eq!(s.fragment_by_key(&nobody).await.expect("ok"), None);
        assert_eq!(s.fragment_by_key(&greg).await.expect("ok"), Some(mentioned));

        // Deleting the source orphans the survivor; the next GC takes it.
        s.delete_fragments_of(sid).await.expect("deletes");
        let dropped = s.gc_keyed_fragments().await.expect("gcs");
        assert_eq!(dropped, vec![mentioned]);
    }

    #[tokio::test]
    async fn keyed_fragments_deduplicate_under_their_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let key = FragmentKey::new("entity:person:greg hunt").expect("valid key");
        let fragment = NewFragment {
            mimetype: Mimetype::parse("text/x-test-entity;kind=person").expect("valid"),
            text: Some("Greg Hunt".into()),
            extent: None,
            content_address: None,
        };
        let first = s.keyed_fragment(&key, &fragment).await.expect("creates");
        let KeyedFragment::Created(e) = first else {
            panic!("first sighting creates, got {first:?}");
        };
        let again = s.keyed_fragment(&key, &fragment).await.expect("finds");
        assert_eq!(again, KeyedFragment::Existing(e));
        assert_eq!(s.fragment_by_key(&key).await.expect("ok"), Some(e));
        let other = FragmentKey::new("entity:person:unknown").expect("valid key");
        assert_eq!(s.fragment_by_key(&other).await.expect("ok"), None);
        let f = s.fragment(e).await.expect("ok").expect("present");
        assert!(f.source.is_none(), "keyed fragments belong to no source");
        assert_eq!(f.mimetype.param("kind"), Some("person"));
        assert_eq!(s.stats().await.expect("ok").keyed_fragments, 1);
    }

    fn plan_for(address: &str, texts: &[&str]) -> crate::subtree::SubtreePlan {
        use crate::subtree::{PlanNode, PlannedFragment, Shape, SubtreePlan};
        SubtreePlan {
            address: addr(address),
            envelope: envelope(100, 10),
            raw_bytes: 10,
            root: NewFragment {
                mimetype: Mimetype::markdown(),
                text: None,
                extent: Some(Extent::lines(1, 3)),
                content_address: None,
            },
            fragments: texts
                .iter()
                .enumerate()
                .map(|(i, t)| PlannedFragment {
                    // A chain: each fragment hangs off the previous one.
                    parent: if i == 0 {
                        PlanNode::Root
                    } else {
                        PlanNode::Fragment(u32::try_from(i - 1).expect("small"))
                    },
                    relation: RelationKind::contains(),
                    fragment: NewFragment {
                        mimetype: Mimetype::text_plain(),
                        text: Some((*t).to_string()),
                        extent: None,
                        content_address: None,
                    },
                })
                .collect(),
            keyed: Vec::new(),
            shape: Shape {
                stamp: "stamp-plan".into(),
                inventory: vec![InventoryEntry {
                    mimetype: "text/markdown".into(),
                    is_root: true,
                }],
            },
        }
    }

    #[tokio::test]
    async fn write_subtree_lands_the_plan_atomically_and_indexed_waits_for_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let plan = plan_for("inseam://fs-test/tmp/plan.md", &["alpha", "beta"]);
        let written = s.write_subtree(&plan).await.expect("writes");
        assert_eq!(written.fragments.len(), 2);
        let stored = s
            .source(written.source).await
            .expect("ok")
            .expect("present");
        assert_eq!(stored.root_fragment, Some(written.root));
        let meta = s.index_meta(&plan.address).await.expect("ok").expect("present");
        assert!(!meta.indexed, "indexed waits for the search rows to land");
        let relations = s.relations_touching(&[written.fragments[1]]).await.expect("ok");
        assert!(
            relations.iter().any(|r| r.from == written.fragments[0] && r.to == written.fragments[1]),
            "plan positions resolve to the inserted ids: {relations:?}"
        );

        let rows: Vec<SearchRow> = written
            .fragments
            .iter()
            .zip(["alpha", "beta"])
            .map(|(id, text)| SearchRow {
                fragment: *id,
                source: Some(written.source),
                text: text.into(),
                vector: Some(vec![0.5; 8]),
                role: SearchRole::Content,
            })
            .collect();
        s.land_search_rows(
            &rows,
            &[SourceCompletion {
                source: written.source,
                stamp: plan.shape.stamp.clone(),
                inventory: plan.shape.inventory.clone(),
            }],
        )
        .await
        .expect("lands");
        let meta = s.index_meta(&plan.address).await.expect("ok").expect("present");
        assert!(meta.indexed);
        assert_eq!(meta.shape_stamp.as_deref(), Some("stamp-plan"));
        assert_eq!(s.search_rows_count().await.expect("ok"), 2);

        // Rewriting the plan replaces the subtree: the old rows and fragments go.
        let again = s.write_subtree(&plan_for("inseam://fs-test/tmp/plan.md", &["gamma"])).await.expect("rewrites");
        assert_eq!(again.source, written.source);
        assert_eq!(s.fragments_of(written.source).await.expect("ok").len(), 2, "root + gamma");
        assert_eq!(s.search_rows_count().await.expect("ok"), 0);
    }

    #[tokio::test]
    async fn write_subtree_resolves_keyed_sprouts_and_anchors() {
        use crate::subtree::{PlanNode, PlannedKeyed};
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let mut plan = plan_for("inseam://fs-test/tmp/k.md", &["Greg was here", "nothing"]);
        plan.keyed.push(PlannedKeyed {
            key: FragmentKey::new("entity:person:greg").expect("valid"),
            fragment: NewFragment {
                mimetype: Mimetype::parse("text/x-test-entity;kind=person").expect("valid"),
                text: Some("Greg".into()),
                extent: None,
                content_address: None,
            },
            relation: RelationKind::new("mentions").expect("valid"),
            anchors: vec![PlanNode::Fragment(0)],
        });
        let first = s.write_subtree(&plan).await.expect("writes");
        let KeyedFragment::Created(entity) = first.keyed[0] else {
            panic!("first sighting creates: {:?}", first.keyed);
        };
        let relations = s.relations_touching(&[entity]).await.expect("ok");
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].from, first.fragments[0]);

        // A second source naming the same key reuses the fragment.
        let mut other = plan_for("inseam://fs-test/tmp/other.md", &["Greg again"]);
        other.keyed = plan.keyed.clone();
        let second = s.write_subtree(&other).await.expect("writes");
        assert_eq!(second.keyed[0], KeyedFragment::Existing(entity));
        assert_eq!(s.stats().await.expect("ok").keyed_fragments, 1);
    }

    #[tokio::test]
    async fn catalog_sources_marks_by_kind_in_one_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let seen = addr("inseam://fs-test/tmp/seen.md");
        let only = addr("inseam://fs-test/tmp/only.md");
        let env = envelope(100, 10);
        s.catalog_sources(&[
            CatalogEntry {
                address: &seen,
                envelope: &env,
                raw_bytes: 10,
                mark: CatalogMark::Seen,
            },
            CatalogEntry {
                address: &only,
                envelope: &env,
                raw_bytes: 10,
                mark: CatalogMark::CatalogOnly,
            },
        ])
        .await
        .expect("catalogs");
        let seen_meta = s.index_meta(&seen).await.expect("ok").expect("present");
        assert!(!seen_meta.indexed);
        let only_meta = s.index_meta(&only).await.expect("ok").expect("present");
        assert!(only_meta.indexed);
        assert_eq!(only_meta.shape_stamp, None, "catalog-only rows carry no shape");
        assert!(s.catalog_sources(&[]).await.is_ok(), "an empty batch is a no-op");
    }
}
