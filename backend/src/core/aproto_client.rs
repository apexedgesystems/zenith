//! Async APROTO client with split reader for push telemetry support.
//!
//! Spawns a background reader task that continuously reads from the TCP
//! socket, decodes SLIP frames, and routes packets:
//!   - ACK/NAK responses -> mpsc channel back to command callers
//!   - Push telemetry -> broadcast channel for WebSocket subscribers
//!
//! This allows Zenith to receive push telemetry at whatever rate the
//! target's TelemetryManager sends it -- no polling, no assumed rates.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// CRC-32C (Castagnoli) used for file transfer integrity.
fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0x82F63B78;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ 0xFFFFFFFF
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;

pub use crate::core::transport::{ClientError, PushTelemetryPacket};
use crate::protocol::{aproto, slip};

/* ----------------------------- Internal ----------------------------- */

struct CommandRequest {
    encoded_packet: Vec<u8>,
    /// Sequence number stamped into the packet header; the writer task
    /// only accepts an ACK whose cmd_sequence matches.
    seq: u16,
    response_tx: oneshot::Sender<Result<aproto::AckResponse, ClientError>>,
}

/* ----------------------------- Client ----------------------------- */

/// Async APROTO client owning one TCP socket to a target. Uses split
/// reader/writer halves: the reader task continuously decodes SLIP
/// frames and routes ACK responses (mpsc) versus push telemetry
/// (broadcast). One client per zenith target.
pub struct AprotoClient {
    cmd_tx: Option<mpsc::Sender<CommandRequest>>,
    push_tlm_tx: broadcast::Sender<PushTelemetryPacket>,
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    writer_handle: Option<tokio::task::JoinHandle<()>>,
    seq: u16,
    command_timeout: Duration,
    /// Optional pipeline counters; when set, every command round trip
    /// records send/error/timeout counts and successful-trip latency.
    metrics: Option<Arc<crate::core::metrics::TargetMetrics>>,
    connected: Arc<AtomicBool>,
    /// Generation counter: increments on each connect. Reader tasks only
    /// clear `connected` if their generation matches (prevents stale tasks
    /// from clearing a newer connection's flag).
    generation: Arc<AtomicU64>,
}

