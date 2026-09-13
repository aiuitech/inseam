//! The replicated log and the roster as the store holds them
//! (`design/address-sync.md`, `design/roster.md`). Every record this node
//! originates enters its own log under a monotonic sequence; every log it
//! has seen from a peer sits beside it in the same table; and the roster
//! tables are the materialized view of the latest entry per key. The store
//! never learns the node's own id — identity is the transport's key,
//! minted above the kernel — so the local log is filed under
//! [`LOCAL_ORIGIN`] and the sync seam maps it to the real id at the API
//! boundary through the `local` parameter every read and apply takes.
//!
//! The catalog writes in `store.rs` log themselves: an upsert that changes
//! a source's envelope appends a `Record::Source`, a delete of a local row
//! appends a `Record::SourceGone`. The roster kinds enter through
//! [`IndexStore::publish`]. Peers' entries enter through
//! [`IndexStore::apply_remote`], which is the one place origin-wins is
//! enforced: a node record from any origin but the node it describes is
//! held (so the vector stays honest) but never materialized.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use libsql::params;

use super::{
    IndexStore, SOURCE_COLUMNS, SourceId, StoreError, corrupt, drain_single_i64, row_to_source,
    search_tables_exist,
};
use crate::address::{Address, Envelope, HostId, Locator};
use crate::network::{
    Endpoint, Epoch, HostRecord, LOG_ENTRIES_PER_BATCH_MAX, LogEntry, NodeCapabilities, NodeId,
    NodeRecord, Record, Sequence, StewardCapabilities, StewardshipRecord, VectorEntry,
    VersionVector,
};

/// The `origin` column value of this node's own log. Empty because the
/// store has no id to write; a remote origin is always 64 hex characters,
/// so the two can never collide.
const LOCAL_ORIGIN: &str = "";
/// Most rows one roster listing or origin summary returns. The roster is
/// the network's self-description, and a network past this size is not
/// one owner's trust domain.
const ROSTER_ROWS_MAX: u32 = 10_000;
/// The largest epoch or sequence an INTEGER column holds. Values are
/// checked against it on the way in (`entry_fits_columns`, the clock
/// mint) so every conversion on the way out is infallible.
const COLUMN_MAX: u64 = i64::MAX.unsigned_abs();
const META_LOG_EPOCH: &str = "log_epoch";
const META_LOG_SEQ: &str = "log_seq";

/// The log and the materialized roster. Provenance columns on every
/// roster row name the entry that produced it, so a purge by origin and a
/// later rebuild from the log both have what they need.
pub(super) const SCHEMA_SQL: &str = "CREATE TABLE IF NOT EXISTS sync_log (
       origin TEXT NOT NULL,
       epoch INTEGER NOT NULL,
       seq INTEGER NOT NULL,
       key TEXT NOT NULL,
       tombstone INTEGER NOT NULL,
       record TEXT NOT NULL,
       PRIMARY KEY (origin, epoch, seq)
     ) WITHOUT ROWID;
     CREATE INDEX IF NOT EXISTS sync_log_by_key ON sync_log(origin, epoch, key);
     CREATE TABLE IF NOT EXISTS roster_nodes (
       node TEXT PRIMARY KEY,
       display_name TEXT NOT NULL,
       endpoints TEXT NOT NULL,
       capabilities TEXT NOT NULL,
       origin TEXT NOT NULL,
       epoch INTEGER NOT NULL,
       seq INTEGER NOT NULL
     ) WITHOUT ROWID;
     CREATE TABLE IF NOT EXISTS roster_hosts (
       host TEXT PRIMARY KEY,
       kind TEXT NOT NULL,
       display_name TEXT NOT NULL,
       origin TEXT NOT NULL,
       epoch INTEGER NOT NULL,
       seq INTEGER NOT NULL
     ) WITHOUT ROWID;
     CREATE TABLE IF NOT EXISTS roster_stewardships (
       node TEXT NOT NULL,
       host TEXT NOT NULL,
       capabilities TEXT NOT NULL,
       roots TEXT NOT NULL,
       origin TEXT NOT NULL,
       epoch INTEGER NOT NULL,
       seq INTEGER NOT NULL,
       PRIMARY KEY (node, host)
     ) WITHOUT ROWID;
     CREATE TABLE IF NOT EXISTS roster_expulsions (
       node TEXT PRIMARY KEY,
       origin TEXT NOT NULL,
       epoch INTEGER NOT NULL,
       seq INTEGER NOT NULL
     ) WITHOUT ROWID;";

/// The inverse of [`SCHEMA_SQL`].
pub(super) const SCHEMA_DROP_SQL: &str = "DROP TABLE IF EXISTS sync_log;
     DROP TABLE IF EXISTS roster_nodes;
     DROP TABLE IF EXISTS roster_hosts;
     DROP TABLE IF EXISTS roster_stewardships;
     DROP TABLE IF EXISTS roster_expulsions;";

/// One statement writes a source row for either writer. The local catalog
/// takes the row over unconditionally (`origin` becomes NULL, the
/// envelope is refreshed); a remote entry updates only a row that is
/// itself remote, which is what keeps a peer's observation from ever
/// overwriting what this node stewards.
const SOURCE_UPSERT_SQL: &str = "INSERT INTO sources
       (host, locator, source_type, content_type, len_unit, len,
        created, modified, observed, hint, properties, digest, raw_bytes, indexed, origin, facets)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0, ?14, ?15)
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
       facets = excluded.facets,
       digest = excluded.digest,
       raw_bytes = excluded.raw_bytes,
       indexed = 0,
       origin = excluded.origin";

/// What one `apply_remote` batch did with its entries. The three sum to
/// the batch length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppliedReport {
    /// Held and materialized.
    pub applied: u64,
    /// Already held, or an echo of this node's own log.
    pub skipped: u64,
    /// Out of bounds (not held), or held but claiming an authority its
    /// origin does not have (a node record about another node).
    pub refused: u64,
}

/// How many entries one origin's log holds here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCount {
    pub origin: NodeId,
    pub entries: u64,
}

/// Who is writing a source row: this node's catalog, or a peer's entry.
enum SourceWriter {
    Local,
    Remote(NodeId),
}

/// Where a materialized roster row came from — the log entry that
/// produced it.
struct EntryPosition<'a> {
    origin: &'a str,
    epoch: Epoch,
    seq: Sequence,
}

/// The highest position held for one origin, as the `origin` column
/// spells it.
struct HeldPosition {
    origin: String,
    epoch: Epoch,
    seq: Sequence,
}

/// What a source row already holds that decides whether a fresh upsert is
/// news worth logging.
struct SourceState {
    origin: Option<NodeId>,
    envelope: Envelope,
    raw_bytes: u64,
}

enum Outcome {
    Applied,
    Skipped,
    Refused,
}

// ----------------------------------------------------------------------
// The local log
// ----------------------------------------------------------------------

/// The epoch the store's log runs under, if the meta table records one.
pub(super) async fn read_log_epoch_in(
    conn: &libsql::Connection,
) -> Result<Option<Epoch>, StoreError> {
    Ok(read_meta_u64_in(conn, META_LOG_EPOCH).await?.map(Epoch))
}

/// Settle the log's epoch after the tables exist: keep the recorded one,
/// or mint a fresh one for a new or rebuilt store. A rebuild supersedes
/// `superseded` (read before the drop), and the mint takes the larger of
/// the clock and `superseded + 1`, so a later rebuild always mints a
/// larger epoch even if the clock went backwards.
pub(super) async fn converge_log_in(
    conn: &libsql::Connection,
    superseded: Option<Epoch>,
) -> Result<Epoch, StoreError> {
    if let Some(epoch) = read_log_epoch_in(conn).await? {
        // Pair to the mint below, which writes both keys together.
        let seq = read_meta_u64_in(conn, META_LOG_SEQ).await?;
        seq.ok_or_else(|| corrupt(0, "meta records a log epoch but no log sequence"))?;
        return Ok(epoch);
    }
    let floor = superseded
        .map_or(0, |e| e.0.saturating_add(1))
        .min(COLUMN_MAX);
    let minted = Epoch(now_unix_nanos().max(floor));
    assert!(minted.0 >= floor);
    assert!(minted.0 <= COLUMN_MAX);
    write_meta_u64_in(conn, META_LOG_EPOCH, minted.0).await?;
    write_meta_u64_in(conn, META_LOG_SEQ, 0).await?;
    tracing::info!(epoch = minted.0, "minted a fresh log epoch");
    Ok(minted)
}

/// Nanoseconds since the Unix epoch, clamped to what an INTEGER column
/// holds (the year 2262). A clock before 1970 reads as zero; the floor in
/// `converge_log_in` still keeps a rebuild's epoch ahead of the last.
fn now_unix_nanos() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    nanos.min(COLUMN_MAX)
}

