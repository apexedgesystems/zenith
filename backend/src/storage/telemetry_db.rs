//! SQLite telemetry storage.
//!
//! Stores telemetry samples in a SQLite database with WAL mode for
//! concurrent read/write. Provides time-range queries and CSV export.
//!
//! Performance tuning:
//!   - WAL mode for non-blocking reads during writes
//!   - Prepared statement caching for hot paths
//!   - Normalized channel dictionary: telemetry rows carry an integer
//!     channel_id instead of repeating target/channel strings, roughly
//!     halving on-disk bytes per sample and index size
//!   - Lookup index on (channel_id, timestamp_ms DESC)
//!   - Separate latest-value table for O(1) latest queries
//!   - Batch inserts within a single transaction
//!   - page_size=4096, mmap_size=256MB for large dataset performance

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};

use crate::core::telemetry::TelemetrySample;

/* ----------------------------- Error ----------------------------- */

/// Storage layer error type. Wraps rusqlite errors and a poisoned-lock
/// case from the writer/reader Mutex paths.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("lock poisoned")]
    Lock,
}

/* ----------------------------- TelemetryDb ----------------------------- */

/// Telemetry storage with WAL-mode SQLite.
///
/// **Concurrency model:**
/// - Single writer connection (`writer`) protected by `Mutex<Connection>`.
///   All write methods (`insert_batch`, `delete_oldest`, `prune`,
///   `delete_target`, `downsample`, layout save/delete, `incremental_vacuum`)
///   acquire it.
/// - Read connection pool (`readers`) -- a `Mutex<Vec<Connection>>` of up
///   to `READ_POOL_MAX` connections. Read methods (`query_range`,
///   `query_latest`, `count`, `target_stats`, `get_layouts`) borrow a
///   connection, run their query, return it. Multiple read methods can
///   run truly concurrently because WAL mode supports concurrent readers.
/// - The mutex around the pool is held only for the borrow/return -- the
///   query itself runs without any zenith-side lock contention. A
///   single shared connection would serialize every read behind every
///   write (with 4 targets x 50 channel polls per cycle, that is 200
///   serial round trips per cycle), which is why reads never touch the
///   writer connection.
pub struct TelemetryDb {
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    db_path: PathBuf,
    /// (target_id, channel name) -> channels.id, consulted on the
    /// insert path so steady-state batches bind integer ids without
    /// touching the channels table. Entries are only added after the
    /// resolving transaction commits -- caching an id from a rolled-
    /// back transaction would leave rows pointing at a dead id.
    /// Dictionary rows are never deleted (see delete_target), so a
    /// cached id can never go stale.
    channel_ids: Mutex<HashMap<ChannelKey, i64>>,
}

/// Insert-path cache key: (target_id, channel name).
type ChannelKey = (Arc<str>, Arc<str>);

/// Maximum number of concurrent reader connections in the pool.
/// Connections beyond this are closed when returned to the pool.
const READ_POOL_MAX: usize = 8;

impl TelemetryDb {
    /// Open or create the telemetry database.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let writer = Connection::open(path)?;
        Self::tune(&writer)?;

        // Integrity check on startup (fast, catches corruption early)
        let integrity: String = writer
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap_or_else(|_| "unknown".to_string());
        if integrity != "ok" {
            tracing::warn!("Database integrity check: {}", integrity);
        }

