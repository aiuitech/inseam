//! The index store. Two layers, as `design/runtime.md` settled: SQLite is the
//! transactional source of truth for the catalog (sources + envelopes) and
//! the semantic graph (fragments + relations + entity registry); LanceDB
//! holds the derived search surfaces — full-text and vectors — and is
//! rebuildable from SQLite at any time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use arrow_array::builder::{FixedSizeListBuilder, Float32Builder};
use arrow_array::{Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lance_index::scalar::FullTextSearchQuery;
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{DistanceType, Table};
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::address::{Address, ContentLength, Envelope, HostId, Locator, Property, Timestamp};
use crate::fragment::{Extent, FragmentId, Mimetype, NewFragment, Relation, RelationKind};

const SCHEMA_VERSION: &str = "2";
const LANCE_TABLE: &str = "fragments";
/// Ids per DELETE predicate; keeps the SQL bounded.
const DELETE_CHUNK: usize = 400;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("catalog database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("lance index error: {0}")]
    Lance(#[from] lancedb::Error),
    #[error("could not prepare index directory {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error(
        "index was embedded with `{stored_model}` ({stored_dims} dims), profile now says \
         `{profile_model}` ({profile_dims} dims); run `inseam index <dir>` to re-embed the search index"
    )]
    ReembedRequired {
        stored_model: String,
        stored_dims: usize,
        profile_model: String,
        profile_dims: usize,
    },
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

/// A row bound for the Lance search table: a text-bearing fragment and its
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

pub struct IndexStore {
    sql: Mutex<Connection>,
    lance_conn: lancedb::Connection,
    /// Behind a mutex because an embedding migration replaces the table;
    /// `Table` is a cheap cloneable handle.
    lance: Mutex<Table>,
    dims: usize,
    embedding_model: String,
    /// `Some((model, dims))` the index was embedded with when that differs
    /// from the current profile: search refuses until an index run re-embeds
    /// (`design/index-maintenance.md`).
    reembed_from: Mutex<Option<(String, usize)>>,
}

impl IndexStore {
    /// Open (or create) the store in `dir`. `dims` of 0 means no vector
    /// column: full-text only. An embedding config differing from the one the
    /// index was built with does not refuse: it puts the store into a
    /// re-embed-pending state that the next index run resolves.
    pub async fn open(dir: &Path, dims: usize, embedding_model: &str) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let sql = Connection::open(dir.join("catalog.sqlite3"))?;
        sql.pragma_update(None, "journal_mode", "WAL")?;
        sql.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&sql)?;
        let reembed_from = check_embedding_meta(&sql, dims, embedding_model)?;

        let lance_conn = lancedb::connect(
            dir.join("lance")
                .to_str()
                .expect("data dir paths are valid UTF-8 by construction"),
        )
        .execute()
        .await?;
        let lance = match lance_conn.open_table(LANCE_TABLE).execute().await {
            Ok(t) => t,
            Err(lancedb::Error::TableNotFound { .. }) => {
                lance_conn
                    .create_empty_table(LANCE_TABLE, lance_schema(dims))
                    .execute()
                    .await?
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            sql: Mutex::new(sql),
            lance_conn,
            lance: Mutex::new(lance),
            dims,
            embedding_model: embedding_model.to_string(),
            reembed_from: Mutex::new(reembed_from),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.sql.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn table(&self) -> Table {
        self.lance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // ------------------------------------------------------------------
    // Catalog
    // ------------------------------------------------------------------

    /// Compare what enumeration sees against the catalog. `Unchanged` demands
    /// a fully indexed prior run under the current profile shape: interrupted
    /// sources, profile-stale sources, and catalog-only rows (which carry no
    /// stamp) all come back `Changed`.
    pub fn freshness(
        &self,
        address: &Address,
        modified: Option<Timestamp>,
        raw_bytes: u64,
        profile_stamp: &str,
    ) -> Result<Freshness, StoreError> {
        let conn = self.conn();
        let row: Option<(Option<i64>, i64, i64, Option<String>)> = conn
            .query_row(
                "SELECT modified, raw_bytes, indexed, profile_stamp
                 FROM sources WHERE host = ?1 AND locator = ?2",
                params![address.host.as_str(), address.locator.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(match row {
            None => Freshness::New,
            Some((m, b, indexed, stamp)) => {
                let same = m == modified.map(|t| t.0)
                    && b == raw_bytes as i64
                    && indexed == 1
                    && stamp.as_deref() == Some(profile_stamp);
                if same {
                    Freshness::Unchanged
                } else {
                    Freshness::Changed
                }
            }
        })
    }

    /// Write a source's envelope into the catalog, clearing its indexed mark
    /// until `mark_indexed` confirms the fragments are in place.
    pub fn upsert_source(
        &self,
        address: &Address,
        envelope: &Envelope,
        raw_bytes: u64,
    ) -> Result<SourceId, StoreError> {
        let conn = self.conn();
        let properties = serde_json::to_string(&envelope.properties)
            .expect("envelope properties serialize to JSON");
        let id: i64 = conn.query_row(
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
                envelope.source_type,
                envelope.content_type.to_string(),
                envelope.length.unit(),
                envelope.length.value() as i64,
                envelope.created.map(|t| t.0),
                envelope.modified.map(|t| t.0),
                envelope.observed.0,
                envelope.hint,
                properties,
                raw_bytes as i64,
            ],
            |r| r.get(0),
        )?;
        Ok(SourceId(id))
    }

    /// Confirm a source's index run completed. A deep-indexed source records
    /// the profile stamp its subtree was built under; a catalog-only source
    /// records none, which is exactly what makes it dirty again the moment
    /// budget or cutoff would let it be deep-indexed.
    pub fn mark_indexed(
        &self,
        source: SourceId,
        profile_stamp: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sources SET indexed = 1, profile_stamp = ?2 WHERE id = ?1",
            params![source.0, profile_stamp],
        )?;
        Ok(())
    }

    /// Every cataloged source of a host, as `(id, locator)` — the sweep's
    /// deletion reconciliation diffs this against what enumeration saw.
    pub fn sources_of_host(&self, host: &HostId) -> Result<Vec<(SourceId, String)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, locator FROM sources WHERE host = ?1")?;
        let rows = stmt
            .query_map(params![host.as_str()], |r| {
                Ok((SourceId(r.get(0)?), r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remove a source's catalog row. Call `delete_fragments_of` first so the
    /// Lance rows can be cleared by id; the row deletion itself cascades any
    /// fragments that remain.
    pub fn delete_source(&self, source: SourceId) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM sources WHERE id = ?1", params![source.0])?;
        Ok(())
    }

    /// Drop entity fragments no relation touches anymore — the consequence of
    /// source deletions, rebuilds that no longer mention them, and disabling
    /// entity extraction. Returns the dropped fragment ids so the caller can
    /// clear the Lance rows; the registry rows cascade.
    pub fn gc_entities(&self) -> Result<Vec<i64>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM fragments WHERE source IS NULL
               AND NOT EXISTS (SELECT 1 FROM relations
                               WHERE from_fragment = fragments.id OR to_fragment = fragments.id)",
        )?;
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for chunk in ids.chunks(DELETE_CHUNK) {
            let list = chunk
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            conn.execute(&format!("DELETE FROM fragments WHERE id IN ({list})"), [])?;
        }
        Ok(ids)
    }

    pub fn set_root_fragment(
        &self,
        source: SourceId,
        fragment: FragmentId,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sources SET root_fragment = ?1 WHERE id = ?2",
            params![fragment.0, source.0],
        )?;
        Ok(())
    }

    pub fn source(&self, id: SourceId) -> Result<Option<StoredSource>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            &format!("SELECT {SOURCE_COLUMNS} FROM sources WHERE id = ?1"),
            params![id.0],
            row_to_source,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn source_by_address(&self, address: &Address) -> Result<Option<StoredSource>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            &format!("SELECT {SOURCE_COLUMNS} FROM sources WHERE host = ?1 AND locator = ?2"),
            params![address.host.as_str(), address.locator.as_str()],
            row_to_source,
        )
        .optional()
        .map_err(StoreError::from)
    }

    // ------------------------------------------------------------------
    // Graph
    // ------------------------------------------------------------------

    /// Drop a source's fragments (relations cascade). Returns the dropped
    /// fragment ids so the caller can clear the Lance rows too. Entity
    /// fragments survive — only their mention edges into this source go.
    pub fn delete_fragments_of(&self, source: SourceId) -> Result<Vec<i64>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM fragments WHERE source = ?1")?;
        let ids: Vec<i64> = stmt
            .query_map(params![source.0], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        conn.execute("DELETE FROM fragments WHERE source = ?1", params![source.0])?;
        Ok(ids)
    }

    pub fn insert_fragment(
        &self,
        source: Option<SourceId>,
        fragment: &NewFragment,
    ) -> Result<FragmentId, StoreError> {
        let (unit, start, end) = extent_columns(fragment.extent);
        let id: i64 = self.conn().query_row(
            "INSERT INTO fragments (source, mimetype, text, extent_unit, extent_start, extent_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING id",
            params![
                source.map(|s| s.0),
                fragment.mimetype.to_string(),
                fragment.text,
                unit,
                start,
                end,
            ],
            |r| r.get(0),
        )?;
        Ok(FragmentId(id))
    }

    pub fn insert_relation(&self, relation: &Relation) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT OR IGNORE INTO relations (from_fragment, kind, to_fragment) VALUES (?1, ?2, ?3)",
            params![relation.from.0, relation.kind.as_str(), relation.to.0],
        )?;
        Ok(())
    }

    pub fn fragment(&self, id: FragmentId) -> Result<Option<StoredFragment>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            &format!("SELECT {FRAGMENT_COLUMNS} FROM fragments WHERE id = ?1"),
            params![id.0],
            row_to_fragment,
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn fragments_of(&self, source: SourceId) -> Result<Vec<StoredFragment>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {FRAGMENT_COLUMNS} FROM fragments WHERE source = ?1 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map(params![source.0], row_to_fragment)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn fragments(&self, ids: &[FragmentId]) -> Result<Vec<StoredFragment>, StoreError> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(f) = self.fragment(*id)? {
                out.push(f);
            }
        }
        Ok(out)
    }

    /// The whole relation graph. Personal-scale indexes keep this cheap; the
    /// Finder's propagation wants all edges anyway.
    pub fn all_relations(&self) -> Result<Vec<Relation>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT from_fragment, kind, to_fragment FROM relations")?;
        let rows = stmt
            .query_map([], row_to_relation)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every relation with either endpoint in `ids`.
    pub fn relations_touching(&self, ids: &[FragmentId]) -> Result<Vec<Relation>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // Numeric ids joined directly: no injection surface, bounded by caller.
        let list = ids
            .iter()
            .map(|f| f.0.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT from_fragment, kind, to_fragment FROM relations
             WHERE from_fragment IN ({list}) OR to_fragment IN ({list})"
        ))?;
        let rows = stmt
            .query_map([], row_to_relation)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Which source each fragment belongs to (entity fragments absent).
    pub fn sources_of_fragments(
        &self,
        ids: &[FragmentId],
    ) -> Result<HashMap<FragmentId, SourceId>, StoreError> {
        let mut map = HashMap::with_capacity(ids.len());
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT source FROM fragments WHERE id = ?1")?;
        for id in ids {
            let source: Option<Option<i64>> = stmt
                .query_row(params![id.0], |r| r.get(0))
                .optional()?;
            if let Some(Some(s)) = source {
                map.insert(*id, SourceId(s));
            }
        }
        Ok(map)
    }

    /// The mandatory summary for a source, if the index has built one.
    pub fn summary_of(&self, source: SourceId) -> Result<Option<String>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            "SELECT f.text FROM fragments f
             JOIN relations r ON r.from_fragment = f.id AND r.kind = 'derived-from'
             JOIN sources s ON r.to_fragment = s.root_fragment
             WHERE s.id = ?1 AND f.mimetype LIKE 'text/x-inseam-summary%'
             LIMIT 1",
            params![source.0],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(StoreError::from)
        .map(|o| o.flatten())
    }

    // ------------------------------------------------------------------
    // Entity registry
    // ------------------------------------------------------------------

    pub fn entity_fragment(&self, key: &str) -> Result<Option<FragmentId>, StoreError> {
        let conn = self.conn();
        conn.query_row(
            "SELECT fragment FROM entities WHERE key = ?1",
            params![key],
            |r| r.get::<_, i64>(0).map(FragmentId),
        )
        .optional()
        .map_err(StoreError::from)
    }

    pub fn register_entity(&self, key: &str, fragment: FragmentId) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT OR IGNORE INTO entities (key, fragment) VALUES (?1, ?2)",
            params![key, fragment.0],
        )?;
        Ok(())
    }

    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        let conn = self.conn();
        let one = |sql: &str| -> Result<u64, rusqlite::Error> {
            conn.query_row(sql, [], |r| r.get::<_, i64>(0)).map(|n| n as u64)
        };
        Ok(StoreStats {
            sources: one("SELECT COUNT(*) FROM sources")?,
            indexed_sources: one("SELECT COUNT(*) FROM sources WHERE indexed = 1")?,
            fragments: one("SELECT COUNT(*) FROM fragments")?,
            relations: one("SELECT COUNT(*) FROM relations")?,
            entities: one("SELECT COUNT(*) FROM entities")?,
        })
    }

    // ------------------------------------------------------------------
    // Search surfaces (Lance)
    // ------------------------------------------------------------------

    pub fn dimensions(&self) -> usize {
        self.dims
    }

    pub async fn add_search_rows(&self, rows: &[SearchRow]) -> Result<(), StoreError> {
        if rows.is_empty() {
            return Ok(());
        }
        let schema = lance_schema(self.dims);
        let ids = Int64Array::from_iter_values(rows.iter().map(|r| r.fragment.0));
        let sources = Int64Array::from(
            rows.iter()
                .map(|r| r.source.map(|s| s.0))
                .collect::<Vec<_>>(),
        );
        let texts = StringArray::from(rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>());
        let mut columns: Vec<arrow_array::ArrayRef> =
            vec![Arc::new(ids), Arc::new(sources), Arc::new(texts)];
        if self.dims > 0 {
            let mut builder =
                FixedSizeListBuilder::new(Float32Builder::new(), self.dims as i32);
            for row in rows {
                match &row.vector {
                    Some(v) => {
                        builder.values().append_slice(v);
                        builder.append(true);
                    }
                    None => {
                        builder.values().append_slice(&vec![0.0; self.dims]);
                        builder.append(false);
                    }
                }
            }
            columns.push(Arc::new(builder.finish()));
        }
        let batch = RecordBatch::try_new(schema, columns)
            .expect("search rows match the lance schema by construction");
        self.table().add(batch).execute().await?;
        Ok(())
    }

    pub async fn delete_search_rows(&self, ids: &[i64]) -> Result<(), StoreError> {
        for chunk in ids.chunks(DELETE_CHUNK) {
            let list = chunk
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            self.table().delete(format!("id IN ({list})").as_str()).await?;
        }
        Ok(())
    }

    /// (Re)build the full-text index over fragment text. Called after index
    /// runs so appended rows are always covered.
    pub async fn rebuild_fts(&self) -> Result<(), StoreError> {
        let table = self.table();
        if table.count_rows(None).await? == 0 {
            return Ok(());
        }
        table
            .create_index(&["text"], Index::FTS(FtsIndexBuilder::default()))
            .replace(true)
            .execute()
            .await?;
        Ok(())
    }

    /// Full-text seed search: fragment ids with BM25 scores, best first.
    pub async fn search_fts(&self, query: &str, k: usize) -> Result<Vec<(i64, f32)>, StoreError> {
        self.refuse_while_reembed_pending()?;
        let cleaned = sanitize_fts_query(query);
        let table = self.table();
        if cleaned.is_empty() || table.count_rows(None).await? == 0 {
            return Ok(Vec::new());
        }
        let fts = FullTextSearchQuery::new(cleaned)
            .with_column("text".to_string())
            .map_err(lancedb::Error::from)?;
        let batches: Vec<RecordBatch> = table
            .query()
            .full_text_search(fts)
            .limit(k)
            .execute()
            .await?
            .try_collect()
            .await?;
        collect_scored(&batches, "_score")
    }

    /// Vector seed search: fragment ids with cosine distances, best first.
    pub async fn search_vector(
        &self,
        vector: &[f32],
        k: usize,
    ) -> Result<Vec<(i64, f32)>, StoreError> {
        self.refuse_while_reembed_pending()?;
        let table = self.table();
        if self.dims == 0 || table.count_rows(None).await? == 0 {
            return Ok(Vec::new());
        }
        let batches: Vec<RecordBatch> = table
            .query()
            .nearest_to(vector)?
            .column("vector")
            .distance_type(DistanceType::Cosine)
            .limit(k)
            .execute()
            .await?
            .try_collect()
            .await?;
        collect_scored(&batches, "_distance")
    }

    pub async fn search_rows_count(&self) -> Result<usize, StoreError> {
        Ok(self.table().count_rows(None).await?)
    }

    // ------------------------------------------------------------------
    // Embedding migration (design/index-maintenance.md)
    // ------------------------------------------------------------------

    /// Whether the index was embedded under a different config than the
    /// current profile's and awaits an index run to re-embed.
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
            Some((stored_model, stored_dims)) => Err(StoreError::ReembedRequired {
                stored_model: stored_model.clone(),
                stored_dims: *stored_dims,
                profile_model: self.embedding_model.clone(),
                profile_dims: self.dims,
            }),
        }
    }

    /// Every text-bearing fragment, for re-populating the search table from
    /// the catalog: `(fragment, source, text)`. Exactly the rows the indexer
    /// would have buffered when it built each subtree.
    pub fn reembed_targets(
        &self,
    ) -> Result<Vec<(FragmentId, Option<SourceId>, String)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, source, text FROM fragments WHERE text IS NOT NULL AND TRIM(text) != ''",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    FragmentId(r.get(0)?),
                    r.get::<_, Option<i64>>(1)?.map(SourceId),
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Recreate the search table empty under the current profile's
    /// dimensions. The rows are re-added by the caller; searches keep
    /// refusing until [`Self::finish_reembed`].
    pub async fn begin_reembed(&self) -> Result<(), StoreError> {
        match self.lance_conn.drop_table(LANCE_TABLE, &[]).await {
            Ok(()) | Err(lancedb::Error::TableNotFound { .. }) => {}
            Err(e) => return Err(e.into()),
        }
        let table = self
            .lance_conn
            .create_empty_table(LANCE_TABLE, lance_schema(self.dims))
            .execute()
            .await?;
        *self.lance.lock().unwrap_or_else(|e| e.into_inner()) = table;
        Ok(())
    }

    /// Record the new embedding config as the one the index is built with
    /// and lift the search refusal. Interrupted migrations never reach this,
    /// so the next open detects the mismatch again and redoes the pass.
    pub fn finish_reembed(&self) -> Result<(), StoreError> {
        set_embedding_meta(&self.conn(), self.dims, &self.embedding_model)?;
        *self.reembed_from.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
}