async fn read_meta_u64_in(conn: &libsql::Connection, key: &str) -> Result<Option<u64>, StoreError> {
    let value = drain_single(
        conn.query("SELECT value FROM meta WHERE key = ?1", params![key])
            .await?,
        |row| row.get::<String>(0).map_err(StoreError::from),
    )
    .await?;
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = value
        .parse::<u64>()
        .map_err(|e| corrupt(0, format!("meta {key} = `{value}`: {e}")))?;
    Ok(Some(parsed))
}

async fn write_meta_u64_in(
    conn: &libsql::Connection,
    key: &str,
    value: u64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, value.to_string()],
    )
    .await?;
    Ok(())
}

/// Append one record to the local log inside the caller's transaction:
/// the next sequence, compaction of every earlier local entry under the
/// same key (a tombstone included — the latest entry is the whole truth),
/// and the high-water mark in `meta`. The mark, not the table, is the
/// sequence authority: compaction may empty the table, and a reused
/// sequence would be invisible to every peer's vector.
async fn append_local_in(
    conn: &libsql::Connection,
    epoch: Epoch,
    record: &Record,
) -> Result<Sequence, StoreError> {
    let last = read_meta_u64_in(conn, META_LOG_SEQ)
        .await?
        .ok_or_else(|| corrupt(0, "meta records no log sequence"))?;
    let held = local_seq_max_in(conn, epoch).await?;
    assert!(
        held <= last,
        "the local log never runs ahead of its high-water mark"
    );
    assert!(last < COLUMN_MAX, "a local log never reaches 2^63 entries");
    let next = last + 1;
    let key = record.key().to_string();
    conn.execute(
        "DELETE FROM sync_log WHERE origin = ?1 AND epoch = ?2 AND key = ?3",
        params![LOCAL_ORIGIN, column(epoch.0), key.as_str()],
    )
    .await?;
    insert_entry_in(conn, LOCAL_ORIGIN, epoch, Sequence(next), record).await?;
    write_meta_u64_in(conn, META_LOG_SEQ, next).await?;
    // Pair to the assertion above: the entry just written is the highest.
    assert_eq!(local_seq_max_in(conn, epoch).await?, next);
    Ok(Sequence(next))
}

async fn local_seq_max_in(conn: &libsql::Connection, epoch: Epoch) -> Result<u64, StoreError> {
    let max = drain_single_i64(
        conn.query(
            "SELECT COALESCE(MAX(seq), 0) FROM sync_log WHERE origin = ?1 AND epoch = ?2",
            params![LOCAL_ORIGIN, column(epoch.0)],
        )
        .await?,
    )
    .await?
    .ok_or_else(|| corrupt(0, "MAX(seq) returned no row"))?;
    u64::try_from(max).map_err(|e| corrupt(0, e))
}

async fn insert_entry_in(
    conn: &libsql::Connection,
    origin: &str,
    epoch: Epoch,
    seq: Sequence,
    record: &Record,
) -> Result<(), StoreError> {
    // Every field of a record is a string, an integer, or a list of those;
    // nothing serde_json refuses (no non-string map keys).
    let json = serde_json::to_string(record).expect("records serialize to JSON");
    conn.execute(
        "INSERT INTO sync_log (origin, epoch, seq, key, tombstone, record)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            origin,
            column(epoch.0),
            column(seq.0),
            record.key().to_string(),
            i64::from(record.is_tombstone()),
            json
        ],
    )
    .await?;
    Ok(())
}

/// An epoch or sequence as its INTEGER column holds it. Every value the
/// store holds was minted from the clock (clamped) or passed
/// `entry_fits_columns` on the way in, so this cannot fail.
fn column(value: u64) -> i64 {
    assert!(value <= COLUMN_MAX);
    i64::try_from(value).expect("checked against COLUMN_MAX above")
}

fn entry_fits_columns(entry: &LogEntry) -> bool {
    entry.epoch.0 <= COLUMN_MAX && entry.seq.0 <= COLUMN_MAX
}

// ----------------------------------------------------------------------
// Catalog writes that log themselves
// ----------------------------------------------------------------------

/// Write a source's envelope into the catalog as this node's own, clearing
/// its indexed mark until it is confirmed again, and log it when that is
/// news: a new row, a row taken over from a remote steward, or an envelope
/// that changed in anything but `observed`. A sweep re-upserts every
/// source it sees each run, and only real change may reach the peers.
pub(super) async fn upsert_source_in(
    conn: &libsql::Connection,
    epoch: Epoch,
    address: &Address,
    envelope: &Envelope,
    raw_bytes: u64,
) -> Result<SourceId, StoreError> {
    let news = match source_state_in(conn, address).await? {
        None => true,
        Some(stored) => source_changed(&stored, envelope, raw_bytes),
    };
    let id = write_source_row_in(conn, &SourceWriter::Local, address, envelope, raw_bytes)
        .await?
        .ok_or_else(|| corrupt(0, "source upsert returned no id"))?;
    if news {
        let record = Record::Source {
            address: address.clone(),
            envelope: envelope.clone(),
            raw_bytes,
        };
        append_local_in(conn, epoch, &record).await?;
    }
    Ok(id)
}

/// Whether a fresh envelope is news against the stored row. A remote row
/// is always news: the local catalog is taking it over.
fn source_changed(stored: &SourceState, envelope: &Envelope, raw_bytes: u64) -> bool {
    if stored.origin.is_some() {
        return true;
    }
    if stored.raw_bytes != raw_bytes {
        return true;
    }
    envelope_changed(&stored.envelope, envelope)
}

/// Everything but `observed`, which advances on every sighting. Struct
/// update over a field list so a field added to `Envelope` joins the
/// comparison instead of silently escaping it.
fn envelope_changed(stored: &Envelope, fresh: &Envelope) -> bool {
    let stored_as_seen_now = Envelope {
        observed: fresh.observed,
        ..stored.clone()
    };
    stored_as_seen_now != *fresh
}

async fn source_state_in(
    conn: &libsql::Connection,
    address: &Address,
) -> Result<Option<SourceState>, StoreError> {
    drain_single(
        conn.query(
            &format!(
                "SELECT {SOURCE_COLUMNS}, raw_bytes FROM sources WHERE host = ?1 AND locator = ?2"
            ),
            params![address.host.as_str(), address.locator.as_str()],
        )
        .await?,
        |row| {
            let stored = row_to_source(row)?;
            // `raw_bytes` follows the source columns, facets included.
            let raw_bytes: i64 = row.get(16)?;
            Ok(SourceState {
                origin: stored.origin,
                envelope: stored.envelope,
                raw_bytes: u64::try_from(raw_bytes).unwrap_or(0),
            })
        },
    )
    .await
}

/// Run [`SOURCE_UPSERT_SQL`] for `writer`. `None` only when a remote
/// writer met a local row and the guard left it alone.
async fn write_source_row_in(
    conn: &libsql::Connection,
    writer: &SourceWriter,
    address: &Address,
    envelope: &Envelope,
    raw_bytes: u64,
) -> Result<Option<SourceId>, StoreError> {
    let (guard, origin) = match writer {
        SourceWriter::Local => ("", None),
        SourceWriter::Remote(node) => ("WHERE sources.origin IS NOT NULL", Some(node.to_hex())),
    };
    let properties =
        serde_json::to_string(&envelope.properties).expect("envelope properties serialize to JSON");
    let facets = serde_json::to_string(&envelope.facets).expect("envelope facets serialize to JSON");
    let id = drain_single_i64(
        conn.query(
            &format!("{SOURCE_UPSERT_SQL} {guard} RETURNING id"),
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
                origin.as_deref(),
                facets,
            ],
        )
        .await?,
    )
    .await?;
    Ok(id.map(SourceId))
}

/// Remove a source, its search rows, and (through the cascades) its
/// fragments; log the removal when the row was this node's own. A remote
/// row just goes: its steward's log is the authority on it, and this
/// node has nothing to tell the network.
pub(super) async fn delete_source_in(
    conn: &libsql::Connection,
    epoch: Epoch,
    source: SourceId,
) -> Result<(), StoreError> {
    let row = drain_single(
        conn.query(
            "SELECT host, locator, origin FROM sources WHERE id = ?1",
            params![source.0],
        )
        .await?,
        |row| {
            let host: String = row.get(0)?;
            let locator: String = row.get(1)?;
            let origin: Option<String> = row.get(2)?;
            Ok((host, locator, origin))
        },
    )
    .await?;
    let Some((host, locator, origin)) = row else {
        return Ok(());
    };
    if search_tables_exist(conn).await? {
        conn.execute(
            "DELETE FROM search_rows WHERE source = ?1",
            params![source.0],
        )
        .await?;
    }
    conn.execute("DELETE FROM sources WHERE id = ?1", params![source.0])
        .await?;
    if origin.is_some() {
        return Ok(());
    }
    let address = Address::new(
        HostId::new(host).map_err(|e| corrupt(source.0, e))?,
        Locator::new(locator).map_err(|e| corrupt(source.0, e))?,
    );
    append_local_in(conn, epoch, &Record::SourceGone { address }).await?;
    Ok(())
}

