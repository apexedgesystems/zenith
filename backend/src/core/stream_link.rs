//! Composable stream transport: bytes in, neutral telemetry packets
//! out, through a framing stage and a packet stage.
//!
//! Wire stacks are layered in practice -- SLIP-framed SPP, EPP-wrapped
//! payloads, bare self-delimiting SPP -- so the pipeline is composed,
//! not enumerated: a delimitation stage (how the stream splits into
//! units) feeds a packet stage (how a unit becomes an addressed
//! payload). A new stack is a new composition in config, not a new
//! transport module. Addressing is per-stage business: SPP maps APIDs
//! to component uids via config, raw frames take the config uid
//! directly, and future codecs (CCSDS EPP: protocol-id routed,
//! variable 1/2/4/8-octet headers per the producer's library) slot in
//! as packet stages.
//!
//! Every composition here is telemetry-only: command surfaces answer
//! "unsupported for this protocol" at the handler layer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::time::{timeout, Duration};

use crate::core::transport::{ClientError, Protocol, PushTelemetryPacket};
use crate::protocol::{ccsds_spp, slip};

/// What to build a pipeline from -- config data, kept so each connect
/// starts a pristine pipeline (no stale partial frames across
/// reconnects).
#[derive(Debug, Clone)]
pub enum PipelineSpec {
    /// Self-delimiting SPP over the raw stream.
    Spp { apid_map: HashMap<u16, u32> },
    /// SLIP delimits; each frame is one SPP packet.
    SlipSpp { apid_map: HashMap<u16, u32> },
    /// SLIP delimits; each frame is one raw payload for the config uid.
    SlipRaw { uid: Option<u32> },
}

/// A live pipeline: stream bytes -> addressed packets.
enum PacketPipeline {
    Spp {
        extractor: ccsds_spp::Extractor,
        apid_map: HashMap<u16, u32>,
    },
    SlipSpp {
        slip: slip::Decoder,
        apid_map: HashMap<u16, u32>,
    },
    SlipRaw {
        slip: slip::Decoder,
        uid: Option<u32>,
    },
}

impl PacketPipeline {
    fn build(spec: &PipelineSpec) -> Self {
        match spec {
            PipelineSpec::Spp { apid_map } => PacketPipeline::Spp {
                extractor: ccsds_spp::Extractor::new(),
                apid_map: apid_map.clone(),
            },
            PipelineSpec::SlipSpp { apid_map } => PacketPipeline::SlipSpp {
                slip: slip::Decoder::new(),
                apid_map: apid_map.clone(),
            },
            PipelineSpec::SlipRaw { uid } => PacketPipeline::SlipRaw {
                slip: slip::Decoder::new(),
                uid: *uid,
            },
        }
    }

    /// Feed stream bytes; emit every addressed packet now available.
    /// Returns (packets, unroutable-unit count) so the reader can
    /// rate-limit its warning without the pipeline owning logging.
    fn feed(&mut self, bytes: &[u8]) -> (Vec<PushTelemetryPacket>, usize) {
        let mut out = Vec::new();
        let mut unroutable = 0usize;
        match self {
            PacketPipeline::Spp {
                extractor,
                apid_map,
            } => {
                for (hdr, payload) in extractor.feed(bytes) {
                    match apid_map.get(&hdr.apid) {
                        Some(&uid) => out.push(PushTelemetryPacket {
                            full_uid: uid,
                            payload,
                        }),
                        None => unroutable += 1,
                    }
                }
            }
            PacketPipeline::SlipSpp { slip, apid_map } => {
                for frame in slip.feed(bytes) {
                    // One frame = one whole SPP packet; the header
                    // still declares its own length, which must agree
                    // with the frame or the unit is unroutable.
                    let parsed = ccsds_spp::parse_header(&frame).and_then(|hdr| {
                        if ccsds_spp::HEADER_SIZE + hdr.data_len == frame.len() {
                            Some((hdr, frame[ccsds_spp::HEADER_SIZE..].to_vec()))
                        } else {
                            None
                        }
                    });
                    match parsed {
                        Some((hdr, payload)) => match apid_map.get(&hdr.apid) {
                            Some(&uid) => out.push(PushTelemetryPacket {
                                full_uid: uid,
                                payload,
                            }),
                            None => unroutable += 1,
                        },
                        None => unroutable += 1,
                    }
                }
            }
            PacketPipeline::SlipRaw { slip, uid } => {
                for frame in slip.feed(bytes) {
                    if frame.is_empty() {
                        continue;
                    }
                    match uid {
                        Some(uid) => out.push(PushTelemetryPacket {
                            full_uid: *uid,
                            payload: frame,
                        }),
                        None => unroutable += 1,
                    }
                }
            }
        }
        (out, unroutable)
    }
}

