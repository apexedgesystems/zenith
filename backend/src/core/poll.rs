//! Block polling: timed reads of declared data blocks for targets that
//! push nothing, published into the target's telemetry stream.
//!
//! Each `[[targets.poll]]` row becomes one task that reads its block
//! with INSPECT at the row's rate and sends the bytes as a
//! [`PushTelemetryPacket`], so the router, storage, charts and
//! dashboard handle a polled block as they handle a pushed one. The
//! packet carries the requested data category: two blocks of one
//! component can share a size, and the category tells the decoder
//! which one was read.

use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use crate::config::PollBlock;
use crate::core::config_manager::DataCategory;
use crate::core::transport::{ProtocolLink, PushTelemetryPacket};

/* ----------------------------- API ----------------------------- */

/// Start one task per poll row. A task ends when the link drops; the
/// next connect starts fresh ones. The caller owns the handles and
/// aborts them on disconnect, remove, reconnect and shutdown.
pub fn spawn_polls(
    blocks: &[PollBlock],
    client: Arc<Mutex<ProtocolLink>>,
    push_tx: broadcast::Sender<PushTelemetryPacket>,
) -> Vec<JoinHandle<()>> {
    blocks
        .iter()
        .filter_map(|b| {
            let uid = b.full_uid()?;
            // Boot refuses a row without a usable period; one that gets
            // here anyway is skipped, never converted.
            let Some(period) = b.period() else {
                tracing::error!(
                    "poll row {} has no usable timer period (rate_hz {:e}); not started",
                    b.uid,
                    b.rate_hz
                );
                return None;
            };
            let category = b.category;
            let data_category = DataCategory::from_index(category);
            let client = client.clone();
            let push_tx = push_tx.clone();
            Some(tokio::spawn(async move {
                let mut tick = tokio::time::interval(period);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    let read = {
                        let mut link = client.lock().await;
                        if !link.is_connected() {
                            break;
                        }
                        let Some(cli) = link.aproto() else {
                            break;
                        };
                        cli.inspect(uid, category, 0, 0).await
                    };
                    if let Ok(resp) = read {
                        if resp.status == 0 && !resp.extra.is_empty() {
                            let _ = push_tx.send(PushTelemetryPacket {
                                full_uid: uid,
                                payload: resp.extra,
                                category: data_category,
                            });
                        }
                    }
                }
            }))
        })
        .collect()
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::aproto_client::AprotoClient;
    use crate::core::config_manager::StructDictionary;
    use crate::core::telemetry::TelemetryDecoder;
    use crate::protocol::{aproto, slip};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// SpecChannel OUTPUT as the shipped spec demo lays it out: value
    /// 2.5 (float32) then sequence 7 (uint32).
    const OUTPUT_BYTES: [u8; 8] = [0x00, 0x00, 0x20, 0x40, 0x07, 0x00, 0x00, 0x00];

    /// A fake target that answers every INSPECT with `block` and keeps
    /// the category each read asked for.
    async fn spawn_block_target(
        block: Vec<u8>,
        asked: Arc<std::sync::Mutex<Vec<u8>>>,
    ) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut decoder = slip::Decoder::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for frame in decoder.feed(&buf[..n]) {
                    let Some(pkt) = aproto::parse_packet(&frame) else {
                        continue;
                    };
                    if pkt.header.opcode != aproto::EXEC_INSPECT {
                        continue;
                    }
                    asked.lock().unwrap().push(pkt.payload[4]);
                    let mut ack = Vec::new();
                    ack.extend_from_slice(&pkt.header.opcode.to_le_bytes());
                    ack.extend_from_slice(&pkt.header.sequence.to_le_bytes());
                    ack.push(0);
                    ack.push(aproto::STAGE_RESULT);
                    ack.extend_from_slice(&[0u8; 2]);
                    ack.extend_from_slice(&block);
                    let mut reply = Vec::new();
                    reply.extend_from_slice(&aproto::MAGIC.to_le_bytes());
                    reply.push(aproto::VERSION);
                    reply.push(aproto::FLAG_RESPONSE);
                    reply.extend_from_slice(&0u32.to_le_bytes());
                    reply.extend_from_slice(&aproto::SYS_ACK.to_le_bytes());
                    reply.extend_from_slice(&0u16.to_le_bytes());
                    reply.extend_from_slice(&(ack.len() as u16).to_le_bytes());
                    reply.extend_from_slice(&ack);
                    if sock.write_all(&slip::encode(&reply)).await.is_err() {
                        return;
                    }
                }
            }
        });
        addr
    }

    /// @test A polled read carries the category it asked for, and the
    /// decoder names the block's fields by it: in the shipped spec demo
    /// SpecChannel's OUTPUT and STATE are both eight bytes, and the
    /// polled OUTPUT decodes as value and sequence, not as STATE's
    /// elapsed and zeros.
    #[tokio::test]
    async fn polled_read_carries_its_category_to_the_decoder() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let addr = spawn_block_target(OUTPUT_BYTES.to_vec(), asked.clone()).await;
        let (push_tx, mut push_rx) = broadcast::channel(16);
        let mut client = AprotoClient::new(push_tx.clone());
        client.connect("127.0.0.1", addr.port()).await.unwrap();
        let link = Arc::new(Mutex::new(ProtocolLink::Aproto(client)));
        let row = PollBlock {
            uid: "0x00DA00".to_string(),
            category: 4,
            rate_hz: 50.0,
        };

        let handles = spawn_polls(&[row], link.clone(), push_tx);
        let pkt = tokio::time::timeout(Duration::from_secs(2), push_rx.recv())
            .await
            .expect("a polled packet within the deadline")
            .unwrap();
        for h in &handles {
            h.abort();
        }
        link.lock().await.disconnect();

        assert_eq!(pkt.full_uid, 0x00DA00);
        assert_eq!(pkt.payload, OUTPUT_BYTES);
        assert_eq!(pkt.category, Some(DataCategory::Output));
        assert_eq!(asked.lock().unwrap()[0], 4, "the read asked for OUTPUT");

        let dicts = StructDictionary::load_dir(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../targets/sil-spec-demo/structs"
        )))
        .unwrap();
        let decoder =
            TelemetryDecoder::new(&dicts, &[(0x00DA00, "SpecChannel", Some("SpecChannel"))]);
        let named: Vec<(String, f64)> = decoder
            .decode(&Arc::from("t"), 1, &pkt)
            .iter()
            .map(|s| (s.channel.to_string(), s.value))
            .collect();
        assert_eq!(
            named,
            [
                ("SpecChannel.value".to_string(), 2.5),
                ("SpecChannel.sequence".to_string(), 7.0)
            ]
        );
    }

    /// @test A row whose rate gives no usable period is skipped, not
    /// converted: the conversion site takes the validated period, so an
    /// unvalidated 1e-300 Hz row starts no task (and does not panic)
    /// while a valid row beside it still starts.
    #[tokio::test]
    async fn spawn_polls_skips_a_row_without_a_usable_period() {
        let (push_tx, _rx) = broadcast::channel(4);
        let link = Arc::new(Mutex::new(ProtocolLink::Aproto(AprotoClient::new(
            push_tx.clone(),
        ))));
        let row = |rate_hz: f64| PollBlock {
            uid: "0x00DE00".to_string(),
            category: 4,
            rate_hz,
        };

        let handles = spawn_polls(&[row(1e-300), row(5.0)], link, push_tx);

        assert_eq!(handles.len(), 1, "only the valid row starts a task");
        for h in &handles {
            h.abort();
        }
    }
}