// ----------------------------------------------------------------------
// Materialized roster rows
// ----------------------------------------------------------------------

async fn materialize_node_in(
    conn: &libsql::Connection,
    at: &EntryPosition<'_>,
    node: &NodeRecord,
) -> Result<(), StoreError> {
    let endpoints = serde_json::to_string(&node.endpoints).expect("endpoints serialize to JSON");
    let capabilities =
        serde_json::to_string(&node.capabilities).expect("capabilities serialize to JSON");
    conn.execute(
        "INSERT OR REPLACE INTO roster_nodes
           (node, display_name, endpoints, capabilities, origin, epoch, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            node.id.to_hex(),
            node.display_name.as_str(),
            endpoints,
            capabilities,
            at.origin,
            column(at.epoch.0),
            column(at.seq.0)
        ],
    )
    .await?;
    Ok(())
}

async fn materialize_host_in(
    conn: &libsql::Connection,
    at: &EntryPosition<'_>,
    host: &HostRecord,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO roster_hosts
           (host, kind, display_name, origin, epoch, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            host.id.as_str(),
            host.kind.as_str(),
            host.display_name.as_str(),
            at.origin,
            column(at.epoch.0),
            column(at.seq.0)
        ],
    )
    .await?;
    Ok(())
}

async fn materialize_stewardship_in(
    conn: &libsql::Connection,
    at: &EntryPosition<'_>,
    stewardship: &StewardshipRecord,
) -> Result<(), StoreError> {
    let capabilities = serde_json::to_string(&stewardship.capabilities)
        .expect("steward capabilities serialize to JSON");
    let roots = serde_json::to_string(&stewardship.roots).expect("roots serialize to JSON");
    conn.execute(
        "INSERT OR REPLACE INTO roster_stewardships
           (node, host, capabilities, roots, origin, epoch, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            stewardship.node.to_hex(),
            stewardship.host.as_str(),
            capabilities,
            roots,
            at.origin,
            column(at.epoch.0),
            column(at.seq.0)
        ],
    )
    .await?;
    Ok(())
}

async fn withdraw_stewardship_in(
    conn: &libsql::Connection,
    node: &NodeId,
    host: &HostId,
) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM roster_stewardships WHERE node = ?1 AND host = ?2",
        params![node.to_hex(), host.as_str()],
    )
    .await?;
    Ok(())
}

async fn materialize_expulsion_in(
    conn: &libsql::Connection,
    at: &EntryPosition<'_>,
    node: &NodeId,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO roster_expulsions (node, origin, epoch, seq)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            node.to_hex(),
            at.origin,
            column(at.epoch.0),
            column(at.seq.0)
        ],
    )
    .await?;
    Ok(())
}

/// Drop everything a node authored: its log, the catalog rows it
/// stewards, and the roster rows it produced or that describe it. Used
/// when the node is expelled and when a newer epoch of its log
/// supersedes the old one. Expulsions it authored stay: an expulsion is
/// the owner's decision, executed on whichever node was at hand, and
/// losing that node later must not readmit what the owner already
/// expelled.
async fn purge_node_in(conn: &libsql::Connection, node: &NodeId) -> Result<(), StoreError> {
    let hex = node.to_hex();
    // A remote row never carries a subtree (`write_source_row_in` lands
    // none, and a takeover makes the row local first), so deleting the
    // rows can leave no fragment or search row behind.
    let fragments = drain_single_i64(
        conn.query(
            "SELECT COUNT(*) FROM fragments f JOIN sources s ON s.id = f.source WHERE s.origin = ?1",
            params![hex.as_str()],
        )
        .await?,
    )
    .await?;
    assert_eq!(fragments, Some(0), "remote source rows carry no fragments");
    conn.execute(
        "DELETE FROM sync_log WHERE origin = ?1",
        params![hex.as_str()],
    )
    .await?;
    conn.execute(
        "DELETE FROM sources WHERE origin = ?1",
        params![hex.as_str()],
    )
    .await?;
    conn.execute(
        "DELETE FROM roster_nodes WHERE node = ?1 OR origin = ?1",
        params![hex.as_str()],
    )
    .await?;
    conn.execute(
        "DELETE FROM roster_hosts WHERE origin = ?1",
        params![hex.as_str()],
    )
    .await?;
    conn.execute(
        "DELETE FROM roster_stewardships WHERE node = ?1 OR origin = ?1",
        params![hex.as_str()],
    )
    .await?;
    Ok(())
}

/// Materialize a record this node just published. The catalog kinds never
/// arrive here: `publish` asserts them away, and the catalog writes
/// materialize themselves.
async fn materialize_local_in(
    conn: &libsql::Connection,
    at: &EntryPosition<'_>,
    record: &Record,
) -> Result<(), StoreError> {
    match record {
        Record::Node(node) => materialize_node_in(conn, at, node).await,
        Record::Host(host) => materialize_host_in(conn, at, host).await,
        Record::Stewardship(stewardship) => materialize_stewardship_in(conn, at, stewardship).await,
        Record::StewardshipWithdrawn { node, host } => {
            withdraw_stewardship_in(conn, node, host).await
        }
        Record::Expulsion { node } => {
            materialize_expulsion_in(conn, at, node).await?;
            purge_node_in(conn, node).await
        }
        Record::Source { .. } | Record::SourceGone { .. } => {
            unreachable!("publish asserts catalog records away")
        }
    }
}

/// Materialize one held remote entry, enforcing origin-wins: a record
/// about a node is only the word of that node. `Refused` leaves the
/// entry held (the vector has moved past it) but changes nothing.
async fn materialize_remote_in(
    conn: &libsql::Connection,
    local: &NodeId,
    entry: &LogEntry,
) -> Result<Outcome, StoreError> {
    let origin = entry.origin.to_hex();
    let at = EntryPosition {
        origin: &origin,
        epoch: entry.epoch,
        seq: entry.seq,
    };
    match &entry.record {
        Record::Source {
            address,
            envelope,
            raw_bytes,
        } => {
            let writer = SourceWriter::Remote(entry.origin);
            write_source_row_in(conn, &writer, address, envelope, *raw_bytes).await?;
            Ok(Outcome::Applied)
        }
        Record::SourceGone { address } => {
            delete_remote_source_in(conn, &origin, address).await?;
            Ok(Outcome::Applied)
        }
        Record::Node(node) => {
            if node.id != entry.origin {
                return Ok(Outcome::Refused);
            }
            materialize_node_in(conn, &at, node).await?;
            Ok(Outcome::Applied)
        }
        Record::Host(host) => {
            materialize_host_in(conn, &at, host).await?;
            Ok(Outcome::Applied)
        }
        Record::Stewardship(stewardship) => {
            if stewardship.node != entry.origin {
                return Ok(Outcome::Refused);
            }
            materialize_stewardship_in(conn, &at, stewardship).await?;
            Ok(Outcome::Applied)
        }
        Record::StewardshipWithdrawn { node, host } => {
            if *node != entry.origin {
                return Ok(Outcome::Refused);
            }
            withdraw_stewardship_in(conn, node, host).await?;
            Ok(Outcome::Applied)
        }
        Record::Expulsion { node } => {
            materialize_expulsion_in(conn, &at, node).await?;
            apply_remote_expulsion_purge_in(conn, local, &entry.origin, node).await?;
            Ok(Outcome::Applied)
        }
    }
}

/// The purge half of a remote expulsion. Two nodes are spared: this node
/// (the sync layer decides what its own expulsion means; the store keeps
/// serving) and the entry's own origin (the purge would erase the entry
/// that ordered it, and the peer would ship it again forever).
async fn apply_remote_expulsion_purge_in(
    conn: &libsql::Connection,
    local: &NodeId,
    origin: &NodeId,
    expelled: &NodeId,
) -> Result<(), StoreError> {
    if expelled == local {
        return Ok(());
    }
    if expelled == origin {
        return Ok(());
    }
    purge_node_in(conn, expelled).await
}

/// A tombstone removes only the row its origin stewards; a local row or
/// another steward's row under the same address is not its to delete.
async fn delete_remote_source_in(
    conn: &libsql::Connection,
    origin: &str,
    address: &Address,
) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM sources WHERE host = ?1 AND locator = ?2 AND origin = ?3",
        params![address.host.as_str(), address.locator.as_str(), origin],
    )
    .await?;
    Ok(())
}

