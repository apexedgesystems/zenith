use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use pprof::criterion::{Output, PProfProfiler};

// Import from the zenith crate
use zenith::protocol::{aproto, slip};

fn bench_slip_encode(c: &mut Criterion) {
    let data = vec![0x42u8; 1024]; // 1KB payload
    let mut group = c.benchmark_group("slip_encode");
    group.throughput(Throughput::Bytes(1024));

    group.bench_function("1KB_payload", |b| b.iter(|| slip::encode(black_box(&data))));

    let small = vec![0x42u8; 8]; // 8 byte payload (typical telemetry)
    group.bench_function("8B_payload", |b| b.iter(|| slip::encode(black_box(&small))));

    group.finish();
}

fn bench_slip_decode(c: &mut Criterion) {
    let data = vec![0x42u8; 1024];
    let encoded = slip::encode(&data);
    let mut group = c.benchmark_group("slip_decode");
    group.throughput(Throughput::Bytes(encoded.len() as u64));

    group.bench_function("1KB_frame", |b| {
        b.iter(|| {
            let mut decoder = slip::Decoder::new();
            decoder.feed(black_box(&encoded))
        })
    });

    // Multiple frames in one feed
    let mut multi = Vec::new();
    for _ in 0..10 {
        multi.extend_from_slice(&slip::encode(&[0x42; 8]));
    }
    group.bench_function("10x_8B_frames", |b| {
        b.iter(|| {
            let mut decoder = slip::Decoder::new();
            decoder.feed(black_box(&multi))
        })
    });

    group.finish();
}

fn bench_aproto_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("aproto_build");

    group.bench_function("command_no_payload", |b| {
        b.iter(|| aproto::build_command(black_box(0x00D000), black_box(0x0100), black_box(42), &[]))
    });

    let payload = vec![0u8; 48]; // Typical INSPECT payload
    group.bench_function("command_48B_payload", |b| {
        b.iter(|| {
            aproto::build_command(
                black_box(0x00D000),
                black_box(0x0130),
                black_box(42),
                black_box(&payload),
            )
        })
    });

    group.finish();
}

fn bench_aproto_parse(c: &mut Criterion) {
    let packet = aproto::build_command(0x00D000, 0x0100, 42, &[0u8; 48]);
    let mut group = c.benchmark_group("aproto_parse");
    group.throughput(Throughput::Bytes(packet.len() as u64));

    group.bench_function("header_only", |b| {
        b.iter(|| aproto::parse_header(black_box(&packet)))
    });

    group.bench_function("full_packet", |b| {
        b.iter(|| aproto::parse_packet(black_box(&packet)))
    });

    // ACK parsing
    let ack = vec![0x00, 0x01, 42, 0, 0, 0, 0, 0, 0xFF, 0xFE]; // 10-byte ACK with 2 bytes extra
    group.bench_function("ack_parse", |b| {
        b.iter(|| aproto::parse_ack(black_box(&ack)))
    });

    group.finish();
}

fn bench_full_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_pipeline");

    // Simulate: build command -> SLIP encode -> SLIP decode -> parse packet
    let payload = vec![0u8; 8]; // WaveGenOutput size
    group.bench_function("command_roundtrip", |b| {
        b.iter(|| {
            let packet = aproto::build_command(0x00D000, 0x0100, 42, &payload);
            let encoded = slip::encode(&packet);
            let mut decoder = slip::Decoder::new();
            let frames = decoder.feed(&encoded);
            for frame in frames {
                let _ = aproto::parse_packet(&frame);
            }
        })
    });

    group.finish();
}

