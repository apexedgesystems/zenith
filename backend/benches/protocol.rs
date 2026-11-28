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

/// ACK frames by stage and status, one ACK per iteration: an immediate
/// RESULT with two extra bytes (the existing ack_parse frame), deferred
/// COMPLETIONs whose status is a handler code (3, 6) and past the shared
/// set (0x20), and an INSPECT RESULT carrying a 256-byte block. Each
/// frame's stage, status and extra length are checked before timing.
fn bench_ack_parse_cases(c: &mut Criterion) {
    fn ack(opcode: u16, status: u8, stage: u8, extra: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(8 + extra.len());
        p.extend_from_slice(&opcode.to_le_bytes());
        p.extend_from_slice(&42u16.to_le_bytes());
        p.extend_from_slice(&[status, stage, 0, 0]);
        p.extend_from_slice(extra);
        p
    }
    let block: Vec<u8> = (0..256u32).map(|i| (i * 7 + 1) as u8).collect();
    let cases: [(&str, Vec<u8>, u8, u8, usize); 5] = [
        (
            "result_success_2B",
            ack(0x0100, 0, 0, &[0xFF, 0xFE]),
            0,
            0,
            2,
        ),
        ("completion_status_3", ack(0x0104, 3, 2, &[]), 2, 3, 0),
        ("completion_status_6", ack(0x0104, 6, 2, &[]), 2, 6, 0),
        (
            "completion_status_0x20",
            ack(0x0104, 0x20, 2, &[]),
            2,
            0x20,
            0,
        ),
        ("inspect_result_256B", ack(0x0130, 0, 0, &block), 0, 0, 256),
    ];
    let mut group = c.benchmark_group("ack_parse_cases");
    for (name, frame, stage, status, extra_len) in &cases {
        let parsed = aproto::parse_ack(frame).expect("fixture frame parses");
        assert_eq!(
            (parsed.stage, parsed.status, parsed.extra.len()),
            (*stage, *status, *extra_len),
            "fixture {name}"
        );
        group.bench_function(*name, |b| {
            b.iter(|| black_box(aproto::parse_ack(black_box(frame))))
        });
    }
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

/// SLIP decode of a frame that is all escape work: a 1 KB payload of
/// alternating END and ESC bytes, so every payload byte arrives as a
/// two-byte escape (2050 encoded bytes with the two delimiters), fed
/// whole and one byte at a time. Each iteration decodes the one frame
/// with a new decoder; both ways of feeding are checked to give back
/// the payload before timing.
fn bench_slip_decode_escaped(c: &mut Criterion) {
    // The two bytes SLIP escapes (RFC 1055): END and ESC.
    let payload: Vec<u8> = (0..1024)
        .map(|i| if i % 2 == 0 { 0xC0 } else { 0xDB })
        .collect();
    let encoded = slip::encode(&payload);
    assert_eq!(encoded.len(), 2 * payload.len() + 2);
    assert_eq!(slip::Decoder::new().feed(&encoded), vec![payload.clone()]);
    let mut decoder = slip::Decoder::new();
    let byte_frames: Vec<Vec<u8>> = encoded
        .iter()
        .flat_map(|byte| decoder.feed(std::slice::from_ref(byte)))
        .collect();
    assert_eq!(byte_frames, vec![payload]);

    let mut group = c.benchmark_group("slip_decode_escaped");
    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function("1KB_whole_feed", |b| {
        b.iter(|| {
            let mut decoder = slip::Decoder::new();
            decoder.feed(black_box(&encoded))
        })
    });
    group.bench_function("1KB_byte_by_byte", |b| {
        b.iter(|| {
            let mut decoder = slip::Decoder::new();
            let mut total = 0;
            for byte in encoded.iter() {
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
        bench_slip_decode_escaped,
        bench_aproto_build,
        bench_aproto_parse,
        bench_ack_parse_cases,
        bench_full_pipeline,
        bench_spp_extract,
        bench_tm_deframe,
        bench_record_walk,
        bench_tprm_review,
        bench_tm_packet_stage,
        bench_tunable_decode,
        bench_validate_token
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

/// The TPRM calls `bench_tprm_review` times, gathered in one place so
/// the same cases compile against another revision of the crate by
/// replacing this module alone: stamp a payload, parse a stamp back to
/// its identity, list a dictionary struct's leaves for hashing, and
/// the hash that leaf rule gives the nested TelemetryManager fixture.
mod tprm_api {
    use zenith::core::config_manager::FieldDef;
    use zenith::core::tprm::{self, LeafSpec};

    pub fn stamp(full_uid: u32, layout_hash: u32, body: &[u8]) -> Vec<u8> {
        tprm::stamp_v4(full_uid, layout_hash, body)
    }

    /// (uid, layout hash, body CRC, declared size, body length).
    pub fn parse(stamped: &[u8]) -> (u32, u32, u32, u64, usize) {
        let (p, body) = tprm::parse_v4(stamped).expect("a valid stamp parses");
        (
            p.full_uid,
            p.layout_hash,
            p.payload_crc,
            p.payload_size,
            body.len(),
        )
    }

    pub fn leaf_specs(fields: &[FieldDef]) -> Vec<LeafSpec<'_>> {
        tprm::dict_leaf_specs(fields)
    }

    pub const NESTED_HASH: u32 = 0x816D_61C8;
}

/// The variable-length layout hash as the tunable update handler
/// computes it: header leaves, then each entry instance's leaves moved
/// to absolute offsets, then the total size.
fn tprm_variable_hash(
    hdr: &zenith::core::config_manager::StructDef,
    ent: &zenith::core::config_manager::StructDef,
    entries: usize,
) -> u32 {
    use zenith::core::tprm;
    let total_size = hdr.size + entries * ent.size;
    let entry_offsets: Vec<Vec<usize>> = (0..entries)
        .map(|i| {
            ent.fields
                .iter()
                .map(|f| hdr.size + i * ent.size + f.offset)
                .collect()
        })
        .collect();
    tprm::layout_hash(
        tprm_api::leaf_specs(&hdr.fields)
            .into_iter()
            .chain(entry_offsets.iter().flat_map(|offs| {
                tprm_api::leaf_specs(&ent.fields)
                    .into_iter()
                    .zip(offs.iter())
                    .map(|(mut leaf, &off)| {
                        leaf.offset = off;
                        leaf
                    })
            })),
        total_size,
    )
}

/// The work one tunable upload does on payload bytes and dictionary
/// layouts: the prelude stamp (allocation, copy and body CRC) and its
/// parse at the body sizes of the contract vector, SystemMonitor,
/// TelemetryManager, the first body that needs a second 4096-byte file
/// chunk, and the largest body a 16-bit size holds; the layout-hash
/// recompute a dictionary without an exported hash falls back to, over
/// a flat and a nested shipped struct (leaf listing, canonical spec and
/// CRC timed; field expansion done once outside); and the
/// variable-length hash over the scheduler's header and 2 or 32
/// entries. Each result is checked before it is timed.
fn bench_tprm_review(c: &mut Criterion) {
    use criterion::BenchmarkId;
    use zenith::core::config_manager::{expanded_fields, ComponentDict};
    use zenith::core::tprm;

    const UID: u32 = 0x00_C800;
    const HASH: u32 = 0xD9C5_3260;
    let mut group = c.benchmark_group("tprm_review");

    for len in [13usize, 122, 520, 4069, 65535] {
        let body: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let stamped = tprm_api::stamp(UID, HASH, &body);
        assert_eq!(
            tprm_api::parse(&stamped),
            (UID, HASH, tprm::crc32(&body), len as u64, len)
        );
        group.bench_with_input(BenchmarkId::new("stamp", len), &body, |b, body| {
            b.iter(|| {
                black_box(tprm_api::stamp(
                    black_box(UID),
                    black_box(HASH),
                    black_box(body),
                ))
            })
        });
        group.bench_with_input(BenchmarkId::new("parse", len), &stamped, |b, stamped| {
            b.iter(|| black_box(tprm_api::parse(black_box(stamped))))
        });
        group.bench_with_input(BenchmarkId::new("stamp_parse", len), &body, |b, body| {
            b.iter(|| {
                let stamped = tprm_api::stamp(black_box(UID), black_box(HASH), black_box(body));
                black_box(tprm_api::parse(&stamped))
            })
        });
    }

    let dict = |text: &str| -> ComponentDict { serde_json::from_str(text).expect("dictionary") };
    let monitor = dict(include_str!(
        "../../targets/sil-ops-demo/structs/SystemMonitor.json"
    ));
    let telemetry = dict(include_str!(
        "../../targets/sil-ops-demo/structs/TelemetryManager.json"
    ));
    for (d, name, want) in [
        (&monitor, "SystemMonitorTunableParams", HASH),
        (&telemetry, "TelemetryManagerTprm", tprm_api::NESTED_HASH),
    ] {
        let sdef = &d.structs[name];
        let leaves = expanded_fields(d, &sdef.fields, 0);
        assert_eq!(
            tprm::layout_hash(tprm_api::leaf_specs(&leaves), sdef.size),
            want
        );
        let id = BenchmarkId::new("hash", format!("{name}_{}_leaves", leaves.len()));
        group.bench_with_input(id, &leaves, |b, leaves| {
            b.iter(|| {
                black_box(tprm::layout_hash(
                    tprm_api::leaf_specs(black_box(leaves)),
                    black_box(sdef.size),
                ))
            })
        });
    }

    let scheduler = dict(include_str!(
        "../../targets/sil-ops-demo/structs/Scheduler.json"
    ));
    let hdr = &scheduler.structs["SchedulerTprmHeader"];
    let ent = &scheduler.structs["SchedulerTaskEntry"];
    // The canonical spec over 33 and 483 bytes, computed independently.
    assert_eq!(tprm_variable_hash(hdr, ent, 2), 0x626A_BA98);
    assert_eq!(tprm_variable_hash(hdr, ent, 32), 0xC2CC_65F1);
    for entries in [2usize, 32] {
        group.bench_with_input(
            BenchmarkId::new("variable_hash", entries),
            &entries,
            |b, &entries| {
                b.iter(|| {
                    black_box(tprm_variable_hash(
                        black_box(hdr),
                        black_box(ent),
                        black_box(entries),
                    ))
                })
            },
        );
    }

    group.finish();
}

/// TM transfer frames through the real packet stage: a StreamLink
/// dialed to a loopback peer, so every chunk its reader takes from the
/// socket passes the link's ingest -- the TM deframer (a CRC per
/// frame), the step that hands each verified frame's data field to the
/// packet stage, and the SPP extractor -- and leaves as routed packets
/// on the broadcast channel. One iteration writes one stream of fifty
/// 1024-byte frames (51,200 bytes) from the peer and receives every
/// packet routed from it, through the stream's last packet, which is
/// addressed to a uid of its own. The receiver runs on the reader's
/// single-threaded runtime.
///
/// The healthy stream carries four 200-byte packets in each frame. The
/// damaged one repeats a recovery ten times: a frame that fails its
/// CRC, carrying two packets and the head of a 2864-byte packet; two
/// frames that carry only that packet's middle (no packet starts in
/// them); a frame that holds its 216-byte tail and then two packets;
/// and a healthy frame. Before timing, each stream's frames are counted
/// as the deframer sees them, and three passes of each must route the
/// same number of packets: each stream ends on a healthy frame, so a
/// pass leaves the pipeline as it found it.
fn bench_tm_packet_stage(c: &mut Criterion) {
    use std::collections::HashMap;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;
    use tokio::sync::broadcast;
    use zenith::core::stream_link::{Carrier, PipelineSpec, StreamLink};
    use zenith::core::transport::{Protocol, PushTelemetryPacket};
    use zenith::protocol::{ccsds_spp, ccsds_tm};

    const FRAME: usize = 1024;
    const FIELD: usize = FRAME - ccsds_tm::HEADER_SIZE - ccsds_tm::TRAILER_SIZE;
    const DATA_APID: u16 = 0x001;
    const LAST_APID: u16 = 0x002;
    const LAST_UID: u32 = 0x00D100;
    /// The first-header pointer of a frame in which no packet starts.
    const NO_PACKET_START: u16 = 0x7FF;

    /// Packets with a running sequence count. Their payload bytes are
    /// 0x5A, which never reads as the first byte of a packet header.
    struct Packer(u16);
    impl Packer {
        fn packet(&mut self, apid: u16, len: usize) -> Vec<u8> {
            let p = ccsds_spp::pack(apid, self.0, &vec![0x5A; len - ccsds_spp::HEADER_SIZE]);
            self.0 = self.0.wrapping_add(1);
            p
        }

        /// Four 200-byte packets, the last on `last_apid`, and idle fill.
        fn healthy_frame(&mut self, count: u8, last_apid: u16) -> Vec<u8> {
            let mut content = Vec::new();
            for i in 0..4 {
                let apid = if i == 3 { last_apid } else { DATA_APID };
                content.extend(self.packet(apid, 200));
            }
            ccsds_tm::pack_frame(FRAME, 0x044, count, &content)
        }
    }

    /// A packed frame with its first-header pointer set and its CRC
    /// recomputed.
    fn restamp(mut frame: Vec<u8>, first_header: u16) -> Vec<u8> {
        frame[4..6].copy_from_slice(&(0x1800u16 | first_header).to_be_bytes());
        let crc = ccsds_tm::crc16(&frame[..FRAME - ccsds_tm::TRAILER_SIZE]);
        frame[FRAME - ccsds_tm::TRAILER_SIZE..].copy_from_slice(&crc.to_be_bytes());
        frame
    }

    let mut packer = Packer(0);
    let mut healthy = Vec::with_capacity(50 * FRAME);
    for count in 0..50u8 {
        let last = if count == 49 { LAST_APID } else { DATA_APID };
        healthy.extend(packer.healthy_frame(count, last));
    }
    let mut damaged = Vec::with_capacity(50 * FRAME);
    for recovery in 0..10u8 {
        let count = recovery * 5;
        let long = packer.packet(DATA_APID, 2864);
        let (head, rest) = long.split_at(FIELD - 400);
        let (middle, tail) = rest.split_at(2 * FIELD);
        let mut content = packer.packet(DATA_APID, 200);
        content.extend(packer.packet(DATA_APID, 200));
        content.extend_from_slice(head);
        let mut lost = ccsds_tm::pack_frame(FRAME, 0x044, count, &content);
        lost[ccsds_tm::HEADER_SIZE + 4] ^= 0xFF; // fails its CRC
        damaged.extend(lost);
        for (i, part) in (1u8..).zip(middle.chunks(FIELD)) {
            let frame = ccsds_tm::pack_frame(FRAME, 0x044, count + i, part);
            damaged.extend(restamp(frame, NO_PACKET_START));
        }
        let mut content = tail.to_vec();
        content.extend(packer.packet(DATA_APID, 200));
        content.extend(packer.packet(DATA_APID, 200));
        let frame = ccsds_tm::pack_frame(FRAME, 0x044, count + 3, &content);
        damaged.extend(restamp(frame, tail.len() as u16));
        let last = if recovery == 9 { LAST_APID } else { DATA_APID };
        damaged.extend(packer.healthy_frame(count + 4, last));
    }

    // (frames, dropped, frames in which no packet starts), as the
    // delimitation stage sees each stream.
    let frames_of = |stream: &[u8]| {
        let frames = ccsds_tm::Deframer::new(FRAME).feed(stream);
        let dropped = frames
            .iter()
            .filter(|f| matches!(f, ccsds_tm::Frame::Dropped))
            .count();
        let no_start = frames
            .iter()
            .filter(|f| {
                matches!(
                    f,
                    ccsds_tm::Frame::Verified {
                        first_header: None,
                        ..
                    }
                )
            })
            .count();
        (frames.len(), dropped, no_start)
    };
    assert_eq!(frames_of(&healthy), (50, 0, 0));
    assert_eq!(frames_of(&damaged), (50, 10, 20));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (push_tx, mut rx) = broadcast::channel::<PushTelemetryPacket>(1024);
    let (mut link, mut peer) = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the peer");
        let port = listener.local_addr().expect("peer address").port();
        let mut link = StreamLink::new(
            Protocol::TmCcsdsSpp,
            PipelineSpec::TmSpp {
                apid_map: HashMap::from([(DATA_APID, 0x00D000), (LAST_APID, LAST_UID)]),
                frame_size: FRAME,
            },
            Carrier::Tcp,
            Vec::new(),
            push_tx,
        );
        link.connect("127.0.0.1", port)
            .await
            .expect("dial the peer");
        let (peer, _) = listener.accept().await.expect("accept the link");
        (link, peer)
    });
    // One pass: write the stream, receive through its last packet, and
    // return how many packets routed before it.
    let mut pass = |stream: &[u8]| -> usize {
        rt.block_on(async {
            peer.write_all(stream).await.expect("write the stream");
            let drain = async {
                let mut routed = 0;
                loop {
                    match rx.recv().await {
                        Ok(p) if p.full_uid == LAST_UID => return routed,
                        Ok(_) => routed += 1,
                        Err(e) => panic!("packet stage output lost: {e}"),
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(10), drain)
                .await
                .expect("the stream's last packet arrives")
        })
    };
    for (name, stream) in [
        ("healthy_50x1024B", &healthy),
        ("damaged_50x1024B", &damaged),
    ] {
        let routed: Vec<usize> = (0..3).map(|_| pass(stream)).collect();
        assert!(
            routed.windows(2).all(|w| w[0] == w[1]),
            "{name}: routed {routed:?} over three passes"
        );
        eprintln!(
            "work: tm_packet_stage/{name} bytes={} routed={} plus the last packet",
            stream.len(),
            routed[0]
        );
    }
    assert_eq!(pass(&healthy), 199, "every packet of the healthy stream");

    let mut group = c.benchmark_group("tm_packet_stage");
    group.throughput(Throughput::Bytes(healthy.len() as u64));
    group.bench_function("healthy_50x1024B", |b| b.iter(|| pass(black_box(&healthy))));
    group.bench_function("damaged_50x1024B", |b| b.iter(|| pass(black_box(&damaged))));
    group.finish();
    link.disconnect();
}

/// The parameter read's decode, gathered in one place so the same cases
/// compile against another revision of the crate by replacing this
/// module alone: what the read handler asks of the dictionaries for one
/// tunable block, the variable-length decode within the owning
/// component first, then the flat decode of its struct.
mod tunable_api {
    use zenith::core::config_manager::StructDictionary;

    pub fn read(
        dicts: &StructDictionary,
        component: &str,
        struct_name: &str,
        block: &[u8],
    ) -> Option<serde_json::Value> {
        match dicts.decode_variable_length_for("TUNABLE_PARAM", block, Some(component)) {
            Some((_, _, _, fields)) => Some(fields),
            None => dicts.decode_payload(component, struct_name, block),
        }
    }
}

/// A parameter block of `sname` with every numeric leaf set, in the
/// dictionary's byte order: floats 1.25, unsigned integers 7, 1000,
/// 100000 or 2^53 + 1 by width, signed ones their negatives, bools 1.
/// Strings and arrays keep a letter pattern; they read back as hex.
fn tunable_block(dict: &zenith::core::config_manager::ComponentDict, sname: &str) -> Vec<u8> {
    use zenith::core::config_manager::expanded_fields;

    let unsigned = |size: usize| -> u64 {
        match size {
            1 => 7,
            2 => 1000,
            4 => 100_000,
            _ => (1 << 53) + 1,
        }
    };
    let sdef = &dict.structs[sname];
    let mut block: Vec<u8> = (0..sdef.size).map(|i| b'A' + (i % 26) as u8).collect();
    for leaf in expanded_fields(dict, &sdef.fields, 0) {
        let mut bytes = match (leaf.field_type.as_str(), leaf.size) {
            ("float", 4) => 1.25f32.to_le_bytes().to_vec(),
            ("float", 8) | ("double", 8) => 1.25f64.to_le_bytes().to_vec(),
            ("uint", n @ (1 | 2 | 4 | 8)) => unsigned(n).to_le_bytes()[..n].to_vec(),
            ("int", n @ (1 | 2 | 4 | 8)) => (-(unsigned(n) as i64)).to_le_bytes()[..n].to_vec(),
            ("bool", 1) => vec![1],
            _ => continue,
        };
        if dict.big_endian() {
            bytes.reverse();
        }
        block[leaf.offset..leaf.offset + leaf.size].copy_from_slice(&bytes);
    }
    block
}

/// The parameter read's decode of one tunable block (`tunable_api`)
/// over shipped dictionaries, each set holding only the component under
/// test: the flat SystemMonitor block (122 bytes, 29 fields: floats,
/// 8- to 64-bit integers, arrays); the flat SpecProtoMax block (80
/// bytes, 16 fields, every scalar type, 64-bit integers past 2^53 and
/// a bool among them) in little- and big-endian byte order; and the
/// Scheduler's header with 32 task entries (483 bytes). Before timing,
/// each decode's values are counted by the JSON kind they read as,
/// with the order in which the Scheduler's two candidate structs are
/// tried (that set's map order, fixed for the run).
fn bench_tunable_decode(c: &mut Criterion) {
    use std::collections::HashMap;
    use zenith::core::config_manager::{ComponentDict, StructDictionary};

    fn kinds(v: &serde_json::Value, counts: &mut [usize; 3]) {
        match v {
            serde_json::Value::Number(_) => counts[0] += 1,
            serde_json::Value::String(_) => counts[1] += 1,
            serde_json::Value::Array(a) => a.iter().for_each(|x| kinds(x, counts)),
            serde_json::Value::Object(o) => o.values().for_each(|x| kinds(x, counts)),
            _ => counts[2] += 1,
        }
    }
    let dict = |text: &str| -> ComponentDict { serde_json::from_str(text).expect("dictionary") };
    let only = |d: ComponentDict| StructDictionary {
        components: HashMap::from([(d.component.clone(), d)]),
    };

    let monitor = dict(include_str!(
        "../../targets/sil-ops-demo/structs/SystemMonitor.json"
    ));
    let protomax = dict(include_str!(
        "../../targets/sil-ops-demo/structs/SpecProtoMax.json"
    ));
    let mut protomax_be = protomax.clone();
    protomax_be.byte_order = Some("be".to_string());
    let scheduler = dict(include_str!(
        "../../targets/sil-ops-demo/structs/Scheduler.json"
    ));
    let monitor_block = tunable_block(&monitor, "SystemMonitorTunableParams");
    let le_block = tunable_block(&protomax, "SpecProtoMaxTunableParams");
    let be_block = tunable_block(&protomax_be, "SpecProtoMaxTunableParams");
    // Header: one pool, two workers, 32 tasks; then the 32 entries.
    let entry = tunable_block(&scheduler, "SchedulerTaskEntry");
    let mut schedule = vec![1u8, 2, 32];
    for _ in 0..32 {
        schedule.extend_from_slice(&entry);
    }
    let scheduler = only(scheduler);
    let tried: Vec<&str> = scheduler.components["Scheduler"]
        .structs
        .iter()
        .filter(|(_, s)| s.category == "TUNABLE_PARAM" && !s.fields.is_empty() && s.size > 0)
        .map(|(name, _)| name.as_str())
        .collect();
    eprintln!("work: tunable_decode/variable_Scheduler_32_entries candidates tried in the order {tried:?}");

    let cases = [
        (
            "flat_SystemMonitor_122B",
            only(monitor),
            "SystemMonitor",
            "SystemMonitorTunableParams",
            monitor_block,
        ),
        (
            "flat_SpecProtoMax_80B_le",
            only(protomax),
            "SpecProtoMax",
            "SpecProtoMaxTunableParams",
            le_block,
        ),
        (
            "flat_SpecProtoMax_80B_be",
            only(protomax_be),
            "SpecProtoMax",
            "SpecProtoMaxTunableParams",
            be_block,
        ),
        (
            "variable_Scheduler_32_entries",
            scheduler,
            "Scheduler",
            "SchedulerTprmHeader",
            schedule,
        ),
    ];
    let mut group = c.benchmark_group("tunable_decode");
    for (name, dicts, component, sname, block) in &cases {
        let fields = tunable_api::read(dicts, component, sname, block).expect("the block decodes");
        let mut counts = [0usize; 3];
        kinds(&fields, &mut counts);
        eprintln!(
            "work: tunable_decode/{name} bytes={} numbers={} strings={} other={}",
            block.len(),
            counts[0],
            counts[1],
            counts[2]
        );
        group.bench_function(*name, |b| {
            b.iter(|| {
                tunable_api::read(
                    black_box(dicts),
                    black_box(component),
                    black_box(sname),
                    black_box(block),
                )
            })
        });
    }
    group.finish();
}

/// Token validation as the auth middleware calls it for a request: a
/// bearer token from the Authorization header, and a socket ticket from
/// the telemetry socket's query string. Both are HS256 with the shipped
/// placeholder secret and are checked to validate before timing; the
/// ticket is minted an hour ahead so its 30 s lifetime outlasts the run.
fn bench_validate_token(c: &mut Criterion) {
    use zenith::core::auth;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    let key = auth::DEFAULT_SECRET;
    let bearer = auth::mint_token("operator", key, now).expect("token");
    let ticket = auth::mint_ws_ticket("operator", key, now + 3600).expect("ticket");
    assert_eq!(
        auth::validate_token(&bearer, key, false),
        Ok("operator".to_string())
    );
    assert_eq!(
        auth::validate_token(&ticket, key, true),
        Ok("operator".to_string())
    );

    let mut group = c.benchmark_group("validate_token");
    group.bench_function("bearer_header", |b| {
        b.iter(|| auth::validate_token(black_box(&bearer), black_box(key), false))
    });
    group.bench_function("ticket_query", |b| {
        b.iter(|| auth::validate_token(black_box(&ticket), black_box(key), true))
    });
    group.finish();
}
