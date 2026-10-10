//! The live telemetry socket: one task per client, streaming one
//! target's decoded samples as JSON text frames until the client goes
//! away, the target's sample channel closes, or -- for a socket opened
//! with a browser session -- the session ends, which closes the socket
//! from the server side with the policy code.
//!
//! The task reads as well as writes. Reading is what answers a client's
//! pings (the WebSocket layer queues the pong and sends it as the next
//! read starts), so a client that keeps its connection alive with pings
//! stays connected while a target is silent; and it is what notices a
//! client that closed or vanished, so its task ends at once instead of
//! at the next failed send.
//!
//! What a client may send is capped (`MAX_CLIENT_MESSAGE_BYTES`): the
//! stream needs nothing from it but control frames, so a larger frame
//! or message ends the stream instead of being held in memory.
//!
//! A client that stops reading cannot hold the task. A send waits while
//! the connection's buffers are full; it waits at most `SEND_TIMEOUT`,
//! after which the client is gone. A session end does not wait for it:
//! it abandons the send in progress and gives the policy close at most
//! `CLOSE_TIMEOUT`, so the stream ends within that bound of the
//! session's end whether or not the client reads.
//!
//! The HTTP layer calls `accept` and writes the audit record; this
//! module owns the limits and the loop, so the test suite drives both
//! over a real connection, through the same `accept` the server uses.

use std::future::Future;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use tokio::sync::{broadcast, watch};

use crate::core::metrics::TargetMetrics;
use crate::core::session::EndReason;
use crate::core::telemetry::TelemetrySample;

/* ----------------------------- Constants ----------------------------- */

/// At most one lag warning per socket in this interval: a slow client
/// would otherwise flood the log at ingest rates.
const LAG_WARN_EVERY: Duration = Duration::from_secs(30);

/// The largest frame, and message, the socket accepts from a client.
/// Control frames carry at most 125 bytes (RFC 6455); 4 KiB admits them
/// and the small text or binary frames some clients send (ignored), and
/// bounds what any client can make the server hold for its socket. A
/// frame declaring more is refused from its header, before its payload
/// is read; a message of several frames is refused once it passes the
/// cap. Either ends the stream as a client that is gone.
pub const MAX_CLIENT_MESSAGE_BYTES: usize = 4 * 1024;

/// The longest a send may wait for the client to take a frame. A
/// sample's frame is small and the connection buffers many of them, so a
/// client that takes nothing for this long has stopped reading or lost
/// its link; a slow one only falls behind (lag). Ten seconds rides out
/// a stall on a congested link.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest the policy close at a session end may wait for the
/// client: a client that is reading takes it at once, and one that is
/// not would never take it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/* ----------------------------- Types ----------------------------- */

/// Why a telemetry socket ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// The client closed the socket, the connection broke, a send to it
    /// failed, or it took nothing for `SEND_TIMEOUT`.
    ClientGone,
    /// The target's sample channel closed (the target was removed).
    SourceClosed,
    /// The socket's session ended; the policy close was attempted.
    SessionEnded(Option<EndReason>),
}

/* ----------------------------- Helpers ----------------------------- */

/// Resolves with the end reason when the socket's session ends; never
/// resolves for a socket without a session.
async fn session_ended(end: &mut Option<watch::Receiver<Option<EndReason>>>) -> Option<EndReason> {
    match end {
        Some(rx) => {
            // An error means the store dropped the session without a
            // reason; either way the session is over.
            let _ = rx.wait_for(Option::is_some).await;
            *rx.borrow()
        }
        None => std::future::pending().await,
    }
}

/// End a socket whose session ended. The policy close tells the page
/// this is not a dropped link; it gets at most `CLOSE_TIMEOUT`, and the
/// stream ends either way. Only this socket is affected -- the target's
/// link, router and recorder never see it.
async fn session_close(socket: &mut WebSocket, reason: Option<EndReason>) -> StreamEnd {
    let close = Message::Close(Some(CloseFrame {
        code: close_code::POLICY,
        reason: close_text(reason).into(),
    }));
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.send(close)).await;
    StreamEnd::SessionEnded(reason)
}

/* ----------------------------- API ----------------------------- */

