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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::broadcast;
use tokio::time::{timeout, Duration};

use crate::core::transport::{ClientError, Protocol, PushTelemetryPacket};
use crate::protocol::{ccsds_spp, slip};

/// How bytes reach zenith -- a third composition axis, orthogonal to
/// the framing and packet stages. TCP dials the target and reads a
/// byte stream; UDP binds a local port and receives datagrams (the
/// common flight-stack ground pattern: telemetry sent to our port,
/// commands sent to the target's). The same pipeline consumes
/// either: a datagram is just a read chunk whose boundaries the
/// stages already tolerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    /// Dial host:port, read the stream.
    Tcp,
    /// Bind listen_port for inbound datagrams; host:port is where
    /// outbound (arm) datagrams go. listen_port 0 = OS-assigned,
    /// useful only when the peer replies to the source address.
    Udp { listen_port: u16 },
}

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

/// The per-chunk work both reader loops share: feed the pipeline,
/// broadcast what routes, rate-limit the unroutable warning.
struct Ingest {
    pipeline: PacketPipeline,
    push_tx: broadcast::Sender<PushTelemetryPacket>,
    proto_name: &'static str,
    last_warn: Option<tokio::time::Instant>,
}

impl Ingest {
    const UNROUTABLE_WARN_EVERY: Duration = Duration::from_secs(30);

    fn feed(&mut self, bytes: &[u8]) {
        let (packets, unroutable) = self.pipeline.feed(bytes);
        for pkt in packets {
            let _ = self.push_tx.send(pkt);
        }
        if unroutable > 0
            && self
                .last_warn
                .is_none_or(|t| t.elapsed() >= Self::UNROUTABLE_WARN_EVERY)
        {
            tracing::warn!(
                "{}: {unroutable} unroutable unit(s) dropped \
                 (unmapped address or malformed packet)",
                self.proto_name
            );
            self.last_warn = Some(tokio::time::Instant::now());
        }
    }
}

/// One target's stream link: socket + composed pipeline.
pub struct StreamLink {
    protocol: Protocol,
    spec: PipelineSpec,
    carrier: Carrier,
    /// Raw bytes sent to the target on every connect, before the
    /// reader starts -- the downlink-arm step. Ground reality for
    /// many targets: nothing is emitted until a ground message
    /// enables the downlink, so arming is part of link bring-up,
    /// not an operator afterthought. The bytes are per-target
    /// config and opaque here -- zenith sends, never interprets.
    arm: Vec<Vec<u8>>,
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
        carrier: Carrier,
        arm: Vec<Vec<u8>>,
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
            carrier,
            arm,
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
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut ingest = Ingest {
            pipeline: PacketPipeline::build(&self.spec),
            push_tx: self.push_tlm_tx.clone(),
            proto_name: self.protocol.name(),
            last_warn: None,
        };
        let conn_flag = self.connected.clone();
        let gen_flag = self.generation.clone();
        let proto_name = self.protocol.name();