impl AprotoClient {
    /// Create a fresh, unconnected client. The caller supplies the
    /// broadcast sender for push telemetry packets so multiple
    /// subscribers can fan out from one TCP read loop.
    pub fn new(push_tlm_tx: broadcast::Sender<PushTelemetryPacket>) -> Self {
        Self {
            cmd_tx: None,
            push_tlm_tx,
            reader_handle: None,
            writer_handle: None,
            seq: 0,
            command_timeout: Duration::from_secs(5),
            metrics: None,
            connected: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Attach pipeline counters for command accounting.
    pub fn set_metrics(&mut self, metrics: Arc<crate::core::metrics::TargetMetrics>) {
        self.metrics = Some(metrics);
    }

    pub async fn connect(&mut self, host: &str, port: u16) -> Result<(), ClientError> {
        let addr = format!("{}:{}", host, port);
        let stream = timeout(Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out"))?
            .map_err(ClientError::Connect)?;
        stream.set_nodelay(true)?;

        // Enable TCP keepalive to detect dead connections
        let sock_ref = socket2::SockRef::from(&stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10));
        let _ = sock_ref.set_tcp_keepalive(&keepalive);

        let (read_half, write_half) = stream.into_split();

        // Reader -> writer: ACK responses
        let (ack_tx, ack_rx) = mpsc::channel::<Result<aproto::AckResponse, ClientError>>(64);

        // Command callers -> writer: outbound packets
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(64);

        let connected = self.connected.clone();
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        connected.store(true, Ordering::Release);

        // Reader task: decode SLIP frames, route ACK vs telemetry
        let push_tx = self.push_tlm_tx.clone();
        let conn_flag = connected.clone();
        let gen_flag = self.generation.clone();
        let reader_handle = tokio::spawn(async move {
            let mut decoder = slip::Decoder::new();
            let mut buf = vec![0u8; 65536];
            let mut read_half = read_half;

            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) => {
                        // Only clear connected if this is still the active generation
                        if gen_flag.load(Ordering::Acquire) == gen {
                            conn_flag.store(false, Ordering::Release);
                        }
                        tracing::info!("Connection closed by remote");
                        break;
                    }
                    Ok(n) => {
                        for frame in decoder.feed(&buf[..n]) {
                            if let Some(pkt) = aproto::parse_packet(&frame) {
                                if !pkt.header.is_response() {
                                    continue;
                                }
                                let opc = pkt.header.opcode;
                                if opc == aproto::SYS_ACK || opc == aproto::SYS_NAK {
                                    if let Some(ack) = aproto::parse_ack(&pkt.payload) {
                                        let _ = ack_tx.send(Ok(ack)).await;
                                    }
                                } else {
                                    // Push telemetry
                                    let _ = push_tx.send(PushTelemetryPacket {
                                        full_uid: pkt.header.full_uid,
                                        payload: pkt.payload,
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if gen_flag.load(Ordering::Acquire) == gen {
                            conn_flag.store(false, Ordering::Release);
                        }
                        tracing::error!("Read error: {}", e);
                        break;
                    }
                }
            }
        });

        // Writer task: send commands, pair each with its ACK by sequence
        // number. The wait is bounded: a target that never ACKs fails
        // that one command with Timeout and the writer keeps serving --
        // an unbounded recv here once wedged the client permanently
        // (full cmd channel, is_connected still true) until an explicit
        // disconnect. A late ACK for a timed-out command is discarded by
        // the sequence match when it eventually arrives.
        let conn_flag_w = connected.clone();
        let ack_timeout = self.command_timeout;
        let writer_handle = tokio::spawn(async move {
            let mut write_half = write_half;
            let mut cmd_rx = cmd_rx;
            let mut ack_rx = ack_rx;

            while let Some(req) = cmd_rx.recv().await {
                if write_half.write_all(&req.encoded_packet).await.is_err() {
                    let _ = req.response_tx.send(Err(ClientError::SendFailed));
                    break;
                }
                let deadline = tokio::time::Instant::now() + ack_timeout;
                let mut saw_queued = false;
                let result = loop {
                    match tokio::time::timeout_at(deadline, ack_rx.recv()).await {
                        Ok(Some(Ok(ack))) => {
                            if ack.cmd_sequence == req.seq {
                                // A QUEUED frame is an interim receipt for
                                // a deferred command: the COMPLETION frame
                                // that follows carries the handler's real
                                // status and extra, so keep waiting on the
                                // same deadline. Legacy vehicles zero the
                                // stage byte and always break here as
                                // RESULT terminals.
                                if ack.stage == aproto::STAGE_QUEUED {
                                    saw_queued = true;
                                    continue;
                                }
                                let mut ack = ack;
                                ack.queued = saw_queued;
                                break Ok(ack);
                            }
                            tracing::warn!(
                                "Discarding stale ACK (seq {}, awaiting {})",
                                ack.cmd_sequence,
                                req.seq
                            );
                        }
                        Ok(Some(Err(e))) => break Err(e),
                        Ok(None) => break Err(ClientError::Closed),
                        Err(_) => break Err(ClientError::Timeout),
                    }
                };
                let closed = matches!(result, Err(ClientError::Closed));
                let _ = req.response_tx.send(result);
                if closed {
                    break;
                }
            }

            conn_flag_w.store(false, Ordering::Release);
        });

        self.cmd_tx = Some(cmd_tx);
        self.reader_handle = Some(reader_handle);
        self.writer_handle = Some(writer_handle);
        self.seq = 0;

        tracing::info!("Connected to {}", addr);
        Ok(())
    }

    /// Tear down the connection: clear the connected flag, drop the
    /// command sender, and abort the reader and writer tasks.
    pub fn disconnect(&mut self) {
        self.connected.store(false, Ordering::Release);
        self.cmd_tx = None;
        if let Some(h) = self.reader_handle.take() {
            h.abort();
        }
        if let Some(h) = self.writer_handle.take() {
            h.abort();
        }
        tracing::info!("Disconnected");
    }

    /// Returns true while the reader task believes the underlying TCP
    /// socket is alive. Goes false on remote close, read error, or
    /// explicit disconnect.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Lock-free handle to the connection flag. Status reads (target
    /// lists, health) must not wait on the client mutex -- a file
    /// transfer holds that mutex for the whole upload -- so they read
    /// this flag instead. The Arc is stable across reconnects (the
    /// generation counter guards stale writers).
    pub fn connected_handle(&self) -> Arc<AtomicBool> {
        self.connected.clone()
    }

    pub async fn send_command(
        &mut self,
        full_uid: u32,
        opcode: u16,
        payload: &[u8],
    ) -> Result<aproto::AckResponse, ClientError> {
        let started = std::time::Instant::now();
        let result = self.send_command_inner(full_uid, opcode, payload).await;
        if let Some(m) = &self.metrics {
            m.commands_sent.fetch_add(1, Ordering::Relaxed);
            match &result {
                // A NAK is still a completed round trip; only transport
                // failures count as errors.
                Ok(_) => {
                    m.command_latency_us_total
                        .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    m.command_errors.fetch_add(1, Ordering::Relaxed);
                    if matches!(e, ClientError::Timeout) {
                        m.command_timeouts.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        result
    }

    async fn send_command_inner(
        &mut self,
        full_uid: u32,
        opcode: u16,
        payload: &[u8],
    ) -> Result<aproto::AckResponse, ClientError> {
        let cmd_tx = self.cmd_tx.clone().ok_or(ClientError::NotConnected)?;

        let seq = self.next_seq();
        let packet = aproto::build_command(full_uid, opcode, seq, payload);
        let encoded = slip::encode(&packet);

        let (response_tx, response_rx) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                encoded_packet: encoded,
                seq,
                response_tx,
            })
            .await
            .map_err(|_| ClientError::NotConnected)?;

        // The writer task owns the ACK timeout; this outer timeout is a
        // backstop with margin so the writer's verdict (Timeout vs a
        // late-but-matched ACK) normally wins the race.
        match timeout(self.command_timeout + Duration::from_secs(2), response_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(_) => Err(ClientError::Timeout),
        }
    }

    /// Override the ACK/command timeout (applies to connections made
    /// after the call; the writer task captures it at connect time).
    /// The server binary uses the 5 s default; the fake-target tests
    /// shorten it. allow(dead_code) because the bin target compiles
    /// this module too and does not call it.
    #[allow(dead_code)]
    pub fn set_command_timeout(&mut self, timeout: Duration) {
        self.command_timeout = timeout;
    }

    /* ----------------------------- Convenience ----------------------------- */

    pub async fn noop(&mut self) -> Result<aproto::AckResponse, ClientError> {
        self.send_command(0x000000, aproto::SYS_NOOP, &[]).await
    }

    pub async fn get_health(&mut self) -> Result<aproto::AckResponse, ClientError> {
        self.send_command(0x000000, aproto::EXEC_GET_HEALTH, &[])
            .await
    }

    pub async fn inspect(
        &mut self,
        full_uid: u32,
        category: u8,
        offset: u16,
        length: u16,
    ) -> Result<aproto::AckResponse, ClientError> {
        let mut payload = Vec::with_capacity(9);
        payload.extend_from_slice(&full_uid.to_le_bytes());
        payload.push(category);
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(&length.to_le_bytes());
        self.send_command(0x000000, aproto::EXEC_INSPECT, &payload)
            .await
    }

    /// Upload a file to the target and reload TPRM for a component.
    ///
    /// Steps: FILE_BEGIN -> FILE_CHUNK(s) -> FILE_END -> RELOAD_TPRM
    pub async fn update_tprm(
        &mut self,
        full_uid: u32,
        data: &[u8],
    ) -> Result<aproto::AckResponse, ClientError> {
        let staged = self.stage_tprm(full_uid, data).await?;
        if staged.status != 0 {
            return Ok(staged);
        }
        self.reload_tprm(full_uid).await
    }

    /// Upload a stamped TPRM payload to the staged bank WITHOUT
    /// reloading -- the first step of the verify-before-apply flow.
    /// Returns the FILE_END response (or the first failing step's).
    pub async fn stage_tprm(
        &mut self,
        full_uid: u32,
        data: &[u8],
    ) -> Result<aproto::AckResponse, ClientError> {
        let chunk_size: usize = 4096;
        let total_chunks = data.len().div_ceil(chunk_size);

        // CRC-32C
        let crc = crc32c(data);

        // Remote path: bank_b/tprm/{fullUid:06x}.tprm
        let remote_path = format!("bank_b/tprm/{:06x}.tprm", full_uid);

        // FILE_BEGIN (76 bytes: u32 totalSize, u16 chunkSize, u16 totalChunks, u32 crc, char[64] path)
        let mut begin_payload = Vec::with_capacity(76);
        begin_payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
        begin_payload.extend_from_slice(&(chunk_size as u16).to_le_bytes());
        begin_payload.extend_from_slice(&(total_chunks as u16).to_le_bytes());
        begin_payload.extend_from_slice(&crc.to_le_bytes());
        let path_bytes = remote_path.as_bytes();
        let mut path_buf = [0u8; 64];
        let copy_len = path_bytes.len().min(63);
        path_buf[..copy_len].copy_from_slice(&path_bytes[..copy_len]);
        begin_payload.extend_from_slice(&path_buf);

        let resp = self
            .send_command(0, aproto::FILE_BEGIN, &begin_payload)
            .await?;
        if resp.status != 0 {
            return Ok(resp);
        }

        // FILE_CHUNK(s)
        for i in 0..total_chunks {
            let offset = i * chunk_size;
            let end = (offset + chunk_size).min(data.len());
            let chunk_data = &data[offset..end];

            let mut chunk_payload = Vec::with_capacity(2 + chunk_data.len());
            chunk_payload.extend_from_slice(&(i as u16).to_le_bytes());
            chunk_payload.extend_from_slice(chunk_data);

            let resp = self
                .send_command(0, aproto::FILE_CHUNK, &chunk_payload)
                .await?;
            if resp.status != 0 {
                return Ok(resp);
            }
        }

        // FILE_END completes the staged upload; RELOAD is the
        // caller's decision.
        self.send_command(0, aproto::FILE_END, &[]).await
    }

    /// Apply the staged TPRM for a component (RELOAD_TPRM). On a
    /// readback-capable vehicle a failed verify refuses here with
    /// status 5 and the TprmPayloadCheck verdict in the extra.
    pub async fn reload_tprm(&mut self, full_uid: u32) -> Result<aproto::AckResponse, ClientError> {
        let mut reload_payload = Vec::with_capacity(4);
        reload_payload.extend_from_slice(&full_uid.to_le_bytes());
        self.send_command(0x000000, 0x0125, &reload_payload).await
    }

    /// Upload an arbitrary file to the target filesystem.
    /// Returns the FILE_END response (SUCCESS if transfer complete).
    pub async fn upload_file(
        &mut self,
        remote_path: &str,
        data: &[u8],
    ) -> Result<aproto::AckResponse, ClientError> {
        let chunk_size: usize = 4096;
        let total_chunks = data.len().div_ceil(chunk_size);
        let crc = crc32c(data);

        // FILE_BEGIN
        let mut begin_payload = Vec::with_capacity(76);
        begin_payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
        begin_payload.extend_from_slice(&(chunk_size as u16).to_le_bytes());
        begin_payload.extend_from_slice(&(total_chunks as u16).to_le_bytes());
        begin_payload.extend_from_slice(&crc.to_le_bytes());
        let path_bytes = remote_path.as_bytes();
        let mut path_buf = [0u8; 64];
        let copy_len = path_bytes.len().min(63);
        path_buf[..copy_len].copy_from_slice(&path_bytes[..copy_len]);
        begin_payload.extend_from_slice(&path_buf);

        let resp = self
            .send_command(0, aproto::FILE_BEGIN, &begin_payload)
            .await?;
        if resp.status != 0 {
            return Ok(resp);
        }

        // FILE_CHUNK(s)
        for i in 0..total_chunks {
            let offset = i * chunk_size;
            let end = (offset + chunk_size).min(data.len());
            let mut chunk_payload = Vec::with_capacity(2 + end - offset);
            chunk_payload.extend_from_slice(&(i as u16).to_le_bytes());
            chunk_payload.extend_from_slice(&data[offset..end]);

            let resp = self
                .send_command(0, aproto::FILE_CHUNK, &chunk_payload)
                .await?;
            if resp.status != 0 {
                return Ok(resp);
            }
        }

        // FILE_END
        self.send_command(0, aproto::FILE_END, &[]).await
    }

    /// Restart the executive via the RELOAD_EXECUTIVE opcode (0x0127).
    ///
    /// Apex defers the execv until after the ACK is on the wire, so this
    /// is a normal send_command. The connection will drop shortly after
    /// the ACK arrives (the process image is replaced); callers should
    /// disconnect and wait before reconnecting.
    pub async fn restart_executive(&mut self) -> Result<aproto::AckResponse, ClientError> {
        let resp = self.send_command(0x000000, 0x0127, &[]).await?;
        self.disconnect();
        Ok(resp)
    }

    /// Orchestrated library hot-swap.
    ///
    /// Steps mirror the apex_csf python client's update_component():
    ///   1. Lock component (CMD_LOCK 0x0114, payload u32 fullUid)
    ///   2. Upload .so to {inactive_bank}/libs/{name}_{instance}.so
    ///   3. Send RELOAD_LIBRARY (0x0126, payload u32 fullUid).
    ///      Executive auto-unlocks on success.
    ///   4. On any pre-reload failure, attempt unlock (best-effort).
    ///
    /// Returns the RELOAD_LIBRARY response on success or the failed
    /// step's response on failure.
    pub async fn swap_library(
        &mut self,
        full_uid: u32,
        component_name: &str,
        instance_index: u32,
        inactive_bank: &str,
        data: &[u8],
    ) -> Result<aproto::AckResponse, ClientError> {
        // Step 1: lock component
        let lock_payload = full_uid.to_le_bytes();
        let lock_resp = self.send_command(0x000000, 0x0114, &lock_payload).await?;
        if lock_resp.status != 0 {
            return Ok(lock_resp);
        }

        // Step 2: upload to inactive bank
        let remote_path = format!(
            "{}/libs/{}_{}.so",
            inactive_bank, component_name, instance_index
        );
        let upload_result = self.upload_file(&remote_path, data).await;
        let upload_resp = match upload_result {
            Ok(r) => r,
            Err(e) => {
                // Best-effort unlock then bubble error
                let unlock_payload = full_uid.to_le_bytes();
                let _ = self.send_command(0x000000, 0x0115, &unlock_payload).await;
                return Err(e);
            }
        };
        if upload_resp.status != 0 {
            // Upload failed at protocol level -- unlock and return the upload error
            let unlock_payload = full_uid.to_le_bytes();
            let _ = self.send_command(0x000000, 0x0115, &unlock_payload).await;
            return Ok(upload_resp);
        }

        // Step 3: reload library (executive auto-unlocks on success)
        let reload_payload = full_uid.to_le_bytes();
        let reload_result = self.send_command(0x000000, 0x0126, &reload_payload).await;
        match reload_result {
            Ok(resp) => {
                if resp.status != 0 {
                    // Reload failed -- attempt unlock
                    let unlock_payload = full_uid.to_le_bytes();
                    let _ = self.send_command(0x000000, 0x0115, &unlock_payload).await;
                }
                Ok(resp)
            }
            Err(e) => {
                let unlock_payload = full_uid.to_le_bytes();
                let _ = self.send_command(0x000000, 0x0115, &unlock_payload).await;
                Err(e)
            }
        }
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    /// Build a SLIP-encoded ACK response packet as a target would send it.
    fn ack_packet(cmd_opcode: u16, cmd_seq: u16, status: u8) -> Vec<u8> {
        staged_ack_packet(cmd_opcode, cmd_seq, status, aproto::STAGE_RESULT)
    }

    fn staged_ack_packet(cmd_opcode: u16, cmd_seq: u16, status: u8, stage: u8) -> Vec<u8> {
        let mut ack_payload = Vec::with_capacity(8);
        ack_payload.extend_from_slice(&cmd_opcode.to_le_bytes());
        ack_payload.extend_from_slice(&cmd_seq.to_le_bytes());
        ack_payload.push(status);
        ack_payload.push(stage);
        ack_payload.extend_from_slice(&[0u8; 2]);

        let mut pkt = Vec::with_capacity(aproto::HEADER_SIZE + ack_payload.len());
        pkt.extend_from_slice(&aproto::MAGIC.to_le_bytes());
        pkt.push(aproto::VERSION);
        pkt.push(aproto::FLAG_RESPONSE);
        pkt.extend_from_slice(&0u32.to_le_bytes());
        pkt.extend_from_slice(&aproto::SYS_ACK.to_le_bytes());
        pkt.extend_from_slice(&0u16.to_le_bytes());
        pkt.extend_from_slice(&(ack_payload.len() as u16).to_le_bytes());
        pkt.extend_from_slice(&ack_payload);
        slip::encode(&pkt)
    }

    /// Spawn a fake target that decides per received command what to
    /// send back. The handler returns SLIP-encoded reply bytes (empty
    /// vec = stay silent for that command).
    async fn spawn_fake_target<F>(mut handler: F) -> std::net::SocketAddr
    where
        F: FnMut(&aproto::Packet) -> Vec<u8> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = match listener.accept().await {
                Ok(a) => a,
                Err(_) => return,
            };
            let mut decoder = slip::Decoder::new();
            let mut buf = [0u8; 65536];
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for frame in decoder.feed(&buf[..n]) {
                    if let Some(pkt) = aproto::parse_packet(&frame) {
                        let reply = handler(&pkt);
                        if !reply.is_empty() && sock.write_all(&reply).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        addr
    }

    async fn connected_client(
        addr: std::net::SocketAddr,
        timeout_ms: u64,
    ) -> (AprotoClient, broadcast::Receiver<PushTelemetryPacket>) {
        let (push_tx, push_rx) = broadcast::channel(64);
        let mut client = AprotoClient::new(push_tx);
        client.set_command_timeout(Duration::from_millis(timeout_ms));
        client
            .connect(&addr.ip().to_string(), addr.port())
            .await
            .unwrap();
        (client, push_rx)
    }

    /// @test A command round-trips against a well-behaved target: the
    /// ACK is matched by sequence and carries the status through, and
    /// a legacy zeroed stage byte reads as an immediate RESULT.
    #[tokio::test]
    async fn command_round_trip_success() {
        let addr =
            spawn_fake_target(|pkt| ack_packet(pkt.header.opcode, pkt.header.sequence, 0)).await;
        let (mut client, _rx) = connected_client(addr, 1000).await;

        let resp = client.noop().await.unwrap();
        assert_eq!(resp.status, 0);
        assert_eq!(resp.cmd_sequence, 0);
        assert_eq!(resp.stage, aproto::STAGE_RESULT);
        assert!(!resp.queued);
        assert!(client.is_connected());
    }

    /// @test A deferred command's QUEUED interim frame is not returned
    /// to the caller: the client waits through it and delivers the
    /// COMPLETION terminal carrying the handler's real status, with
    /// the queued flag set so the UI can show the lifecycle.
    #[tokio::test]
    async fn queued_command_waits_for_completion_frame() {
        let addr = spawn_fake_target(|pkt| {
            let mut reply = staged_ack_packet(
                pkt.header.opcode,
                pkt.header.sequence,
                0,
                aproto::STAGE_QUEUED,
            );
            reply.extend(staged_ack_packet(
                pkt.header.opcode,
                pkt.header.sequence,
                5,
                aproto::STAGE_COMPLETION,
            ));
            reply
        })
        .await;
        let (mut client, _rx) = connected_client(addr, 1000).await;

        let resp = client.noop().await.unwrap();
        assert_eq!(resp.stage, aproto::STAGE_COMPLETION);
        assert!(resp.queued, "interim QUEUED frame must set the flag");
        assert_eq!(resp.status, 5, "terminal status comes from COMPLETION");
        assert!(client.is_connected());
    }

    /// @test A QUEUED interim with no COMPLETION inside the deadline is
    /// a Timeout -- the interim receipt alone is never presented as an
    /// outcome, and the client stays usable afterward.
    #[tokio::test]
    async fn queued_without_completion_times_out() {
        let addr = spawn_fake_target(|pkt| {
            if pkt.header.sequence == 0 {
                staged_ack_packet(pkt.header.opcode, 0, 0, aproto::STAGE_QUEUED)
            } else {
                ack_packet(pkt.header.opcode, pkt.header.sequence, 0)
            }
        })
        .await;
        let (mut client, _rx) = connected_client(addr, 300).await;

        let first = client.noop().await;
        assert!(matches!(first, Err(ClientError::Timeout)));

        let second = client.noop().await.unwrap();
        assert_eq!(second.status, 0);
        assert!(client.is_connected());
    }

    /// @test A target that never ACKs fails that command with Timeout,
    /// and the client remains connected and usable: the next command
    /// (which the target does ACK) succeeds. Guards against the
    /// unbounded ACK wait that permanently wedged the writer task.
    #[tokio::test]
    async fn missing_ack_times_out_and_client_recovers() {
        let addr = spawn_fake_target(|pkt| {
            if pkt.header.sequence == 0 {
                Vec::new() // stay silent on the first command
            } else {
                ack_packet(pkt.header.opcode, pkt.header.sequence, 0)
            }
        })
        .await;
        let (mut client, _rx) = connected_client(addr, 200).await;

        let first = client.noop().await;
        assert!(matches!(first, Err(ClientError::Timeout)), "{first:?}");
        assert!(
            client.is_connected(),
            "timeout must not tear down the connection"
        );

        let second = client.noop().await.unwrap();
        assert_eq!(second.status, 0);
        assert_eq!(second.cmd_sequence, 1);
    }

    /// @test An ACK that arrives after its command already timed out is
    /// discarded by the sequence match instead of being delivered to
    /// the next command.
    #[tokio::test]
    async fn late_ack_is_discarded_not_misdelivered() {
        let addr = spawn_fake_target(|pkt| {
            if pkt.header.sequence == 0 {
                Vec::new() // silent: seq 0 will time out; ACK it later via seq-1 handler
            } else {
                // Deliver the stale seq-0 ACK first, then the real one.
                let mut both = ack_packet(aproto::SYS_NOOP, 0, 0);
                both.extend_from_slice(&ack_packet(pkt.header.opcode, pkt.header.sequence, 0));
                both
            }
        })
        .await;
        let (mut client, _rx) = connected_client(addr, 200).await;

        assert!(matches!(client.noop().await, Err(ClientError::Timeout)));
        let resp = client.noop().await.unwrap();
        assert_eq!(
            resp.cmd_sequence, 1,
            "second command must get its own ACK, not the stale seq-0 one"
        );
    }

    /// @test An ACK whose sequence never matches the in-flight command
    /// is not delivered; the command times out rather than receiving a
    /// mismatched response.
    #[tokio::test]
    async fn mismatched_seq_ack_is_not_delivered() {
        let addr = spawn_fake_target(|pkt| ack_packet(pkt.header.opcode, 999, 0)).await;
        let (mut client, _rx) = connected_client(addr, 200).await;

        assert!(matches!(client.noop().await, Err(ClientError::Timeout)));
        assert!(client.is_connected());
    }

    /// @test A remote close while a command is in flight yields Closed
    /// and drops is_connected.
    #[tokio::test]
    async fn remote_close_yields_closed_and_disconnects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await; // receive the command
            drop(sock); // close without ACKing
        });
        let (mut client, _rx) = connected_client(addr, 1000).await;

        let resp = client.noop().await;
        assert!(
            matches!(resp, Err(ClientError::Closed) | Err(ClientError::Timeout)),
            "{resp:?}"
        );
        // Reader observes the close and clears the flag.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!client.is_connected());
    }

    /// @test Sending without a connection returns NotConnected.
    #[tokio::test]
    async fn send_without_connection_is_not_connected() {
        let (push_tx, _rx) = broadcast::channel(4);
        let mut client = AprotoClient::new(push_tx);
        assert!(matches!(
            client.noop().await,
            Err(ClientError::NotConnected)
        ));
    }

    /// @test upload_file emits FILE_BEGIN with correct size/chunking/CRC
    /// fields, the right number of FILE_CHUNKs in order, and FILE_END.
    #[tokio::test]
    async fn upload_file_chunks_and_crc_are_correct() {
        type SeenPackets = Arc<Mutex<Vec<(u16, Vec<u8>)>>>;
        let seen: SeenPackets = Arc::new(Mutex::new(Vec::new()));
        let seen_writer = seen.clone();
        let addr = spawn_fake_target(move |pkt| {
            seen_writer
                .lock()
                .unwrap()
                .push((pkt.header.opcode, pkt.payload.clone()));
            ack_packet(pkt.header.opcode, pkt.header.sequence, 0)
        })
        .await;
        let (mut client, _rx) = connected_client(addr, 1000).await;

        // 10000 bytes -> 3 chunks of 4096/4096/1808
        let data: Vec<u8> = (0..10000u32).map(|i| (i % 251) as u8).collect();
        let resp = client.upload_file("test/upload.bin", &data).await.unwrap();
        assert_eq!(resp.status, 0);

        let seen = seen.lock().unwrap();
        let opcodes: Vec<u16> = seen.iter().map(|(o, _)| *o).collect();
        assert_eq!(
            opcodes,
            vec![
                aproto::FILE_BEGIN,
                aproto::FILE_CHUNK,
                aproto::FILE_CHUNK,
                aproto::FILE_CHUNK,
                aproto::FILE_END
            ]
        );

        let begin = &seen[0].1;
        assert_eq!(u32::from_le_bytes(begin[0..4].try_into().unwrap()), 10000);
        assert_eq!(u16::from_le_bytes(begin[4..6].try_into().unwrap()), 4096);
        assert_eq!(u16::from_le_bytes(begin[6..8].try_into().unwrap()), 3);
        assert_eq!(
            u32::from_le_bytes(begin[8..12].try_into().unwrap()),
            crc32c(&data)
        );
        assert!(begin[12..].starts_with(b"test/upload.bin\0"));

        // Chunk indices ascend from 0 and sizes partition the data.
        let chunk_sizes: Vec<usize> = seen[1..4]
            .iter()
            .enumerate()
            .map(|(i, (_, p))| {
                assert_eq!(u16::from_le_bytes(p[0..2].try_into().unwrap()) as usize, i);
                p.len() - 2
            })
            .collect();
        assert_eq!(chunk_sizes, vec![4096, 4096, 1808]);
    }

    /// @test crc32c matches the CRC-32C (Castagnoli) check vectors.
    #[test]
    fn crc32c_known_vectors() {
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }
}