/// Accept a telemetry socket upgrade with the limits on what the client
/// may send, and run `serve` on the socket. The server and the tests
/// open telemetry sockets only through here.
pub fn accept<F, Fut>(upgrade: WebSocketUpgrade, serve: F) -> Response
where
    F: FnOnce(WebSocket) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    upgrade
        .max_frame_size(MAX_CLIENT_MESSAGE_BYTES)
        .max_message_size(MAX_CLIENT_MESSAGE_BYTES)
        // The loop polls the read side as often as once per delivered
        // sample, and the WebSocket layer zero-fills its read buffer before
        // every read, one that finds nothing included. A client sends only
        // small frames, so the buffer is the size of the largest one it may
        // send.
        .read_buffer_size(MAX_CLIENT_MESSAGE_BYTES)
        .on_upgrade(serve)
}

/// The text of a session-end close frame, also used for its audit row.
pub fn close_text(reason: Option<EndReason>) -> &'static str {
    reason.map_or("session ended", EndReason::describe)
}

/// Stream `samples` to `socket` until the stream ends, and say why.
/// `session_end` is the store's end signal for a socket opened with a
/// browser session; token and ticket sockets pass None. The target's
/// client count covers the call.
pub async fn stream_samples(
    mut socket: WebSocket,
    mut samples: broadcast::Receiver<TelemetrySample>,
    target_id: &str,
    metrics: &TargetMetrics,
    mut session_end: Option<watch::Receiver<Option<EndReason>>>,
) -> StreamEnd {
    tracing::info!("WebSocket client connected for target {}", target_id);
    metrics.ws_clients.fetch_add(1, Ordering::Relaxed);
    let mut last_lag_warn: Option<Instant> = None;

    let end = loop {
        tokio::select! {
            received = samples.recv() => match received {
                Ok(sample) => {
                    let json = serde_json::to_string(&sample).unwrap_or_default();
                    let send = socket.send(Message::Text(json.into()));
                    // The send waits while the client is not reading; the
                    // session end must not wait with it.
                    tokio::select! {
                        sent = tokio::time::timeout(SEND_TIMEOUT, send) => {
                            if !matches!(sent, Ok(Ok(()))) {
                                break StreamEnd::ClientGone;
                            }
                        }
                        reason = session_ended(&mut session_end) => {
                            break session_close(&mut socket, reason).await;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    metrics.ws_lag_drops.fetch_add(n, Ordering::Relaxed);
                    if last_lag_warn.is_none_or(|t| t.elapsed() >= LAG_WARN_EVERY) {
                        tracing::warn!(
                            "[{}] WebSocket client lagged, {} samples dropped ({} total across clients)",
                            target_id,
                            n,
                            metrics.ws_lag_drops.load(Ordering::Relaxed)
                        );
                        last_lag_warn = Some(Instant::now());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break StreamEnd::SourceClosed,
            },
            incoming = socket.recv() => match incoming {
                // A close is answered like a ping (see the module
                // documentation) and ends the stream at the next read.
                // Text and binary frames carry nothing the stream uses.
                Some(Ok(_)) => {}
                None | Some(Err(_)) => break StreamEnd::ClientGone,
            },
            reason = session_ended(&mut session_end) => {
                break session_close(&mut socket, reason).await;
            }
        }
    };

    metrics.ws_clients.fetch_sub(1, Ordering::Relaxed);
    tracing::info!("WebSocket client disconnected for target {}", target_id);
    end
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::{routing::get, Router};
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
    use tokio_tungstenite::tungstenite::protocol::frame::Frame as RawFrame;
    use tokio_tungstenite::tungstenite::Message as Frame;
    use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

    const SECOND: Duration = Duration::from_secs(1);

    type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

    /// One telemetry socket served on a loopback port, wired to channels
    /// the test holds: the target's samples, the session's end signal,
    /// the target's metrics, and the stream's end as `stream_samples`
    /// reports it. The route serves one socket: it takes the receivers
    /// it is given, so dropping the test's senders is all that is left.
    struct Harness {
        url: String,
        samples: broadcast::Sender<TelemetrySample>,
        session: watch::Sender<Option<EndReason>>,
        metrics: Arc<TargetMetrics>,
        ends: mpsc::UnboundedReceiver<StreamEnd>,
    }

    async fn harness(with_session: bool) -> Harness {
        let (samples, samples_rx) = broadcast::channel(64);
        let (session, session_rx) = watch::channel(None);
        let metrics = TargetMetrics::new();
        let (end_tx, ends) = mpsc::unbounded_channel();
        let samples_slot = Arc::new(Mutex::new(Some(samples_rx)));
        let session_slot = Arc::new(Mutex::new(with_session.then_some(session_rx)));
        let route_metrics = metrics.clone();
        let app = Router::new().route(
            "/live",
            get(move |ws: WebSocketUpgrade| {
                let samples_rx = samples_slot.lock().unwrap().take();
                let session_rx = session_slot.lock().unwrap().take();
                let metrics = route_metrics.clone();
                let end_tx = end_tx.clone();
                async move {
                    accept(ws, move |socket| async move {
                        let samples_rx = samples_rx.expect("one socket per harness");
                        let end =
                            stream_samples(socket, samples_rx, "target-0", &metrics, session_rx)
                                .await;
                        let _ = end_tx.send(end);
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Harness {
            url: format!("ws://{addr}/live"),
            samples,
            session,
            metrics,
            ends,
        }
    }

    async fn connect(h: &Harness) -> Client {
        let (client, _) = connect_async(h.url.as_str()).await.unwrap();
        assert!(until(|| clients(h) == 1).await, "the stream never started");
        client
    }

    fn clients(h: &Harness) -> u64 {
        h.metrics.ws_clients.load(Ordering::Relaxed)
    }

    /// Wait up to a second for `cond`.
    async fn until(cond: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < SECOND {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cond()
    }

    fn sample(channel: &str, value: f64) -> TelemetrySample {
        TelemetrySample {
            target_id: Arc::from("target-0"),
            timestamp_ms: 1,
            channel: Arc::from(channel),
            value,
            envelope: None,
        }
    }

    /// The next frame within a second.
    async fn next_frame(client: &mut Client) -> Frame {
        timeout(SECOND, client.next())
            .await
            .expect("no frame within a second")
            .expect("the stream ended")
            .expect("a protocol error")
    }

    /// @test Samples reach the client as JSON text frames, in order.
    #[tokio::test]
    async fn samples_stream_as_json_text() {
        let h = harness(false).await;
        let mut client = connect(&h).await;
        h.samples.send(sample("Pump.flow", 1.5)).unwrap();
        h.samples.send(sample("Pump.temp", 40.0)).unwrap();
        for (channel, value) in [("Pump.flow", 1.5), ("Pump.temp", 40.0)] {
            let Frame::Text(text) = next_frame(&mut client).await else {
                panic!("expected a text frame");
            };
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["channel"], channel);
            assert_eq!(v["value"], value);
        }
    }

    /// @test When the socket's session ends the server closes it with the
    /// policy code and the reason text, reports the session end, and the
    /// target's client count drops back.
    #[tokio::test]
    async fn the_session_end_closes_with_the_policy_code() {
        let mut h = harness(true).await;
        let mut client = connect(&h).await;
        h.session.send_replace(Some(EndReason::Idle));
        let Frame::Close(Some(frame)) = next_frame(&mut client).await else {
            panic!("expected a close frame");
        };
        assert_eq!(u16::from(frame.code), close_code::POLICY);
        assert_eq!(frame.reason.as_str(), "session ended: idle limit");
        let end = timeout(SECOND, h.ends.recv()).await.unwrap();
        assert_eq!(end, Some(StreamEnd::SessionEnded(Some(EndReason::Idle))));
        assert!(until(|| clients(&h) == 0).await);
    }

    /// @test When the target's sample channel closes the stream ends and
    /// says so.
    #[tokio::test]
    async fn a_closed_sample_source_ends_the_stream() {
        let mut h = harness(false).await;
        let _client = connect(&h).await;
        // Drop the only sender of the target's channel.
        drop(std::mem::replace(&mut h.samples, broadcast::channel(1).0));
        let end = timeout(SECOND, h.ends.recv()).await.unwrap();
        assert_eq!(end, Some(StreamEnd::SourceClosed));
        assert!(until(|| clients(&h) == 0).await);
    }

    /// @test A ping is answered within a second on a target that sends
    /// nothing at all: the reply does not wait for a sample.
    #[tokio::test]
    async fn a_ping_on_a_silent_stream_is_answered_within_a_second() {
        let h = harness(false).await;
        let mut client = connect(&h).await;
        client.send(Frame::Ping("probe".into())).await.unwrap();
        assert_eq!(next_frame(&mut client).await, Frame::Pong("probe".into()));
    }

    /// @test A client that keeps the connection alive with pings, and
    /// gives up when a pong is late, keeps receiving samples well past
    /// its ping timeout: every ping is answered in time and samples
    /// still arrive at the end of the run.
    #[tokio::test]
    async fn a_pinging_client_keeps_receiving_past_its_ping_timeout() {
        const PING_EVERY: Duration = Duration::from_millis(100);
        const PING_TIMEOUT: Duration = Duration::from_millis(500);
        const RUN: Duration = Duration::from_millis(2000);
        let h = harness(false).await;
        let mut client = connect(&h).await;
        let producer = tokio::spawn({
            let tx = h.samples.clone();
            async move {
                loop {
                    let _ = tx.send(sample("Pump.flow", 1.0));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        });
        let start = Instant::now();
        let mut pings = 0;
        let mut last_sample = start;
        while start.elapsed() < RUN {
            pings += 1;
            let payload = pings.to_string();
            client
                .send(Frame::Ping(payload.clone().into()))
                .await
                .unwrap();
            let sent = Instant::now();
            loop {
                let left = PING_TIMEOUT.saturating_sub(sent.elapsed());
                match timeout(left, client.next()).await {
                    Ok(Some(Ok(Frame::Pong(p)))) if p == payload.as_bytes() => break,
                    Ok(Some(Ok(Frame::Text(_)))) => last_sample = Instant::now(),
                    Ok(other) => panic!("unexpected {other:?}"),
                    Err(_) => panic!(
                        "ping {pings} unanswered within {PING_TIMEOUT:?}, {:?} into the run",
                        start.elapsed()
                    ),
                }
            }
            tokio::time::sleep(PING_EVERY).await;
        }
        producer.abort();
        assert!(pings >= 5, "only {pings} pings in the run");
        assert!(
            last_sample.elapsed() < PING_TIMEOUT,
            "no sample in the last {PING_TIMEOUT:?}"
        );
    }

    /// @test A client that closes its socket ends the stream at once: the
    /// stream reports the client gone (so no session-end close is ever
    /// audited for it), the client count drops, and the session's end
    /// signal is released, so a later session end reaches nothing.
    #[tokio::test]
    async fn a_client_close_ends_the_stream_and_releases_its_session() {
        let mut h = harness(true).await;
        let mut client = connect(&h).await;
        assert_eq!(h.session.receiver_count(), 1);
        client.close(None).await.unwrap();
        let end = timeout(SECOND, h.ends.recv())
            .await
            .expect("the stream outlived its client's close");
        assert_eq!(end, Some(StreamEnd::ClientGone));
        assert!(until(|| clients(&h) == 0).await);
        assert_eq!(h.session.receiver_count(), 0);
        h.session.send_replace(Some(EndReason::Idle));
        assert!(h.ends.try_recv().is_err());
    }

    /// @test A connection that drops without a close frame ends the
    /// stream at once, though the target sends nothing to fail on.
    #[tokio::test]
    async fn a_dropped_connection_ends_the_stream() {
        let mut h = harness(false).await;
        let client = connect(&h).await;
        drop(client);
        let end = timeout(SECOND, h.ends.recv())
            .await
            .expect("the stream outlived its dropped connection");
        assert_eq!(end, Some(StreamEnd::ClientGone));
        assert!(until(|| clients(&h) == 0).await);
    }

    /// @test Text and binary frames from the client are ignored: the
    /// stream stays open and samples keep coming.
    #[tokio::test]
    async fn frames_from_the_client_are_ignored() {
        let mut h = harness(false).await;
        let mut client = connect(&h).await;
        client.send(Frame::Text("hello".into())).await.unwrap();
        client
            .send(Frame::Binary(vec![1u8, 2, 3].into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.samples.send(sample("Pump.flow", 2.0)).unwrap();
        assert!(matches!(next_frame(&mut client).await, Frame::Text(_)));
        assert!(h.ends.try_recv().is_err(), "the stream ended");
        assert_eq!(clients(&h), 1);
    }

    /// @test A frame whose header declares more than the cap ends the
    /// stream at once, from the header alone: no payload follows it, so
    /// a server that waited to read one would never get there.
    #[tokio::test]
    async fn a_frame_over_the_cap_is_refused_from_its_header() {
        let mut h = harness(false).await;
        let mut client = connect(&h).await;
        let declared = u16::try_from(MAX_CLIENT_MESSAGE_BYTES + 1).unwrap();
        // FIN + binary; masked, 16-bit length; the masking key; nothing more.
        let mut header = vec![0x82, 0x80 | 126];
        header.extend_from_slice(&declared.to_be_bytes());
        header.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        let MaybeTlsStream::Plain(tcp) = client.get_mut() else {
            panic!("a plain loopback stream");
        };
        tcp.write_all(&header).await.unwrap();
        let end = timeout(SECOND, h.ends.recv())
            .await
            .expect("the stream waited for the oversized payload");
        assert_eq!(end, Some(StreamEnd::ClientGone));
        assert!(until(|| clients(&h) == 0).await);
    }

    /// @test A message of frames each under the cap, together over it,
    /// ends the stream.
    #[tokio::test]
    async fn a_message_over_the_cap_ends_the_stream() {
        let mut h = harness(false).await;
        let mut client = connect(&h).await;
        let part = vec![0u8; MAX_CLIENT_MESSAGE_BYTES / 2 + 1];
        let first = RawFrame::message(part.clone(), OpCode::Data(Data::Binary), false);
        let last = RawFrame::message(part, OpCode::Data(Data::Continue), true);
        client.send(Frame::Frame(first)).await.unwrap();
        client.send(Frame::Frame(last)).await.unwrap();
        let end = timeout(SECOND, h.ends.recv())
            .await
            .expect("the oversized message was accepted");
        assert_eq!(end, Some(StreamEnd::ClientGone));
        assert!(until(|| clients(&h) == 0).await);
    }

    /// @test A message of exactly the cap is accepted and ignored: the
    /// stream stays open and samples keep coming.
    #[tokio::test]
    async fn a_message_at_the_cap_is_ignored() {
        let mut h = harness(false).await;
        let mut client = connect(&h).await;
        client
            .send(Frame::Binary(vec![0u8; MAX_CLIENT_MESSAGE_BYTES].into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.samples.send(sample("Pump.flow", 3.0)).unwrap();
        assert!(matches!(next_frame(&mut client).await, Frame::Text(_)));
        assert!(h.ends.try_recv().is_err(), "the stream ended");
        assert_eq!(clients(&h), 1);
    }

    /// Fill the connection while the client reads nothing, until the
    /// stream is stuck in a send: its backlog in the sample channel stops
    /// draining. Large samples only make the buffers fill sooner.
    async fn stall(h: &Harness) {
        let large = sample(&"x".repeat(128 * 1024), 1.0);
        for _ in 0..200 {
            h.samples.send(large.clone()).unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let backlog = h.samples.len();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            backlog > 0 && h.samples.len() == backlog,
            "the stream kept up, so no send blocked: backlog {backlog}, then {}",
            h.samples.len()
        );
    }

    /// @test A session end ends a stream stuck in a send to a client that
    /// has stopped reading: within two seconds, with the session's reason
    /// for the audit row, and the client count drops.
    #[tokio::test]
    async fn a_session_end_interrupts_a_blocked_send() {
        let mut h = harness(true).await;
        let client = connect(&h).await;
        stall(&h).await;
        h.session.send(Some(EndReason::SignedOut)).unwrap();
        let end = timeout(2 * SECOND, h.ends.recv())
            .await
            .expect("the session end waited for the blocked send");
        assert_eq!(
            end,
            Some(StreamEnd::SessionEnded(Some(EndReason::SignedOut)))
        );
        assert!(until(|| clients(&h) == 0).await);
        drop(client);
    }

    /// @test A client that takes nothing for the send timeout is gone: a
    /// stream stuck in a send to it ends as the client gone, no sooner
    /// than the timeout and without any session end, and the client
    /// count drops.
    #[tokio::test]
    async fn a_client_that_stops_reading_is_dropped() {
        let mut h = harness(false).await;
        let client = connect(&h).await;
        let started = Instant::now();
        stall(&h).await;
        let end = timeout(SEND_TIMEOUT + 2 * SECOND, h.ends.recv())
            .await
            .expect("the stream waited on a client that takes nothing");
        assert_eq!(end, Some(StreamEnd::ClientGone));
        assert!(started.elapsed() >= SEND_TIMEOUT);
        assert!(until(|| clients(&h) == 0).await);
        drop(client);
    }
}