/// One target's stream link: socket + composed pipeline.
pub struct StreamLink {
    protocol: Protocol,
    spec: PipelineSpec,
    push_tlm_tx: broadcast::Sender<PushTelemetryPacket>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    connected: Arc<AtomicBool>,
    /// Generation counter, the shared link discipline: a
    /// stale reader must not clear a newer connection's flag.
    generation: Arc<AtomicU64>,
}

impl StreamLink {
    pub fn new(
        protocol: Protocol,
        spec: PipelineSpec,
        push_tlm_tx: broadcast::Sender<PushTelemetryPacket>,
    ) -> Self {
        if let PipelineSpec::SlipRaw { uid: None } = &spec {
            tracing::warn!(
                "raw-slip target has no raw_uid configured; frames will be dropped until one is set"
            );
        }
        Self {
            protocol,
            spec,
            push_tlm_tx,
            reader_handle: None,
            connected: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Telemetry-only link: no command round trips to account; the
    /// pipeline counters attach downstream at the router and writer.
    pub fn set_metrics(&mut self, _metrics: Arc<crate::core::metrics::TargetMetrics>) {}

    pub async fn connect(&mut self, host: &str, port: u16) -> Result<(), ClientError> {
        let addr = format!("{}:{}", host, port);
        let stream = timeout(Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out"))?
            .map_err(ClientError::Connect)?;
        stream.set_nodelay(true)?;

        let connected = self.connected.clone();
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        connected.store(true, Ordering::Release);

        let push_tx = self.push_tlm_tx.clone();
        let mut pipeline = PacketPipeline::build(&self.spec);
        let conn_flag = connected.clone();
        let gen_flag = self.generation.clone();
        let proto_name = self.protocol.name();
        let reader_handle = tokio::spawn(async move {
            const UNROUTABLE_WARN_EVERY: Duration = Duration::from_secs(30);
            let mut last_warn: Option<tokio::time::Instant> = None;
            let mut buf = vec![0u8; 65536];
            let mut stream = stream;
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => {
                        if gen_flag.load(Ordering::Acquire) == gen {
                            conn_flag.store(false, Ordering::Release);
                        }
                        tracing::info!("{proto_name} connection closed by remote");
                        break;
                    }
                    Ok(n) => {
                        let (packets, unroutable) = pipeline.feed(&buf[..n]);
                        for pkt in packets {
                            let _ = push_tx.send(pkt);
                        }
                        if unroutable > 0
                            && last_warn.is_none_or(|t| t.elapsed() >= UNROUTABLE_WARN_EVERY)
                        {
                            tracing::warn!(
                                "{proto_name}: {unroutable} unroutable unit(s) dropped \
                                 (unmapped address or malformed packet)"
                            );
                            last_warn = Some(tokio::time::Instant::now());
                        }
                    }
                    Err(e) => {
                        if gen_flag.load(Ordering::Acquire) == gen {
                            conn_flag.store(false, Ordering::Release);
                        }
                        tracing::error!("{proto_name} read error: {e}");
                        break;
                    }
                }
            }
        });

        self.reader_handle = Some(reader_handle);
        tracing::info!("Connected ({proto_name}) to {addr}");
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.connected.store(false, Ordering::Release);
        if let Some(h) = self.reader_handle.take() {
            h.abort();
        }
        tracing::info!("Disconnected ({})", self.protocol.name());
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn connected_handle(&self) -> Arc<AtomicBool> {
        self.connected.clone()
    }
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config_manager::{ComponentDict, FieldDef, StructDef, StructDictionary};
    use crate::core::telemetry::TelemetryDecoder;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn wavegen_dict() -> StructDictionary {
        let f = |name: &str, off: usize| FieldDef {
            name: name.to_string(),
            field_type: "float".to_string(),
            offset: off,
            size: 4,
            value: serde_json::Value::Null,
            element_type: None,
            dims: None,
            constraints: None,
        };
        let comp = ComponentDict {
            component: "WaveGenerator".to_string(),
            structs: std::collections::HashMap::from([(
                "Output".to_string(),
                StructDef {
                    category: "OUTPUT".to_string(),
                    size: 8,
                    opcode: None,
                    fields: vec![f("output", 0), f("phase", 4)],
                    layout_hash: None,
                    canonical_spec: None,
                },
            )]),
            enums: std::collections::HashMap::new(),
            capabilities: Vec::new(),
        };
        StructDictionary {
            components: std::collections::HashMap::from([("WaveGenerator".to_string(), comp)]),
        }
    }

    fn body() -> Vec<u8> {
        [0.5f32, 1.0f32]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    }

    async fn serve_bytes(chunks: Vec<Vec<u8>>) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            for c in chunks {
                sock.write_all(&c).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        });
        addr
    }

    async fn recv_one(rx: &mut broadcast::Receiver<PushTelemetryPacket>) -> PushTelemetryPacket {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("packet within deadline")
            .unwrap()
    }

