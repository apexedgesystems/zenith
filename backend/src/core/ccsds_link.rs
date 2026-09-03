//! CCSDS SPP telemetry link: TCP stream of space packets in, neutral
//! telemetry packets out.
//!
//! Telemetry-only by design: a command convention over SPP is a
//! cross-repo contract that does not exist yet, so this transport
//! implements the generic link surface and nothing else -- the
//! handler layer answers "unsupported for this protocol" for command
//! paths, which is the honest shape for a telemetry-only target.
//!
//! APID routing: per-target config maps each APID to the component
//! fullUid whose struct dictionaries decode the payload. Payload
//! bytes are the same struct bytes the dictionaries describe, so the
//! entire decode/storage/UI pipeline downstream of PushTelemetryPacket
//! is untouched wire-to-chart.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::time::{timeout, Duration};

use crate::core::aproto_client::ClientError;
use crate::core::transport::PushTelemetryPacket;
use crate::protocol::ccsds_spp;

pub struct SppLink {
    /// APID -> component fullUid, from per-target config.
    apid_map: HashMap<u16, u32>,
    push_tlm_tx: broadcast::Sender<PushTelemetryPacket>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    connected: Arc<AtomicBool>,
    /// Generation counter: reader tasks only clear `connected` when
    /// their generation is still current (same discipline as the
    /// APROTO client -- a stale task must not clear a newer
    /// connection's flag).
    generation: Arc<AtomicU64>,
}

impl SppLink {
    pub fn new(
        apid_map: HashMap<u16, u32>,
        push_tlm_tx: broadcast::Sender<PushTelemetryPacket>,
    ) -> Self {
        Self {
            apid_map,
            push_tlm_tx,
            reader_handle: None,
            connected: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Telemetry-only link: there is no command round trip to account,
    /// and the pipeline counters (decoded/written/drops) attach at the
    /// router and writer stages downstream. Accepted and discarded so
    /// the generic link surface stays uniform.
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
        let apid_map = self.apid_map.clone();
        let conn_flag = connected.clone();
        let gen_flag = self.generation.clone();
        let reader_handle = tokio::spawn(async move {
            const UNKNOWN_WARN_EVERY: Duration = Duration::from_secs(30);
            let mut last_unknown_warn: Option<tokio::time::Instant> = None;
            let mut extractor = ccsds_spp::Extractor::new();
            let mut buf = vec![0u8; 65536];
            let mut stream = stream;
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => {
                        if gen_flag.load(Ordering::Acquire) == gen {
                            conn_flag.store(false, Ordering::Release);
                        }
                        tracing::info!("SPP connection closed by remote");
                        break;
                    }
                    Ok(n) => {
                        for (hdr, payload) in extractor.feed(&buf[..n]) {
                            match apid_map.get(&hdr.apid) {
                                Some(&full_uid) => {
                                    let _ = push_tx.send(PushTelemetryPacket { full_uid, payload });
                                }
                                None => {
                                    // Unmapped APIDs are counted, not
                                    // fatal: a partial map is a valid
                                    // config that watches a subset.
                                    if last_unknown_warn
                                        .is_none_or(|t| t.elapsed() >= UNKNOWN_WARN_EVERY)
                                    {
                                        tracing::warn!(
                                            "SPP packet for unmapped APID 0x{:03X} dropped",
                                            hdr.apid
                                        );
                                        last_unknown_warn = Some(tokio::time::Instant::now());
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if gen_flag.load(Ordering::Acquire) == gen {
                            conn_flag.store(false, Ordering::Release);
                        }
                        tracing::error!("SPP read error: {}", e);
                        break;
                    }
                }
            }
        });

        self.reader_handle = Some(reader_handle);
        tracing::info!("Connected (CCSDS SPP) to {}", addr);
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.connected.store(false, Ordering::Release);
        if let Some(h) = self.reader_handle.take() {
            h.abort();
        }
        tracing::info!("Disconnected (CCSDS SPP)");
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

    /// @test THE protocol-agnosticism proof at unit scale: the same
    /// struct bytes delivered over CCSDS SPP framing decode to exactly
    /// the samples the decoder produces for those bytes directly --
    /// one dictionary, one decoder, two wire protocols. The SPP leg
    /// runs the real transport (TCP, extractor, APID routing) end to
    /// end into the neutral packet stream.
    #[tokio::test]
    async fn spp_wire_decodes_identically_to_direct_decode() {
        // 0.5f32, 1.0f32 -- the struct bytes both paths carry.
        let body: Vec<u8> = [0.5f32, 1.0f32]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();

        let dict = wavegen_dict();
        let decoder =
            TelemetryDecoder::new(&dict, &[(0x00D000, "WaveGenerator", Some("WaveGenerator"))]);
        let target: Arc<str> = Arc::from("t");

        // Reference: decode the bytes directly (what the APROTO path
        // hands the decoder after its own framing).
        let direct = decoder.decode(
            &target,
            1000,
            &PushTelemetryPacket {
                full_uid: 0x00D000,
                payload: body.clone(),
            },
        );
        assert_eq!(direct.len(), 2);

        // SPP leg: a fake target streams the same bytes wrapped in
        // space packets, split mid-packet across two writes to
        // exercise the extractor on a real socket.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let wire = crate::protocol::ccsds_spp::pack(0x0D0, 1, &body);
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&wire[..4]).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            sock.write_all(&wire[4..]).await.unwrap();
            // Hold the socket open until the test finishes reading.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        });

        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut link = SppLink::new(
            std::collections::HashMap::from([(0x0D0u16, 0x00D000u32)]),
            push_tx,
        );
        link.connect(&addr.ip().to_string(), addr.port())
            .await
            .unwrap();
        assert!(link.is_connected());

        let pkt = tokio::time::timeout(std::time::Duration::from_secs(2), push_rx.recv())
            .await
            .expect("SPP packet within deadline")
            .unwrap();
        assert_eq!(pkt.full_uid, 0x00D000);

        let via_spp = decoder.decode(&target, 1000, &pkt);
        assert_eq!(via_spp.len(), direct.len());
        for (a, b) in direct.iter().zip(via_spp.iter()) {
            assert_eq!(&*a.channel, &*b.channel);
            assert_eq!(a.value, b.value);
        }
        link.disconnect();
    }

    /// @test Packets for unmapped APIDs drop without disturbing the
    /// stream: the mapped packet that follows still arrives.
    #[tokio::test]
    async fn unmapped_apid_drops_and_stream_continues() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let unmapped = crate::protocol::ccsds_spp::pack(0x111, 1, &[1, 2, 3, 4]);
            let mapped = crate::protocol::ccsds_spp::pack(0x0D0, 2, &[5, 6, 7, 8]);
            sock.write_all(&unmapped).await.unwrap();
            sock.write_all(&mapped).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        });

        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut link = SppLink::new(
            std::collections::HashMap::from([(0x0D0u16, 0x00BEEF00u32)]),
            push_tx,
        );
        link.connect(&addr.ip().to_string(), addr.port())
            .await
            .unwrap();

        let pkt = tokio::time::timeout(std::time::Duration::from_secs(2), push_rx.recv())
            .await
            .expect("mapped packet within deadline")
            .unwrap();
        assert_eq!(pkt.full_uid, 0x00BEEF00);
        assert_eq!(pkt.payload, vec![5, 6, 7, 8]);
        assert!(
            push_rx.try_recv().is_err(),
            "unmapped packet must not arrive"
        );
        link.disconnect();
    }
}
