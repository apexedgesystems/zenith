//! Raw SLIP telemetry link: header-less SLIP frames in, neutral
//! telemetry packets out.
//!
//! The simplest transport zenith speaks, and deliberately so -- it
//! demonstrates the seam's floor. There is no packet header at all:
//! every SLIP frame's payload is one struct's bytes, and the
//! component fullUid comes entirely from per-target config
//! (`raw_uid`), the degenerate case of per-protocol addressing (a
//! bare instrument streaming its own telemetry). The decoder still
//! routes on (fullUid, payload size), so one device may interleave
//! structs of different sizes and each finds its dictionary layout.
//!
//! Telemetry-only, like any protocol without a command convention:
//! command surfaces answer "unsupported for this protocol".

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::time::{timeout, Duration};

use crate::core::transport::{ClientError, PushTelemetryPacket};
use crate::protocol::slip;

pub struct RawSlipLink {
    /// The one component this stream speaks for, from config.
    raw_uid: Option<u32>,
    push_tlm_tx: broadcast::Sender<PushTelemetryPacket>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    connected: Arc<AtomicBool>,
    /// Generation counter, same discipline as the other links: a
    /// stale reader must not clear a newer connection's flag.
    generation: Arc<AtomicU64>,
}

impl RawSlipLink {
    pub fn new(raw_uid: Option<u32>, push_tlm_tx: broadcast::Sender<PushTelemetryPacket>) -> Self {
        if raw_uid.is_none() {
            tracing::warn!(
                "raw-slip target has no raw_uid configured; frames will be dropped until one is set"
            );
        }
        Self {
            raw_uid,
            push_tlm_tx,
            reader_handle: None,
            connected: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Telemetry-only link: no command accounting exists; pipeline
    /// counters attach downstream at the router and writer.
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
        let raw_uid = self.raw_uid;
        let conn_flag = connected.clone();
        let gen_flag = self.generation.clone();
        let reader_handle = tokio::spawn(async move {
            let mut decoder = slip::Decoder::new();
            let mut buf = vec![0u8; 65536];
            let mut stream = stream;
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => {
                        if gen_flag.load(Ordering::Acquire) == gen {
                            conn_flag.store(false, Ordering::Release);
                        }
                        tracing::info!("raw-slip connection closed by remote");
                        break;
                    }
                    Ok(n) => {
                        for frame in decoder.feed(&buf[..n]) {
                            if frame.is_empty() {
                                continue;
                            }
                            if let Some(full_uid) = raw_uid {
                                let _ = push_tx.send(PushTelemetryPacket {
                                    full_uid,
                                    payload: frame,
                                });
                            }
                        }
                    }
                    Err(e) => {
                        if gen_flag.load(Ordering::Acquire) == gen {
                            conn_flag.store(false, Ordering::Release);
                        }
                        tracing::error!("raw-slip read error: {}", e);
                        break;
                    }
                }
            }
        });

        self.reader_handle = Some(reader_handle);
        tracing::info!("Connected (raw SLIP) to {}", addr);
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.connected.store(false, Ordering::Release);
        if let Some(h) = self.reader_handle.take() {
            h.abort();
        }
        tracing::info!("Disconnected (raw SLIP)");
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
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// @test The floor of the transport seam: header-less SLIP frames
    /// carrying struct bytes arrive as neutral packets stamped with
    /// the config uid -- split across writes mid-frame, with an empty
    /// frame (back-to-back END) skipped. Payload identity means the
    /// decoder downstream behaves exactly as for any other protocol.
    #[tokio::test]
    async fn slip_frames_become_neutral_packets() {
        let body: Vec<u8> = [0.5f32, 1.0f32]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let wire = slip::encode(&body);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let wire2 = wire.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Mid-frame split plus a bare END (empty frame) between.
            sock.write_all(&wire2[..3]).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            sock.write_all(&wire2[3..]).await.unwrap();
            sock.write_all(&[0xC0]).await.unwrap();
            sock.write_all(&slip::encode(&[7, 8])).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        });

        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut link = RawSlipLink::new(Some(0x00D000), push_tx);
        link.connect(&addr.ip().to_string(), addr.port())
            .await
            .unwrap();
        assert!(link.is_connected());

        let pkt = tokio::time::timeout(std::time::Duration::from_secs(2), push_rx.recv())
            .await
            .expect("frame within deadline")
            .unwrap();
        assert_eq!(pkt.full_uid, 0x00D000);
        assert_eq!(pkt.payload, body);

        let pkt2 = tokio::time::timeout(std::time::Duration::from_secs(2), push_rx.recv())
            .await
            .expect("second frame within deadline")
            .unwrap();
        assert_eq!(pkt2.payload, vec![7, 8]);
        link.disconnect();
    }

    /// @test Without a configured raw_uid, frames drop instead of
    /// inventing an address -- the link stays connected and harmless.
    #[tokio::test]
    async fn missing_uid_drops_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&slip::encode(&[1, 2, 3])).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        });

        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut link = RawSlipLink::new(None, push_tx);
        link.connect(&addr.ip().to_string(), addr.port())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(push_rx.try_recv().is_err());
        assert!(link.is_connected());
        link.disconnect();
    }
}