        // Channel dictionary: one row per (target, channel) ever seen.
        // Telemetry rows reference it by integer id -- measured at 1M
        // rows this halves bytes/row (150 -> 76) vs repeating the
        // strings, because the name would otherwise be stored twice
        // per sample (row + lookup index).
        writer.execute_batch(
            "CREATE TABLE IF NOT EXISTS channels (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                target_id TEXT NOT NULL,
                name TEXT NOT NULL,
                UNIQUE(target_id, name)
            );",
        )?;

        // Rebuild a legacy denormalized table (target_id/channel TEXT on
        // every row) into the dictionary schema before creating the new
        // table -- the legacy layout is detected by its 'channel' column.
        Self::migrate_legacy_telemetry(&writer)?;

        // Main telemetry table. tier 0 = full resolution; higher tiers
        // are age-downsampled envelope buckets where value is the mean
        // and v_min/v_max/agg_count carry the spread. On full-res rows
        // the extra columns cost one record-header byte each (NULL and
        // the integer 0 store no data bytes).
        writer.execute_batch(
            "CREATE TABLE IF NOT EXISTS telemetry (
                id INTEGER PRIMARY KEY,
                channel_id INTEGER NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                value REAL NOT NULL,
                tier INTEGER NOT NULL DEFAULT 0,
                v_min REAL,
                v_max REAL,
                agg_count INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_telemetry_chan_ts
                ON telemetry (channel_id, timestamp_ms DESC);
            CREATE INDEX IF NOT EXISTS idx_telemetry_ts
                ON telemetry (timestamp_ms ASC);",
        )?;

        // A dictionary-schema table from before the tier columns gets
        // them added in place (cheap: no rebuild, existing rows read
        // as tier 0 with NULL envelope).
        let has_tier: i64 = writer.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('telemetry') WHERE name = 'tier'",
            [],
            |row| row.get(0),
        )?;
        if has_tier == 0 {
            writer.execute_batch(
                "ALTER TABLE telemetry ADD COLUMN tier INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE telemetry ADD COLUMN v_min REAL;
                 ALTER TABLE telemetry ADD COLUMN v_max REAL;
                 ALTER TABLE telemetry ADD COLUMN agg_count INTEGER;",
            )?;
        }

        // Separate table for latest values per channel (fast O(1) lookup).
        // Updated on each insert batch via UPSERT.
        writer.execute_batch(
            "CREATE TABLE IF NOT EXISTS telemetry_latest (
                target_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                value REAL NOT NULL,
                PRIMARY KEY (target_id, channel)
            );",
        )?;

        // Layout persistence tables
        writer.execute_batch(
            "CREATE TABLE IF NOT EXISTS telemetry_layouts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                target_id TEXT NOT NULL,
                name TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT 'user',
                grid TEXT NOT NULL DEFAULT '1x1',
                time_window_s INTEGER DEFAULT 30,
                created_at INTEGER NOT NULL,
                UNIQUE(target_id, name)
            );
            CREATE TABLE IF NOT EXISTS telemetry_plots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                layout_id INTEGER NOT NULL REFERENCES telemetry_layouts(id) ON DELETE CASCADE,
                title TEXT NOT NULL,
                channels TEXT NOT NULL,
                height INTEGER NOT NULL DEFAULT 180,
                position_row INTEGER NOT NULL DEFAULT 0,
                position_col INTEGER NOT NULL DEFAULT 0,
                y_min REAL,
                y_max REAL,
                y_label TEXT,
                thresholds TEXT
            );",
        )?;

        // Audit log: append-only record of operator actions for compliance
        // and post-incident review. Read-only via API; new rows are added
        // by log_audit(). No automatic pruning -- operators are expected
        // to archive and rotate this table out of band.
        writer.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_ms INTEGER NOT NULL,
                actor TEXT NOT NULL,
                action TEXT NOT NULL,
                target_id TEXT,
                detail TEXT,
                status TEXT NOT NULL,
                source_ip TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log (ts_ms DESC);
            CREATE INDEX IF NOT EXISTS idx_audit_target ON audit_log (target_id, ts_ms DESC);",
        )?;

        tracing::info!(
            "Telemetry database opened: {} (read pool max {})",
            path.display(),
            READ_POOL_MAX
        );
        Ok(Self {
            writer: Mutex::new(writer),
            readers: Mutex::new(Vec::with_capacity(READ_POOL_MAX)),
            db_path: path.to_path_buf(),
            channel_ids: Mutex::new(HashMap::new()),
        })
    }

    /// One-shot rebuild of a legacy telemetry table (TEXT target_id and
    /// channel on every row) into the channel-dictionary schema. Runs
    /// inside a transaction: either the whole history arrives in the
    /// new shape or the legacy table is left untouched. Indexes are
    /// created by open() after the copy, which is faster than
    /// maintaining them during the bulk INSERT.
    fn migrate_legacy_telemetry(conn: &Connection) -> Result<(), DbError> {
        let legacy: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('telemetry') WHERE name = 'channel'",
            [],
            |row| row.get(0),
        )?;
        if legacy == 0 {
            return Ok(());
        }

        let started = std::time::Instant::now();
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE telemetry RENAME TO telemetry_legacy;
             INSERT OR IGNORE INTO channels (target_id, name)
                 SELECT DISTINCT target_id, channel FROM telemetry_legacy;
             CREATE TABLE telemetry_v2 (
                 id INTEGER PRIMARY KEY,
                 channel_id INTEGER NOT NULL,
                 timestamp_ms INTEGER NOT NULL,
                 value REAL NOT NULL
             );
             INSERT INTO telemetry_v2 (channel_id, timestamp_ms, value)
                 SELECT c.id, o.timestamp_ms, o.value
                 FROM telemetry_legacy o
                 JOIN channels c ON c.target_id = o.target_id AND c.name = o.channel;
             DROP TABLE telemetry_legacy;
             ALTER TABLE telemetry_v2 RENAME TO telemetry;
             COMMIT;",
        )?;
        let rows: i64 = conn.query_row("SELECT COUNT(*) FROM telemetry", [], |row| row.get(0))?;
        // The legacy table's pages are now free; hand them back so the
        // file shrinks to the new schema's footprint.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum;")?;
        tracing::info!(
            "Migrated {} telemetry rows to the channel-dictionary schema in {:.1}s",
            rows,
            started.elapsed().as_secs_f64()
        );
        Ok(())
    }

    /// Apply the standard pragmas to a connection (writer or reader).
    /// busy_timeout is load-bearing: with one writer, up to 8 readers,
    /// checkpoints, and incremental_vacuum sharing the file, a locked
    /// database must retry briefly instead of surfacing SQLITE_BUSY to
    /// the ingest path.
    fn tune(conn: &Connection) -> Result<(), DbError> {
        conn.execute_batch(
            "PRAGMA busy_timeout=5000;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA page_size=4096;
             PRAGMA mmap_size=268435456;
             PRAGMA cache_size=-64000;
             PRAGMA auto_vacuum=INCREMENTAL;",
        )?;
        Ok(())
    }

    /// Borrow a reader connection from the pool, run the closure, return it.
    /// If the pool is empty, opens a fresh reader connection on demand.
    /// On return, drops the connection if the pool is at READ_POOL_MAX.
    fn with_reader<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&Connection) -> Result<R, DbError>,
    {
        // Pop a connection from the pool (or create a new one)
        let conn = {
            let mut pool = self.readers.lock().map_err(|_| DbError::Lock)?;
            pool.pop()
        };
        let conn = match conn {
            Some(c) => c,
            None => {
                let c = Connection::open(&self.db_path)?;
                Self::tune(&c)?;
                c
            }
        };

        // Run the query OUTSIDE any zenith-side lock so multiple readers
        // can proceed concurrently against WAL.
        let result = f(&conn);

        // Return the connection to the pool (or drop if pool is full)
        {
            let mut pool = self.readers.lock().map_err(|_| DbError::Lock)?;
            if pool.len() < READ_POOL_MAX {
                pool.push(conn);
            }
            // else: drop -- closes the underlying SQLite connection
        }

        result
    }

    /// Insert a batch of samples within a single transaction.
    pub fn insert_batch(&self, samples: &[TelemetrySample]) -> Result<(), DbError> {
        if samples.is_empty() {
            return Ok(());
        }

        let mut conn = self.writer.lock().map_err(|_| DbError::Lock)?;

        // The transaction must be RAII (rolls back on drop). A raw BEGIN
        // with `?` early-returns leaves the transaction open on the pooled
        // writer connection, and every later insert_batch then fails with
        // "cannot start a transaction within a transaction" until restart.
        let tx = conn.transaction()?;

        // Resolve channel ids. Steady state hits the cache for every
        // sample; a first-seen channel is inserted into the dictionary
        // inside this transaction but cached only after commit --
        // caching a rolled-back id would point rows at a dead channel.
        let mut cache = self.channel_ids.lock().map_err(|_| DbError::Lock)?;
        let mut fresh: HashMap<(Arc<str>, Arc<str>), i64> = HashMap::new();
        for s in samples {
            let key = (Arc::clone(&s.target_id), Arc::clone(&s.channel));
            if cache.contains_key(&key) || fresh.contains_key(&key) {
                continue;
            }
            tx.execute(
                "INSERT OR IGNORE INTO channels (target_id, name) VALUES (?1, ?2)",
                params![&*s.target_id, &*s.channel],
            )?;
            let id: i64 = tx.query_row(
                "SELECT id FROM channels WHERE target_id = ?1 AND name = ?2",
                params![&*s.target_id, &*s.channel],
                |row| row.get(0),
            )?;
            fresh.insert(key, id);
        }

        // Insert into main table
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO telemetry (channel_id, timestamp_ms, value) \
                 VALUES (?1, ?2, ?3)",
            )?;
            for s in samples {
                let key = (Arc::clone(&s.target_id), Arc::clone(&s.channel));
                let id = cache
                    .get(&key)
                    .or_else(|| fresh.get(&key))
                    .copied()
                    .expect("channel id resolved above");
                stmt.execute(params![id, s.timestamp_ms as i64, s.value])?;
            }
        }

        // Update latest values table (UPSERT)
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO telemetry_latest (target_id, channel, timestamp_ms, value) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(target_id, channel) DO UPDATE SET \
                   timestamp_ms = excluded.timestamp_ms, \
                   value = excluded.value \
                 WHERE excluded.timestamp_ms >= telemetry_latest.timestamp_ms",
            )?;
            for s in samples {
                stmt.execute(params![
                    &*s.target_id,
                    &*s.channel,
                    s.timestamp_ms as i64,
                    s.value
                ])?;
            }
        }

        tx.commit()?;
        // The dictionary rows are durable now -- safe to remember them.
        cache.extend(fresh);
        Ok(())
    }

    /// Query samples in a time range (returned in chronological order).
    pub fn query_range(
        &self,
        target_id: &str,
        channel: Option<&str>,
        start_ms: u64,
        end_ms: u64,
        limit: usize,
    ) -> Result<Vec<TelemetrySample>, DbError> {
        // Runs on a pooled reader so concurrent query_range calls
        // proceed in parallel against WAL.
        let target_arc: Arc<str> = Arc::from(target_id);
        self.with_reader(|conn| {
            if let Some(ch) = channel {
                // Single channel: resolve the dictionary id first, then
                // range-scan its (channel_id, timestamp_ms) index run.
                let chan_id: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM channels WHERE target_id = ?1 AND name = ?2",
                        params![target_id, ch],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(chan_id) = chan_id else {
                    return Ok(Vec::new());
                };
                let ch_arc: Arc<str> = Arc::from(ch);
                let mut stmt = conn.prepare_cached(
                    "SELECT timestamp_ms, value, v_min, v_max, agg_count FROM telemetry \
                     WHERE channel_id = ?1 \
                       AND timestamp_ms >= ?2 AND timestamp_ms <= ?3 \
                     ORDER BY timestamp_ms ASC LIMIT ?4",
                )?;
                let rows = stmt.query_map(
                    params![chan_id, start_ms as i64, end_ms as i64, limit as i64],
                    |row| {
                        Ok(TelemetrySample {
                            target_id: Arc::clone(&target_arc),
                            timestamp_ms: row.get::<_, i64>(0)? as u64,
                            channel: Arc::clone(&ch_arc),
                            value: row.get(1)?,
                            envelope: envelope_from_row(row.get(2)?, row.get(3)?, row.get(4)?),
                        })
                    },
                )?;
                let mut samples = Vec::new();
                for row in rows {
                    samples.push(row?);
                }
                Ok(samples)
            } else {
                // All channels, newest first. INDEXED BY pins the plan
                // to a backward walk of the time index with an O(1)
                // dictionary probe per row, stopping at LIMIT -- the
                // planner otherwise flips to a channels-outer join that
                // scans and sorts the whole range (measured ~5.8 ms for
                // LIMIT 100 over 100k rows, scaling with table size).
                let mut stmt = conn.prepare_cached(
                    "SELECT c.name, t.timestamp_ms, t.value, t.v_min, t.v_max, t.agg_count \
                     FROM telemetry t INDEXED BY idx_telemetry_ts \
                     JOIN channels c ON c.id = t.channel_id \
                     WHERE c.target_id = ?1 \
                       AND t.timestamp_ms >= ?2 AND t.timestamp_ms <= ?3 \
                     ORDER BY t.timestamp_ms DESC LIMIT ?4",
                )?;
                // One Arc per distinct channel name, shared across rows.
                let mut name_arcs: HashMap<String, Arc<str>> = HashMap::new();
                let rows = stmt.query_map(
                    params![target_id, start_ms as i64, end_ms as i64, limit as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)? as u64,
                            row.get::<_, f64>(2)?,
                            envelope_from_row(row.get(3)?, row.get(4)?, row.get(5)?),
                        ))
                    },
                )?;
                let mut samples = Vec::new();
                for row in rows {
                    let (name, timestamp_ms, value, envelope) = row?;
                    let channel = Arc::clone(
                        name_arcs
                            .entry(name)
                            .or_insert_with_key(|k| Arc::from(k.as_str())),
                    );
                    samples.push(TelemetrySample {
                        target_id: Arc::clone(&target_arc),
                        timestamp_ms,
                        channel,
                        value,
                        envelope,
                    });
                }
                Ok(samples)
            }
        })
    }

    /// Get latest value per channel for a target.
    /// Uses the telemetry_latest table for O(1) lookup instead of scanning
    /// the entire telemetry table.
    pub fn query_latest(&self, target_id: &str) -> Result<Vec<TelemetrySample>, DbError> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT target_id, timestamp_ms, channel, value FROM telemetry_latest \
                 WHERE target_id = ?1",
            )?;
            let rows = stmt.query_map(params![target_id], |row| {
                let target: String = row.get(0)?;
                let channel: String = row.get(2)?;
                Ok(TelemetrySample {
                    target_id: std::sync::Arc::from(target),
                    timestamp_ms: row.get::<_, i64>(1)? as u64,
                    channel: std::sync::Arc::from(channel),
                    value: row.get(3)?,
                    envelope: None,
                })
            })?;

            let mut samples = Vec::new();
            for row in rows {
                samples.push(row?);
            }
            Ok(samples)
        })
    }

    /// Delete samples older than retention period.
    pub fn prune(&self, retention_ms: u64) -> Result<usize, DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let cutoff = now_ms.saturating_sub(retention_ms);

        // Prune telemetry samples
        let deleted = conn.execute(
            "DELETE FROM telemetry WHERE timestamp_ms < ?1",
            params![cutoff as i64],
        )?;

        // Clean up stale telemetry_latest entries
        conn.execute(
            "DELETE FROM telemetry_latest WHERE timestamp_ms < ?1",
            params![cutoff as i64],
        )?;

        // Clean up old user-created layouts (keep last 100 per target,
        // config layouts always kept). The ranking must partition by
        // target_id: a global newest-100 would let one target's saves
        // evict another target's layouts. Plots are deleted explicitly
        // because the FK cascade only fires with PRAGMA foreign_keys on.
        const RETAIN_PER_TARGET: i64 = 100;
        let stale_layouts = "SELECT id FROM (
                SELECT id, ROW_NUMBER() OVER (
                    PARTITION BY target_id
                    ORDER BY created_at DESC, id DESC
                ) AS rn
                FROM telemetry_layouts WHERE source = 'user'
            ) WHERE rn > ?1";
        conn.execute(
            &format!("DELETE FROM telemetry_plots WHERE layout_id IN ({stale_layouts})"),
            params![RETAIN_PER_TARGET],
        )?;
        conn.execute(
            &format!("DELETE FROM telemetry_layouts WHERE id IN ({stale_layouts})"),
            params![RETAIN_PER_TARGET],
        )?;

        // Periodic DB maintenance (WAL checkpoint + optimize)
        if deleted > 1000 {
            let _ = conn.execute_batch(
                "PRAGMA wal_checkpoint(PASSIVE);
                 PRAGMA optimize;",
            );
        }

        Ok(deleted)
    }

    /// Prune audit rows older than the cutoff. The audit log is
    /// append-only in normal operation; retention runs from the
    /// maintenance loop so the log cannot grow without bound inside
    /// the size-capped DB file.
    pub fn prune_audit(&self, cutoff_ms: u64) -> Result<usize, DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let deleted = conn.execute(
            "DELETE FROM audit_log WHERE ts_ms < ?1",
            params![cutoff_ms as i64],
        )?;
        Ok(deleted)
    }

    /// Cheap writability probe for health reporting. BEGIN IMMEDIATE
    /// acquires SQLite's RESERVED lock -- it fails on a read-only file
    /// or filesystem -- then rolls back without touching data. A busy
    /// writer mutex short-circuits to Ok: an active writer is itself
    /// proof that writes are flowing.
    pub fn probe_writable(&self) -> Result<(), DbError> {
        match self.writer.try_lock() {
            Ok(conn) => {
                conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")?;
                Ok(())
            }
            Err(std::sync::TryLockError::WouldBlock) => Ok(()),
            Err(std::sync::TryLockError::Poisoned(_)) => Err(DbError::Lock),
        }
    }

    /// Get total sample count.
    pub fn count(&self) -> Result<u64, DbError> {
        self.with_reader(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM telemetry", [], |row| row.get(0))?;
            Ok(count as u64)
        })
    }

    /// Global stats for size-based FIFO.
    ///
    /// Reports total disk usage = main db file bytes + WAL file bytes,
    /// obtained via filesystem `stat`. Deliberately no checkpoint here:
    /// forcing one would turn a stats query into a write-side fsync.
    /// WAL checkpointing happens on its own schedule from the
    /// maintenance loop and after large deletions.
    pub fn global_stats(&self) -> Result<GlobalStats, DbError> {
        // Must be a real row count, not MAX(ROWID): rowids keep climbing
        // after FIFO deletions, and the maintenance loop divides db bytes
        // by this value to size evictions -- a high-water mark makes the
        // eviction volume systematically wrong. COUNT(*) is an index-only
        // scan and this runs once per maintenance tick, not per request.
        let total_samples: i64 = self
            .with_reader(|conn| {
                Ok(conn
                    .query_row("SELECT COUNT(*) FROM telemetry", [], |row| row.get(0))
                    .unwrap_or(0))
            })
            .unwrap_or(0);

        // Stat the .db file and the .db-wal sidecar (no lock).
        let db_bytes = std::fs::metadata(&self.db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let wal_path = {
            let mut p = self.db_path.clone().into_os_string();
            p.push("-wal");
            PathBuf::from(p)
        };
        let wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);

        Ok(GlobalStats {
            total_samples: total_samples as u64,
            db_size_bytes: db_bytes + wal_bytes,
        })
    }

    /// Delete the oldest N samples (FIFO, global across all targets).
    pub fn delete_oldest(&self, count: usize) -> Result<usize, DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let deleted = conn.execute(
            "DELETE FROM telemetry WHERE ROWID IN (SELECT ROWID FROM telemetry ORDER BY timestamp_ms ASC LIMIT ?1)",
            params![count as i64],
        )?;

        // Drop latest-value rows only for channels with no remaining
        // history. Correlated NOT EXISTS resolves the dictionary id via
        // the unique (target_id, name) index, then probes the lookup
        // index once per telemetry_latest row (one row per channel) --
        // the NOT IN form materialized a DISTINCT over the entire
        // telemetry index on every FIFO pass, under the writer lock.
        conn.execute(
            "DELETE FROM telemetry_latest
             WHERE NOT EXISTS (
               SELECT 1 FROM telemetry t
               JOIN channels c ON c.id = t.channel_id
               WHERE c.target_id = telemetry_latest.target_id
                 AND c.name = telemetry_latest.channel
             )",
            [],
        )?;

        // WAL checkpoint after large deletion
        if deleted > 1000 {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE); PRAGMA optimize;");
        }

        Ok(deleted)
    }

    /// Delete the oldest N samples for a SPECIFIC target. Used by the
    /// per-target storage management UI for "Trim oldest" actions and
    /// the eventual per-target FIFO when allocations are configured.
    pub fn delete_oldest_for_target(
        &self,
        target_id: &str,
        count: usize,
    ) -> Result<usize, DbError> {
        if count == 0 {
            return Ok(0);
        }
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let deleted = conn.execute(
            "DELETE FROM telemetry WHERE ROWID IN (
                SELECT ROWID FROM telemetry
                WHERE channel_id IN (SELECT id FROM channels WHERE target_id = ?1)
                ORDER BY timestamp_ms ASC
                LIMIT ?2
             )",
            params![target_id, count as i64],
        )?;

        // Clean up stale telemetry_latest rows for this target only.
        // Same correlated-probe shape as delete_oldest: index seek per
        // latest row instead of a DISTINCT scan of the target's history.
        conn.execute(
            "DELETE FROM telemetry_latest
             WHERE target_id = ?1
               AND NOT EXISTS (
                 SELECT 1 FROM telemetry t
                 JOIN channels c ON c.id = t.channel_id
                 WHERE c.target_id = telemetry_latest.target_id
                   AND c.name = telemetry_latest.channel
               )",
            params![target_id],
        )?;

        if deleted > 1000 {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE); PRAGMA optimize;");
        }

        Ok(deleted)
    }

    /// Per-target live row counts, for fair FIFO eviction sizing.
    pub fn target_sample_counts(&self) -> Result<Vec<(String, u64)>, DbError> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT c.target_id, COUNT(*)
                 FROM telemetry t JOIN channels c ON c.id = t.channel_id
                 GROUP BY c.target_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// Reclaim freed pages (works with auto_vacuum=INCREMENTAL).
    pub fn incremental_vacuum(&self) -> Result<(), DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        conn.execute_batch("PRAGMA incremental_vacuum; PRAGMA optimize;")?;
        Ok(())
    }

    /// Get per-target storage statistics.
    pub fn target_stats(&self, target_id: &str) -> Result<TargetStats, DbError> {
        let file_bytes = self.db_size_bytes().unwrap_or(0);
        self.with_reader(|conn| {
            // One pass: per-channel COUNT/MIN/MAX ride the
            // (channel_id, timestamp_ms) index runs, then aggregate
            // across the target's channels.
            let (sample_count, oldest_ms, newest_ms): (i64, Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT COALESCE(SUM(cnt), 0), MIN(mn), MAX(mx) FROM (
                        SELECT COUNT(*) AS cnt,
                               MIN(timestamp_ms) AS mn,
                               MAX(timestamp_ms) AS mx
                        FROM telemetry
                        WHERE channel_id IN (SELECT id FROM channels WHERE target_id = ?1)
                        GROUP BY channel_id
                     )",
                    params![target_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap_or((0, None, None));

            let channel_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM telemetry_latest WHERE target_id = ?1",
                    params![target_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            // Live bytes/sample: this target's share of the real file
            // size, prorated by row count. Below the threshold the file
            // is dominated by fixed overhead (schema pages, WAL) and the
            // ratio would wildly overstate, so a measured baseline for
            // the dictionary schema is used instead.
            let total_rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM telemetry", [], |row| row.get(0))
                .unwrap_or(0);
            let avg_bytes = if total_rows >= LIVE_AVG_MIN_ROWS {
                file_bytes / total_rows as u64
            } else {
                BASELINE_BYTES_PER_SAMPLE
            };

            Ok(TargetStats {
                sample_count: sample_count as u64,
                channel_count: channel_count as u64,
                oldest_ms: oldest_ms.map(|v| v as u64),
                newest_ms: newest_ms.map(|v| v as u64),
                span_seconds: match (oldest_ms, newest_ms) {
                    (Some(o), Some(n)) => Some(((n - o) / 1000) as u64),
                    _ => None,
                },
                byte_estimate: (sample_count as u64) * avg_bytes,
            })
        })
    }

    /// One bounded step of the retention ladder: rows below `to_tier`
    /// older than `cutoff_ms` are aggregated into `bucket_ms`-wide
    /// envelope buckets at `to_tier` -- value becomes the (weighted)
    /// mean, v_min/v_max keep the spread, agg_count the source-sample
    /// total. Weighting by agg_count makes re-tiering exact: a tier-1
    /// bucket of 9 samples pulls a tier-2 mean 9x harder than a
    /// bucket of 1.
    ///
    /// Processes at most `slice_ms` of source data per call, starting
    /// at the oldest eligible row, so a large backlog converges over
    /// successive maintenance ticks instead of one full-history
    /// rewrite under the writer lock. Returns zeros when nothing is
    /// eligible (converged).
    pub fn tier_pass(
        &self,
        to_tier: u32,
        cutoff_ms: u64,
        bucket_ms: u64,
        slice_ms: u64,
    ) -> Result<TierPassResult, DbError> {
        let mut conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let bucket = bucket_ms.max(1) as i64;
        let cutoff = cutoff_ms as i64;

        let oldest: Option<i64> = conn
            .query_row(
                "SELECT MIN(timestamp_ms) FROM telemetry
                 WHERE tier < ?1 AND timestamp_ms < ?2",
                params![to_tier, cutoff],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(oldest) = oldest else {
            return Ok(TierPassResult {
                source_rows: 0,
                bucket_rows: 0,
            });
        };

        // Bucket-align the window so every bucket this pass writes is
        // complete; the window never crosses the cutoff.
        let window_start = (oldest / bucket) * bucket;
        let window_end = window_start
            .saturating_add(slice_ms.min(i64::MAX as u64) as i64)
            .max(window_start + bucket)
            .min(cutoff);

        // RAII transaction: an error between DELETE and re-INSERT must
        // roll back atomically, never leave history deleted without its
        // envelope replacement (or an open transaction on the writer).
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS tier_temp ( \
                 channel_id INTEGER, timestamp_ms INTEGER, value REAL, \
                 v_min REAL, v_max REAL, agg_count INTEGER \
             )",
        )?;
        tx.execute("DELETE FROM tier_temp", [])?;

        tx.execute(
            "INSERT INTO tier_temp (channel_id, timestamp_ms, value, v_min, v_max, agg_count) \
             SELECT channel_id, \
                    (timestamp_ms / ?1) * ?1 + (?1 / 2), \
                    SUM(value * COALESCE(agg_count, 1)) / SUM(COALESCE(agg_count, 1)), \
                    MIN(COALESCE(v_min, value)), \
                    MAX(COALESCE(v_max, value)), \
                    SUM(COALESCE(agg_count, 1)) \
             FROM telemetry \
             WHERE tier < ?2 AND timestamp_ms >= ?3 AND timestamp_ms < ?4 \
             GROUP BY channel_id, timestamp_ms / ?1",
            params![bucket, to_tier, window_start, window_end],
        )?;

        let source_rows = tx.execute(
            "DELETE FROM telemetry \
             WHERE tier < ?1 AND timestamp_ms >= ?2 AND timestamp_ms < ?3",
            params![to_tier, window_start, window_end],
        )?;
        let bucket_rows = tx.execute(
            "INSERT INTO telemetry (channel_id, timestamp_ms, value, tier, v_min, v_max, agg_count) \
             SELECT channel_id, timestamp_ms, value, ?1, v_min, v_max, agg_count FROM tier_temp",
            params![to_tier],
        )?;
        tx.execute("DELETE FROM tier_temp", [])?;
        tx.commit()?;

        Ok(TierPassResult {
            source_rows: source_rows as u64,
            bucket_rows: bucket_rows as u64,
        })
    }

    /// Rows in a timestamp range (index-only count). The maintenance
    /// loop uses this to report per-tier populations by age band
    /// without scanning row data.
    pub fn count_range(&self, start_ms: u64, end_ms: u64) -> Result<u64, DbError> {
        self.with_reader(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM telemetry \
                 WHERE timestamp_ms >= ?1 AND timestamp_ms < ?2",
                params![start_ms as i64, end_ms as i64],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
    }

    /// Downsample old data: replace detailed samples with envelope
    /// buckets (operator-triggered; the scheduled ladder in the
    /// maintenance loop uses the same tier_pass machinery).
    ///
    /// Example: downsample(3600000, 60000) keeps the last hour at full
    /// resolution and averages everything older into 1-minute envelope
    /// buckets at the coarse tier.
    pub fn downsample(&self, age_ms: u64, bucket_ms: u64) -> Result<DownsampleResult, DbError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let cutoff = now_ms.saturating_sub(age_ms);

        // Loop bounded passes until converged: same incremental unit
        // the maintenance ladder uses, so the writer is never held for
        // one full-history rewrite even on the manual path.
        const SLICE_MS: u64 = 3_600_000;
        let mut source_total = 0u64;
        let mut bucket_total = 0u64;
        loop {
            let pass = self.tier_pass(COARSE_TIER, cutoff, bucket_ms, SLICE_MS)?;
            if pass.source_rows == 0 {
                break;
            }
            source_total += pass.source_rows;
            bucket_total += pass.bucket_rows;
        }

        Ok(DownsampleResult {
            samples_before: source_total,
            samples_after: bucket_total,
            removed: source_total.saturating_sub(bucket_total),
        })
    }

    /// Delete all data for a specific target. Dictionary rows are kept
    /// on purpose: they are a handful of bytes per channel ever seen,
    /// and keeping them means cached channel ids can never dangle.
    pub fn delete_target(&self, target_id: &str) -> Result<usize, DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let deleted = conn.execute(
            "DELETE FROM telemetry
             WHERE channel_id IN (SELECT id FROM channels WHERE target_id = ?1)",
            params![target_id],
        )?;
        conn.execute(
            "DELETE FROM telemetry_latest WHERE target_id = ?1",
            params![target_id],
        )?;
        Ok(deleted)
    }

    /// Append a new audit log entry. Best-effort: errors are returned
    /// but the caller should usually log+ignore so audit failures don't
    /// block the underlying action.
    pub fn log_audit(
        &self,
        actor: &str,
        action: &str,
        target_id: Option<&str>,
        detail: Option<&str>,
        status: &str,
        source_ip: Option<&str>,
    ) -> Result<(), DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        conn.execute(
            "INSERT INTO audit_log (ts_ms, actor, action, target_id, detail, status, source_ip)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![ts_ms, actor, action, target_id, detail, status, source_ip],
        )?;
        Ok(())
    }

    /// Query the audit log, newest entries first. The tiebreak on `id DESC`
    /// is necessary because ts_ms is millisecond-resolution and multiple
    /// entries within the same ms would otherwise be order-undefined.
    pub fn query_audit(&self, limit: usize, offset: usize) -> Result<Vec<AuditEntry>, DbError> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, ts_ms, actor, action, target_id, detail, status, source_ip
                 FROM audit_log
                 ORDER BY ts_ms DESC, id DESC
                 LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit as i64, offset as i64], |row| {
                Ok(AuditEntry {
                    id: row.get(0)?,
                    ts_ms: row.get::<_, i64>(1)? as u64,
                    actor: row.get(2)?,
                    action: row.get(3)?,
                    target_id: row.get(4)?,
                    detail: row.get(5)?,
                    status: row.get(6)?,
                    source_ip: row.get(7)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// Get database file size in bytes (main file + WAL sidecar).
    /// Uses filesystem stat -- no DB lock taken, no checkpoint forced.
    pub fn db_size_bytes(&self) -> Result<u64, DbError> {
        let (db, wal) = self.file_sizes();
        Ok(db + wal)
    }

    /// (main file bytes, WAL sidecar bytes) via filesystem stat.
    pub fn file_sizes(&self) -> (u64, u64) {
        let db_bytes = std::fs::metadata(&self.db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let wal_path = {
            let mut p = self.db_path.clone().into_os_string();
            p.push("-wal");
            PathBuf::from(p)
        };
        let wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        (db_bytes, wal_bytes)
    }

    /// Total audit rows (the audit log shares the size-capped file, so
    /// its footprint belongs on the storage panel).
    pub fn audit_count(&self) -> Result<u64, DbError> {
        self.with_reader(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))?;
            Ok(count as u64)
        })
    }
}