/// Realistic stream: many small frames fed in 4KB TCP-sized chunks
/// (the actual production read path).
fn bench_slip_decode_chunked(c: &mut Criterion) {
    // Build a stream of 50 8B frames, then chunk it like a TCP read would.
    let mut stream = Vec::new();
    for _ in 0..50 {
        stream.extend_from_slice(&slip::encode(&[0x42; 8]));
    }

    let mut group = c.benchmark_group("slip_decode_chunked");
    group.throughput(Throughput::Bytes(stream.len() as u64));

    group.bench_function("50_frames_in_4KB_chunks", |b| {
        b.iter(|| {
            let mut decoder = slip::Decoder::new();
            let mut total = 0;
            for chunk in stream.chunks(4096) {
                total += decoder.feed(black_box(chunk)).len();
            }
            black_box(total)
        })
    });

    // Same payload but fed one byte at a time -- worst case decoder state
    group.bench_function("50_frames_byte_by_byte", |b| {
        b.iter(|| {
            let mut decoder = slip::Decoder::new();
            let mut total = 0;
            for byte in stream.iter() {
                total += decoder.feed(black_box(std::slice::from_ref(byte))).len();
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
    targets =
        bench_slip_encode,
        bench_slip_decode,
        bench_slip_decode_chunked,
        bench_aproto_build,
        bench_aproto_parse,
        bench_full_pipeline,
        bench_spp_extract,
        bench_tm_deframe,
        bench_record_walk
}
criterion_main!(benches);

fn bench_spp_extract(c: &mut Criterion) {
    use zenith::protocol::ccsds_spp;

    // A realistic telemetry burst: 100 packets, 32-byte payloads.
    let mut stream = Vec::new();
    for i in 0..100u16 {
        stream.extend(ccsds_spp::pack(0x0D0 + (i % 4), i, &[0xA5; 32]));
    }

    let mut group = c.benchmark_group("ccsds_spp_extract");
    group.throughput(Throughput::Bytes(stream.len() as u64));
    group.bench_function("clean_stream_100pkts", |b| {
        b.iter(|| {
            let mut ex = ccsds_spp::Extractor::new();
            black_box(ex.feed(&stream))
        })
    });

    // The adversarial case the cursor rewrite exists for: a garbage
    // run before every packet forces resync work; cost must stay
    // linear in the garbage volume.
    let mut dirty = Vec::new();
    for i in 0..100u16 {
        dirty.extend(std::iter::repeat_n(0xFFu8, 64));
        dirty.extend(ccsds_spp::pack(0x0D0, i, &[0xA5; 32]));
    }
    group.throughput(Throughput::Bytes(dirty.len() as u64));
    group.bench_function("garbage_resync_100pkts", |b| {
        b.iter(|| {
            let mut ex = ccsds_spp::Extractor::new();
            black_box(ex.feed(&dirty))
        })
    });
    group.finish();
}

fn bench_tm_deframe(c: &mut Criterion) {
    use zenith::protocol::{ccsds_spp, ccsds_tm};

    // A realistic downlink burst: 50 fixed 1024-byte frames, each
    // carrying four 200-byte packets plus idle fill, CRC-verified.
    let mut stream = Vec::new();
    for f in 0..50u8 {
        let mut content = Vec::new();
        for p in 0..4u16 {
            content.extend(ccsds_spp::pack(0x001, f as u16 * 4 + p, &[0x5A; 194]));
        }
        stream.extend(ccsds_tm::pack_frame(1024, 0x044, f, &content));
    }

    let mut group = c.benchmark_group("tm_deframe");
    group.throughput(Throughput::Bytes(stream.len() as u64));
    group.bench_function("50x1024B_frames_whole", |b| {
        b.iter(|| {
            let mut d = ccsds_tm::Deframer::new(1024);
            black_box(d.feed(&stream))
        })
    });
    // Socket-sized reads: frames straddle every chunk boundary.
    group.bench_function("50x1024B_frames_in_1500B_chunks", |b| {
        b.iter(|| {
            let mut d = ccsds_tm::Deframer::new(1024);
            let mut n = 0usize;
            for chunk in stream.chunks(1500) {
                n += d.feed(chunk).len();
            }
            black_box(n)
        })
    });
    group.finish();
}

fn bench_record_walk(c: &mut Criterion) {
    use std::collections::HashMap;
    use zenith::core::config_manager::LayoutStep;
    use zenith::core::stream_link::RecordSpec;

    // One aggregate packet as a record-shaped downlink emits it: a 2B
    // packet descriptor, then 20 records of [4B id][11B time][value],
    // eighteen 4-byte channels and two length-prefixed strings to
    // step over.
    let rec = |id: u32, value: &[u8]| -> Vec<u8> {
        let mut r = id.to_be_bytes().to_vec();
        r.extend_from_slice(&[0u8; 11]);
        r.extend_from_slice(value);
        r
    };
    let mut payload = vec![0x00, 0x01];
    let mut by_id = HashMap::new();
    for id in 0..18u32 {
        by_id.insert(id, 4usize);
        payload.extend(rec(id, &[0, 0, 0, id as u8]));
    }
    let text = b"version-string";
    let mut sval = (text.len() as u16).to_be_bytes().to_vec();
    sval.extend_from_slice(text);
    payload.extend(rec(100, &sval));
    payload.extend(rec(101, &sval));
    let spec = RecordSpec {
        record_apid: 0x001,
        packet_prefix: 2,
        id_offset: 0,
        id_size: 4,
        header_size: 15,
        by_id,
        skip_apids: Default::default(),
        skip_by_id: HashMap::from([
            (100, vec![LayoutStep::Prefixed { prefixed: 2 }]),
            (101, vec![LayoutStep::Prefixed { prefixed: 2 }]),
        ]),
    };

    let mut group = c.benchmark_group("record_walk");
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function("20_records_2_skipped", |b| {
        b.iter(|| black_box(spec.walk(black_box(&payload))))
    });
    group.finish();
}
