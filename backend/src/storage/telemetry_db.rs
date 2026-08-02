//! SQLite telemetry storage.
//!
//! Stores telemetry samples in a SQLite database with WAL mode for
//! concurrent read/write. Provides time-range queries and CSV export.
//!
//! Performance tuning:
//!   - WAL mode for non-blocking reads during writes
//!   - Prepared statement caching for hot paths
//!   - Lookup index on (target_id, channel, timestamp_ms DESC)
//!   - Separate latest-value table for O(1) latest queries
//!   - Batch inserts within a single transaction
//!   - page_size=4096, mmap_size=256MB for large dataset performance

use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
///   query itself runs without any zenith-side lock contention.
///
/// This replaces the previous single-Mutex<Connection> design which
/// serialized all reads behind writes. With 4 targets x 50 channel polls
/// per cycle that was 200 serial round trips per cycle.
pub struct TelemetryDb {
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    db_path: PathBuf,
}

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

        // Main telemetry table
        writer.execute_batch(
            "CREATE TABLE IF NOT EXISTS telemetry (
                id INTEGER PRIMARY KEY,
                target_id TEXT NOT NULL,
                timestamp_ms INTEGER NOT NULL,
                channel TEXT NOT NULL,
                value REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_telemetry_lookup
                ON telemetry (target_id, channel, timestamp_ms DESC);
            CREATE INDEX IF NOT EXISTS idx_telemetry_timestamp_asc
                ON telemetry (timestamp_ms ASC);",
        )?;

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
        })
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

        // Insert into main table
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO telemetry (target_id, timestamp_ms, channel, value) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for s in samples {
                stmt.execute(params![
                    &*s.target_id,
                    s.timestamp_ms as i64,
                    &*s.channel,
                    s.value
                ])?;
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
        // Build the query parameters once, then run on a pooled reader.
        // Multiple query_range calls can now run concurrently across the
        // pool instead of serializing on the writer mutex.
        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::types::ToSql + Send>>) =
            if let Some(ch) = channel {
                (
                    "SELECT target_id, timestamp_ms, channel, value FROM telemetry \
                     WHERE target_id = ?1 AND channel = ?2 \
                       AND timestamp_ms >= ?3 AND timestamp_ms <= ?4 \
                     ORDER BY timestamp_ms ASC LIMIT ?5"
                        .to_string(),
                    vec![
                        Box::new(target_id.to_string()),
                        Box::new(ch.to_string()),
                        Box::new(start_ms as i64),
                        Box::new(end_ms as i64),
                        Box::new(limit as i64),
                    ],
                )
            } else {
                (
                    "SELECT target_id, timestamp_ms, channel, value FROM telemetry \
                         WHERE target_id = ?1 \
                           AND timestamp_ms >= ?2 AND timestamp_ms <= ?3 \
                         ORDER BY timestamp_ms DESC LIMIT ?4"
                        .to_string(),
                    vec![
                        Box::new(target_id.to_string()),
                        Box::new(start_ms as i64),
                        Box::new(end_ms as i64),
                        Box::new(limit as i64),
                    ],
                )
            };

        self.with_reader(|conn| {
            let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec
                .iter()
                .map(|p| p.as_ref() as &dyn rusqlite::types::ToSql)
                .collect();
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                let target: String = row.get(0)?;
                let channel: String = row.get(2)?;
                Ok(TelemetrySample {
                    target_id: std::sync::Arc::from(target),
                    timestamp_ms: row.get::<_, i64>(1)? as u64,
                    channel: std::sync::Arc::from(channel),
                    value: row.get(3)?,
                })
            })?;

            let mut samples = Vec::new();
            for row in rows {
                samples.push(row?);
            }
            Ok(samples)
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

        // Clean up old user-created layouts (keep last 100 per target, config layouts always kept)
        conn.execute_batch(
            "DELETE FROM telemetry_plots WHERE layout_id IN (
                SELECT id FROM telemetry_layouts
                WHERE source = 'user'
                AND id NOT IN (
                    SELECT id FROM telemetry_layouts WHERE source = 'user'
                    ORDER BY created_at DESC LIMIT 100
                )
            );
            DELETE FROM telemetry_layouts
            WHERE source = 'user'
            AND id NOT IN (
                SELECT id FROM telemetry_layouts WHERE source = 'user'
                ORDER BY created_at DESC LIMIT 100
            );",
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
    /// obtained via filesystem `stat` (no checkpoint, no lock contention).
    /// Previously this issued `PRAGMA wal_checkpoint(PASSIVE)` on every
    /// call, which turned a stats query into a write-side fsync; that's
    /// gone now. WAL checkpointing happens on its own schedule from the
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
        // history. Correlated NOT EXISTS probes the lookup index once per
        // telemetry_latest row (one row per channel) -- the NOT IN form
        // materialized a DISTINCT over the entire telemetry index on every
        // FIFO pass, under the writer lock.
        conn.execute(
            "DELETE FROM telemetry_latest
             WHERE NOT EXISTS (
               SELECT 1 FROM telemetry
               WHERE telemetry.target_id = telemetry_latest.target_id
                 AND telemetry.channel = telemetry_latest.channel
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
                WHERE target_id = ?1
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
                 SELECT 1 FROM telemetry
                 WHERE telemetry.target_id = telemetry_latest.target_id
                   AND telemetry.channel = telemetry_latest.channel
               )",
            params![target_id],
        )?;

        if deleted > 1000 {
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE); PRAGMA optimize;");
        }

        Ok(deleted)
    }

    /// Reclaim freed pages (works with auto_vacuum=INCREMENTAL).
    pub fn incremental_vacuum(&self) -> Result<(), DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        conn.execute_batch("PRAGMA incremental_vacuum; PRAGMA optimize;")?;
        Ok(())
    }

    /// Get per-target storage statistics.
    pub fn target_stats(&self, target_id: &str) -> Result<TargetStats, DbError> {
        self.with_reader(|conn| {
            let sample_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM telemetry WHERE target_id = ?1",
                    params![target_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            let channel_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM telemetry_latest WHERE target_id = ?1",
                    params![target_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            let oldest_ms: Option<i64> = conn
                .query_row(
                    "SELECT MIN(timestamp_ms) FROM telemetry WHERE target_id = ?1",
                    params![target_id],
                    |row| row.get(0),
                )
                .ok();

            let newest_ms: Option<i64> = conn
                .query_row(
                    "SELECT MAX(timestamp_ms) FROM telemetry WHERE target_id = ?1",
                    params![target_id],
                    |row| row.get(0),
                )
                .ok();

            Ok(TargetStats {
                sample_count: sample_count as u64,
                channel_count: channel_count as u64,
                oldest_ms: oldest_ms.map(|v| v as u64),
                newest_ms: newest_ms.map(|v| v as u64),
                span_seconds: match (oldest_ms, newest_ms) {
                    (Some(o), Some(n)) => Some(((n - o) / 1000) as u64),
                    _ => None,
                },
                byte_estimate: (sample_count as u64) * AVG_BYTES_PER_SAMPLE,
            })
        })
    }

    /// Downsample old data: replace detailed samples with averaged values.
    ///
    /// For data older than `age_ms`, groups samples into `bucket_ms`-wide
    /// buckets and replaces them with averaged values. This reduces storage
    /// while preserving trends.
    ///
    /// Example: downsample(3600000, 60000) keeps last hour at full resolution,
    /// then averages into 1-minute buckets for everything older.
    pub fn downsample(&self, age_ms: u64, bucket_ms: u64) -> Result<DownsampleResult, DbError> {
        let mut conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let cutoff = now_ms.saturating_sub(age_ms) as i64;
        let bucket = bucket_ms as i64;

        // Count samples that will be affected
        let before_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM telemetry WHERE timestamp_ms < ?1",
                params![cutoff],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if before_count == 0 {
            return Ok(DownsampleResult {
                samples_before: 0,
                samples_after: 0,
                removed: 0,
            });
        }

        // RAII transaction for the same reason as insert_batch: an error
        // between DELETE and re-INSERT must roll back atomically, never
        // leave an open transaction on the writer connection (or worse,
        // history deleted without the averaged replacement).
        let tx = conn.transaction()?;

        // Create averaged samples in a temp table
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS ds_temp ( \
                 target_id TEXT, timestamp_ms INTEGER, channel TEXT, value REAL \
             )",
        )?;
        tx.execute("DELETE FROM ds_temp", [])?;

        tx.execute(
            "INSERT INTO ds_temp (target_id, timestamp_ms, channel, value) \
             SELECT target_id, \
                    (timestamp_ms / ?1) * ?1 + (?1 / 2), \
                    channel, \
                    AVG(value) \
             FROM telemetry \
             WHERE timestamp_ms < ?2 \
             GROUP BY target_id, channel, timestamp_ms / ?1",
            params![bucket, cutoff],
        )?;

        let after_count: i64 = tx
            .query_row("SELECT COUNT(*) FROM ds_temp", [], |row| row.get(0))
            .unwrap_or(0);

        // Replace old detailed data with averaged data
        tx.execute(
            "DELETE FROM telemetry WHERE timestamp_ms < ?1",
            params![cutoff],
        )?;

        tx.execute(
            "INSERT INTO telemetry (target_id, timestamp_ms, channel, value) \
             SELECT target_id, timestamp_ms, channel, value FROM ds_temp",
            [],
        )?;

        tx.execute("DROP TABLE IF EXISTS ds_temp", [])?;
        tx.commit()?;

        Ok(DownsampleResult {
            samples_before: before_count as u64,
            samples_after: after_count as u64,
            removed: (before_count - after_count) as u64,
        })
    }

    /// Delete all data for a specific target.
    pub fn delete_target(&self, target_id: &str) -> Result<usize, DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let deleted = conn.execute(
            "DELETE FROM telemetry WHERE target_id = ?1",
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
        let db_bytes = std::fs::metadata(&self.db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let wal_path = {
            let mut p = self.db_path.clone().into_os_string();
            p.push("-wal");
            PathBuf::from(p)
        };
        let wal_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        Ok(db_bytes + wal_bytes)
    }
}

/* ----------------------------- Stats Types ----------------------------- */

/// Aggregate database statistics: total sample count (estimated via
/// MAX(ROWID) for cheapness) plus on-disk size in bytes (main file +
/// WAL sidecar from filesystem stat).
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
    /// Rough estimate of disk bytes occupied by this target's samples.
    /// Computed as sample_count * AVG_BYTES_PER_SAMPLE so it doesn't
    /// require an expensive per-target page-size scan. Treat as
    /// approximate -- the actual on-disk usage is shared across the
    /// whole telemetry table because of WAL/auto_vacuum.
    pub byte_estimate: u64,
}

