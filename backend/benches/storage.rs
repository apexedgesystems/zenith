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

    group.bench_function("count", |b| {
        b.iter(|| {
            db.count().unwrap();
        })
    });

    group.finish();
}

fn pprof_profiler() -> Criterion {
    Criterion::default().with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)))
}

criterion_group! {
    name = benches;
    config = pprof_profiler();
    targets = bench_insert, bench_query
}
criterion_main!(benches);