// ----------------------------------------------------------------------
// Held positions
// ----------------------------------------------------------------------

/// The highest (epoch, seq) held per origin, as the column spells the
/// origin. One row per origin: an origin's older epoch is purged the
/// moment a newer one arrives, and the local log has one epoch by
/// construction.
async fn held_positions_in(conn: &libsql::Connection) -> Result<Vec<HeldPosition>, StoreError> {
    let mut rows = conn
        .query(
            "SELECT origin, epoch, MAX(seq) FROM sync_log
             GROUP BY origin, epoch ORDER BY origin, epoch LIMIT ?1",
            params![i64::from(ROSTER_ROWS_MAX)],
        )
        .await?;
    let mut out: Vec<HeldPosition> = Vec::new();
    while let Some(row) = rows.next().await? {
        let origin: String = row.get(0)?;
        let epoch: i64 = row.get(1)?;
        let seq: i64 = row.get(2)?;
        assert!(
            out.last().is_none_or(|last| last.origin != origin),
            "an origin holds exactly one epoch"
        );
        out.push(HeldPosition {
            origin,
            epoch: Epoch(u64::try_from(epoch).map_err(|e| corrupt(0, e))?),
            seq: Sequence(u64::try_from(seq).map_err(|e| corrupt(0, e))?),
        });
    }
    assert!(out.len() <= usize::try_from(ROSTER_ROWS_MAX).expect("u32 fits usize"));
    Ok(out)
}

/// The origin column mapped to the id the network knows: the local log
/// becomes `local`, everything else parses. A remote row can never carry
/// the local id — `apply_remote` skips echoes — so that is asserted.
fn mapped_origin(local: &NodeId, stored: &str) -> Result<NodeId, StoreError> {
    if stored == LOCAL_ORIGIN {
        return Ok(*local);
    }
    let origin: NodeId = stored.parse().map_err(|e| corrupt(0, e))?;
    assert!(
        origin != *local,
        "the store never holds an echo of the local log"
    );
    Ok(origin)
}

/// Whether an entry at (`epoch`, `seq`) from `origin` is news against a
/// known position — the one definition of "lacks", borrowed from
/// [`VersionVector`] so a batch and a peer's vector can never disagree.
fn position_lacks(
    known: Option<(Epoch, Sequence)>,
    origin: &NodeId,
    epoch: Epoch,
    seq: Sequence,
) -> bool {
    let vector = match known {
        None => VersionVector::default(),
        Some((known_epoch, known_seq)) => VersionVector::new(vec![VectorEntry {
            origin: *origin,
            epoch: known_epoch,
            seq: known_seq,
        }]),
    };
    vector.lacks(origin, epoch, seq)
}

/// Hold one remote entry: purge a superseded epoch, compact the key, and
/// insert. The caller has already decided the entry is news.
async fn hold_entry_in(
    conn: &libsql::Connection,
    known: Option<(Epoch, Sequence)>,
    entry: &LogEntry,
) -> Result<(), StoreError> {
    let supersedes_held_epoch = known.is_some_and(|(known_epoch, _)| entry.epoch > known_epoch);
    if supersedes_held_epoch {
        purge_node_in(conn, &entry.origin).await?;
    }
    let origin = entry.origin.to_hex();
    conn.execute(
        "DELETE FROM sync_log
         WHERE origin = ?1 AND epoch = ?2 AND key = ?3 AND seq < ?4",
        params![
            origin.as_str(),
            column(entry.epoch.0),
            entry.record.key().to_string(),
            column(entry.seq.0)
        ],
    )
    .await?;
    insert_entry_in(conn, &origin, entry.epoch, entry.seq, &entry.record).await
}

/// Decide and apply one entry of a batch, keeping `positions` current so
/// later entries of the same origin in the batch see the earlier ones.
async fn apply_entry_in(
    conn: &libsql::Connection,
    local: &NodeId,
    positions: &mut HashMap<NodeId, (Epoch, Sequence)>,
    entry: &LogEntry,
) -> Result<Outcome, StoreError> {
    if entry.origin == *local {
        return Ok(Outcome::Skipped);
    }
    if entry.record.check_bounds().is_err() {
        return Ok(Outcome::Refused);
    }
    if !entry_fits_columns(entry) {
        return Ok(Outcome::Refused);
    }
    let known = positions.get(&entry.origin).copied();
    if !position_lacks(known, &entry.origin, entry.epoch, entry.seq) {
        return Ok(Outcome::Skipped);
    }
    hold_entry_in(conn, known, entry).await?;
    positions.insert(entry.origin, (entry.epoch, entry.seq));
    materialize_remote_in(conn, local, entry).await
}