/// Average per-row footprint of a telemetry sample on disk:
/// 4-byte target_id varint + 8-byte timestamp + ~16-byte channel string +
/// 8-byte float + index entries + page overhead ~= 70 bytes.
/// Tuned empirically against the global_stats output for a 100K-row DB.
const AVG_BYTES_PER_SAMPLE: u64 = 70;

/// Result of a downsample operation.
#[derive(Debug, serde::Serialize)]
pub struct DownsampleResult {
    pub samples_before: u64,
    pub samples_after: u64,
    pub removed: u64,
}

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
    /// Seed layouts from a TelemetryConfig (from telemetry.json).
    /// Only inserts layouts with source='config' that don't already exist.
    pub fn seed_layouts(
        &self,
        target_id: &str,
        config: &crate::core::config_manager::TelemetryConfig,
    ) -> Result<usize, DbError> {
        let conn = self.writer.lock().map_err(|_| DbError::Lock)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut count = 0;
        for layout in &config.layouts {
            // Check if already exists
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM telemetry_layouts WHERE target_id = ?1 AND name = ?2",
                    params![target_id, layout.name],
                    |row| row.get(0),
                )
                .unwrap_or(false);

            if exists {
                continue;
            }

            conn.execute(
                "INSERT INTO telemetry_layouts (target_id, name, source, grid, time_window_s, created_at)
                 VALUES (?1, ?2, 'config', '1x1', 30, ?3)",
                params![target_id, layout.name, now],
            )?;
            let layout_id = conn.last_insert_rowid();

            for (i, plot) in layout.plots.iter().enumerate() {
                let channels_json = serde_json::to_string(&plot.channels).unwrap_or_default();
                conn.execute(
                    "INSERT INTO telemetry_plots (layout_id, title, channels, height, position_row, position_col)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                    params![layout_id, plot.title, channels_json, plot.height, i as i64],
                )?;
            }
            count += 1;
        }

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

    /// @test target_stats.byte_estimate scales linearly with the
    /// sample count via the AVG_BYTES_PER_SAMPLE constant.
    #[test]
    fn target_stats_byte_estimate_scales_with_samples() {
        let (_dir, db) = open_temp_db();
        let t: Arc<str> = Arc::from("t1");
        let ch: Arc<str> = Arc::from("X");
        let samples: Vec<TelemetrySample> =
            (0..100).map(|i| sample(&t, &ch, i, i as f64)).collect();
        db.insert_batch(&samples).unwrap();
        let stats = db.target_stats("t1").unwrap();
        assert_eq!(stats.sample_count, 100);
        assert!(stats.byte_estimate > 0);
        assert_eq!(stats.byte_estimate, 100 * AVG_BYTES_PER_SAMPLE);
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
}
