use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use pprof::criterion::{Output, PProfProfiler};
use tempfile::TempDir;

use zenith::core::telemetry::TelemetrySample;
use zenith::storage::telemetry_db::TelemetryDb;

/// Single-channel batch (worst case for the latest-table UPSERT loop --
/// every row hits the same row in telemetry_latest).
fn make_samples(n: usize) -> Vec<TelemetrySample> {
    let target: Arc<str> = Arc::from("target-0");
    let channel: Arc<str> = Arc::from("WaveGen#0.output");
    (0..n)
        .map(|i| TelemetrySample {
            target_id: target.clone(),
            timestamp_ms: 1700000000000 + i as u64,
            channel: channel.clone(),
            value: (i as f64 * 0.01).sin(),
            envelope: None,
        })
        .collect()
}

/// Realistic batch: 1 target, K distinct channels, N/K samples each.
/// Mirrors actual production batches where the writer task accumulates
/// samples for ~50ms across all channels.
fn make_realistic_batch(n: usize, channels: usize) -> Vec<TelemetrySample> {
    let target: Arc<str> = Arc::from("target-0");
    // Pre-build channel arcs once
    let channel_arcs: Vec<Arc<str>> = (0..2)
        .flat_map(|wg| {
            (0..channels)
                .map(move |c| Arc::<str>::from(format!("WaveGen#{}.field{}", wg, c).as_str()))
        })
        .collect();
    (0..n)
        .map(|i| {
            let ch_idx = (i % 2) * channels + ((i / 2) % channels);
            TelemetrySample {
                target_id: target.clone(),
                timestamp_ms: 1700000000000 + (i as u64),
                channel: channel_arcs[ch_idx].clone(),
                value: (i as f64 * 0.01).sin(),
                envelope: None,
            }
        })
        .collect()
}

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("sqlite_insert");

    // Batch insert: 50 samples (current batch size)
    group.throughput(Throughput::Elements(50));
    group.bench_function("batch_50", |b| {
        let dir = TempDir::new().unwrap();
        let db = TelemetryDb::open(&dir.path().join("bench.db")).unwrap();
        let samples = make_samples(50);
        b.iter(|| {
            db.insert_batch(&samples).unwrap();
        })
    });

    // Batch insert: 1 sample (worst case)
    group.throughput(Throughput::Elements(1));
    group.bench_function("batch_1", |b| {
        let dir = TempDir::new().unwrap();
        let db = TelemetryDb::open(&dir.path().join("bench.db")).unwrap();
        let samples = make_samples(1);
        b.iter(|| {
            db.insert_batch(&samples).unwrap();
        })
    });

    // Batch insert: 200 samples
    group.throughput(Throughput::Elements(200));
    group.bench_function("batch_200", |b| {
        let dir = TempDir::new().unwrap();
        let db = TelemetryDb::open(&dir.path().join("bench.db")).unwrap();
        let samples = make_samples(200);
        b.iter(|| {
            db.insert_batch(&samples).unwrap();
        })
    });

    // Realistic: 50 samples spread across 10 channels (5 each).
    // Most representative of actual production batch flushes.
    group.throughput(Throughput::Elements(50));
    group.bench_function("realistic_50_across_10ch", |b| {
        let dir = TempDir::new().unwrap();
        let db = TelemetryDb::open(&dir.path().join("bench.db")).unwrap();
        let samples = make_realistic_batch(50, 10);
        b.iter(|| {
            db.insert_batch(&samples).unwrap();
        })
    });

    // Realistic: 200 samples spread across 20 channels (10 each).
    group.throughput(Throughput::Elements(200));
    group.bench_function("realistic_200_across_20ch", |b| {
        let dir = TempDir::new().unwrap();
        let db = TelemetryDb::open(&dir.path().join("bench.db")).unwrap();
        let samples = make_realistic_batch(200, 20);
        b.iter(|| {
            db.insert_batch(&samples).unwrap();
        })
    });

    group.finish();
}

