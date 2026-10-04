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
//! The HTTP layer does the upgrade and writes the audit record; this
//! module owns the loop, so the test suite can drive it over a real
//! connection.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket};
use tokio::sync::{broadcast, watch};

use crate::core::metrics::TargetMetrics;
use crate::core::session::EndReason;
use crate::core::telemetry::TelemetrySample;

/* ----------------------------- Constants ----------------------------- */

/// At most one lag warning per socket in this interval: a slow client
/// would otherwise flood the log at ingest rates.
const LAG_WARN_EVERY: Duration = Duration::from_secs(30);

/* ----------------------------- Types ----------------------------- */

/// Why a telemetry socket ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// The client closed the socket, the connection broke, or a send to
    /// it failed.
    ClientGone,
    /// The target's sample channel closed (the target was removed).
    SourceClosed,
    /// The socket's session ended; the client was sent a policy close.
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

/* ----------------------------- API ----------------------------- */

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
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break StreamEnd::ClientGone;
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
                // Close from the server side with the policy code, so the
                // page knows this is not a dropped link. Only this socket
                // is affected -- the target's link, router and recorder
                // never see it.
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code: close_code::POLICY,
                        reason: close_text(reason).into(),
                    })))
                    .await;
                break StreamEnd::SessionEnded(reason);
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

    use axum::{extract::ws::WebSocketUpgrade, routing::get, Router};
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
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
                    ws.on_upgrade(move |socket| async move {
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
}
