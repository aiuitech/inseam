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

use libsql::params;
use thiserror::Error;

use crate::address::{Address, ContentLength, Envelope, HostId, Locator, Property, Timestamp};
use crate::fragment::{
    Extent, FragmentId, FragmentKey, Mimetype, NewFragment, Relation, RelationKind,
};
use crate::subtree::{PlanNode, SubtreePlan};

const SCHEMA_VERSION: &str = "6";
/// Ids per `IN (...)` predicate: every id-list query and delete is issued in
/// chunks of this many, so no caller can build unbounded SQL.
const ID_LIST_CHUNK: usize = 400;

/// Drops the derived search tables (and their sync triggers) — the inverse
/// of [`search_schema_sql`], used by re-embeds and the schema converge.
const SEARCH_SCHEMA_DROP_SQL: &str = "DROP TRIGGER IF EXISTS search_rows_after_insert;
     DROP TRIGGER IF EXISTS search_rows_after_delete;
     DROP TABLE IF EXISTS search_fts;
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
        "index was embedded with `{stored_model}` ({stored_dims} dims), the mounted embedder \
         declares `{declared_model}` ({declared_dims} dims); run `inseam index <dir>` to re-embed \
         the search index"
    )]
    ReembedRequired {
        stored_model: String,
        stored_dims: usize,
        declared_model: String,
        declared_dims: usize,
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
    pub indexed: bool,
    pub shape_stamp: Option<String>,
    pub mimetypes: Vec<InventoryEntry>,
}

/// One inventory record: a mimetype present in a source's subtree and
/// whether it appeared at the root.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InventoryEntry {
    pub mimetype: String,
    pub is_root: bool,
}

/// A row bound for the search tables: a text-bearing fragment and its
/// (optional) embedding.
#[derive(Debug, Clone)]
pub struct SearchRow {
    pub fragment: FragmentId,
    pub source: Option<SourceId>,
    pub text: String,
    pub vector: Option<Vec<f32>>,
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

/// The live search surface: the embedding identity the search tables are
/// bound to once an embedder declares it. The tables themselves live in the
/// one store database alongside the catalog.
#[derive(Clone)]
struct SearchSurface {
    dims: usize,
    model: String,
}

/// What the store knows about its search surface, under one lock so the two
/// facts are never observed out of step.
#[derive(Default)]
struct SearchState {
    /// `None` until an embedder plugin declares the embedding identity; the
    /// search surface belongs to that identity, not to the store's opening.
    surface: Option<SearchSurface>,
    /// `Some((model, dims))` the index was embedded with when that differs
    /// from the declared identity: search refuses until an index run
    /// re-embeds (`design/index-maintenance.md`).
    reembed_from: Option<(String, usize)>,
}

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
    pub async fn declare_embedding(&self, model: &str, dims: usize) -> Result<(), StoreError> {
        let _write = self.write().await;
        let stored = read_embedding_meta(&self.catalog).await?;
        let pending = match stored {
            None => {
                set_embedding_meta(&self.catalog, dims, model).await?;
                None
            }
            Some((stored_model, stored_dims))
                if stored_model == model && stored_dims == dims =>
            {
                None
            }
            Some(mismatch) => Some(mismatch),
        };

        // `IF NOT EXISTS` deliberately leaves tables built under a different
        // identity in place: searches refuse while the re-embed is pending,
        // and `begin_reembed` recreates them under the new dimensions.
        self.catalog.execute_batch(&search_schema_sql(dims)).await?;
        let mut search = self.search();
        search.surface = Some(SearchSurface {
            dims,
            model: model.to_string(),
        });
        search.reembed_from = pending;
        Ok(())
    }

    /// Withdraw the search surface (the embedder unmounted). Catalog and
    /// graph stay serviceable; searches refuse until a new declaration.
    pub fn withdraw_embedding(&self) {
        self.search().surface = None;
    }

