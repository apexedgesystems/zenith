//! Per-target pipeline counters.
//!
//! Every stage of the telemetry pipeline that can drop data or fail
//! does so through a counter here instead of silently. Plain atomics:
//! incremented from the router / DB writer / WebSocket / client tasks
//! without locking, snapshotted by status endpoints without touching
//! the hot path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Default)]
pub struct TargetMetrics {
    /// Samples produced by the decoder (post-decode, pre-dedup).
    pub decoded_samples: AtomicU64,
    /// Samples dropped by the per-channel min-interval dedup.
    pub dedup_drops: AtomicU64,
    /// Push packets missed by the router (broadcast lag).
    pub router_lag_drops: AtomicU64,
    /// Samples missed by the DB writer (broadcast lag).
    pub db_writer_lag_drops: AtomicU64,
    /// Samples committed to the DB.
    pub db_written_samples: AtomicU64,
    /// Batches that failed to insert.
    pub db_write_failures: AtomicU64,
    /// Samples lost inside failed batches.
    pub db_failed_samples: AtomicU64,
    /// Samples missed by WebSocket subscribers (broadcast lag, summed
    /// over all clients).
    pub ws_lag_drops: AtomicU64,
    /// Currently connected WebSocket subscribers.
    pub ws_clients: AtomicU64,
    /// Commands written to the target.
    pub commands_sent: AtomicU64,
    /// Commands that failed (send failure, closed, or error response
    /// path -- timeouts counted separately below as well as here).
    pub command_errors: AtomicU64,
    /// Commands that timed out awaiting their ACK.
    pub command_timeouts: AtomicU64,
    /// Sum of round-trip latency over successful commands, microseconds.
    pub command_latency_us_total: AtomicU64,
    /// Wall-clock ms timestamp of the most recent decoded sample
    /// (0 = never).
    pub last_sample_ms: AtomicU64,
}

impl TargetMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Point-in-time JSON snapshot. `now_ms` supplies the reference for
    /// last_sample_age_ms so callers control the clock.
    pub fn snapshot(&self, now_ms: u64) -> serde_json::Value {
        let last = self.last_sample_ms.load(Ordering::Relaxed);
        let sent = self.commands_sent.load(Ordering::Relaxed);
        let errors = self.command_errors.load(Ordering::Relaxed);
        let lat_total = self.command_latency_us_total.load(Ordering::Relaxed);
        let ok = sent.saturating_sub(errors);
        serde_json::json!({
            "decoded_samples": self.decoded_samples.load(Ordering::Relaxed),
            "dedup_drops": self.dedup_drops.load(Ordering::Relaxed),
            "router_lag_drops": self.router_lag_drops.load(Ordering::Relaxed),
            "db_writer_lag_drops": self.db_writer_lag_drops.load(Ordering::Relaxed),
            "db_written_samples": self.db_written_samples.load(Ordering::Relaxed),
            "db_write_failures": self.db_write_failures.load(Ordering::Relaxed),
            "db_failed_samples": self.db_failed_samples.load(Ordering::Relaxed),
            "ws_lag_drops": self.ws_lag_drops.load(Ordering::Relaxed),
            "ws_clients": self.ws_clients.load(Ordering::Relaxed),
            "commands_sent": sent,
            "command_errors": errors,
            "command_timeouts": self.command_timeouts.load(Ordering::Relaxed),
            "command_latency_avg_us": lat_total.checked_div(ok).unwrap_or(0),
            "last_sample_age_ms": if last > 0 { Some(now_ms.saturating_sub(last)) } else { None },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// @test snapshot reports raw counters, derives average command
    /// latency over successful commands only, and reports a null
    /// last_sample_age_ms until a sample has been seen.
    #[test]
    fn snapshot_derives_average_and_age() {
        let m = TargetMetrics::new();
        assert_eq!(
            m.snapshot(10_000)["last_sample_age_ms"],
            serde_json::Value::Null
        );

        m.commands_sent.store(4, Ordering::Relaxed);
        m.command_errors.store(1, Ordering::Relaxed);
        m.command_latency_us_total.store(3000, Ordering::Relaxed);
        m.last_sample_ms.store(9_000, Ordering::Relaxed);

        let snap = m.snapshot(10_000);
        assert_eq!(snap["command_latency_avg_us"], 1000); // 3000us / 3 ok
        assert_eq!(snap["last_sample_age_ms"], 1000);
        assert_eq!(snap["commands_sent"], 4);
    }
}