    /// @test THE protocol-agnosticism proof, now across THREE stream
    /// compositions: the same struct bytes delivered as (a) bare
    /// self-delimiting SPP, (b) SLIP-framed SPP -- the layered stack
    /// -- and (c) raw SLIP frames all decode to exactly the samples
    /// the decoder produces for those bytes directly. One dictionary,
    /// one decoder, one pipeline seam; the stacks differ only in
    /// config.
    #[tokio::test]
    async fn all_stream_compositions_decode_identically() {
        let dict = wavegen_dict();
        let decoder =
            TelemetryDecoder::new(&dict, &[(0x00D000, "WaveGenerator", Some("WaveGenerator"))]);
        let target: Arc<str> = Arc::from("t");
        let direct = decoder.decode(
            &target,
            1000,
            &PushTelemetryPacket {
                full_uid: 0x00D000,
                payload: body(),
            },
        );
        assert_eq!(direct.len(), 2);

        let map = HashMap::from([(0x0D0u16, 0x00D000u32)]);
        let spp_wire = ccsds_spp::pack(0x0D0, 1, &body());
        let cases: Vec<(Protocol, PipelineSpec, Vec<Vec<u8>>)> = vec![
            (
                Protocol::CcsdsSpp,
                PipelineSpec::Spp {
                    apid_map: map.clone(),
                },
                // Mid-packet split exercises the extractor.
                vec![spp_wire[..4].to_vec(), spp_wire[4..].to_vec()],
            ),
            (
                Protocol::SlipCcsdsSpp,
                PipelineSpec::SlipSpp {
                    apid_map: map.clone(),
                },
                // The layered stack: the SPP packet inside SLIP
                // framing, split mid-frame.
                {
                    let framed = slip::encode(&spp_wire);
                    vec![framed[..5].to_vec(), framed[5..].to_vec()]
                },
            ),
            (
                Protocol::RawSlip,
                PipelineSpec::SlipRaw {
                    uid: Some(0x00D000),
                },
                vec![slip::encode(&body())],
            ),
        ];

        for (proto, spec, chunks) in cases {
            let addr = serve_bytes(chunks).await;
            let (push_tx, mut push_rx) = broadcast::channel(16);
            let mut link = StreamLink::new(proto, spec, push_tx);
            link.connect(&addr.ip().to_string(), addr.port())
                .await
                .unwrap();
            let pkt = recv_one(&mut push_rx).await;
            assert_eq!(pkt.full_uid, 0x00D000, "{}", proto.name());
            let via_wire = decoder.decode(&target, 1000, &pkt);
            assert_eq!(via_wire.len(), direct.len(), "{}", proto.name());
            for (a, b) in direct.iter().zip(via_wire.iter()) {
                assert_eq!(&*a.channel, &*b.channel, "{}", proto.name());
                assert_eq!(a.value, b.value, "{}", proto.name());
            }
            link.disconnect();
        }
    }

    /// @test Unroutable units (unmapped APID; length-lying SLIP-SPP
    /// frame) drop without disturbing the stream: the routable packet
    /// that follows still arrives.
    #[tokio::test]
    async fn unroutable_units_drop_and_stream_continues() {
        let map = HashMap::from([(0x0D0u16, 0x00BEEF00u32)]);
        let unmapped = slip::encode(&ccsds_spp::pack(0x111, 1, &[1, 2, 3, 4]));
        // A frame whose SPP header lies about its length.
        let mut lying = ccsds_spp::pack(0x0D0, 2, &[9, 9]);
        lying[5] = 7; // declares 8 data bytes, frame carries 2
        let lying = slip::encode(&lying);
        let good = slip::encode(&ccsds_spp::pack(0x0D0, 3, &[5, 6, 7, 8]));

        let addr = serve_bytes(vec![unmapped, lying, good]).await;
        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut link = StreamLink::new(
            Protocol::SlipCcsdsSpp,
            PipelineSpec::SlipSpp { apid_map: map },
            push_tx,
        );
        link.connect(&addr.ip().to_string(), addr.port())
            .await
            .unwrap();

        let pkt = recv_one(&mut push_rx).await;
        assert_eq!(pkt.full_uid, 0x00BEEF00);
        assert_eq!(pkt.payload, vec![5, 6, 7, 8]);
        assert!(push_rx.try_recv().is_err());
        link.disconnect();
    }

    /// @test Without a configured raw_uid, raw frames drop instead of
    /// inventing an address -- the link stays connected and harmless.
    #[tokio::test]
    async fn missing_uid_drops_frames() {
        let addr = serve_bytes(vec![slip::encode(&[1, 2, 3])]).await;
        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut link = StreamLink::new(
            Protocol::RawSlip,
            PipelineSpec::SlipRaw { uid: None },
            push_tx,
        );
        link.connect(&addr.ip().to_string(), addr.port())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(push_rx.try_recv().is_err());
        assert!(link.is_connected());
        link.disconnect();
    }
}