    fn surface(&self) -> Result<SearchSurface, StoreError> {
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
                "SELECT modified, raw_bytes, indexed, shape_stamp, mimetypes
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
        Ok(Some(SourceIndexMeta {
            modified: modified.map(Timestamp),
            raw_bytes: u64::try_from(raw_bytes).unwrap_or(0),
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
            let dims = self.surface()?.dims;
            insert_search_rows_in(&tx, rows, dims).await?;
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
    dims: usize,
) -> Result<(), StoreError> {
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
        let vector = match &row.vector {
            Some(v) if dims > 0 => libsql::Value::Blob(vector_blob(v)),
            _ => libsql::Value::Null,
        };
        conn.execute(
            "INSERT INTO search_rows (id, source, text, vector) VALUES (?1, ?2, ?3, ?4)",
            libsql::params![row.fragment.0, source, row.text.as_str(), vector],
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
        self.search().surface.as_ref().map_or(0, |s| s.dims)
    }

    /// The embedding identity the search surface is bound to, if any.
    pub fn embedding_identity(&self) -> Option<(String, usize)> {
        self.search()
            .surface
            .as_ref()
            .map(|s| (s.model.clone(), s.dims))
    }

    pub async fn add_search_rows(&self, rows: &[SearchRow]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let _write = self.write().await;
        let dims = self.surface()?.dims;
        let tx = self.catalog.transaction().await?;
        insert_search_rows_in(&tx, rows, dims).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Compact the full-text index. FTS5 stays transactionally in sync with
    /// `search_rows` through triggers, so this is maintenance, not a rebuild:
    /// it merges the incremental b-trees appended since the last index run.
    pub async fn rebuild_fts(&self) -> Result<(), StoreError> {
        let _write = self.write().await;
        self.surface()?;
        self.catalog
            .execute("INSERT INTO search_fts (search_fts) VALUES ('optimize')", ())
            .await?;
        Ok(())
    }

    /// Full-text seed search: fragment ids with BM25 scores, best first.
    pub async fn search_fts(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<(FragmentId, f32)>, StoreError> {
        self.refuse_while_reembed_pending()?;
        self.surface()?;
        let matcher = fts_match_expression(query);
        if matcher.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .catalog
            .query(
                "SELECT rowid, bm25(search_fts) FROM search_fts
                 WHERE search_fts MATCH ?1 ORDER BY bm25(search_fts) LIMIT ?2",
                libsql::params![matcher, bounded_limit(k)],
            )
            .await?;
        // FTS5's bm25() is lower-is-better and negative; negate so callers
        // get the same higher-is-better score shape BM25 seeds always had.
        collect_scored(rows, |raw| -raw).await
    }

    /// Vector seed search: fragment ids with cosine distances, best first.
    /// An exact scan by design — see `design/runtime.md` for when DiskANN
    /// (`libsql_vector_idx`) earns its place.
    pub async fn search_vector(
        &self,
        vector: &[f32],
        k: usize,
    ) -> Result<Vec<(FragmentId, f32)>, StoreError> {
        self.refuse_while_reembed_pending()?;
        let surface = self.surface()?;
        if surface.dims == 0 {
            return Ok(Vec::new());
        }
        check_dimensions(surface.dims, vector)?;
        let rows = self
            .catalog
            .query(
                "SELECT id, vector_distance_cos(vector, ?1) AS distance FROM search_rows
                 WHERE vector IS NOT NULL ORDER BY distance LIMIT ?2",
                libsql::params![libsql::Value::Blob(vector_blob(vector)), bounded_limit(k)],
            )
            .await?;
        collect_scored(rows, |raw| raw).await
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
            Some((stored_model, stored_dims)) => {
                let (declared_model, declared_dims) = search
                    .surface
                    .as_ref()
                    .map(|s| (s.model.clone(), s.dims))
                    .unwrap_or_default();
                Err(StoreError::ReembedRequired {
                    stored_model: stored_model.clone(),
                    stored_dims: *stored_dims,
                    declared_model,
                    declared_dims,
                })
            }
        }
    }

    /// Every text-bearing fragment, for re-populating the search table from
    /// the catalog: `(fragment, source, text)`. Exactly the rows the indexer
    /// would have buffered when it built each subtree.
    pub async fn reembed_targets(
        &self,
    ) -> Result<Vec<(FragmentId, Option<SourceId>, String)>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT id, source, text FROM fragments WHERE text IS NOT NULL AND TRIM(text) != ''",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((
                FragmentId(row.get(0)?),
                row.get::<Option<i64>>(1)?.map(SourceId),
                row.get::<String>(2)?,
            ));
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
            .execute_batch(&search_schema_sql(surface.dims))
            .await?;
        Ok(())
    }

    /// Record the declared embedding identity as the one the index is built
    /// with and lift the search refusal. Interrupted migrations never reach
    /// this, so the next declaration detects the mismatch again and redoes
    /// the pass.
    pub async fn finish_reembed(&self) -> Result<(), StoreError> {
        let _write = self.write().await;
        let (model, dims) = self.embedding_identity().ok_or(StoreError::NoSearchSurface)?;
        set_embedding_meta(&self.catalog, dims, &model).await?;
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
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION],
    )
    .await?;
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
       extent_end INTEGER
     );
     CREATE INDEX IF NOT EXISTS fragments_by_source ON fragments(source);
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
     DROP TABLE IF EXISTS meta;";

/// The embedding identity the index was built with, if one is recorded.
async fn read_embedding_meta(
    conn: &libsql::Connection,
) -> Result<Option<(String, usize)>, StoreError> {
    let mut rows = conn
        .query(
            "SELECT
               (SELECT value FROM meta WHERE key = 'embedding_model'),
               (SELECT value FROM meta WHERE key = 'embedding_dims')",
            (),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let model: Option<String> = row.get(0)?;
    let dims: Option<String> = row.get(1)?;
    match (model, dims) {
        (Some(model), Some(dims)) => Ok(Some((model, dims.parse().unwrap_or(0)))),
        _ => Ok(None),
    }
}

async fn set_embedding_meta(
    conn: &libsql::Connection,
    dims: usize,
    model: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_model', ?1)",
        params![model],
    )
    .await?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_dims', ?1)",
        params![dims.to_string()],
    )
    .await?;
    Ok(())
}

