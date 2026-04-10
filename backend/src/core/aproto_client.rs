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

use crate::protocol::{aproto, slip};

/* ----------------------------- Error ----------------------------- */

/// APROTO client error type. Surfaces TCP-level connection failures,
/// timeouts, remote-side disconnects, and channel-send failures.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connection failed: {0}")]
    Connect(#[from] std::io::Error),
    #[error("not connected")]
    NotConnected,
    #[error("timeout waiting for response")]
    Timeout,
    #[error("connection closed by remote")]
    Closed,
    #[error("send failed")]
    SendFailed,
}

/* ----------------------------- Push Telemetry ----------------------------- */

/// A raw push telemetry packet received from the target. Carries the
/// component fullUid and the raw payload bytes; the decoder converts
/// this into named samples via the per-target struct dictionary.
///
/// The original opcode is intentionally NOT stored: zenith routes
/// purely on (fullUid, payload size) so the opcode is opaque after
/// the protocol parser has stripped the header.
#[derive(Debug, Clone)]
pub struct PushTelemetryPacket {
    pub full_uid: u32,
    pub payload: Vec<u8>,
}

/* ----------------------------- Internal ----------------------------- */

struct CommandRequest {
    encoded_packet: Vec<u8>,
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
            connected: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
        }
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

        // Writer task: send commands, pair with ACK from reader.
        //
        // LIMITATION: ACK/NAK responses are matched to requests by order,
        // not by sequence number. If the target reorders responses, the
        // wrong response will be delivered to the wrong caller. A full fix
        // would require matching by the sequence number in the ACK payload.
        let conn_flag_w = connected.clone();
        let writer_handle = tokio::spawn(async move {
            let mut write_half = write_half;
            let mut cmd_rx = cmd_rx;
            let mut ack_rx = ack_rx;

            while let Some(req) = cmd_rx.recv().await {
                if write_half.write_all(&req.encoded_packet).await.is_err() {
                    let _ = req.response_tx.send(Err(ClientError::SendFailed));
                    break;
                }
                match ack_rx.recv().await {
                    Some(result) => {
                        let _ = req.response_tx.send(result);
                    }
                    None => {
                        let _ = req.response_tx.send(Err(ClientError::Closed));
                        break;
                    }
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

    pub async fn send_command(
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
                response_tx,
            })
            .await
            .map_err(|_| ClientError::NotConnected)?;

        match timeout(self.command_timeout, response_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(_) => Err(ClientError::Timeout),
        }
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

        // FILE_END
        let resp = self.send_command(0, aproto::FILE_END, &[]).await?;
        if resp.status != 0 {
            return Ok(resp);
        }

        // RELOAD_TPRM
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