impl std::fmt::Debug for IndexStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexStore")
            .field("dims", &self.dims)
            .finish_non_exhaustive()
    }
}

fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
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
           profile_stamp TEXT,
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
         );",
    )?;
    // v1 -> v2: the per-source profile stamp (design/index-maintenance.md).
    let has_stamp: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sources') WHERE name = 'profile_stamp'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if !has_stamp {
        conn.execute("ALTER TABLE sources ADD COLUMN profile_stamp TEXT", [])?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION],
    )?;
    Ok(())
}

/// Compare the embedding config the index was built with against the
/// profile's. First open records it; a mismatch returns the stored config so
/// the store enters the re-embed-pending state instead of refusing.
fn check_embedding_meta(
    conn: &Connection,
    dims: usize,
    model: &str,
) -> Result<Option<(String, usize)>, StoreError> {
    let stored: Option<(String, String)> = conn
        .query_row(
            "SELECT
               (SELECT value FROM meta WHERE key = 'embedding_model'),
               (SELECT value FROM meta WHERE key = 'embedding_dims')",
            [],
            |r| {
                Ok(match (r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?) {
                    (Some(m), Some(d)) => Some((m, d)),
                    _ => None,
                })
            },
        )
        .optional()?
        .flatten();
    match stored {
        None => {
            set_embedding_meta(conn, dims, model)?;
            Ok(None)
        }
        Some((stored_model, stored_dims)) => {
            let stored_dims: usize = stored_dims.parse().unwrap_or(0);
            if stored_model == model && stored_dims == dims {
                Ok(None)
            } else {
                Ok(Some((stored_model, stored_dims)))
            }
        }
    }
}