/// The derived search tables' schema. The FTS5 table is external-content over
/// `search_rows`, kept in sync by triggers so inserts and deletes never
/// leave the two out of step — the pair to `rebuild_fts` only compacting.
/// The vector column exists only under an embedding identity with dims;
/// at zero dims the surface is FTS-only.
fn search_schema_sql(dims: usize) -> String {
    let vector_column = if dims > 0 {
        format!(",\n           vector F32_BLOB({dims})")
    } else {
        String::new()
    };
    format!(
        "CREATE TABLE IF NOT EXISTS search_rows (
           id INTEGER PRIMARY KEY,
           source INTEGER,
           text TEXT NOT NULL{vector_column}
         );
         -- Subtree rebuilds and source deletions purge by source; without
         -- this index each purge scans every (vector-wide) row, and a full
         -- index run scans the table once per source.
         CREATE INDEX IF NOT EXISTS search_rows_by_source ON search_rows(source);
         CREATE VIRTUAL TABLE IF NOT EXISTS search_fts
           USING fts5(text, content='search_rows', content_rowid='id');
         CREATE TRIGGER IF NOT EXISTS search_rows_after_insert
           AFTER INSERT ON search_rows BEGIN
             INSERT INTO search_fts (rowid, text) VALUES (new.id, new.text);
           END;
         CREATE TRIGGER IF NOT EXISTS search_rows_after_delete
           AFTER DELETE ON search_rows BEGIN
             INSERT INTO search_fts (search_fts, rowid, text)
               VALUES ('delete', old.id, old.text);
           END;"
    )
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

const FRAGMENT_COLUMNS: &str = "id, source, mimetype, text, extent_unit, extent_start, extent_end";

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
    Ok(StoredFragment {
        id: FragmentId(id),
        source: r.get::<Option<i64>>(1)?.map(SourceId),
        mimetype: Mimetype::parse(&mimetype).map_err(|e| corrupt(id, e))?,
        text: r.get(3)?,
        extent,
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
    "INSERT INTO fragments (source, mimetype, text, extent_unit, extent_start, extent_end)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id";

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

    async fn store(dir: &Path) -> IndexStore {
        let s = IndexStore::open(dir).await.expect("opens");
        s.declare_embedding("test-model", 8).await.expect("declares");
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

        let rels = s.relations_touching(&[section]).await.expect("ok");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].kind, RelationKind::contains());

        // Deleting the source's fragments cascades relations.
        s.delete_fragments_of(sid).await.expect("deletes");
        assert!(s.fragments_of(sid).await.expect("ok").is_empty());
        assert!(s.all_relations().await.expect("ok").is_empty());
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
            },
            SearchRow {
                fragment: FragmentId(2),
                source: Some(SourceId(2)),
                text: "quarterly tax filing checklist".into(),
                vector: Some(unit(3)),
            },
            SearchRow {
                fragment: FragmentId(3),
                source: None,
                text: "unembedded fragment about renovation permits".into(),
                vector: None,
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
    async fn embedding_change_pends_a_reembed_instead_of_refusing() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let s = store(dir.path()).await;
            s.add_search_rows(&[SearchRow {
                fragment: FragmentId(1),
                source: Some(SourceId(1)),
                text: "kitchen renovation".into(),
                vector: Some(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            }])
            .await
            .expect("adds");
            s.rebuild_fts().await.expect("indexes");
        }

        let s = IndexStore::open(dir.path()).await.expect("opens");
        s.declare_embedding("other-model", 16)
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
        s.declare_embedding("other-model", 16).await.expect("declares");
        assert!(!s.reembed_pending());
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
    async fn search_refuses_until_an_embedder_declares() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = IndexStore::open(dir.path()).await.expect("opens");
        assert!(matches!(
            s.search_fts("anything", 5).await,
            Err(StoreError::NoSearchSurface)
        ));
        s.declare_embedding("test-model", 8).await.expect("declares");
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
            },
            SearchRow {
                fragment: orphan,
                source: None,
                text: "Nobody".into(),
                vector: None,
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
