//! The index store — the kernel's second responsibility (`design/kernel.md`).
//! Two layers in one engine, as `design/runtime.md` settled: a libSQL catalog
//! database is the transactional source of truth for the catalog (sources +
//! envelopes), the semantic graph (fragments + relations + entity registry),
//! and plugin state namespaces; a second libSQL database holds the derived
//! search surfaces — FTS5 full-text and native vectors — and is rebuildable
//! from the catalog at any time.
//!
//! There are no data migrations, anywhere, ever: a schema-version bump drops
//! and recreates the tables (everything here is derived and rebuilt by the
//! next sweep), and plugins extend by vocabulary — mimetypes, relation kinds,
//! properties — never by DDL. The search surface binds lazily to whatever
//! embedding identity the mounted embedder declares; a changed identity pends
//! an in-place re-embed instead of refusing to open.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use libsql::params;
use thiserror::Error;

use crate::address::{Address, ContentLength, Envelope, HostId, Locator, Property, Timestamp};
use crate::fragment::{Extent, FragmentId, Mimetype, NewFragment, Relation, RelationKind};

const SCHEMA_VERSION: &str = "3";
/// Ids per DELETE predicate; keeps the SQL bounded.
const DELETE_CHUNK: usize = 400;

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

/// A fragment as the graph stores it. `source` is `None` only for entity
/// fragments, which are deduplicated across the whole index.
#[derive(Debug, Clone)]
pub struct StoredFragment {
    pub id: FragmentId,
    pub source: Option<SourceId>,
    pub mimetype: Mimetype,
    pub text: Option<String>,
    pub extent: Option<Extent>,
}

/// What the catalog knows about a source relative to what enumeration sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    New,
    Changed,
    Unchanged,
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

/// A row bound for the search database: a text-bearing fragment and its
/// (optional) embedding.
#[derive(Debug, Clone)]
pub struct SearchRow {
    pub fragment: FragmentId,
    pub source: Option<SourceId>,
    pub text: String,
    pub vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct StoreStats {
    pub sources: u64,
    pub indexed_sources: u64,
    pub fragments: u64,
    pub relations: u64,
    pub entities: u64,
}

/// The live search surface: bound when an embedder declares its identity.
/// The `Database` handle owns the file; connections are cheap clones over it.
struct SearchSurface {
    #[expect(dead_code, reason = "keeps the database handle alive for its connections")]
    db: libsql::Database,
    conn: libsql::Connection,
    dims: usize,
    model: String,
}

pub struct IndexStore {
    #[expect(dead_code, reason = "keeps the database handle alive for its connections")]
    catalog_db: libsql::Database,
    catalog: libsql::Connection,
    search_path: std::path::PathBuf,
    /// `None` until an embedder plugin declares the embedding identity; the
    /// search surface belongs to that identity, not to the store's opening.
    surface: Mutex<Option<Arc<SearchSurface>>>,
    /// `Some((model, dims))` the index was embedded with when that differs
    /// from the declared identity: search refuses until an index run
    /// re-embeds (`design/index-maintenance.md`).
    reembed_from: Mutex<Option<(String, usize)>>,
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
        let catalog_db = libsql::Builder::new_local(dir.join("catalog.sqlite3"))
            .build()
            .await?;
        let catalog = catalog_db.connect()?;
        catalog.query("PRAGMA journal_mode = WAL", ()).await?;
        catalog.query("PRAGMA foreign_keys = ON", ()).await?;
        converge_schema(&catalog).await?;
        Ok(Self {
            catalog_db,
            catalog,
            search_path: dir.join("search.sqlite3"),
            surface: Mutex::new(None),
            reembed_from: Mutex::new(None),
        })
    }