fn set_embedding_meta(conn: &Connection, dims: usize, model: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_model', ?1)",
        params![model],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_dims', ?1)",
        params![dims.to_string()],
    )?;
    Ok(())
}

fn lance_schema(dims: usize) -> SchemaRef {
    let mut fields = vec![
        Field::new("id", DataType::Int64, false),
        Field::new("source", DataType::Int64, true),
        Field::new("text", DataType::Utf8, false),
    ];
    if dims > 0 {
        fields.push(Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dims as i32,
            ),
            true,
        ));
    }
    Arc::new(Schema::new(fields))
}

fn collect_scored(batches: &[RecordBatch], score_column: &str) -> Result<Vec<(i64, f32)>, StoreError> {
    let mut out = Vec::new();
    for batch in batches {
        let ids = batch
            .column_by_name("id")
            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
            .ok_or_else(|| StoreError::Corrupt(0, "lance batch missing id column".into()))?;
        let scores = batch
            .column_by_name(score_column)
            .and_then(|c| c.as_any().downcast_ref::<arrow_array::Float32Array>())
            .ok_or_else(|| {
                StoreError::Corrupt(0, format!("lance batch missing {score_column} column"))
            })?;
        for i in 0..batch.num_rows() {
            out.push((ids.value(i), scores.value(i)));
        }
    }
    Ok(out)
}