/// Build an Envelope from a row's nullable tier columns: present only
/// when the row was written by the tier ladder (all three non-null).
fn envelope_from_row(
    v_min: Option<f64>,
    v_max: Option<f64>,
    agg_count: Option<i64>,
) -> Option<crate::core::telemetry::Envelope> {
    match (v_min, v_max, agg_count) {
        (Some(min), Some(max), Some(count)) => Some(crate::core::telemetry::Envelope {
            min,
            max,
            count: count.max(0) as u32,
        }),
        _ => None,
    }
}

/* ----------------------------- FIFO Fairness ----------------------------- */

/// Split a FIFO eviction volume across targets by lowering a common
/// waterline: the targets holding the most rows are trimmed down to a
/// shared level until `to_delete` rows are freed, and any target
/// already below that level loses nothing. This is what keeps one
/// chatty target from evicting a quiet target's history -- a global
/// oldest-first delete removes whatever happens to be oldest, which
/// under mixed rates is almost always the quiet target's only copy.
///
/// Returns (target_id, rows_to_delete) pairs; targets with a zero
/// allocation are omitted. Allocations sum to exactly `to_delete`
/// (or to the total row count when `to_delete` exceeds it).
pub fn allocate_evictions(counts: &[(String, u64)], to_delete: u64) -> Vec<(String, u64)> {
    let total: u64 = counts.iter().map(|(_, c)| c).sum();
    if to_delete == 0 || total == 0 {
        return Vec::new();
    }
    if to_delete >= total {
        return counts.iter().filter(|(_, c)| *c > 0).cloned().collect();
    }

    // Largest first; name tiebreak keeps the result deterministic.
    let mut sorted: Vec<(String, u64)> = counts.to_vec();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // Find the waterline L and the k targets above it such that
    // sum(c_i - L) over those k equals to_delete.
    let n = sorted.len();
    let mut prefix = 0u64;
    let mut k = n;
    let mut level = 0u64;
    for (i, (_, c)) in sorted.iter().enumerate() {
        prefix += c;
        let next = if i + 1 < n { sorted[i + 1].1 } else { 0 };
        if prefix >= to_delete {
            let l = (prefix - to_delete) / (i as u64 + 1);
            if l >= next {
                k = i + 1;
                level = l;
                break;
            }
        }
    }

    let mut allocs: Vec<(String, u64)> = sorted[..k]
        .iter()
        .map(|(t, c)| (t.clone(), c - level))
        .collect();

    // Integer division leaves up to k-1 rows over-allocated; shave
    // them off the smallest allocations so the sum lands exactly.
    let mut over = allocs.iter().map(|(_, d)| d).sum::<u64>() - to_delete;
    for a in allocs.iter_mut().rev() {
        if over == 0 {
            break;
        }
        let cut = a.1.min(1).min(over);
        a.1 -= cut;
        over -= cut;
    }
    allocs.retain(|(_, d)| *d > 0);
    allocs
}