fn bench_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("sqlite_query");

    // Setup: insert 100K samples
    let dir = TempDir::new().unwrap();
    let db = TelemetryDb::open(&dir.path().join("bench.db")).unwrap();
    let samples = make_samples(100_000);
    for chunk in samples.chunks(1000) {
        db.insert_batch(chunk).unwrap();
    }

    // What each history shape returns from this database, once, before
    // timing: rows, the first and last timestamps (ms after the first
    // sample), and whether they come in time order.
    for (name, channel, limit) in [
        ("history_100", None, 100),
        ("history_1000", None, 1000),
        ("history_1ch_100", Some("WaveGen#0.output"), 100),
        ("history_1ch_50000", Some("WaveGen#0.output"), 50_000),
    ] {
        let rows = db
            .query_range("target-0", channel, 0, 9999999999999, limit)
            .unwrap();
        let ts: Vec<u64> = rows
            .iter()
            .map(|s| s.timestamp_ms - 1700000000000)
            .collect();
        eprintln!(
            "work: sqlite_query/{name} rows={} first={:?} last={:?} time_order={}",
            rows.len(),
            ts.first(),
            ts.last(),
            ts.windows(2).all(|w| w[0] < w[1])
        );
    }

    group.bench_function("latest", |b| {
        b.iter(|| {
            db.query_latest("target-0").unwrap();
        })
    });

    group.bench_function("history_100", |b| {
        b.iter(|| {
            db.query_range("target-0", None, 0, 9999999999999, 100)
                .unwrap();
        })
    });

    group.bench_function("history_1000", |b| {
        b.iter(|| {
            db.query_range("target-0", None, 0, 9999999999999, 1000)
                .unwrap();
        })
    });

    // One channel, as a chart's history poll reads it: the dictionary
    // probe, then that channel's index run, at a limit of 100 rows and
    // at the 50,000 the console asks for.
    group.bench_function("history_1ch_100", |b| {
        b.iter(|| {
            db.query_range("target-0", Some("WaveGen#0.output"), 0, 9999999999999, 100)
                .unwrap();
        })
    });

    group.bench_function("history_1ch_50000", |b| {
        b.iter(|| {
            db.query_range(
                "target-0",
                Some("WaveGen#0.output"),
                0,
                9999999999999,
                50_000,
            )
            .unwrap();
        })
    });

    group.bench_function("count", |b| {
        b.iter(|| {
            db.count().unwrap();
        })
    });

    group.finish();
}

/// One step of the retention ladder as the maintenance loop takes it,
/// tier_pass(MID_TIER, cutoff, 1 s buckets, 600 s slice), over a fresh
/// copy of one seeded database per iteration: two channels of 50,000
/// samples each, 10 ms apart, from a bucket boundary. The cutoff falls
/// 400 s in, on a bucket boundary or halfway into a bucket, and the
/// slice reaches past it, so the pass converts the buckets before the
/// cutoff. Seeding, the copy and the open happen outside the timed
/// region, and the database is closed after it. The rows and buckets
/// each cutoff converts are reported before timing.
fn bench_tier_pass(c: &mut Criterion) {
    use criterion::BatchSize;
    use zenith::storage::telemetry_db::MID_TIER;

    const T0: u64 = 1_700_000_000_000;
    let seeded_dir = TempDir::new().unwrap();
    let seeded = seeded_dir.path().join("seeded.db");
    {
        let db = TelemetryDb::open(&seeded).unwrap();
        let target: Arc<str> = Arc::from("target-0");
        let channels: [Arc<str>; 2] =
            [Arc::from("WaveGen#0.output"), Arc::from("WaveGen#1.output")];
        let samples: Vec<TelemetrySample> = (0..100_000u64)
            .map(|i| TelemetrySample {
                target_id: target.clone(),
                timestamp_ms: T0 + (i / 2) * 10,
                channel: channels[(i % 2) as usize].clone(),
                value: (i as f64 * 0.01).sin(),
                envelope: None,
            })
            .collect();
        for chunk in samples.chunks(1000) {
            db.insert_batch(chunk).unwrap();
        }
    }
    let fresh = || {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pass.db");
        std::fs::copy(&seeded, &path).unwrap();
        let wal = seeded.with_extension("db-wal");
        if wal.exists() {
            std::fs::copy(&wal, path.with_extension("db-wal")).unwrap();
        }
        let db = TelemetryDb::open(&path).unwrap();
        (dir, db)
    };

    let mut group = c.benchmark_group("tier_pass");
    group.sample_size(10);
    for (name, cutoff) in [
        ("aligned_cutoff", T0 + 400_000),
        ("mid_bucket_cutoff", T0 + 400_500),
    ] {
        let (_dir, db) = fresh();
        let rows = db.count().unwrap();
        let pass = db.tier_pass(MID_TIER, cutoff, 1_000, 600_000).unwrap();
        eprintln!(
            "work: tier_pass/{name} rows={rows} source_rows={} bucket_rows={}",
            pass.source_rows, pass.bucket_rows
        );
        group.bench_function(name, |b| {
            b.iter_batched(
                &fresh,
                |(dir, db)| {
                    let pass = db.tier_pass(MID_TIER, cutoff, 1_000, 600_000).unwrap();
                    (dir, db, pass)
                },
                BatchSize::PerIteration,
            )
        });
    }
    group.finish();
}

fn pprof_profiler() -> Criterion {
    Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)))
}

criterion_group! {
    name = benches;
    config = pprof_profiler();
    targets = bench_insert, bench_query, bench_tier_pass
}
criterion_main!(benches);