async fn entries_after_in(
    conn: &libsql::Connection,
    stored_origin: &str,
    origin: NodeId,
    known: (Epoch, Sequence),
    limit: usize,
    out: &mut Vec<LogEntry>,
) -> Result<(), StoreError> {
    assert!(limit >= 1);
    let mut rows = conn
        .query(
            "SELECT epoch, seq, record FROM sync_log
             WHERE origin = ?1 AND (epoch > ?2 OR (epoch = ?2 AND seq > ?3))
             ORDER BY epoch, seq LIMIT ?4",
            params![
                stored_origin,
                column(known.0.0),
                column(known.1.0),
                i64::try_from(limit).expect("a batch limit fits i64")
            ],
        )
        .await?;
    let mut taken: usize = 0;
    while let Some(row) = rows.next().await? {
        assert!(taken < limit, "LIMIT bounds the rows");
        let epoch: i64 = row.get(0)?;
        let seq: i64 = row.get(1)?;
        let record: String = row.get(2)?;
        out.push(LogEntry {
            origin,
            epoch: Epoch(u64::try_from(epoch).map_err(|e| corrupt(0, e))?),
            seq: Sequence(u64::try_from(seq).map_err(|e| corrupt(0, e))?),
            record: serde_json::from_str(&record).map_err(|e| corrupt(0, e))?,
        });
        taken += 1;
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Row readers
// ----------------------------------------------------------------------

/// Read the at-most-one row of a result through `read` and run the
/// statement to completion, so a surrounding transaction can commit
/// afterwards. The value is read before the cursor steps on: a libSQL row
/// reads from the statement's current step, so a row held across the next
/// step reads NULL.
async fn drain_single<T>(
    mut rows: libsql::Rows,
    read: impl Fn(&libsql::Row) -> Result<T, StoreError>,
) -> Result<Option<T>, StoreError> {
    let mut first: Option<T> = None;
    while let Some(row) = rows.next().await? {
        assert!(
            first.is_none(),
            "single-row query returned more than one row"
        );
        first = Some(read(&row)?);
    }
    Ok(first)
}

fn row_to_node_record(row: &libsql::Row) -> Result<NodeRecord, StoreError> {
    let node: String = row.get(0)?;
    let endpoints: String = row.get(2)?;
    let capabilities: String = row.get(3)?;
    Ok(NodeRecord {
        id: node.parse().map_err(|e| corrupt(0, e))?,
        display_name: row.get(1)?,
        endpoints: serde_json::from_str::<Vec<Endpoint>>(&endpoints).map_err(|e| corrupt(0, e))?,
        capabilities: serde_json::from_str::<NodeCapabilities>(&capabilities)
            .map_err(|e| corrupt(0, e))?,
    })
}

fn row_to_host_record(row: &libsql::Row) -> Result<HostRecord, StoreError> {
    let host: String = row.get(0)?;
    Ok(HostRecord {
        id: HostId::new(host).map_err(|e| corrupt(0, e))?,
        kind: row.get(1)?,
        display_name: row.get(2)?,
    })
}

fn row_to_stewardship_record(row: &libsql::Row) -> Result<StewardshipRecord, StoreError> {
    let node: String = row.get(0)?;
    let host: String = row.get(1)?;
    let capabilities: String = row.get(2)?;
    let roots: String = row.get(3)?;
    Ok(StewardshipRecord {
        node: node.parse().map_err(|e| corrupt(0, e))?,
        host: HostId::new(host).map_err(|e| corrupt(0, e))?,
        capabilities: serde_json::from_str::<StewardCapabilities>(&capabilities)
            .map_err(|e| corrupt(0, e))?,
        roots: serde_json::from_str::<Vec<String>>(&roots).map_err(|e| corrupt(0, e))?,
    })
}

const STEWARDSHIP_COLUMNS: &str = "node, host, capabilities, roots";

// ----------------------------------------------------------------------
// The store's replication surface
// ----------------------------------------------------------------------

impl IndexStore {
    /// The epoch this store's log runs under, minted when the store was
    /// created or last rebuilt.
    pub fn log_epoch(&self) -> Epoch {
        self.log_epoch
    }

    /// Append a roster record to the local log and materialize it. The
    /// catalog kinds never come through here — the catalog writes log
    /// themselves — so passing one is a programmer error.
    pub async fn publish(&self, record: &Record) -> Result<Sequence, StoreError> {
        assert!(
            !matches!(record, Record::Source { .. } | Record::SourceGone { .. }),
            "catalog records enter the log through the catalog writes"
        );
        record.check_bounds()?;
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        let seq = append_local_in(&tx, self.log_epoch, record).await?;
        let at = EntryPosition {
            origin: LOCAL_ORIGIN,
            epoch: self.log_epoch,
            seq,
        };
        materialize_local_in(&tx, &at, record).await?;
        tx.commit().await?;
        Ok(seq)
    }

    /// The highest position held per origin, the local log reported as
    /// `local`. An origin with no held entries is absent — including the
    /// local one before it has logged anything.
    pub async fn version_vector(&self, local: &NodeId) -> Result<VersionVector, StoreError> {
        let held = held_positions_in(&self.catalog).await?;
        let mut entries = Vec::with_capacity(held.len());
        for position in held {
            entries.push(VectorEntry {
                origin: mapped_origin(local, &position.origin)?,
                epoch: position.epoch,
                seq: position.seq,
            });
        }
        Ok(VersionVector::new(entries))
    }

    /// Every held entry `vector` lacks, ordered by (origin, epoch, seq)
    /// with the local log reported as `local`, at most `limit` of them
    /// (clamped to [`LOG_ENTRIES_PER_BATCH_MAX`]). A peer far behind
    /// catches up over several calls.
    pub async fn log_after(
        &self,
        local: &NodeId,
        vector: &VersionVector,
        limit: usize,
    ) -> Result<Vec<LogEntry>, StoreError> {
        let limit = limit.min(LOG_ENTRIES_PER_BATCH_MAX);
        let mut origins: Vec<(NodeId, String)> = Vec::new();
        for position in held_positions_in(&self.catalog).await? {
            origins.push((mapped_origin(local, &position.origin)?, position.origin));
        }
        origins.sort_by_key(|(origin, _)| *origin);
        let mut out = Vec::new();
        for (origin, stored) in origins {
            let remaining = limit - out.len();
            if remaining == 0 {
                break;
            }
            let known = vector
                .position_of(&origin)
                .unwrap_or((Epoch(0), Sequence(0)));
            entries_after_in(&self.catalog, &stored, origin, known, remaining, &mut out).await?;
        }
        assert!(out.len() <= limit);
        Ok(out)
    }

    /// Hold and materialize a batch of peers' entries in one transaction.
    /// Echoes of the local log and entries already held are skipped; an
    /// entry out of bounds is refused unheld; an entry claiming an
    /// authority its origin lacks is held but not materialized.
    pub async fn apply_remote(
        &self,
        local: &NodeId,
        entries: &[LogEntry],
    ) -> Result<AppliedReport, StoreError> {
        assert!(entries.len() <= LOG_ENTRIES_PER_BATCH_MAX);
        if entries.is_empty() {
            return Ok(AppliedReport::default());
        }
        let _write = self.write().await;
        let tx = self.catalog.transaction().await?;
        let mut positions: HashMap<NodeId, (Epoch, Sequence)> = HashMap::new();
        for position in held_positions_in(&tx).await? {
            if position.origin == LOCAL_ORIGIN {
                continue;
            }
            let origin = mapped_origin(local, &position.origin)?;
            positions.insert(origin, (position.epoch, position.seq));
        }
        let mut report = AppliedReport::default();
        for entry in entries {
            match apply_entry_in(&tx, local, &mut positions, entry).await? {
                Outcome::Applied => report.applied += 1,
                Outcome::Skipped => report.skipped += 1,
                Outcome::Refused => report.refused += 1,
            }
        }
        tx.commit().await?;
        let total = report.applied + report.skipped + report.refused;
        assert_eq!(
            total,
            u64::try_from(entries.len()).expect("a bounded batch fits u64")
        );
        Ok(report)
    }

    /// Every node the roster knows and has not expelled, by id.
    pub async fn roster_nodes(&self) -> Result<Vec<NodeRecord>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT node, display_name, endpoints, capabilities FROM roster_nodes
                 WHERE node NOT IN (SELECT node FROM roster_expulsions)
                 ORDER BY node LIMIT ?1",
                params![i64::from(ROSTER_ROWS_MAX)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_node_record(&row)?);
        }
        Ok(out)
    }

    pub async fn roster_hosts(&self) -> Result<Vec<HostRecord>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT host, kind, display_name FROM roster_hosts ORDER BY host LIMIT ?1",
                params![i64::from(ROSTER_ROWS_MAX)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_host_record(&row)?);
        }
        Ok(out)
    }

    pub async fn roster_stewardships(&self) -> Result<Vec<StewardshipRecord>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "SELECT {STEWARDSHIP_COLUMNS} FROM roster_stewardships
                     ORDER BY host, node LIMIT ?1"
                ),
                params![i64::from(ROSTER_ROWS_MAX)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_stewardship_record(&row)?);
        }
        Ok(out)
    }

    /// The stewardship claims on one host, by node.
    pub async fn stewards_of(&self, host: &HostId) -> Result<Vec<StewardshipRecord>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                &format!(
                    "SELECT {STEWARDSHIP_COLUMNS} FROM roster_stewardships
                     WHERE host = ?1 ORDER BY node LIMIT ?2"
                ),
                params![host.as_str(), i64::from(ROSTER_ROWS_MAX)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_stewardship_record(&row)?);
        }
        Ok(out)
    }

    pub async fn expelled(&self) -> Result<Vec<NodeId>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT node FROM roster_expulsions ORDER BY node LIMIT ?1",
                params![i64::from(ROSTER_ROWS_MAX)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let node: String = row.get(0)?;
            out.push(node.parse().map_err(|e| corrupt(0, e))?);
        }
        Ok(out)
    }

    /// How many entries each held log has, the local one reported as
    /// `local` — what a status surface shows.
    pub async fn log_counts(&self, local: &NodeId) -> Result<Vec<LogCount>, StoreError> {
        let mut rows = self
            .catalog
            .query(
                "SELECT origin, COUNT(*) FROM sync_log GROUP BY origin ORDER BY origin LIMIT ?1",
                params![i64::from(ROSTER_ROWS_MAX)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let origin: String = row.get(0)?;
            let entries: i64 = row.get(1)?;
            out.push(LogCount {
                origin: mapped_origin(local, &origin)?,
                entries: u64::try_from(entries).expect("row counts are non-negative"),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::tests::{addr, envelope};
    use super::*;
    use crate::address::Timestamp;

    async fn open(dir: &Path) -> IndexStore {
        IndexStore::open(dir).await.expect("opens")
    }

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    fn node_record(id: NodeId, name: &str) -> NodeRecord {
        NodeRecord {
            id,
            display_name: name.to_string(),
            endpoints: vec![Endpoint::new("relay:https://relay.example").expect("valid")],
            capabilities: NodeCapabilities {
                always_on: true,
                deep_index: false,
                relays: true,
            },
        }
    }

    fn host_record(host: &str, name: &str) -> HostRecord {
        HostRecord {
            id: HostId::new(host).expect("valid"),
            kind: "fs".to_string(),
            display_name: name.to_string(),
        }
    }

    fn stewardship(node: NodeId, host: &str) -> StewardshipRecord {
        StewardshipRecord {
            node,
            host: HostId::new(host).expect("valid"),
            capabilities: StewardCapabilities {
                enumerates: true,
                change_feed: false,
                writable: false,
            },
            roots: vec!["notes".to_string()],
        }
    }

    fn entry(origin: NodeId, epoch: u64, seq: u64, record: Record) -> LogEntry {
        LogEntry {
            origin,
            epoch: Epoch(epoch),
            seq: Sequence(seq),
            record,
        }
    }

    fn source_record(address: &str, modified: i64) -> Record {
        Record::Source {
            address: addr(address),
            envelope: envelope(modified, 10),
            raw_bytes: 10,
        }
    }

    /// Everything the store holds, as a peer with an empty vector sees it.
    async fn whole_log(s: &IndexStore, local: &NodeId) -> Vec<LogEntry> {
        s.log_after(local, &VersionVector::default(), LOG_ENTRIES_PER_BATCH_MAX)
            .await
            .expect("lists")
    }

    fn local_position(vector: &VersionVector, local: &NodeId) -> Option<(Epoch, Sequence)> {
        vector.position_of(local)
    }

    #[tokio::test]
    async fn local_upsert_logs_the_source_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let a = addr("inseam://fs-test/notes/a.md");
        s.upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let log = whole_log(&s, &local).await;
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].origin, local);
        assert_eq!(log[0].epoch, s.log_epoch());
        assert_eq!(log[0].seq, Sequence(1));
        assert_eq!(
            log[0].record,
            source_record("inseam://fs-test/notes/a.md", 1)
        );
        let stored = s.source_by_address(&a).await.expect("ok").expect("present");
        assert_eq!(
            stored.origin, None,
            "the catalog's own rows carry no origin"
        );
    }

    #[tokio::test]
    async fn unchanged_reupsert_does_not_relog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let a = addr("inseam://fs-test/notes/a.md");
        s.upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let mut seen_again = envelope(1, 10);
        seen_again.observed = Timestamp(1_800_000_000);
        s.upsert_source(&a, &seen_again, 10).await.expect("upserts");
        let log = whole_log(&s, &local).await;
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].seq, Sequence(1));
        let vector = s.version_vector(&local).await.expect("reads");
        assert_eq!(
            local_position(&vector, &local),
            Some((s.log_epoch(), Sequence(1)))
        );
    }

    #[tokio::test]
    async fn changed_upsert_compacts_to_one_entry_at_a_higher_sequence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let a = addr("inseam://fs-test/notes/a.md");
        s.upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        s.upsert_source(&a, &envelope(2, 10), 10)
            .await
            .expect("upserts");
        let log = whole_log(&s, &local).await;
        assert_eq!(
            log.len(),
            1,
            "the earlier entry under the key is compacted away"
        );
        assert_eq!(log[0].seq, Sequence(2));
        assert_eq!(
            log[0].record,
            source_record("inseam://fs-test/notes/a.md", 2)
        );
        // A size change alone is news too: the record carries raw_bytes.
        s.upsert_source(&a, &envelope(2, 10), 11)
            .await
            .expect("upserts");
        assert_eq!(whole_log(&s, &local).await[0].seq, Sequence(3));
    }

    #[tokio::test]
    async fn delete_logs_a_tombstone_that_survives_compaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let a = addr("inseam://fs-test/notes/a.md");
        let b = addr("inseam://fs-test/notes/b.md");
        let sid = s
            .upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        s.upsert_source(&b, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        s.delete_source(sid).await.expect("deletes");
        let log = whole_log(&s, &local).await;
        assert_eq!(log.len(), 2);
        let gone = log
            .iter()
            .find(|e| e.record.is_tombstone())
            .expect("a tombstone");
        assert_eq!(gone.seq, Sequence(3));
        assert_eq!(gone.record, Record::SourceGone { address: a.clone() });
        assert!(s.source_by_address(&a).await.expect("ok").is_none());

        // The tombstone stands until a later entry under the key replaces it.
        s.upsert_source(&a, &envelope(5, 10), 10)
            .await
            .expect("upserts");
        let log = whole_log(&s, &local).await;
        assert_eq!(log.len(), 2);
        assert!(log.iter().all(|e| !e.record.is_tombstone()));
        assert!(log.iter().any(|e| e.seq == Sequence(4)));
        assert!(
            s.delete_source(SourceId(9_999)).await.is_ok(),
            "a missing row is no news"
        );
        assert_eq!(whole_log(&s, &local).await.len(), 2);
    }

    #[tokio::test]
    async fn version_vector_maps_the_local_log_to_the_given_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let empty = s.version_vector(&local).await.expect("reads");
        assert!(
            empty.entries().is_empty(),
            "an empty local log is not reported"
        );
        s.upsert_source(&addr("inseam://fs-test/a.md"), &envelope(1, 10), 10)
            .await
            .expect("upserts");
        s.upsert_source(&addr("inseam://fs-test/b.md"), &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let vector = s.version_vector(&local).await.expect("reads");
        assert_eq!(vector.entries().len(), 1);
        assert_eq!(
            vector.position_of(&local),
            Some((s.log_epoch(), Sequence(2)))
        );
        // The same store answers another id the same way: the store has no id.
        let other = s.version_vector(&node(2)).await.expect("reads");
        assert_eq!(
            other.position_of(&node(2)),
            Some((s.log_epoch(), Sequence(2)))
        );
        assert_eq!(
            s.log_counts(&local).await.expect("counts"),
            vec![LogCount {
                origin: local,
                entries: 2
            }]
        );
    }

    #[tokio::test]
    async fn log_after_honors_the_vector_the_limit_and_epochs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        let entries = vec![
            entry(peer, 1, 1, source_record("inseam://fs-peer/a.md", 1)),
            entry(peer, 1, 2, source_record("inseam://fs-peer/b.md", 1)),
            entry(peer, 1, 3, source_record("inseam://fs-peer/c.md", 1)),
        ];
        let report = s.apply_remote(&local, &entries).await.expect("applies");
        assert_eq!(
            report,
            AppliedReport {
                applied: 3,
                skipped: 0,
                refused: 0
            }
        );

        let behind = VersionVector::new(vec![VectorEntry {
            origin: peer,
            epoch: Epoch(1),
            seq: Sequence(1),
        }]);
        let after = s.log_after(&local, &behind, 10).await.expect("lists");
        assert_eq!(
            after.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![Sequence(2), Sequence(3)]
        );
        let limited = s.log_after(&local, &behind, 1).await.expect("lists");
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].seq, Sequence(2));

        let ahead = VersionVector::new(vec![VectorEntry {
            origin: peer,
            epoch: Epoch(2),
            seq: Sequence(1),
        }]);
        assert!(
            s.log_after(&local, &ahead, 10)
                .await
                .expect("lists")
                .is_empty(),
            "an older epoch is not news"
        );
        let older = VersionVector::new(vec![VectorEntry {
            origin: peer,
            epoch: Epoch(0),
            seq: Sequence(50),
        }]);
        assert_eq!(
            s.log_after(&local, &older, 10).await.expect("lists").len(),
            3,
            "a newer epoch is all news"
        );
        let caught_up = s.version_vector(&local).await.expect("reads");
        assert!(
            s.log_after(&local, &caught_up, 10)
                .await
                .expect("lists")
                .is_empty()
        );
    }

    /// One peer's opening batch: its node, a host, a stewardship, a source.
    fn peer_batch(peer: NodeId) -> Vec<LogEntry> {
        vec![
            entry(peer, 7, 1, Record::Node(node_record(peer, "mini"))),
            entry(
                peer,
                7,
                2,
                Record::Host(host_record("fs-peer", "Peer's disk")),
            ),
            entry(
                peer,
                7,
                3,
                Record::Stewardship(stewardship(peer, "fs-peer")),
            ),
            entry(peer, 7, 4, source_record("inseam://fs-peer/a.md", 1)),
        ]
    }

    #[tokio::test]
    async fn apply_remote_materializes_sources_and_roster_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        let report = s
            .apply_remote(&local, &peer_batch(peer))
            .await
            .expect("applies");
        assert_eq!(
            report,
            AppliedReport {
                applied: 4,
                skipped: 0,
                refused: 0
            }
        );

        assert_eq!(
            s.roster_nodes().await.expect("reads"),
            vec![node_record(peer, "mini")]
        );
        assert_eq!(
            s.roster_hosts().await.expect("reads"),
            vec![host_record("fs-peer", "Peer's disk")]
        );
        assert_eq!(
            s.roster_stewardships().await.expect("reads"),
            vec![stewardship(peer, "fs-peer")]
        );
        let host = HostId::new("fs-peer").expect("valid");
        assert_eq!(s.stewards_of(&host).await.expect("reads").len(), 1);
        assert!(
            s.stewards_of(&HostId::new("fs-none").expect("valid"))
                .await
                .expect("reads")
                .is_empty()
        );

        let stored = s
            .source_by_address(&addr("inseam://fs-peer/a.md"))
            .await
            .expect("ok")
            .expect("present");
        assert_eq!(stored.origin, Some(peer));
        assert!(stored.root_fragment.is_none());
        let meta = s
            .index_meta(&addr("inseam://fs-peer/a.md"))
            .await
            .expect("ok")
            .expect("present");
        assert!(!meta.indexed);
        assert_eq!(s.stats().await.expect("ok").remote_sources, 1);
        let rows = s
            .catalog_rows(None, super::super::CatalogSelection::All, 10)
            .await
            .expect("lists");
        assert_eq!(rows[0].origin, Some(peer));
    }

    #[tokio::test]
    async fn apply_remote_holds_entries_under_their_origins_epoch_and_skips_repeats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        s.apply_remote(&local, &peer_batch(peer))
            .await
            .expect("applies");

        // Held under the peer's epoch, and re-shipped as such.
        let vector = s.version_vector(&local).await.expect("reads");
        assert_eq!(vector.position_of(&peer), Some((Epoch(7), Sequence(4))));
        assert_eq!(whole_log(&s, &local).await.len(), 4);
        assert_eq!(
            s.log_counts(&local).await.expect("counts"),
            vec![LogCount {
                origin: peer,
                entries: 4
            }]
        );

        // Applying the batch again is a no-op, and so is an empty batch.
        let again = s
            .apply_remote(&local, &peer_batch(peer))
            .await
            .expect("applies");
        assert_eq!(
            again,
            AppliedReport {
                applied: 0,
                skipped: 4,
                refused: 0
            }
        );
        assert_eq!(
            s.apply_remote(&local, &[]).await.expect("applies"),
            AppliedReport::default()
        );
    }

    #[tokio::test]
    async fn apply_remote_withdraws_a_stewardship_from_its_own_node_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        let other = node(3);
        let host = HostId::new("fs-peer").expect("valid");
        s.apply_remote(
            &local,
            &[entry(
                peer,
                1,
                1,
                Record::Stewardship(stewardship(peer, "fs-peer")),
            )],
        )
        .await
        .expect("applies");
        let forged = entry(
            other,
            1,
            1,
            Record::StewardshipWithdrawn {
                node: peer,
                host: host.clone(),
            },
        );
        let report = s.apply_remote(&local, &[forged]).await.expect("applies");
        assert_eq!(report.refused, 1);
        assert_eq!(s.stewards_of(&host).await.expect("reads").len(), 1);
        let forged_claim = entry(
            other,
            1,
            2,
            Record::Stewardship(stewardship(peer, "fs-peer")),
        );
        assert_eq!(
            s.apply_remote(&local, &[forged_claim])
                .await
                .expect("applies")
                .refused,
            1
        );

        let genuine = entry(
            peer,
            1,
            2,
            Record::StewardshipWithdrawn {
                node: peer,
                host: host.clone(),
            },
        );
        let report = s.apply_remote(&local, &[genuine]).await.expect("applies");
        assert_eq!(report.applied, 1);
        assert!(s.stewards_of(&host).await.expect("reads").is_empty());
    }

    #[tokio::test]
    async fn apply_remote_skips_echoes_of_the_local_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let echo = entry(local, 1, 1, source_record("inseam://fs-test/a.md", 1));
        let report = s.apply_remote(&local, &[echo]).await.expect("applies");
        assert_eq!(
            report,
            AppliedReport {
                applied: 0,
                skipped: 1,
                refused: 0
            }
        );
        assert!(
            s.source_by_address(&addr("inseam://fs-test/a.md"))
                .await
                .expect("ok")
                .is_none()
        );
        assert!(
            s.version_vector(&local)
                .await
                .expect("reads")
                .entries()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn apply_remote_never_overwrites_a_local_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        let a = addr("inseam://fs-shared/a.md");
        s.upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        let remote = entry(peer, 1, 1, source_record("inseam://fs-shared/a.md", 99));
        let report = s.apply_remote(&local, &[remote]).await.expect("applies");
        assert_eq!(
            report.applied, 1,
            "the entry is held even though the row stands"
        );
        let stored = s.source_by_address(&a).await.expect("ok").expect("present");
        assert_eq!(stored.origin, None);
        assert_eq!(stored.envelope.modified, Some(Timestamp(1)));
        assert_eq!(
            s.version_vector(&local)
                .await
                .expect("reads")
                .position_of(&peer),
            Some((Epoch(1), Sequence(1)))
        );

        // The reverse: a local upsert takes a remote row over, and logs it.
        let b = addr("inseam://fs-shared/b.md");
        s.apply_remote(
            &local,
            &[entry(
                peer,
                1,
                2,
                source_record("inseam://fs-shared/b.md", 1),
            )],
        )
        .await
        .expect("applies");
        assert_eq!(
            s.source_by_address(&b)
                .await
                .expect("ok")
                .expect("present")
                .origin,
            Some(peer)
        );
        s.upsert_source(&b, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        assert_eq!(
            s.source_by_address(&b)
                .await
                .expect("ok")
                .expect("present")
                .origin,
            None
        );
        let local_entries: Vec<_> = whole_log(&s, &local)
            .await
            .into_iter()
            .filter(|e| e.origin == local)
            .collect();
        assert_eq!(local_entries.len(), 2);
    }

    #[tokio::test]
    async fn a_tombstone_deletes_only_its_origins_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        let other = node(3);
        let a = addr("inseam://fs-shared/a.md");
        s.upsert_source(&a, &envelope(1, 10), 10)
            .await
            .expect("upserts");
        s.apply_remote(
            &local,
            &[entry(peer, 1, 1, Record::SourceGone { address: a.clone() })],
        )
        .await
        .expect("applies");
        assert!(
            s.source_by_address(&a).await.expect("ok").is_some(),
            "a local row is not a peer's to delete"
        );

        let b = addr("inseam://fs-shared/b.md");
        s.apply_remote(
            &local,
            &[entry(
                peer,
                1,
                2,
                source_record("inseam://fs-shared/b.md", 1),
            )],
        )
        .await
        .expect("applies");
        s.apply_remote(
            &local,
            &[entry(
                other,
                1,
                1,
                Record::SourceGone { address: b.clone() },
            )],
        )
        .await
        .expect("applies");
        assert!(
            s.source_by_address(&b).await.expect("ok").is_some(),
            "another steward's row is not its to delete"
        );
        s.apply_remote(
            &local,
            &[entry(peer, 1, 3, Record::SourceGone { address: b.clone() })],
        )
        .await
        .expect("applies");
        assert!(s.source_by_address(&b).await.expect("ok").is_none());
        // The tombstone compacted the peer's earlier entry under the key.
        let peer_entries: Vec<_> = whole_log(&s, &local)
            .await
            .into_iter()
            .filter(|e| e.origin == peer)
            .collect();
        assert_eq!(peer_entries.len(), 2);
    }

    #[tokio::test]
    async fn a_newer_epoch_purges_the_old_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        let old = vec![
            entry(peer, 1, 1, Record::Node(node_record(peer, "before"))),
            entry(peer, 1, 2, source_record("inseam://fs-peer/old.md", 1)),
            entry(
                peer,
                1,
                3,
                Record::Stewardship(stewardship(peer, "fs-peer")),
            ),
        ];
        s.apply_remote(&local, &old).await.expect("applies");
        let rebuilt = entry(peer, 2, 1, Record::Node(node_record(peer, "after")));
        let report = s.apply_remote(&local, &[rebuilt]).await.expect("applies");
        assert_eq!(report.applied, 1);
        assert!(
            s.source_by_address(&addr("inseam://fs-peer/old.md"))
                .await
                .expect("ok")
                .is_none()
        );
        assert!(s.roster_stewardships().await.expect("reads").is_empty());
        assert_eq!(
            s.roster_nodes().await.expect("reads"),
            vec![node_record(peer, "after")]
        );
        let log = whole_log(&s, &local).await;
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].epoch, Epoch(2));
        // The old epoch's entries are no longer news, whatever their sequence.
        let stale = entry(peer, 1, 9, source_record("inseam://fs-peer/stale.md", 1));
        assert_eq!(
            s.apply_remote(&local, &[stale])
                .await
                .expect("applies")
                .skipped,
            1
        );
    }

    #[tokio::test]
    async fn a_remote_expulsion_purges_the_expelled_nodes_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let expeller = node(2);
        let expelled = node(3);
        let survivor = node(4);
        s.apply_remote(
            &local,
            &[
                entry(expelled, 1, 1, Record::Node(node_record(expelled, "lost"))),
                entry(expelled, 1, 2, source_record("inseam://fs-lost/a.md", 1)),
                entry(
                    expelled,
                    1,
                    3,
                    Record::Stewardship(stewardship(expelled, "fs-lost")),
                ),
                entry(
                    expelled,
                    1,
                    4,
                    Record::Host(host_record("fs-lost", "Lost disk")),
                ),
                entry(survivor, 1, 1, Record::Node(node_record(survivor, "kept"))),
            ],
        )
        .await
        .expect("applies");
        let order = entry(expeller, 1, 1, Record::Expulsion { node: expelled });
        assert_eq!(
            s.apply_remote(&local, &[order])
                .await
                .expect("applies")
                .applied,
            1
        );

        assert_eq!(s.expelled().await.expect("reads"), vec![expelled]);
        assert_eq!(
            s.roster_nodes().await.expect("reads"),
            vec![node_record(survivor, "kept")]
        );
        assert!(s.roster_hosts().await.expect("reads").is_empty());
        assert!(s.roster_stewardships().await.expect("reads").is_empty());
        assert!(
            s.source_by_address(&addr("inseam://fs-lost/a.md"))
                .await
                .expect("ok")
                .is_none()
        );
        let vector = s.version_vector(&local).await.expect("reads");
        assert_eq!(vector.position_of(&expelled), None, "its log is dropped");
        assert_eq!(vector.position_of(&survivor), Some((Epoch(1), Sequence(1))));

        // The node's record comes back through sync: still listed as expelled,
        // still hidden from the roster.
        let returns = entry(expelled, 2, 1, Record::Node(node_record(expelled, "back")));
        s.apply_remote(&local, &[returns]).await.expect("applies");
        assert_eq!(s.roster_nodes().await.expect("reads").len(), 1);
    }

    #[tokio::test]
    async fn an_expulsion_of_the_local_node_is_recorded_without_a_purge() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let expeller = node(2);
        s.upsert_source(&addr("inseam://fs-test/a.md"), &envelope(1, 10), 10)
            .await
            .expect("upserts");
        s.publish(&Record::Node(node_record(local, "me")))
            .await
            .expect("publishes");
        let order = entry(expeller, 1, 1, Record::Expulsion { node: local });
        assert_eq!(
            s.apply_remote(&local, &[order])
                .await
                .expect("applies")
                .applied,
            1
        );
        assert_eq!(s.expelled().await.expect("reads"), vec![local]);
        assert!(
            s.source_by_address(&addr("inseam://fs-test/a.md"))
                .await
                .expect("ok")
                .is_some()
        );
        assert_eq!(
            whole_log(&s, &local).await.len(),
            3,
            "the local log is intact"
        );
        assert!(
            s.roster_nodes().await.expect("reads").is_empty(),
            "but hidden from the roster"
        );
    }

    #[tokio::test]
    async fn publish_materializes_roster_records_and_compacts_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let host = HostId::new("fs-mine").expect("valid");
        assert_eq!(
            s.publish(&Record::Node(node_record(local, "me")))
                .await
                .expect("publishes"),
            Sequence(1)
        );
        assert_eq!(
            s.publish(&Record::Host(host_record("fs-mine", "My disk")))
                .await
                .expect("publishes"),
            Sequence(2)
        );
        assert_eq!(
            s.publish(&Record::Stewardship(stewardship(local, "fs-mine")))
                .await
                .expect("publishes"),
            Sequence(3)
        );
        assert_eq!(
            s.roster_nodes().await.expect("reads"),
            vec![node_record(local, "me")]
        );
        assert_eq!(s.roster_hosts().await.expect("reads").len(), 1);
        assert_eq!(s.stewards_of(&host).await.expect("reads").len(), 1);

        // Republishing the node compacts its earlier entry.
        s.publish(&Record::Node(node_record(local, "renamed")))
            .await
            .expect("publishes");
        let log = whole_log(&s, &local).await;
        assert_eq!(log.len(), 3);
        assert_eq!(
            s.roster_nodes().await.expect("reads")[0].display_name,
            "renamed"
        );

        s.publish(&Record::StewardshipWithdrawn {
            node: local,
            host: host.clone(),
        })
        .await
        .expect("publishes");
        assert!(s.stewards_of(&host).await.expect("reads").is_empty());
        assert!(
            whole_log(&s, &local)
                .await
                .iter()
                .any(|e| e.record.is_tombstone())
        );
    }

    #[tokio::test]
    async fn publish_expulsion_purges_and_bounds_are_checked_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        s.apply_remote(
            &local,
            &[
                entry(peer, 1, 1, Record::Node(node_record(peer, "peer"))),
                entry(peer, 1, 2, source_record("inseam://fs-peer/a.md", 1)),
            ],
        )
        .await
        .expect("applies");
        s.publish(&Record::Expulsion { node: peer })
            .await
            .expect("publishes");
        assert_eq!(s.expelled().await.expect("reads"), vec![peer]);
        assert!(s.roster_nodes().await.expect("reads").is_empty());
        assert!(
            s.source_by_address(&addr("inseam://fs-peer/a.md"))
                .await
                .expect("ok")
                .is_none()
        );
        assert!(
            s.version_vector(&local)
                .await
                .expect("reads")
                .position_of(&peer)
                .is_none()
        );

        let too_long = NodeRecord {
            display_name: "n".repeat(crate::network::DISPLAY_NAME_CHARS_MAX + 1),
            ..node_record(local, "me")
        };
        assert!(matches!(
            s.publish(&Record::Node(too_long)).await,
            Err(StoreError::RecordOutOfBounds(_))
        ));
        // Refused before the log: the sequence did not advance.
        assert_eq!(
            s.publish(&Record::Host(host_record("fs-mine", "disk")))
                .await
                .expect("publishes"),
            Sequence(2)
        );
    }

    #[tokio::test]
    async fn a_node_record_from_a_foreign_origin_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let forger = node(2);
        let victim = node(3);
        let forged = entry(forger, 1, 1, Record::Node(node_record(victim, "forged")));
        let report = s.apply_remote(&local, &[forged]).await.expect("applies");
        assert_eq!(
            report,
            AppliedReport {
                applied: 0,
                skipped: 0,
                refused: 1
            }
        );
        assert!(s.roster_nodes().await.expect("reads").is_empty());
        // Held, so the forger's vector position moves past it and the peer
        // stops re-shipping it.
        assert_eq!(
            s.version_vector(&local)
                .await
                .expect("reads")
                .position_of(&forger),
            Some((Epoch(1), Sequence(1)))
        );

        // Out of bounds is refused unheld.
        let oversized = entry(
            forger,
            1,
            2,
            Record::Node(NodeRecord {
                display_name: "n".repeat(crate::network::DISPLAY_NAME_CHARS_MAX + 1),
                ..node_record(forger, "x")
            }),
        );
        assert_eq!(
            s.apply_remote(&local, &[oversized])
                .await
                .expect("applies")
                .refused,
            1
        );
        assert_eq!(
            s.version_vector(&local)
                .await
                .expect("reads")
                .position_of(&forger),
            Some((Epoch(1), Sequence(1)))
        );
        let unstorable = entry(forger, u64::MAX, 3, Record::Node(node_record(forger, "x")));
        assert_eq!(
            s.apply_remote(&local, &[unstorable])
                .await
                .expect("applies")
                .refused,
            1
        );
    }

    #[tokio::test]
    async fn sources_of_host_lists_local_rows_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open(dir.path()).await;
        let local = node(1);
        let peer = node(2);
        let host = HostId::new("fs-shared").expect("valid");
        s.upsert_source(&addr("inseam://fs-shared/mine.md"), &envelope(1, 10), 10)
            .await
            .expect("upserts");
        s.apply_remote(
            &local,
            &[entry(
                peer,
                1,
                1,
                source_record("inseam://fs-shared/theirs.md", 1),
            )],
        )
        .await
        .expect("applies");
        let rows = s.sources_of_host(&host).await.expect("lists");
        assert_eq!(
            rows.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(),
            vec!["mine.md"]
        );
        assert_eq!(s.stats().await.expect("ok").sources, 2);
        assert_eq!(s.stats().await.expect("ok").remote_sources, 1);
    }

    #[tokio::test]
    async fn schema_converge_from_version_7_rebuilds_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local = node(1);
        let before = {
            let s = open(dir.path()).await;
            s.upsert_source(&addr("inseam://fs-test/a.md"), &envelope(1, 10), 10)
                .await
                .expect("upserts");
            s.publish(&Record::Node(node_record(local, "me")))
                .await
                .expect("publishes");
            s.catalog
                .execute(
                    "UPDATE meta SET value = '7' WHERE key = 'schema_version'",
                    (),
                )
                .await
                .expect("ages the schema");
            s.log_epoch()
        };
        let s = open(dir.path()).await;
        assert!(
            s.log_epoch() > before,
            "a rebuilt log runs under a later epoch"
        );
        assert!(
            s.source_by_address(&addr("inseam://fs-test/a.md"))
                .await
                .expect("ok")
                .is_none()
        );
        assert!(s.roster_nodes().await.expect("reads").is_empty());
        assert!(
            s.version_vector(&local)
                .await
                .expect("reads")
                .entries()
                .is_empty()
        );
        // The fresh log starts at one again.
        assert_eq!(
            s.publish(&Record::Node(node_record(local, "me")))
                .await
                .expect("publishes"),
            Sequence(1)
        );

        // Reopening a current store keeps its epoch and its sequence.
        let epoch = s.log_epoch();
        drop(s);
        let s = open(dir.path()).await;
        assert_eq!(s.log_epoch(), epoch);
        assert_eq!(
            s.publish(&Record::Host(host_record("fs-test", "disk")))
                .await
                .expect("publishes"),
            Sequence(2)
        );
    }
}