/* ----------------------------- Stats Types ----------------------------- */

/// Aggregate database statistics: live total row count plus on-disk
/// size in bytes (main file + WAL sidecar from filesystem stat).
#[derive(Debug, serde::Serialize)]
pub struct GlobalStats {
    pub total_samples: u64,
    pub db_size_bytes: u64,
}

/// Per-target storage statistics.
#[derive(Debug, serde::Serialize)]
pub struct TargetStats {
    pub sample_count: u64,
    pub channel_count: u64,
    pub oldest_ms: Option<u64>,
    pub newest_ms: Option<u64>,
    pub span_seconds: Option<u64>,
    /// Rough estimate of disk bytes occupied by this target's samples:
    /// the target's row count times the database's live bytes/sample
    /// ratio (real file size / real row count), so it tracks the true
    /// footprint instead of a guessed constant. Treat as approximate --
    /// the actual on-disk usage is shared across the whole telemetry
    /// table because of WAL/auto_vacuum.
    pub byte_estimate: u64,
}

/// Measured per-row footprint of the dictionary schema (1M-row
/// experiment: 76.2 B/row including indexes). Only used below
/// LIVE_AVG_MIN_ROWS, where fixed overhead dominates the file and the
/// live ratio would overstate by orders of magnitude.
const BASELINE_BYTES_PER_SAMPLE: u64 = 76;

/// Row count above which the live file-size/row-count ratio is a
/// better bytes/sample estimate than the measured baseline.
const LIVE_AVG_MIN_ROWS: i64 = 10_000;

/// Result of a downsample operation.
#[derive(Debug, serde::Serialize)]
pub struct DownsampleResult {
    pub samples_before: u64,
    pub samples_after: u64,
    pub removed: u64,
}

/// One tier_pass step's accounting.
#[derive(Debug, Clone, Copy)]
pub struct TierPassResult {
    /// Rows consumed from lower tiers in this pass's window.
    pub source_rows: u64,
    /// Envelope bucket rows written in their place.
    pub bucket_rows: u64,
}

/// Tier levels of the retention ladder. Full resolution is 0; the
/// ladder's names match the config keys.
pub const MID_TIER: u32 = 1;
pub const COARSE_TIER: u32 = 2;

/// One entry in the audit log.
#[derive(Debug, serde::Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub ts_ms: u64,
    pub actor: String,
    pub action: String,
    pub target_id: Option<String>,
    pub detail: Option<String>,
    pub status: String,
    pub source_ip: Option<String>,
}

/* ----------------------------- Layout Persistence ----------------------------- */

/// Saved plot definition (from DB).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedPlot {
    pub title: String,
    pub channels: Vec<String>,
    pub height: u16,
    pub position: [u16; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y_max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<Vec<Threshold>>,
}

/// One horizontal threshold line on a saved plot. Renders as a dashed
/// line at `value` in the given `color` with an optional label.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Threshold {
    pub value: f64,
    pub color: String,
    #[serde(default)]
    pub label: String,
}

/// Saved layout definition (from DB).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedLayout {
    pub id: i64,
    pub name: String,
    pub source: String,
    pub grid: String,
    pub time_window_s: u32,
    pub plots: Vec<SavedPlot>,
}