/// Keep letters, digits and spaces: the seed query goes to a match query, and
/// tantivy syntax characters in user text would otherwise error or skew it.
fn sanitize_fts_query(q: &str) -> String {
    let cleaned: String = q
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

const SOURCE_COLUMNS: &str =
    "id, host, locator, source_type, content_type, len_unit, len, created, modified, observed, \
     hint, properties, root_fragment";

fn row_to_source(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSource> {
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
    let length = match len_unit.as_str() {
        "lines" => ContentLength::Lines(len as u64),
        _ => ContentLength::Bytes(len as u64),
    };
    Ok(StoredSource {
        id: SourceId(id),
        address,
        envelope: Envelope {
            source_type: r.get(3)?,
            content_type: Mimetype::parse(&content_type).map_err(|e| corrupt(id, e))?,
            length,
            created: r.get::<_, Option<i64>>(7)?.map(Timestamp),
            modified: r.get::<_, Option<i64>>(8)?.map(Timestamp),
            observed: Timestamp(r.get(9)?),
            hint: r.get(10)?,
            properties: serde_json::from_str::<Vec<Property>>(&properties)
                .map_err(|e| corrupt(id, e))?,
        },
        root_fragment: r.get::<_, Option<i64>>(12)?.map(FragmentId),
    })
}

const FRAGMENT_COLUMNS: &str = "id, source, mimetype, text, extent_unit, extent_start, extent_end";

fn row_to_fragment(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFragment> {
    let id: i64 = r.get(0)?;
    let mimetype: String = r.get(2)?;
    let extent = match (
        r.get::<_, Option<String>>(4)?,
        r.get::<_, Option<i64>>(5)?,
        r.get::<_, Option<i64>>(6)?,
    ) {
        (Some(unit), Some(start), Some(end)) => Some(match unit.as_str() {
            "lines" => Extent::Lines {
                start: start as u64,
                end: end as u64,
            },
            "millis" => Extent::Millis {
                start: start as u64,
                end: end as u64,
            },
            _ => Extent::Bytes {
                start: start as u64,
                end: end as u64,
            },
        }),
        _ => None,
    };
    Ok(StoredFragment {
        id: FragmentId(id),
        source: r.get::<_, Option<i64>>(1)?.map(SourceId),
        mimetype: Mimetype::parse(&mimetype).map_err(|e| corrupt(id, e))?,
        text: r.get(3)?,
        extent,
    })
}

fn row_to_relation(r: &rusqlite::Row<'_>) -> rusqlite::Result<Relation> {
    let from: i64 = r.get(0)?;
    let kind: String = r.get(1)?;
    let to: i64 = r.get(2)?;
    Ok(Relation {
        from: FragmentId(from),
        kind: kind.parse::<RelationKind>().map_err(|e| corrupt(from, e))?,
        to: FragmentId(to),
    })
}

fn corrupt(id: i64, err: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        id as usize,
        rusqlite::types::Type::Text,
        format!("{err}").into(),
    )
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
        IndexStore::open(dir, 8, "test-model").await.expect("opens")
    }

    #[tokio::test]
    async fn source_lifecycle_tracks_freshness() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");
        let env = envelope(100, 10);

        assert_eq!(
            s.freshness(&a, env.modified, 10, "stamp-a").expect("ok"),
            Freshness::New
        );
        let sid = s.upsert_source(&a, &env, 10).expect("upserts");
        // Not yet marked indexed: an interrupted run must come back Changed.
        assert_eq!(
            s.freshness(&a, env.modified, 10, "stamp-a").expect("ok"),
            Freshness::Changed
        );
        s.mark_indexed(sid, Some("stamp-a")).expect("marks");
        assert_eq!(
            s.freshness(&a, env.modified, 10, "stamp-a").expect("ok"),
            Freshness::Unchanged
        );
        assert_eq!(
            s.freshness(&a, Some(Timestamp(999)), 10, "stamp-a").expect("ok"),
            Freshness::Changed
        );
        assert_eq!(
            s.freshness(&a, env.modified, 11, "stamp-a").expect("ok"),
            Freshness::Changed
        );
        // A profile-shape change dirties the source without any file change.
        assert_eq!(
            s.freshness(&a, env.modified, 10, "stamp-b").expect("ok"),
            Freshness::Changed
        );
        // Catalog-only rows carry no stamp and never read Unchanged.
        s.mark_indexed(sid, None).expect("marks catalog-only");
        assert_eq!(
            s.freshness(&a, env.modified, 10, "stamp-a").expect("ok"),
            Freshness::Changed
        );

        let stored = s.source_by_address(&a).expect("ok").expect("present");
        assert_eq!(stored.id, sid);
        assert_eq!(stored.envelope.hint.as_deref(), Some("note.md"));
    }

    #[tokio::test]
    async fn fragments_relations_and_summary_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");
        let sid = s.upsert_source(&a, &envelope(1, 10), 10).expect("upserts");

        let root = s
            .insert_fragment(
                Some(sid),
                &NewFragment {
                    mimetype: Mimetype::markdown(),
                    text: None,
                    extent: Some(Extent::lines(1, 10)),
                },
            )
            .expect("inserts");
        s.set_root_fragment(sid, root).expect("sets root");
        let section = s
            .insert_fragment(
                Some(sid),
                &NewFragment {
                    mimetype: Mimetype::markdown(),
                    text: Some("# Kitchen\nbudget notes".into()),
                    extent: Some(Extent::lines(1, 2)),
                },
            )
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
            .expect("inserts");
        s.insert_relation(&RelationKind::Contains.edge(root, section))
            .expect("relates");
        s.insert_relation(&RelationKind::DerivedFrom.edge(root, summary))
            .expect("relates");

        assert_eq!(
            s.summary_of(sid).expect("ok").as_deref(),
            Some("Notes about the kitchen budget.")
        );
        let frags = s.fragments_of(sid).expect("ok");
        assert_eq!(frags.len(), 3);
        assert_eq!(frags[1].text.as_deref(), Some("# Kitchen\nbudget notes"));
        assert_eq!(frags[1].extent, Some(Extent::lines(1, 2)));

        let rels = s.relations_touching(&[section]).expect("ok");
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].kind, RelationKind::Contains);

        // Deleting the source's fragments cascades relations and reports ids.
        let dropped = s.delete_fragments_of(sid).expect("deletes");
        assert_eq!(dropped.len(), 3);
        assert!(s.all_relations().expect("ok").is_empty());
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

        let s = IndexStore::open(dir.path(), 16, "other-model")
            .await
            .expect("opens despite the change");
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
        s.finish_reembed().expect("records the new config");
        s.rebuild_fts().await.expect("indexes");
        assert!(!s.reembed_pending());
        let hits = s.search_fts("kitchen", 5).await.expect("searches again");
        assert_eq!(hits.first().map(|(id, _)| *id), Some(1));

        // A fresh open under the new config is clean.
        drop(s);
        let s = IndexStore::open(dir.path(), 16, "other-model")
            .await
            .expect("opens");
        assert!(!s.reembed_pending());
    }

    #[tokio::test]
    async fn gc_entities_drops_only_unrelated_entity_fragments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = store(dir.path()).await;
        let a = addr("inseam://fs-test/tmp/note.md");
        let sid = s.upsert_source(&a, &envelope(1, 10), 10).expect("upserts");
        let root = s
            .insert_fragment(
                Some(sid),
                &NewFragment {
                    mimetype: Mimetype::markdown(),
                    text: Some("greg's note".into()),
                    extent: None,
                },
            )
            .expect("inserts");
        let entity = |name: &str| {
            s.insert_fragment(
                None,
                &NewFragment {
                    mimetype: Mimetype::entity().with_param("kind", "person"),
                    text: Some(name.into()),
                    extent: None,
                },
            )
            .expect("inserts")
        };
        let mentioned = entity("Greg");
        let orphan = entity("Nobody");
        s.register_entity("person:greg", mentioned).expect("registers");
        s.register_entity("person:nobody", orphan).expect("registers");
        s.insert_relation(&RelationKind::Mentions.edge(root, mentioned))
            .expect("relates");

        let dropped = s.gc_entities().expect("gcs");
        assert_eq!(dropped, vec![orphan.0]);
        assert!(s.fragment(mentioned).expect("ok").is_some());
        assert!(s.fragment(orphan).expect("ok").is_none());
        // The registry row cascaded with the fragment.
        assert_eq!(s.entity_fragment("person:nobody").expect("ok"), None);
        assert_eq!(s.entity_fragment("person:greg").expect("ok"), Some(mentioned));

        // Deleting the source orphans the survivor; the next GC takes it.
        s.delete_fragments_of(sid).expect("deletes");
        let dropped = s.gc_entities().expect("gcs");
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
            .expect("inserts");
        s.register_entity("person:greg hunt", e).expect("registers");
        assert_eq!(
            s.entity_fragment("person:greg hunt").expect("ok"),
            Some(e)
        );
        assert_eq!(s.entity_fragment("person:unknown").expect("ok"), None);
        let f = s.fragment(e).expect("ok").expect("present");
        assert!(f.source.is_none());
        assert_eq!(f.mimetype.param("kind"), Some("person"));
    }
}