    /// Bind the search surface to the mounted embedder's identity. Called by
    /// the embedder provider when it activates. A mismatch with the identity
    /// the index was built under pends an in-place re-embed rather than
    /// refusing; searches refuse until an index run performs it.
    pub async fn declare_embedding(&self, model: &str, dims: usize) -> Result<(), StoreError> {
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

        let db = libsql::Builder::new_local(&self.search_path).build().await?;
        let conn = db.connect()?;
        conn.query("PRAGMA journal_mode = WAL", ()).await?;
        // `IF NOT EXISTS` deliberately leaves a table built under a different
        // identity in place: searches refuse while the re-embed is pending,
        // and `begin_reembed` recreates the table under the new dimensions.
        conn.execute_batch(&search_schema_sql(dims)).await?;
        *self.surface.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(SearchSurface {
            db,
            conn,
            dims,
            model: model.to_string(),
        }));
        *self.reembed_from.lock().unwrap_or_else(|e| e.into_inner()) = pending;
        Ok(())
    }

    /// Withdraw the search surface (the embedder unmounted). Catalog and
    /// graph stay serviceable; searches refuse until a new declaration.
    pub fn withdraw_embedding(&self) {
        *self.surface.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn surface(&self) -> Result<Arc<SearchSurface>, StoreError> {
        self.surface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(StoreError::NoSearchSurface)
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
        let properties = serde_json::to_string(&envelope.properties)
            .expect("envelope properties serialize to JSON");
        let row = self
            .first_row(
                "INSERT INTO sources
                   (host, locator, source_type, content_type, len_unit, len,
                    created, modified, observed, hint, properties, raw_bytes, indexed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0)
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
                    i64::try_from(raw_bytes).unwrap_or(i64::MAX),
                ],
            )
            .await?
            .ok_or_else(|| StoreError::Corrupt(0, "source upsert returned no id".into()))?;
        Ok(SourceId(row.get(0)?))
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
        let (stamp, inventory) = match shape {
            Some((stamp, inventory)) => (
                Some(stamp),
                Some(
                    serde_json::to_string(inventory)
                        .expect("inventory entries serialize to JSON"),
                ),
            ),
            None => (None, None),
        };
        self.catalog
            .execute(
                "UPDATE sources SET indexed = 1, shape_stamp = ?2, mimetypes = ?3 WHERE id = ?1",
                params![source.0, stamp, inventory],
            )
            .await?;
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

    /// Remove a source's catalog row. Call `delete_fragments_of` first so the
    /// search rows can be cleared by id; the row deletion itself cascades any
    /// fragments that remain.
    pub async fn delete_source(&self, source: SourceId) -> Result<(), StoreError> {
        self.catalog
            .execute("DELETE FROM sources WHERE id = ?1", params![source.0])
            .await?;
        Ok(())
    }

    /// Drop entity fragments no relation touches anymore — the consequence of
    /// source deletions, rebuilds that no longer mention them, and disabling
    /// entity extraction. Returns the dropped fragment ids so the caller can
    /// clear the search rows; the registry rows cascade.
    pub async fn gc_entities(&self) -> Result<Vec<i64>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT id FROM fragments WHERE source IS NULL
                   AND NOT EXISTS (SELECT 1 FROM relations
                                   WHERE from_fragment = fragments.id OR to_fragment = fragments.id)",
                (),
            )
            .await?;
        let mut ids: Vec<i64> = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get(0)?);
        }
        for chunk in ids.chunks(DELETE_CHUNK) {
            let list = chunk
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            self.catalog
                .execute(&format!("DELETE FROM fragments WHERE id IN ({list})"), ())
                .await?;
        }
        Ok(ids)
    }

    pub async fn set_root_fragment(
        &self,
        source: SourceId,
        fragment: FragmentId,
    ) -> Result<(), StoreError> {
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

    /// Drop a source's fragments (relations cascade). Returns the dropped
    /// fragment ids so the caller can clear the search rows too. Entity
    /// fragments survive — only their mention edges into this source go.
    pub async fn delete_fragments_of(&self, source: SourceId) -> Result<Vec<i64>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT id FROM fragments WHERE source = ?1",
                params![source.0],
            )
            .await?;
        let mut ids: Vec<i64> = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get(0)?);
        }
        self.catalog
            .execute("DELETE FROM fragments WHERE source = ?1", params![source.0])
            .await?;
        Ok(ids)
    }

    pub async fn insert_fragment(
        &self,
        source: Option<SourceId>,
        fragment: &NewFragment,
    ) -> Result<FragmentId, StoreError> {
        let (unit, start, end) = extent_columns(fragment.extent);
        let row = self
            .first_row(
                "INSERT INTO fragments (source, mimetype, text, extent_unit, extent_start, extent_end)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
                params![
                    source.map(|s| s.0),
                    fragment.mimetype.to_string(),
                    fragment.text.as_deref(),
                    unit,
                    start,
                    end,
                ],
            )
            .await?
            .ok_or_else(|| StoreError::Corrupt(0, "fragment insert returned no id".into()))?;
        Ok(FragmentId(row.get(0)?))
    }

    pub async fn insert_relation(&self, relation: &Relation) -> Result<(), StoreError> {
        self.catalog
            .execute(
                "INSERT OR IGNORE INTO relations (from_fragment, kind, to_fragment) VALUES (?1, ?2, ?3)",
                params![relation.from.0, relation.kind.as_str(), relation.to.0],
            )
            .await?;
        Ok(())
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

    pub async fn fragments(&self, ids: &[FragmentId]) -> Result<Vec<StoredFragment>, StoreError> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(f) = self.fragment(*id).await? {
                out.push(f);
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

    /// Every relation with either endpoint in `ids`.
    pub async fn relations_touching(&self, ids: &[FragmentId]) -> Result<Vec<Relation>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Numeric ids joined directly: no injection surface, bounded by caller.
        let list = ids
            .iter()
            .map(|f| f.0.to_string())
            .collect::<Vec<_>>()
            .join(",");
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
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_relation(&row)?);
        }
        Ok(out)
    }

    /// Which source each fragment belongs to (entity fragments absent).
    pub async fn sources_of_fragments(
        &self,
        ids: &[FragmentId],
    ) -> Result<HashMap<FragmentId, SourceId>, StoreError> {
        let mut map = HashMap::with_capacity(ids.len());
        for id in ids {
            let row = self
                .first_row("SELECT source FROM fragments WHERE id = ?1", params![id.0])
                .await?;
            if let Some(row) = row {
                let source: Option<i64> = row.get(0)?;
                if let Some(s) = source {
                    map.insert(*id, SourceId(s));
                }
            }
        }
        Ok(map)
    }

    /// The mandatory summary for a source, if the index has built one.
    pub async fn summary_of(&self, source: SourceId) -> Result<Option<String>, StoreError> {
        let row = self
            .first_row(
                "SELECT f.text FROM fragments f
                 JOIN relations r ON r.from_fragment = f.id AND r.kind = 'derived-from'
                 JOIN sources s ON r.to_fragment = s.root_fragment
                 WHERE s.id = ?1 AND f.mimetype LIKE 'text/x-inseam-summary%'
                 LIMIT 1",
                params![source.0],
            )
            .await?;
        match row {
            None => Ok(None),
            Some(row) => Ok(row.get::<Option<String>>(0)?),
        }
    }

    // ------------------------------------------------------------------
    // Entity registry
    // ------------------------------------------------------------------

    pub async fn entity_fragment(&self, key: &str) -> Result<Option<FragmentId>, StoreError> {
        let row = self
            .first_row("SELECT fragment FROM entities WHERE key = ?1", params![key])
            .await?;
        match row {
            None => Ok(None),
            Some(row) => Ok(Some(FragmentId(row.get(0)?))),
        }
    }

    pub async fn register_entity(&self, key: &str, fragment: FragmentId) -> Result<(), StoreError> {
        self.catalog
            .execute(
                "INSERT OR IGNORE INTO entities (key, fragment) VALUES (?1, ?2)",
                params![key, fragment.0],
            )
            .await?;
        Ok(())
    }

    pub async fn stats(&self) -> Result<StoreStats, StoreError> {
        Ok(StoreStats {
            sources: self.count_of("SELECT COUNT(*) FROM sources").await?,
            indexed_sources: self
                .count_of("SELECT COUNT(*) FROM sources WHERE indexed = 1")
                .await?,
            fragments: self.count_of("SELECT COUNT(*) FROM fragments").await?,
            relations: self.count_of("SELECT COUNT(*) FROM relations").await?,
            entities: self.count_of("SELECT COUNT(*) FROM entities").await?,
        })
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
        self.surface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.dims)
            .unwrap_or(0)
    }

    /// The embedding identity the search surface is bound to, if any.
    pub fn embedding_identity(&self) -> Option<(String, usize)> {
        self.surface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| (s.model.clone(), s.dims))
    }

    pub async fn add_search_rows(&self, rows: &[SearchRow]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let surface = self.surface()?;
        let dims = surface.dims;
        let tx = surface.conn.transaction().await?;
        for row in rows {
            if let Some(vector) = &row.vector {
                assert_eq!(vector.len(), dims, "search row vector matches declared dims");
            }
            let source = match row.source {
                Some(s) => libsql::Value::Integer(s.0),
                None => libsql::Value::Null,
            };
            let vector = match &row.vector {
                Some(v) if dims > 0 => libsql::Value::Blob(vector_blob(v)),
                _ => libsql::Value::Null,
            };
            tx.execute(
                "INSERT INTO search_rows (id, source, text, vector) VALUES (?1, ?2, ?3, ?4)",
                libsql::params![row.fragment.0, source, row.text.as_str(), vector],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_search_rows(&self, ids: &[i64]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let surface = self.surface()?;
        for chunk in ids.chunks(DELETE_CHUNK) {
            let list = chunk
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            surface
                .conn
                .execute(&format!("DELETE FROM search_rows WHERE id IN ({list})"), ())
                .await?;
        }
        Ok(())
    }

    /// Compact the full-text index. FTS5 stays transactionally in sync with
    /// `search_rows` through triggers, so this is maintenance, not a rebuild:
    /// it merges the incremental b-trees appended since the last index run.
    pub async fn rebuild_fts(&self) -> Result<(), StoreError> {
        let surface = self.surface()?;
        surface
            .conn
            .execute("INSERT INTO search_fts (search_fts) VALUES ('optimize')", ())
            .await?;
        Ok(())
    }

    /// Full-text seed search: fragment ids with BM25 scores, best first.
    pub async fn search_fts(&self, query: &str, k: usize) -> Result<Vec<(i64, f32)>, StoreError> {
        self.refuse_while_reembed_pending()?;
        let surface = self.surface()?;
        let matcher = fts_match_expression(query);
        if matcher.is_empty() {
            return Ok(Vec::new());
        }
        let rows = surface
            .conn
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
    ) -> Result<Vec<(i64, f32)>, StoreError> {
        self.refuse_while_reembed_pending()?;
        let surface = self.surface()?;
        if surface.dims == 0 {
            return Ok(Vec::new());
        }
        assert_eq!(vector.len(), surface.dims, "query vector matches declared dims");
        let rows = surface
            .conn
            .query(
                "SELECT id, vector_distance_cos(vector, ?1) AS distance FROM search_rows
                 WHERE vector IS NOT NULL ORDER BY distance LIMIT ?2",
                libsql::params![libsql::Value::Blob(vector_blob(vector)), bounded_limit(k)],
            )
            .await?;
        collect_scored(rows, |raw| raw).await
    }

    pub async fn search_rows_count(&self) -> Result<usize, StoreError> {
        let surface = self.surface()?;
        let mut rows = surface
            .conn
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
        self.reembed_from
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn refuse_while_reembed_pending(&self) -> Result<(), StoreError> {
        let pending = self.reembed_from.lock().unwrap_or_else(|e| e.into_inner());
        match pending.as_ref() {
            None => Ok(()),
            Some((stored_model, stored_dims)) => {
                let (declared_model, declared_dims) =
                    self.embedding_identity().unwrap_or_default();
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
        let surface = self.surface()?;
        surface
            .conn
            .execute_batch(
                "DROP TRIGGER IF EXISTS search_rows_after_insert;
                 DROP TRIGGER IF EXISTS search_rows_after_delete;
                 DROP TABLE IF EXISTS search_fts;
                 DROP TABLE IF EXISTS search_rows;",
            )
            .await?;
        surface
            .conn
            .execute_batch(&search_schema_sql(surface.dims))
            .await?;
        Ok(())
    }

    /// Record the declared embedding identity as the one the index is built
    /// with and lift the search refusal. Interrupted migrations never reach
    /// this, so the next declaration detects the mismatch again and redoes
    /// the pass.
    pub async fn finish_reembed(&self) -> Result<(), StoreError> {
        let (model, dims) = self.embedding_identity().ok_or(StoreError::NoSearchSurface)?;
        set_embedding_meta(&self.catalog, dims, &model).await?;
        *self.reembed_from.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        self.catalog
            .execute(
                "INSERT OR REPLACE INTO plugin_state (namespace, key, value) VALUES (?1, ?2, ?3)",
                params![ns, key, value],
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn state_delete(&self, ns: &str, key: &str) -> Result<(), StoreError> {
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
        conn.execute_batch(
            "DROP TABLE IF EXISTS relations;
             DROP TABLE IF EXISTS entities;
             DROP TABLE IF EXISTS fragments;
             DROP TABLE IF EXISTS sources;
             DROP TABLE IF EXISTS plugin_state;
             DROP TABLE IF EXISTS plugin_state_meta;
             DROP TABLE IF EXISTS meta;",
        )
        .await?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
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
         CREATE TABLE IF NOT EXISTS entities (
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
         ) WITHOUT ROWID;",
    )
    .await?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION],
    )
    .await?;
    Ok(())
}

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

/// The search database schema. The FTS5 table is external-content over
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

/// An embedding as SQLite stores it: the raw little-endian `f32` bytes an
/// `F32_BLOB` column holds and the vector functions read.
fn vector_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
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
) -> Result<Vec<(i64, f32)>, StoreError> {
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: i64 = row.get(0)?;
        let raw: f64 = row.get(1)?;
        // Scores narrow to f32 at the API boundary, as they always have;
        // callers only consume rank order and coarse magnitudes.
        #[expect(clippy::cast_possible_truncation, reason = "deliberate score narrowing")]
        out.push((id, shape(raw) as f32));
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
     hint, properties, root_fragment";

fn row_to_source(r: &libsql::Row) -> Result<StoredSource, StoreError> {
    let id: i64 = r.get(0)?;
    let host: String = r.get(1)?;
    let locator: String = r.get(2)?;
    let content_type: String = r.get(4)?;
    let len_unit: String = r.get(5)?;
    let len: i64 = r.get(6)?;
    let properties: String = r.get(11)?;
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
        kind: kind.parse::<RelationKind>().map_err(|e| corrupt(from, e))?,
        to: FragmentId(to),
    })
}

fn corrupt(id: i64, err: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(id, format!("{err}"))
}

fn extent_columns(extent: Option<Extent>) -> (Option<&'static str>, Option<i64>, Option<i64>) {
    match extent {
        None => (None, None, None),
        Some(Extent::Lines { start, end }) => (Some("lines"), Some(start as i64), Some(end as i64)),
        Some(Extent::Bytes { start, end }) => (Some("bytes"), Some(start as i64), Some(end as i64)),
        Some(Extent::Millis { start, end }) => {
            (Some("millis"), Some(start as i64), Some(end as i64))
        }
    }
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
                Some(sid),
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
                Some(sid),
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
                Some(sid),
                &NewFragment {
                    mimetype: Mimetype::summary(),
                    text: Some("Notes about the kitchen budget.".into()),
                    extent: None,
                },
            )
            .await
            .expect("inserts");
        s.insert_relation(&RelationKind::Contains.edge(root, section))
            .await
            .expect("relates");
        s.insert_relation(&RelationKind::DerivedFrom.edge(root, summary))
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
        assert_eq!(rels[0].kind, RelationKind::Contains);

        // Deleting the source's fragments cascades relations and reports ids.
        let dropped = s.delete_fragments_of(sid).await.expect("deletes");
        assert_eq!(dropped.len(), 3);
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
        let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1) && ids.contains(&3), "got {ids:?}");
        assert!(!ids.contains(&2));

        let near = s.search_vector(&unit(0), 2).await.expect("searches");
        assert_eq!(near[0].0, 1);
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
        assert_eq!(hits.first().map(|(id, _)| *id), Some(9));

        s.delete_search_rows(&[1, 3]).await.expect("deletes");
        let hits = s.search_fts("renovation", 10).await.expect("searches");
        let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
        assert!(!ids.contains(&1) && !ids.contains(&3), "got {ids:?}");
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
        assert_eq!(hits.first().map(|(id, _)| *id), Some(1));

        // A fresh open + declaration under the new identity is clean.
        drop(s);
        let s = IndexStore::open(dir.path()).await.expect("opens");
        s.declare_embedding("other-model", 16).await.expect("declares");
        assert!(!s.reembed_pending());
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
    async fn gc_entities_drops_only_unrelated_entity_fragments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");
        let sid = s
            .upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let root = s
            .insert_fragment(
                Some(sid),
                &NewFragment {
                    mimetype: Mimetype::markdown(),
                    text: Some("greg's note".into()),
                    extent: None,
                },
            )
            .await
            .expect("inserts");
        async fn entity(s: &IndexStore, name: &str) -> FragmentId {
            s.insert_fragment(
                None,
                &NewFragment {
                    mimetype: Mimetype::entity().with_param("kind", "person"),
                    text: Some(name.into()),
                    extent: None,
                },
            )
            .await
            .expect("inserts")
        }
        let mentioned = entity(&s, "Greg").await;
        let orphan = entity(&s, "Nobody").await;
        s.register_entity("person:greg", mentioned)
            .await
            .expect("registers");
        s.register_entity("person:nobody", orphan)
            .await
            .expect("registers");
        s.insert_relation(&RelationKind::Mentions.edge(root, mentioned))
            .await
            .expect("relates");

        let dropped = s.gc_entities().await.expect("gcs");
        assert_eq!(dropped, vec![orphan.0]);
        assert!(s.fragment(mentioned).await.expect("ok").is_some());
        assert!(s.fragment(orphan).await.expect("ok").is_none());
        // The registry row cascaded with the fragment.
        assert_eq!(s.entity_fragment("person:nobody").await.expect("ok"), None);
        assert_eq!(
            s.entity_fragment("person:greg").await.expect("ok"),
            Some(mentioned)
        );

        // Deleting the source orphans the survivor; the next GC takes it.
        s.delete_fragments_of(sid).await.expect("deletes");
        let dropped = s.gc_entities().await.expect("gcs");
        assert_eq!(dropped, vec![mentioned.0]);
    }

    #[tokio::test]
    async fn entity_registry_deduplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let e = s
            .insert_fragment(
                None,
                &NewFragment {
                    mimetype: Mimetype::entity().with_param("kind", "person"),
                    text: Some("Greg Hunt".into()),
                    extent: None,
                },
            )
            .await
            .expect("inserts");
        s.register_entity("person:greg hunt", e).await.expect("registers");
        assert_eq!(
            s.entity_fragment("person:greg hunt").await.expect("ok"),
            Some(e)
        );
        assert_eq!(s.entity_fragment("person:unknown").await.expect("ok"), None);
        let f = s.fragment(e).await.expect("ok").expect("present");
        assert!(f.source.is_none());
        assert_eq!(f.mimetype.param("kind"), Some("person"));
    }
}
