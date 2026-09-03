//! Benchmarks for the telemetry decoder hot path.
//!
//! Exercises the realistic production path: a TelemetryDecoder built
//! from a small struct dictionary plus an app manifest, then decode()
//! called repeatedly on a sequence of PushTelemetryPackets that match
//! the decoder's lookup table.

use std::collections::HashMap;
use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use pprof::criterion::{Output, PProfProfiler};

use zenith::core::aproto_client::PushTelemetryPacket;
use zenith::core::config_manager::{ComponentDict, FieldDef, StructDef, StructDictionary};

/* ----------------------------- Test fixture ----------------------------- */

/// Build a struct dictionary that mirrors a realistic Apex demo target:
///   - WaveGenerator: OUTPUT(8B), STATE(48B), TUNABLE_PARAM(32B)
///   - SystemMonitor: OUTPUT(24B)
fn make_dict() -> StructDictionary {
    let wavegen = ComponentDict {
        component: "WaveGenerator".to_string(),
        structs: HashMap::from([
            (
                "Output".to_string(),
                StructDef {
                    category: "OUTPUT".to_string(),
                    size: 8,
                    opcode: None,
                    layout_hash: None,
                    canonical_spec: None,
                    fields: vec![
                        FieldDef {
                            name: "output".to_string(),
                            field_type: "float".to_string(),
                            offset: 0,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "phase".to_string(),
                            field_type: "float".to_string(),
                            offset: 4,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                    ],
                },
            ),
            (
                "State".to_string(),
                StructDef {
                    category: "STATE".to_string(),
                    size: 48,
                    opcode: None,
                    layout_hash: None,
                    canonical_spec: None,
                    fields: vec![
                        FieldDef {
                            name: "output".to_string(),
                            field_type: "float".to_string(),
                            offset: 0,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "phase".to_string(),
                            field_type: "float".to_string(),
                            offset: 4,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "cycleCount".to_string(),
                            field_type: "uint".to_string(),
                            offset: 8,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "amplitude".to_string(),
                            field_type: "float".to_string(),
                            offset: 12,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "frequency".to_string(),
                            field_type: "float".to_string(),
                            offset: 16,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "offset".to_string(),
                            field_type: "float".to_string(),
                            offset: 20,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "lastUpdateMs".to_string(),
                            field_type: "uint".to_string(),
                            offset: 24,
                            size: 8,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "errorCount".to_string(),
                            field_type: "uint".to_string(),
                            offset: 32,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "lastSampleNs".to_string(),
                            field_type: "uint".to_string(),
                            offset: 36,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "warmupRemain".to_string(),
                            field_type: "float".to_string(),
                            offset: 40,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                        FieldDef {
                            name: "padding".to_string(),
                            field_type: "uint".to_string(),
                            offset: 44,
                            size: 4,
                            value: serde_json::Value::Null,
                            element_type: None,
                            dims: None,
                            constraints: None,
                        },
                    ],
                },
            ),
        ]),
        enums: HashMap::new(),
        capabilities: Vec::new(),
    };

    let sysmon = ComponentDict {
        component: "SystemMonitor".to_string(),
        structs: HashMap::from([(
            "Output".to_string(),
            StructDef {
                category: "OUTPUT".to_string(),
                size: 24,
                opcode: None,
                layout_hash: None,
                canonical_spec: None,
                fields: vec![
                    FieldDef {
                        name: "cpuLoad".to_string(),
                        field_type: "float".to_string(),
                        offset: 0,
                        size: 4,
                        value: serde_json::Value::Null,
                        element_type: None,
                        dims: None,
                        constraints: None,
                    },
                    FieldDef {
                        name: "tempC".to_string(),
                        field_type: "float".to_string(),
                        offset: 4,
                        size: 4,
                        value: serde_json::Value::Null,
                        element_type: None,
                        dims: None,
                        constraints: None,
                    },
                    FieldDef {
                        name: "ramKb".to_string(),
                        field_type: "uint".to_string(),
                        offset: 8,
                        size: 4,
                        value: serde_json::Value::Null,
                        element_type: None,
                        dims: None,
                        constraints: None,
                    },
                    FieldDef {
                        name: "fdCount".to_string(),
                        field_type: "uint".to_string(),
                        offset: 12,
                        size: 4,
                        value: serde_json::Value::Null,
                        element_type: None,
                        dims: None,
                        constraints: None,
                    },
                    FieldDef {
                        name: "uptimeS".to_string(),
                        field_type: "uint".to_string(),
                        offset: 16,
                        size: 8,
                        value: serde_json::Value::Null,
                        element_type: None,
                        dims: None,
                        constraints: None,
                    },
                ],
            },
        )]),
        enums: HashMap::new(),
        capabilities: Vec::new(),
    };

    StructDictionary {
        components: HashMap::from([
            ("WaveGenerator".to_string(), wavegen),
            ("SystemMonitor".to_string(), sysmon),
        ]),
    }
}

/// Manifest UIDs that match the dict above.
fn make_uids() -> Vec<(u32, String)> {
    vec![
        (0x00D000, "WaveGenerator#0".to_string()),
        (0x00D001, "WaveGenerator#1".to_string()),
        (0x00D100, "SystemMonitor".to_string()),
    ]
}

/// Build a sequence of realistic push packets for the bench loop.
fn make_packets() -> Vec<PushTelemetryPacket> {
    let mut packets = Vec::new();
    // 2 WaveGen Output (8B), 2 WaveGen State (48B), 1 SystemMonitor Output (24B)
    for &uid in &[0x00D000u32, 0x00D001] {
        packets.push(PushTelemetryPacket {
            full_uid: uid,
            payload: vec![
                0x00, 0x00, 0x00, 0x3F, // output = 0.5
                0x00, 0x00, 0x80, 0x3F, // phase = 1.0
            ],
        });
        packets.push(PushTelemetryPacket {
            full_uid: uid,
            payload: vec![0u8; 48],
        });
    }
    packets.push(PushTelemetryPacket {
        full_uid: 0x00D100,
        payload: vec![0u8; 24],
    });
    packets
}

/* ----------------------------- Benches ----------------------------- */

fn bench_decoder_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("decoder_construction");
    let dict = make_dict();
    let uids = make_uids();
    let uid_refs: Vec<(u32, &str, Option<&str>)> =
        uids.iter().map(|(u, n)| (*u, n.as_str(), None)).collect();

    group.bench_function("build_3_components", |b| {
        b.iter(|| {
            // We can't construct TelemetryDecoder directly (private),
            // so this measures the spawn_router setup cost approximately
            // by recreating the same work via the public API path.
            let _ = black_box(zenith::core::telemetry::TelemetryDecoder::new(
                &dict, &uid_refs,
            ));
        })
    });
    group.finish();
}

fn bench_decode_hot_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("decoder_decode");

    let dict = make_dict();
    let uids = make_uids();
    let uid_refs: Vec<(u32, &str, Option<&str>)> =
        uids.iter().map(|(u, n)| (*u, n.as_str(), None)).collect();
    let decoder = zenith::core::telemetry::TelemetryDecoder::new(&dict, &uid_refs);
    let packets = make_packets();
    let target: Arc<str> = Arc::from("target-0");

    // Per-packet decode
    group.throughput(Throughput::Elements(1));
    group.bench_function("single_8B_output", |b| {
        let pkt = &packets[0];
        b.iter(|| {
            black_box(decoder.decode(black_box(&target), black_box(1700000000000), black_box(pkt)))
        })
    });

    group.bench_function("single_48B_state", |b| {
        let pkt = &packets[1];
        b.iter(|| {
            black_box(decoder.decode(black_box(&target), black_box(1700000000000), black_box(pkt)))
        })
    });

    // Realistic burst (5 packets covering 2 components, OUTPUT + STATE + sysmon)
    group.throughput(Throughput::Elements(packets.len() as u64));
    group.bench_function("realistic_burst_5_packets", |b| {
        b.iter(|| {
            let mut total = 0;
            for pkt in &packets {
                let samples = decoder.decode(&target, 1700000000000, pkt);
                total += samples.len();
            }
            black_box(total)
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
    targets = bench_decoder_construction, bench_decode_hot_path
}
criterion_main!(benches);