        let reader_handle = match self.carrier {
            Carrier::Tcp => {
                let mut stream = timeout(Duration::from_secs(5), TcpStream::connect(&addr))
                    .await
                    .map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out")
                    })?
                    .map_err(ClientError::Connect)?;
                stream.set_nodelay(true)?;
                // Arm the downlink before the reader owns the socket:
                // one-shot writes, so no writer half to keep.
                for bytes in &self.arm {
                    stream.write_all(bytes).await?;
                }
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) => {
                                if gen_flag.load(Ordering::Acquire) == gen {
                                    conn_flag.store(false, Ordering::Release);
                                }
                                tracing::info!("{proto_name} connection closed by remote");
                                break;
                            }
                            Ok(n) => ingest.feed(&buf[..n]),
                            Err(e) => {
                                if gen_flag.load(Ordering::Acquire) == gen {
                                    conn_flag.store(false, Ordering::Release);
                                }
                                tracing::error!("{proto_name} read error: {e}");
                                break;
                            }
                        }
                    }
                })
            }
            Carrier::Udp { listen_port } => {
                // One unconnected socket does both directions: bound
                // locally so telemetry datagrams land here, send_to
                // for arming. Deliberately NOT connect()ed -- the
                // target's telemetry sender is typically a different
                // socket than its command receiver, and a connected
                // UDP socket would filter those datagrams out.
                let sock = UdpSocket::bind(("0.0.0.0", listen_port))
                    .await
                    .map_err(ClientError::Connect)?;
                for bytes in &self.arm {
                    sock.send_to(bytes, &addr)
                        .await
                        .map_err(ClientError::Connect)?;
                }
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    loop {
                        match sock.recv_from(&mut buf).await {
                            Ok((n, _src)) => ingest.feed(&buf[..n]),
                            Err(e) => {
                                if gen_flag.load(Ordering::Acquire) == gen {
                                    conn_flag.store(false, Ordering::Release);
                                }
                                tracing::error!("{proto_name} udp recv error: {e}");
                                break;
                            }
                        }
                    }
                })
            }
        };

        self.connected.store(true, Ordering::Release);
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
            struct_ref: None,
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
                    packed: None,
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
            let mut link = StreamLink::new(proto, spec, Carrier::Tcp, Vec::new(), push_tx);
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
            Carrier::Tcp,
            Vec::new(),
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

    /// @test The UDP carrier end to end: connect binds a local
    /// socket, fires the arm datagram at the target's command port,
    /// and telemetry datagrams flowing back parse through the same
    /// SPP pipeline the TCP carrier uses. The fake target verifies
    /// the arm bytes arrive verbatim before it emits anything --
    /// downlink-silent-until-armed, enforced.
    #[tokio::test]
    async fn udp_carrier_arms_then_receives() {
        let arm: Vec<u8> = vec![0x18, 0x80, 0xC0, 0x00, 0x00, 0x11, 0x06, 0x00];
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();

        let expect_arm = arm.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 256];
            let (n, src) = target.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], &expect_arm[..], "arm datagram must arrive first");
            let pkt = ccsds_spp::pack(0x0D0, 1, &[7, 7, 7, 7]);
            target.send_to(&pkt, src).await.unwrap();
        });

        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut link = StreamLink::new(
            Protocol::CcsdsSpp,
            PipelineSpec::Spp {
                apid_map: HashMap::from([(0x0D0u16, 0x00D000u32)]),
            },
            // Port 0: the fake target replies to the arm's source
            // address, so the test needs no fixed-port coordination.
            Carrier::Udp { listen_port: 0 },
            vec![arm],
            push_tx,
        );
        link.connect(&target_addr.ip().to_string(), target_addr.port())
            .await
            .unwrap();
        assert!(link.is_connected());

        let pkt = recv_one(&mut push_rx).await;
        assert_eq!(pkt.full_uid, 0x00D000);
        assert_eq!(pkt.payload, vec![7, 7, 7, 7]);
        link.disconnect();
    }

    /// @test The TCP carrier writes arm bytes before the reader owns
    /// the socket -- same arm semantics on both carriers.
    #[tokio::test]
    async fn tcp_carrier_writes_arm_before_reading() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4];
            tokio::io::AsyncReadExt::read_exact(&mut sock, &mut buf)
                .await
                .unwrap();
            assert_eq!(&buf, b"ARM!");
            sock.write_all(&slip::encode(&[1, 2, 3])).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        });

        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut link = StreamLink::new(
            Protocol::RawSlip,
            PipelineSpec::SlipRaw { uid: Some(0xAB) },
            Carrier::Tcp,
            vec![b"ARM!".to_vec()],
            push_tx,
        );
        link.connect(&addr.ip().to_string(), addr.port())
            .await
            .unwrap();
        let pkt = recv_one(&mut push_rx).await;
        assert_eq!(pkt.full_uid, 0xAB);
        assert_eq!(pkt.payload, vec![1, 2, 3]);
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
            Carrier::Tcp,
            Vec::new(),
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