impl TelemetryDb {
    /// Synchronize config-sourced layouts with the target's telemetry
    /// config file: insert new, replace changed, delete config layouts
    /// no longer in the file. User-sourced layouts are never touched.
    /// This keeps the file a pure regenerable default -- insert-if-
    /// absent seeding left stale config rows behind forever, which is
    /// what made targets/ un-refreshable.
    pub fn seed_layouts(
        &self,
        target_id: &str,
        config: &crate::core::config_manager::TelemetryConfig,
    ) -> Result<usize, DbError> {
        let mut conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let tx = conn.transaction()?;
        let mut count = 0;

        // Delete config-sourced layouts absent from the file (plots
        // first: the FK cascade only fires with foreign_keys on).
        let stale_ids: Vec<i64> = {
            let file_names: Vec<&str> = config.layouts.iter().map(|l| l.name.as_str()).collect();
            let mut stmt = tx.prepare(
                "SELECT id, name FROM telemetry_layouts
                 WHERE target_id = ?1 AND source = 'config'",
            )?;
            let rows = stmt.query_map(params![target_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.filter_map(|r| r.ok())
                .filter(|(_, name)| !file_names.contains(&name.as_str()))
                .map(|(id, _)| id)
                .collect()
        };
        for id in &stale_ids {
            tx.execute(
                "DELETE FROM telemetry_plots WHERE layout_id = ?1",
                params![id],
            )?;
            tx.execute("DELETE FROM telemetry_layouts WHERE id = ?1", params![id])?;
        }

        // Upsert each file layout; replacing plots in place keeps the
        // layout id stable when only content changed. Names are unique
        // per target across sources, and a user-saved layout owns its
        // name: the file default is skipped rather than clobbering it.
        for layout in &config.layouts {
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id, source FROM telemetry_layouts
                     WHERE target_id = ?1 AND name = ?2",
                    params![target_id, layout.name],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;

            let layout_id = match existing {
                Some((_, source)) if source != "config" => continue,
                Some((id, _)) => {
                    tx.execute(
                        "DELETE FROM telemetry_plots WHERE layout_id = ?1",
                        params![id],
                    )?;
                    id
                }
                None => {
                    tx.execute(
                        "INSERT INTO telemetry_layouts (target_id, name, source, grid, time_window_s, created_at)
                         VALUES (?1, ?2, 'config', '1x1', 30, ?3)",
                        params![target_id, layout.name, now],
                    )?;
                    count += 1;
                    tx.last_insert_rowid()
                }
            };

            for (i, plot) in layout.plots.iter().enumerate() {
                let channels_json = serde_json::to_string(&plot.channels).unwrap_or_default();
                tx.execute(
                    "INSERT INTO telemetry_plots (layout_id, title, channels, height, position_row, position_col)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                    params![layout_id, plot.title, channels_json, plot.height, i as i64],
                )?;
            }
        }

        tx.commit()?;
        Ok(count)
    }

    /// Get all layouts for a target.
    pub fn get_layouts(&self, target_id: &str) -> Result<Vec<SavedLayout>, DbError> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT id, name, source, grid, time_window_s FROM telemetry_layouts
                 WHERE target_id = ?1 ORDER BY source DESC, name",
            )?;

            let layouts: Vec<(i64, String, String, String, u32)> = stmt
                .query_map(params![target_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
                })?
                .filter_map(|r| r.ok())
                .collect();

            let mut result = Vec::new();
            for (id, name, source, grid, time_window_s) in layouts {
                let mut plot_stmt = conn.prepare_cached(
                    "SELECT title, channels, height, position_row, position_col, y_min, y_max, y_label, thresholds
                     FROM telemetry_plots WHERE layout_id = ?1 ORDER BY position_row, position_col",
                )?;

                let plots: Vec<SavedPlot> = plot_stmt
                    .query_map(params![id], |row| {
                        let channels_json: String = row.get(1)?;
                        let channels: Vec<String> =
                            serde_json::from_str(&channels_json).unwrap_or_default();
                        let thresholds_json: Option<String> = row.get(8)?;
                        let thresholds: Option<Vec<Threshold>> =
                            thresholds_json.and_then(|j| serde_json::from_str(&j).ok());
                        Ok(SavedPlot {
                            title: row.get(0)?,
                            channels,
                            height: row.get(2)?,
                            position: [row.get(3)?, row.get(4)?],
                            y_min: row.get(5)?,
                            y_max: row.get(6)?,
                            y_label: row.get(7)?,
                            thresholds,
                        })
                    })?
                    .filter_map(|r| r.ok())
                    .collect();

                result.push(SavedLayout {
                    id,
                    name,
                    source,
                    grid,
                    time_window_s,
                    plots,
                });
            }

            Ok(result)
        })
    }

    /// Save a new user layout.
    ///
    /// Returns `Err(DbError::Sqlite(SqliteFailure))` if a config-source
    /// layout with the same name already exists -- refuses to let an
    /// `INSERT OR REPLACE` clobber a config layout (which would also
    /// flip its source from `config` to `user` and make it deletable).
    /// The caller should surface this as a 409 Conflict to the user.
    pub fn save_layout(
        &self,
        target_id: &str,
        name: &str,
        grid: &str,
        time_window_s: u32,
        plots: &[SavedPlot],
    ) -> Result<i64, DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;

        // Reject if there's already a config-sourced layout with this name.
        let config_clash: Option<i64> = conn
            .query_row(
                "SELECT id FROM telemetry_layouts
                 WHERE target_id = ?1 AND name = ?2 AND source = 'config'",
                params![target_id, name],
                |row| row.get(0),
            )
            .optional()?;
        if config_clash.is_some() {
            return Err(DbError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
                Some(format!(
                    "A config-sourced layout named '{}' already exists. Pick a different name to save as a user layout.",
                    name
                )),
            )));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        conn.execute(
            "INSERT OR REPLACE INTO telemetry_layouts (target_id, name, source, grid, time_window_s, created_at)
             VALUES (?1, ?2, 'user', ?3, ?4, ?5)",
            params![target_id, name, grid, time_window_s, now],
        )?;
        let layout_id = conn.last_insert_rowid();

        // Clear old plots for this layout
        conn.execute(
            "DELETE FROM telemetry_plots WHERE layout_id = ?1",
            params![layout_id],
        )?;

        for plot in plots {
            let channels_json = serde_json::to_string(&plot.channels).unwrap_or_default();
            let thresholds_json = plot
                .thresholds
                .as_ref()
                .and_then(|t| serde_json::to_string(t).ok());
            conn.execute(
                "INSERT INTO telemetry_plots (layout_id, title, channels, height, position_row, position_col, y_min, y_max, y_label, thresholds)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    layout_id,
                    plot.title,
                    channels_json,
                    plot.height,
                    plot.position[0],
                    plot.position[1],
                    plot.y_min,
                    plot.y_max,
                    plot.y_label,
                    thresholds_json,
                ],
            )?;
        }

        Ok(layout_id)
    }

    /// Delete a user layout (refuses to delete config-sourced layouts).
    pub fn delete_layout(&self, layout_id: i64) -> Result<bool, DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;

        let source: Option<String> = conn
            .query_row(
                "SELECT source FROM telemetry_layouts WHERE id = ?1",
                params![layout_id],
                |row| row.get(0),
            )
            .optional()?;

        match source.as_deref() {
            Some("config") => Ok(false), // Can't delete config-sourced layouts
            Some(_) => {
                conn.execute(
                    "DELETE FROM telemetry_plots WHERE layout_id = ?1",
                    params![layout_id],
                )?;
                conn.execute(
                    "DELETE FROM telemetry_layouts WHERE id = ?1",
                    params![layout_id],
                )?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn open_temp_db() -> (TempDir, TelemetryDb) {
        let dir = TempDir::new().unwrap();
        let db = TelemetryDb::open(&dir.path().join("test.db")).unwrap();
        (dir, db)
    }

    fn sample(target: &Arc<str>, channel: &Arc<str>, ts: u64, v: f64) -> TelemetrySample {
        TelemetrySample {
            target_id: Arc::clone(target),
            timestamp_ms: ts,
            channel: Arc::clone(channel),
            value: v,
            envelope: None,
        }
    }

    /// @test Inserting a batch then querying the same time range
    /// returns the samples in chronological order with correct
    /// timestamps and values.
    #[test]
    fn insert_and_query_round_trip() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X.field");
        let samples = vec![
            sample(&t, &ch, 100, 1.0),
            sample(&t, &ch, 200, 2.0),
            sample(&t, &ch, 300, 3.0),
        ];
        db.insert_batch(&samples).unwrap();

        let out = db.query_range("t1", Some("X.field"), 0, 1000, 100).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].timestamp_ms, 100);
        assert_eq!(out[0].value, 1.0);
        assert_eq!(out[2].timestamp_ms, 300);
    }

    /// @test insert_batch with an empty slice returns Ok and writes
    /// nothing to the database.
    #[test]
    fn insert_empty_batch_is_noop() {
        let (_dir, db) = open_temp_db();
        db.insert_batch(&[]).unwrap();
        assert_eq!(db.count().unwrap(), 0);
    }

    /// @test query_range with a start_ms/end_ms window only returns
    /// samples whose timestamps fall inside that window.
    #[test]
    fn query_range_respects_time_window() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X.field");
        let samples: Vec<TelemetrySample> = (0..100)
            .map(|i| sample(&t, &ch, i * 10, i as f64))
            .collect();
        db.insert_batch(&samples).unwrap();

        let out = db
            .query_range("t1", Some("X.field"), 200, 500, 100)
            .unwrap();
        assert!(!out.is_empty());
        for s in &out {
            assert!(s.timestamp_ms >= 200 && s.timestamp_ms <= 500);
        }
    }

    /// @test query_range with a limit returns at most that many rows.
    #[test]
    fn query_range_limit_clamps_results() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X.field");
        let samples: Vec<TelemetrySample> =
            (0..100).map(|i| sample(&t, &ch, i, i as f64)).collect();
        db.insert_batch(&samples).unwrap();

        let out = db.query_range("t1", Some("X.field"), 0, 1000, 10).unwrap();
        assert_eq!(out.len(), 10);
    }

    /// @test query_latest returns one row per channel containing the
    /// most-recent value, even when samples were inserted in
    /// shuffled timestamp order.
    #[test]
    fn query_latest_returns_most_recent_per_channel() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let a: Arc<str> = Arc::from("A");
        let b: Arc<str> = Arc::from("B");
        // Insert in shuffled timestamp order
        let samples = vec![
            sample(&t, &a, 100, 1.0),
            sample(&t, &b, 100, 10.0),
            sample(&t, &a, 300, 3.0),
            sample(&t, &b, 200, 20.0),
            sample(&t, &a, 200, 2.0),
        ];
        db.insert_batch(&samples).unwrap();

        let latest = db.query_latest("t1").unwrap();
        assert_eq!(latest.len(), 2);
        let a_latest = latest.iter().find(|s| &*s.channel == "A").unwrap();
        assert_eq!(a_latest.value, 3.0);
        assert_eq!(a_latest.timestamp_ms, 300);
        let b_latest = latest.iter().find(|s| &*s.channel == "B").unwrap();
        assert_eq!(b_latest.value, 20.0);
        assert_eq!(b_latest.timestamp_ms, 200);
    }

    /// @test delete_oldest deletes samples in timestamp-ascending
    /// order and the remaining samples are the newest N - count.
    #[test]
    fn delete_oldest_removes_in_order() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");
        let samples: Vec<TelemetrySample> = (0..50).map(|i| sample(&t, &ch, i, i as f64)).collect();
        db.insert_batch(&samples).unwrap();

        let deleted = db.delete_oldest(20).unwrap();
        assert_eq!(deleted, 20);
        assert_eq!(db.count().unwrap(), 30);

        // Remaining samples should be timestamps 20..50
        let out = db.query_range("t1", Some("X"), 0, 1000, 100).unwrap();
        assert_eq!(out.first().unwrap().timestamp_ms, 20);
        assert_eq!(out.last().unwrap().timestamp_ms, 49);
    }

    /// @test log_audit followed by query_audit returns the entries
    /// in newest-first order with all fields preserved.
    #[test]
    fn audit_log_round_trip() {
        let (_dir, db) = open_temp_db();
        db.log_audit(
            "admin",
            "send_command",
            Some("t1"),
            Some("opcode=0x0116"),
            "ok",
            Some("10.0.0.1"),
        )
        .unwrap();
        db.log_audit(
            "admin",
            "trim",
            Some("t1"),
            Some("count=100"),
            "ok",
            Some("10.0.0.1"),
        )
        .unwrap();
        db.log_audit(
            "admin",
            "send_command",
            Some("t2"),
            Some("opcode=0x0117"),
            "err: timeout",
            Some("10.0.0.2"),
        )
        .unwrap();

        let entries = db.query_audit(10, 0).unwrap();
        assert_eq!(entries.len(), 3);
        // Newest first
        assert_eq!(entries[0].action, "send_command");
        assert_eq!(entries[0].target_id.as_deref(), Some("t2"));
        assert_eq!(entries[0].status, "err: timeout");
        assert_eq!(entries[1].action, "trim");
        assert_eq!(entries[2].action, "send_command");
        assert_eq!(entries[2].status, "ok");
    }

    /// @test query_audit with offset returns a non-overlapping page
    /// of results, exercising the OFFSET clause.
    #[test]
    fn audit_log_pagination_via_offset() {
        let (_dir, db) = open_temp_db();
        for i in 0..20 {
            db.log_audit("admin", &format!("action_{}", i), None, None, "ok", None)
                .unwrap();
        }
        let page1 = db.query_audit(5, 0).unwrap();
        let page2 = db.query_audit(5, 5).unwrap();
        assert_eq!(page1.len(), 5);
        assert_eq!(page2.len(), 5);
        // No overlap
        let p1_ids: std::collections::HashSet<_> = page1.iter().map(|e| e.id).collect();
        for e in &page2 {
            assert!(!p1_ids.contains(&e.id));
        }
    }

    /// @test delete_oldest_for_target removes only that target's
    /// oldest samples, leaving other targets untouched.
    #[test]
    fn delete_oldest_for_target_only_touches_that_target() {
        let (_dir, db) = open_temp_db();
        let t1: Arc<str> = Arc::from("t1");
        let t2: Arc<str> = Arc::from("t2");
        let ch: Arc<str> = Arc::from("X");
        let mut samples = Vec::new();
        for i in 0..50 {
            samples.push(sample(&t1, &ch, i, i as f64));
            samples.push(sample(&t2, &ch, 1000 + i, i as f64));
        }
        db.insert_batch(&samples).unwrap();
        assert_eq!(db.count().unwrap(), 100);

        let deleted = db.delete_oldest_for_target("t1", 20).unwrap();
        assert_eq!(deleted, 20);
        assert_eq!(db.count().unwrap(), 80);

        // Verify t2 is untouched
        let t2_stats = db.target_stats("t2").unwrap();
        assert_eq!(t2_stats.sample_count, 50);

        // Verify t1 has the newer 30 samples (oldest 20 removed)
        let t1_remaining = db.query_range("t1", Some("X"), 0, 1000, 100).unwrap();
        assert_eq!(t1_remaining.len(), 30);
        assert_eq!(t1_remaining[0].timestamp_ms, 20);
    }

    /// @test target_stats.byte_estimate uses the measured baseline on a
    /// young database (fixed overhead would swamp the live ratio) and
    /// switches to the live file-size/row-count ratio once the table is
    /// large enough -- where a single-target DB's estimate equals the
    /// real file size by construction.
    #[test]
    fn target_stats_byte_estimate_tracks_real_footprint() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");

        // Young DB: baseline path.
        let samples: Vec<TelemetrySample> =
            (0..100).map(|i| sample(&t, &ch, i, i as f64)).collect();
        db.insert_batch(&samples).unwrap();
        let stats = db.target_stats("t1").unwrap();
        assert_eq!(stats.sample_count, 100);
        assert_eq!(stats.byte_estimate, 100 * BASELINE_BYTES_PER_SAMPLE);

        // Past the threshold: live ratio. All rows belong to t1, so the
        // estimate must equal the whole measured file size exactly.
        let more: Vec<TelemetrySample> = (100..LIVE_AVG_MIN_ROWS as u64)
            .map(|i| sample(&t, &ch, i, i as f64))
            .collect();
        for chunk in more.chunks(1000) {
            db.insert_batch(chunk).unwrap();
        }
        let stats = db.target_stats("t1").unwrap();
        assert_eq!(stats.sample_count, LIVE_AVG_MIN_ROWS as u64);
        let file_bytes = db.db_size_bytes().unwrap();
        let expected = (file_bytes / LIVE_AVG_MIN_ROWS as u64) * LIVE_AVG_MIN_ROWS as u64;
        assert_eq!(stats.byte_estimate, expected);
        assert!(stats.byte_estimate > 0);
    }

    /// @test delete_target removes all samples for the given target
    /// id and leaves other targets' samples intact.
    #[test]
    fn delete_target_only_removes_matching_target() {
        let (_dir, db) = open_temp_db();
        let t1: Arc<str> = Arc::from("t1");
        let t2: Arc<str> = Arc::from("t2");
        let ch: Arc<str> = Arc::from("X");
        db.insert_batch(&[sample(&t1, &ch, 100, 1.0), sample(&t2, &ch, 100, 2.0)])
            .unwrap();

        let deleted = db.delete_target("t1").unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(db.count().unwrap(), 1);
        assert!(db.query_latest("t1").unwrap().is_empty());
        assert_eq!(db.query_latest("t2").unwrap().len(), 1);
    }

    /// @test target_stats reports the right sample_count, channel
    /// count, oldest/newest timestamps, and computed span_seconds.
    #[test]
    fn target_stats_returns_correct_span_and_counts() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch1: Arc<str> = Arc::from("A");
        let ch2: Arc<str> = Arc::from("B");
        db.insert_batch(&[
            sample(&t, &ch1, 1000, 1.0),
            sample(&t, &ch1, 5000, 2.0),
            sample(&t, &ch2, 3000, 3.0),
        ])
        .unwrap();

        let stats = db.target_stats("t1").unwrap();
        assert_eq!(stats.sample_count, 3);
        assert_eq!(stats.channel_count, 2);
        assert_eq!(stats.oldest_ms, Some(1000));
        assert_eq!(stats.newest_ms, Some(5000));
        assert_eq!(stats.span_seconds, Some(4));
    }

    /// @test global_stats reports a non-zero db_size_bytes after data
    /// has been written, computed via filesystem stat without
    /// triggering a wal_checkpoint.
    #[test]
    fn global_stats_includes_db_file_size() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");
        let samples: Vec<TelemetrySample> =
            (0..200).map(|i| sample(&t, &ch, i, i as f64)).collect();
        db.insert_batch(&samples).unwrap();

        let stats = db.global_stats().unwrap();
        assert!(stats.total_samples >= 200);
        assert!(stats.db_size_bytes > 0);
        // Sanity: a 200-row WAL db should be at least one page (4KB)
        assert!(stats.db_size_bytes >= 4096);
    }

    /// @test save_layout followed by get_layouts returns the saved
    /// layout with all plot fields (title, channels, height, y bounds,
    /// label, thresholds) preserved.
    #[test]
    fn save_and_load_user_layout_round_trip() {
        let (_dir, db) = open_temp_db();
        let plots = vec![SavedPlot {
            title: "Wave".to_string(),
            channels: vec!["A".to_string(), "B".to_string()],
            height: 200,
            position: [0, 0],
            y_min: Some(-1.0),
            y_max: Some(1.0),
            y_label: Some("V".to_string()),
            thresholds: None,
        }];
        let id = db.save_layout("t1", "MyLayout", "1x1", 60, &plots).unwrap();
        assert!(id > 0);

        let loaded = db.get_layouts("t1").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "MyLayout");
        assert_eq!(loaded[0].source, "user");
        assert_eq!(loaded[0].plots.len(), 1);
        assert_eq!(loaded[0].plots[0].title, "Wave");
        assert_eq!(loaded[0].plots[0].channels, vec!["A", "B"]);
        assert_eq!(loaded[0].plots[0].y_min, Some(-1.0));
    }

    /// @test 16 concurrent threads each running 50 query_range calls
    /// against the read pool all succeed without deadlocks. Verifies
    /// the connection pool actually allows parallel reads vs the
    /// previous Mutex<Connection> design.
    #[test]
    fn concurrent_readers_succeed_in_parallel() {
        // Verifies the read pool actually allows concurrent reads. We
        // spawn many threads, each running query_range, and assert they
        // all succeed without deadlocks. With the old Mutex<Connection>
        // they would have serialized; with the pool they run in parallel.
        use std::sync::Arc as StdArc;
        use std::thread;

        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");
        let samples: Vec<TelemetrySample> =
            (0..1000).map(|i| sample(&t, &ch, i, i as f64)).collect();
        db.insert_batch(&samples).unwrap();

        let db = StdArc::new(db);
        let mut handles = Vec::new();
        for _ in 0..16 {
            let db = StdArc::clone(&db);
            handles.push(thread::spawn(move || {
                for _ in 0..50 {
                    let out = db.query_range("t1", Some("X"), 0, 10000, 1000).unwrap();
                    assert_eq!(out.len(), 1000);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    /// @test After many sequential reads, the pool retains at most
    /// READ_POOL_MAX cached connections (no fd leak).
    #[test]
    fn read_pool_returns_connections_for_reuse() {
        // After running many sequential reads, the pool should hold up
        // to READ_POOL_MAX connections (capped). We can't observe pool
        // size directly through public API, but the API can be verified
        // doesn't leak file descriptors by running far more reads than
        // the cap and confirming everything still works.
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");
        db.insert_batch(&[sample(&t, &ch, 100, 1.0)]).unwrap();
        for _ in 0..100 {
            let out = db.query_range("t1", Some("X"), 0, 1000, 10).unwrap();
            assert_eq!(out.len(), 1);
        }
        // Pool internals: at most READ_POOL_MAX connections should be cached
        let pool = db.readers.lock().unwrap();
        assert!(pool.len() <= READ_POOL_MAX);
    }

    /// @test Saving a user layout with the same name as an existing
    /// config-sourced layout returns an error -- prevents the user
    /// from accidentally clobbering shipped layouts.
    #[test]
    fn save_layout_refuses_to_overwrite_config_layout() {
        let (_dir, db) = open_temp_db();
        // Manually plant a config-sourced layout
        {
            let conn = db.writer.lock().unwrap();
            conn.execute(
                "INSERT INTO telemetry_layouts (target_id, name, source, grid, time_window_s, created_at)
                 VALUES ('t1', 'StockLayout', 'config', '1x1', 30, 0)",
                [],
            ).unwrap();
        }
        // Attempt to save a user layout with the same name -- must fail
        let result = db.save_layout("t1", "StockLayout", "1x1", 30, &[]);
        assert!(
            result.is_err(),
            "expected save to refuse config-name collision"
        );
        // Verify the config layout is still intact and still source='config'
        let layouts = db.get_layouts("t1").unwrap();
        let stock = layouts.iter().find(|l| l.name == "StockLayout").unwrap();
        assert_eq!(stock.source, "config");
    }

    /// @test Saving a user layout with a name that doesn't collide
    /// with any config layout succeeds and creates a new row.
    #[test]
    fn save_layout_allows_distinct_user_name() {
        let (_dir, db) = open_temp_db();
        {
            let conn = db.writer.lock().unwrap();
            conn.execute(
                "INSERT INTO telemetry_layouts (target_id, name, source, grid, time_window_s, created_at)
                 VALUES ('t1', 'StockLayout', 'config', '1x1', 30, 0)",
                [],
            ).unwrap();
        }
        // Save with a different name -- should succeed
        let id = db.save_layout("t1", "MyLayout", "1x1", 60, &[]).unwrap();
        assert!(id > 0);
        // Both layouts should now exist
        let layouts = db.get_layouts("t1").unwrap();
        assert_eq!(layouts.len(), 2);
    }

    /// @test Saving a user layout with the same name as an existing
    /// user layout replaces the previous version (intended behavior
    /// for "Save" with the same name).
    #[test]
    fn save_layout_overwrites_own_user_layout_in_place() {
        let (_dir, db) = open_temp_db();
        // Save once
        let plots1 = vec![SavedPlot {
            title: "v1".into(),
            channels: vec!["A".into()],
            height: 100,
            position: [0, 0],
            y_min: None,
            y_max: None,
            y_label: None,
            thresholds: None,
        }];
        db.save_layout("t1", "MyLayout", "1x1", 30, &plots1)
            .unwrap();
        // Save again with same name and different content -- should replace
        let plots2 = vec![SavedPlot {
            title: "v2".into(),
            channels: vec!["B".into()],
            height: 200,
            position: [0, 0],
            y_min: None,
            y_max: None,
            y_label: None,
            thresholds: None,
        }];
        db.save_layout("t1", "MyLayout", "1x1", 30, &plots2)
            .unwrap();
        let layouts = db.get_layouts("t1").unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].plots[0].title, "v2");
        assert_eq!(layouts[0].plots[0].height, 200);
    }

    /// @test delete_layout removes user layouts but refuses to remove
    /// config-sourced layouts (returns false instead of deleting).
    #[test]
    fn delete_user_layout_succeeds_but_config_layout_refuses() {
        let (_dir, db) = open_temp_db();

        // User layout - should be deletable
        let user_id = db.save_layout("t1", "UserL", "1x1", 30, &[]).unwrap();
        assert!(db.delete_layout(user_id).unwrap());
        assert!(db.get_layouts("t1").unwrap().is_empty());

        // Manually insert a config-sourced layout to simulate the seed path
        {
            let conn = db.writer.lock().unwrap();
            conn.execute(
                "INSERT INTO telemetry_layouts (target_id, name, source, grid, time_window_s, created_at)
                 VALUES ('t1', 'ConfigL', 'config', '1x1', 30, 0)",
                [],
            ).unwrap();
        }
        let config_id: i64 = {
            let conn = db.writer.lock().unwrap();
            conn.query_row(
                "SELECT id FROM telemetry_layouts WHERE name='ConfigL'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        // Should refuse and return false
        assert!(!db.delete_layout(config_id).unwrap());
        assert_eq!(db.get_layouts("t1").unwrap().len(), 1);
    }

    /// @test probe_writable succeeds on a healthy database and does not
    /// disturb data or leave a transaction open (a follow-up write on
    /// the same connection works).
    #[test]
    fn probe_writable_on_healthy_db() {
        let (_dir, db) = open_temp_db();
        db.probe_writable().unwrap();

        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X.f");
        db.insert_batch(&[sample(&t, &ch, 100, 1.0)]).unwrap();
        assert_eq!(db.count().unwrap(), 1);
        db.probe_writable().unwrap();
    }

    fn telemetry_config(
        layouts: &[(&str, &[&str])],
    ) -> crate::core::config_manager::TelemetryConfig {
        use crate::core::config_manager::{PlotDef, TelemetryConfig, TelemetryLayout};
        TelemetryConfig {
            layouts: layouts
                .iter()
                .map(|(name, channels)| TelemetryLayout {
                    name: name.to_string(),
                    plots: vec![PlotDef {
                        title: format!("{name} plot"),
                        channels: channels.iter().map(|c| c.to_string()).collect(),
                        height: 180,
                    }],
                })
                .collect(),
        }
    }

    /// @test seed_layouts mirrors the config file: a renamed layout in
    /// the file replaces the old config row (stale one deleted), a
    /// content change updates plots in place, and user layouts survive
    /// every resync untouched.
    #[test]
    fn seed_layouts_syncs_config_source_with_file() {
        let (_dir, db) = open_temp_db();

        // Initial file: two config layouts; plus one user layout.
        db.seed_layouts(
            "t1",
            &telemetry_config(&[("Waves", &["A.x"]), ("Health", &["B.y"])]),
        )
        .unwrap();
        db.save_layout("t1", "MyCustom", "1x1", 30, &[]).unwrap();
        assert_eq!(db.get_layouts("t1").unwrap().len(), 3);

        // File evolves: Health renamed to Monitor, Waves gains a channel.
        db.seed_layouts(
            "t1",
            &telemetry_config(&[("Waves", &["A.x", "A.z"]), ("Monitor", &["B.y"])]),
        )
        .unwrap();

        let layouts = db.get_layouts("t1").unwrap();
        let names: Vec<&str> = layouts.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"Waves") && names.contains(&"Monitor"));
        assert!(!names.contains(&"Health"), "stale config layout must go");
        assert!(names.contains(&"MyCustom"), "user layout must survive");

        let waves = layouts.iter().find(|l| l.name == "Waves").unwrap();
        assert_eq!(waves.plots[0].channels, vec!["A.x", "A.z"]);
    }

    /// @test A user-saved layout owns its name: a config layout with
    /// the same name in the file is skipped, never clobbers the user's.
    #[test]
    fn seed_layouts_user_name_wins_over_file_default() {
        let (_dir, db) = open_temp_db();
        let plots = vec![SavedPlot {
            title: "mine".into(),
            channels: vec!["C.custom".into()],
            height: 200,
            position: [0, 0],
            y_min: None,
            y_max: None,
            y_label: None,
            thresholds: None,
        }];
        db.save_layout("t1", "Waves", "1x1", 30, &plots).unwrap();

        db.seed_layouts("t1", &telemetry_config(&[("Waves", &["A.x"])]))
            .unwrap();

        let layouts = db.get_layouts("t1").unwrap();
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].source, "user");
        assert_eq!(layouts[0].plots[0].channels, vec!["C.custom"]);
    }

    /// @test prune_audit removes rows strictly older than the cutoff
    /// and leaves newer rows intact.
    #[test]
    fn prune_audit_removes_only_old_rows() {
        let (_dir, db) = open_temp_db();
        {
            let conn = db.writer.lock().unwrap();
            for (ts, action) in [(1_000i64, "old"), (2_000, "old2"), (9_000, "new")] {
                conn.execute(
                    "INSERT INTO audit_log (ts_ms, actor, action, status) VALUES (?1, 'op', ?2, 'ok')",
                    params![ts, action],
                )
                .unwrap();
            }
        }
        assert_eq!(db.prune_audit(5_000).unwrap(), 2);
        let remaining = db.query_audit(10, 0).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    /// @test prune retains the newest 100 user layouts per target, not
    /// globally: a target with few layouts is untouched by another
    /// target's overflow.
    #[test]
    fn prune_layout_retention_is_per_target() {
        let (_dir, db) = open_temp_db();
        for i in 0..105 {
            db.save_layout("t1", &format!("L{i}"), "1x1", 30, &[])
                .unwrap();
        }
        for i in 0..5 {
            db.save_layout("t2", &format!("M{i}"), "1x1", 30, &[])
                .unwrap();
        }

        // Retention window large enough that no samples are pruned; only
        // the layout-retention pass acts.
        db.prune(u64::MAX).unwrap();

        assert_eq!(db.get_layouts("t1").unwrap().len(), 100);
        assert_eq!(db.get_layouts("t2").unwrap().len(), 5);
    }

    /// @test delete_oldest removes telemetry_latest rows only for channels
    /// whose history is entirely gone; channels with remaining rows keep
    /// their latest value.
    #[test]
    fn delete_oldest_cleans_latest_only_for_emptied_channels() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let a: Arc<str> = Arc::from("A.old");
        let b: Arc<str> = Arc::from("B.live");
        db.insert_batch(&[
            sample(&t, &a, 100, 1.0),
            sample(&t, &a, 200, 2.0),
            sample(&t, &b, 300, 3.0),
            sample(&t, &b, 400, 4.0),
        ])
        .unwrap();
        assert_eq!(db.query_latest("t1").unwrap().len(), 2);

        // Deleting the two oldest wipes channel A's history entirely.
        db.delete_oldest(2).unwrap();
        let latest = db.query_latest("t1").unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(&*latest[0].channel, "B.live");
    }

    /// @test global_stats total_samples reflects live rows, not the rowid
    /// high-water mark: after a FIFO deletion the count drops by the number
    /// of deleted rows. Guards the maintenance loop's eviction sizing,
    /// which divides db bytes by this value.
    #[test]
    fn global_stats_counts_live_rows_after_deletion() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X.f");
        let samples: Vec<_> = (0..100u64).map(|i| sample(&t, &ch, i, i as f64)).collect();
        db.insert_batch(&samples).unwrap();
        assert_eq!(db.global_stats().unwrap().total_samples, 100);

        db.delete_oldest(60).unwrap();
        assert_eq!(db.global_stats().unwrap().total_samples, 40);
    }

    /// @test A failed insert_batch rolls back completely (no partial rows
    /// from the main-table inserts) and leaves the writer connection
    /// usable: the next insert_batch succeeds. Guards against the raw
    /// BEGIN leaking an open transaction, which wedged every subsequent
    /// insert with "cannot start a transaction within a transaction".
    #[test]
    fn insert_batch_failure_rolls_back_and_recovers() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X.field");
        let samples = vec![sample(&t, &ch, 100, 1.0), sample(&t, &ch, 200, 2.0)];

        // Sabotage the second statement in the transaction: renaming
        // telemetry_latest makes the UPSERT fail after the main-table
        // inserts have already executed.
        {
            let conn = db.writer.lock().unwrap();
            conn.execute_batch("ALTER TABLE telemetry_latest RENAME TO tl_sabotaged")
                .unwrap();
        }
        assert!(db.insert_batch(&samples).is_err());
        {
            let conn = db.writer.lock().unwrap();
            conn.execute_batch("ALTER TABLE tl_sabotaged RENAME TO telemetry_latest")
                .unwrap();
        }

        // Rollback must have removed the partial main-table inserts.
        assert_eq!(db.count().unwrap(), 0);

        // The connection must be reusable: no open transaction left behind.
        db.insert_batch(&samples).unwrap();
        assert_eq!(db.count().unwrap(), 2);
        let latest = db.query_latest("t1").unwrap();
        assert_eq!(latest.len(), 1);
    }

    /// @test The standard pragmas set a nonzero busy_timeout on both the
    /// writer connection and pooled reader connections, so transient
    /// SQLITE_BUSY during checkpoints/vacuum retries instead of failing
    /// the ingest path.
    #[test]
    fn busy_timeout_applied_to_writer_and_readers() {
        let (_dir, db) = open_temp_db();

        let writer_timeout: i64 = {
            let conn = db.writer.lock().unwrap();
            conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                .unwrap()
        };
        assert!(writer_timeout >= 1000, "writer busy_timeout too low");

        let reader_timeout: i64 = db
            .with_reader(|conn| Ok(conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?))
            .unwrap();
        assert!(reader_timeout >= 1000, "reader busy_timeout too low");
    }

    fn counts(v: &[(&str, u64)]) -> Vec<(String, u64)> {
        v.iter().map(|(t, c)| (t.to_string(), *c)).collect()
    }

    /// @test Fair eviction trims only the largest holder when its
    /// excess covers the whole volume; the quiet target loses nothing.
    #[test]
    fn allocate_evictions_spares_quiet_targets() {
        let out = allocate_evictions(&counts(&[("big", 80), ("small", 20)]), 40);
        assert_eq!(out, vec![("big".to_string(), 40)]);
        let out = allocate_evictions(&counts(&[("big", 100), ("small", 5)]), 50);
        assert_eq!(out, vec![("big".to_string(), 50)]);
    }

    /// @test Equal holders split the eviction volume equally, and the
    /// integer remainder is shaved so allocations sum exactly.
    #[test]
    fn allocate_evictions_splits_equals_and_sums_exactly() {
        let out = allocate_evictions(&counts(&[("a", 50), ("b", 50)]), 20);
        assert_eq!(out.iter().map(|(_, d)| d).sum::<u64>(), 20);
        assert!(out.iter().all(|(_, d)| *d == 10));

        let out = allocate_evictions(&counts(&[("a", 50), ("b", 50)]), 21);
        assert_eq!(out.iter().map(|(_, d)| d).sum::<u64>(), 21);

        let out = allocate_evictions(&counts(&[("a", 5), ("b", 5), ("c", 5)]), 1);
        assert_eq!(out.iter().map(|(_, d)| d).sum::<u64>(), 1);
    }

    /// @test A waterline between targets trims each proportionally to
    /// its excess: mixed sizes converge to a common survivor level.
    #[test]
    fn allocate_evictions_waterlines_mixed_sizes() {
        // 7/3/3 evicting 6: waterline 2 -> allocations 5/1/1 minus one
        // shaved row = exactly 6, and every survivor holds >= 2.
        let out = allocate_evictions(&counts(&[("a", 7), ("b", 3), ("c", 3)]), 6);
        assert_eq!(out.iter().map(|(_, d)| d).sum::<u64>(), 6);
        let get = |t: &str| {
            out.iter()
                .find(|(n, _)| n == t)
                .map(|(_, d)| *d)
                .unwrap_or(0)
        };
        assert!(get("a") >= 4);
        assert!(get("b") <= 1 && get("c") <= 1);
    }

    /// @test Requesting at least the total row count returns every
    /// nonzero target in full; zero requests return nothing.
    #[test]
    fn allocate_evictions_edges() {
        let all = allocate_evictions(&counts(&[("a", 10), ("b", 0), ("c", 3)]), 999);
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().map(|(_, d)| d).sum::<u64>(), 13);
        assert!(allocate_evictions(&counts(&[("a", 10)]), 0).is_empty());
        assert!(allocate_evictions(&[], 5).is_empty());
    }

    /// @test target_sample_counts reports live per-target row counts.
    #[test]
    fn target_sample_counts_reports_live_rows() {
        let (_dir, db) = open_temp_db();
        let t1: Arc<str> = Arc::from("t1");
        let t2: Arc<str> = Arc::from("t2");
        let ch: Arc<str> = Arc::from("X");
        let mut samples = Vec::new();
        for i in 0..30u64 {
            samples.push(sample(&t1, &ch, i, 1.0));
        }
        for i in 0..10u64 {
            samples.push(sample(&t2, &ch, i, 2.0));
        }
        db.insert_batch(&samples).unwrap();
        let mut out = db.target_sample_counts().unwrap();
        out.sort();
        assert_eq!(out, vec![("t1".to_string(), 30), ("t2".to_string(), 10)]);
    }

    /// @test A transient spike survives both ladder transitions as a
    /// min/max excursion: the tier-1 bucket keeps max=spike, and
    /// re-tiering to the coarse tier keeps it again while the mean
    /// dampens. This is the ticket's core acceptance.
    #[test]
    fn spike_survives_both_tier_transitions() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");
        // 100 samples at 1.0, one spike of 100.0 in the middle,
        // 100 ms apart starting at t=0.
        let mut samples: Vec<TelemetrySample> =
            (0..100u64).map(|i| sample(&t, &ch, i * 100, 1.0)).collect();
        samples[50].value = 100.0;
        db.insert_batch(&samples).unwrap();

        // Tier 1: 1 s buckets over everything.
        let pass = db.tier_pass(MID_TIER, 20_000, 1_000, u64::MAX).unwrap();
        assert_eq!(pass.source_rows, 100);
        assert_eq!(pass.bucket_rows, 10);

        let mid = db.query_range("t1", Some("X"), 0, 20_000, 100).unwrap();
        assert_eq!(mid.len(), 10);
        let spike_bucket = mid.iter().find(|s| s.timestamp_ms == 5_500).unwrap();
        let env = spike_bucket.envelope.unwrap();
        assert_eq!(env.max, 100.0);
        assert_eq!(env.min, 1.0);
        assert_eq!(env.count, 10);
        // Mean dampened: (9*1 + 100)/10 = 10.9
        assert!((spike_bucket.value - 10.9).abs() < 1e-9);

        // Tier 2: 10 s buckets over the tier-1 rows.
        let pass = db.tier_pass(COARSE_TIER, 20_000, 10_000, u64::MAX).unwrap();
        assert_eq!(pass.source_rows, 10);
        assert_eq!(pass.bucket_rows, 1);

        let coarse = db.query_range("t1", Some("X"), 0, 20_000, 100).unwrap();
        assert_eq!(coarse.len(), 1);
        let env = coarse[0].envelope.unwrap();
        assert_eq!(env.max, 100.0, "spike must survive the second hop");
        assert_eq!(env.min, 1.0);
        assert_eq!(env.count, 100);
        // Weighted mean over all 100 source samples: (99 + 100)/100.
        assert!((coarse[0].value - 1.99).abs() < 1e-9);
    }

    /// @test Re-tiering weights bucket means by their source counts:
    /// a 9-sample bucket pulls the coarse mean 9x harder than a
    /// 1-sample bucket.
    #[test]
    fn tier_pass_weights_means_by_count() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");
        // Bucket A (0..1s): nine samples at 10.0. Bucket B (1..2s):
        // one sample at 20.0.
        let mut samples: Vec<TelemetrySample> =
            (0..9u64).map(|i| sample(&t, &ch, i * 100, 10.0)).collect();
        samples.push(sample(&t, &ch, 1_500, 20.0));
        db.insert_batch(&samples).unwrap();

        db.tier_pass(MID_TIER, 10_000, 1_000, u64::MAX).unwrap();
        db.tier_pass(COARSE_TIER, 10_000, 10_000, u64::MAX).unwrap();

        let out = db.query_range("t1", Some("X"), 0, 10_000, 10).unwrap();
        assert_eq!(out.len(), 1);
        // Weighted: (9*10 + 1*20)/10 = 11.0 -- NOT (10+20)/2.
        assert!((out[0].value - 11.0).abs() < 1e-9);
        assert_eq!(out[0].envelope.unwrap().count, 10);
    }

    /// @test tier_pass processes a bounded window per call and leaves
    /// rows newer than the cutoff untouched, converging over repeated
    /// calls -- the property that keeps one tick from rewriting a full
    /// backlog under the writer lock.
    #[test]
    fn tier_pass_is_bounded_and_respects_cutoff() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");
        // 30 minutes of 1 Hz data; cutoff protects the newest 10 min.
        let samples: Vec<TelemetrySample> = (0..1800u64)
            .map(|i| sample(&t, &ch, i * 1_000, i as f64))
            .collect();
        db.insert_batch(&samples).unwrap();
        let cutoff = 20 * 60 * 1_000;

        // Slice of 10 min: first pass consumes exactly that window.
        let p1 = db.tier_pass(MID_TIER, cutoff, 60_000, 600_000).unwrap();
        assert_eq!(p1.source_rows, 600);
        assert_eq!(p1.bucket_rows, 10);

        let p2 = db.tier_pass(MID_TIER, cutoff, 60_000, 600_000).unwrap();
        assert_eq!(p2.source_rows, 600);

        // Converged: everything below the cutoff is tiered.
        let p3 = db.tier_pass(MID_TIER, cutoff, 60_000, 600_000).unwrap();
        assert_eq!(p3.source_rows, 0);

        // The protected window is untouched full-res.
        let recent = db
            .query_range("t1", Some("X"), cutoff, 3_600_000, 2000)
            .unwrap();
        assert_eq!(recent.len(), 600);
        assert!(recent.iter().all(|s| s.envelope.is_none()));
    }

    /// @test A dictionary-schema database from before the tier columns
    /// opens cleanly, gains them in place, and its old rows read as
    /// full-resolution samples with no envelope.
    #[test]
    fn pre_tier_schema_gains_columns_in_place() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v2.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE channels (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    target_id TEXT NOT NULL, name TEXT NOT NULL,
                    UNIQUE(target_id, name));
                 CREATE TABLE telemetry (
                    id INTEGER PRIMARY KEY,
                    channel_id INTEGER NOT NULL,
                    timestamp_ms INTEGER NOT NULL,
                    value REAL NOT NULL);
                 CREATE INDEX idx_telemetry_chan_ts
                    ON telemetry (channel_id, timestamp_ms DESC);
                 CREATE INDEX idx_telemetry_ts
                    ON telemetry (timestamp_ms ASC);
                 INSERT INTO channels (target_id, name) VALUES ('t1', 'X');
                 INSERT INTO telemetry (channel_id, timestamp_ms, value)
                    VALUES (1, 100, 42.0);",
            )
            .unwrap();
        }
        let db = TelemetryDb::open(&path).unwrap();
        let out = db.query_range("t1", Some("X"), 0, 1_000, 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 42.0);
        assert!(out[0].envelope.is_none());
        // And the ladder can process the migrated rows.
        let pass = db.tier_pass(COARSE_TIER, 1_000, 1_000, u64::MAX).unwrap();
        assert_eq!(pass.source_rows, 1);
    }

    /// @test audit_count reports the live number of audit rows.
    #[test]
    fn audit_count_reports_rows() {
        let (_dir, db) = open_temp_db();
        assert_eq!(db.audit_count().unwrap(), 0);
        db.log_audit("op", "a", None, None, "ok", None).unwrap();
        db.log_audit("op", "b", None, None, "ok", None).unwrap();
        assert_eq!(db.audit_count().unwrap(), 2);
    }

    /// @test The channel dictionary stores one row per distinct
    /// (target, channel) pair no matter how many samples reference it.
    #[test]
    fn channel_dictionary_dedups_names() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let a: Arc<str> = Arc::from("A.x");
        let b: Arc<str> = Arc::from("B.y");
        for i in 0..10u64 {
            db.insert_batch(&[sample(&t, &a, i, 1.0), sample(&t, &b, i, 2.0)])
                .unwrap();
        }
        assert_eq!(db.count().unwrap(), 20);
        let dict_rows: i64 = db
            .with_reader(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM channels", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(dict_rows, 2);
    }

    /// @test Opening a database created with the legacy denormalized
    /// schema rebuilds it into the dictionary schema with every row,
    /// channel name, and query shape preserved, and accepts new writes.
    #[test]
    fn legacy_schema_migrates_to_dictionary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE telemetry (
                    id INTEGER PRIMARY KEY,
                    target_id TEXT NOT NULL,
                    timestamp_ms INTEGER NOT NULL,
                    channel TEXT NOT NULL,
                    value REAL NOT NULL
                );
                CREATE INDEX idx_telemetry_lookup
                    ON telemetry (target_id, channel, timestamp_ms DESC);
                CREATE INDEX idx_telemetry_timestamp_asc
                    ON telemetry (timestamp_ms ASC);",
            )
            .unwrap();
            for i in 0..100i64 {
                let (target, channel) = if i % 2 == 0 {
                    ("t1", "A.x")
                } else {
                    ("t2", "B.y")
                };
                conn.execute(
                    "INSERT INTO telemetry (target_id, timestamp_ms, channel, value)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![target, i, channel, i as f64],
                )
                .unwrap();
            }
        }

        let db = TelemetryDb::open(&path).unwrap();
        assert_eq!(db.count().unwrap(), 100);

        let t1 = db.query_range("t1", Some("A.x"), 0, 1000, 200).unwrap();
        assert_eq!(t1.len(), 50);
        assert_eq!(t1[0].timestamp_ms, 0);
        assert_eq!(t1[0].value, 0.0);

        let t2_all = db.query_range("t2", None, 0, 1000, 200).unwrap();
        assert_eq!(t2_all.len(), 50);
        assert_eq!(&*t2_all[0].channel, "B.y");

        let dict_rows: i64 = db
            .with_reader(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM channels", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(dict_rows, 2);

        // The migrated database keeps working as a live store.
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("A.x");
        db.insert_batch(&[sample(&t, &ch, 2000, 42.0)]).unwrap();
        assert_eq!(db.count().unwrap(), 101);
        let latest = db.query_latest("t1").unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].value, 42.0);
    }

    /// @test A rolled-back batch must not leave the id of its brand-new
    /// dictionary entry in the channel cache: the entry died with the
    /// rollback, and a cached copy would point every later sample for
    /// that channel at a dead id (invisible to queries).
    #[test]
    fn failed_batch_does_not_poison_channel_cache() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("N.fresh");

        // Sabotage the latest-table UPSERT so the batch fails after the
        // dictionary insert for the never-seen channel has run.
        {
            let conn = db.writer.lock().unwrap();
            conn.execute_batch("ALTER TABLE telemetry_latest RENAME TO tl_sabotaged")
                .unwrap();
        }
        assert!(db.insert_batch(&[sample(&t, &ch, 100, 1.0)]).is_err());
        {
            let conn = db.writer.lock().unwrap();
            conn.execute_batch("ALTER TABLE tl_sabotaged RENAME TO telemetry_latest")
                .unwrap();
        }

        // Retry resolves a live dictionary id; the row must be visible
        // through the (target, channel) query path.
        db.insert_batch(&[sample(&t, &ch, 200, 2.0)]).unwrap();
        let out = db.query_range("t1", Some("N.fresh"), 0, 1000, 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].timestamp_ms, 200);
        assert_eq!(db.query_latest("t1").unwrap().len(), 1);
    }
}
