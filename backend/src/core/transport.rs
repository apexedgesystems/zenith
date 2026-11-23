//! The per-target transport seam.
//!
//! Zenith's generic layers (storage, decoder, router, HTTP surface)
//! speak to a target only through `ProtocolLink`: link lifecycle plus
//! a raw telemetry-packet stream. Everything else -- command opcodes,
//! file transfer, TPRM staging -- is a protocol-FAMILY operation:
//! handlers ask the link for its family surface and answer
//! "unsupported for this target's protocol" when it is absent, so a
//! telemetry-only protocol is a first-class target, not a broken one.
//!
//! Dispatch is a closed enum on purpose: adding a protocol is code
//! (a new variant + implementation) selected by per-target config,
//! and the compiler enforces every generic call site handles it.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::core::aproto_client::AprotoClient;

/// Transport-layer errors, shared by every protocol link.
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

/// A raw telemetry packet from any transport: the component fullUid
/// plus raw payload bytes. Protocol-neutral by design -- every
/// transport's reader produces these, and the decoder consumes them
/// knowing nothing about the wire that carried them.
///
/// The original opcode/APID is intentionally NOT stored: zenith
/// routes on (fullUid, payload size), so wire addressing is opaque
/// past the transport.
#[derive(Debug, Clone)]
pub struct PushTelemetryPacket {
    pub full_uid: u32,
    pub payload: Vec<u8>,
    /// The data category the payload was read as, when the sender
    /// asked for one block (a polled read). Two blocks of one
    /// component can share a size; the category tells the decoder
    /// which one this is. None for pushed telemetry, which is
    /// identified by (fullUid, payload size) alone.
    pub category: Option<crate::core::config_manager::DataCategory>,
}

/// Wire protocol a target speaks, from per-target config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// APROTO packets over SLIP framing on TCP (the apex family).
    AprotoSlip,
    /// CCSDS Space Packets over TCP, self-delimiting (telemetry-only).
    CcsdsSpp,
    /// SLIP-framed CCSDS Space Packets -- the layered stack
    /// (telemetry-only).
    SlipCcsdsSpp,
    /// CCSDS TM Transfer Frames carrying Space Packets -- the
    /// space-data-link stack (telemetry-only).
    TmCcsdsSpp,
    /// The TM stack with a record stage on top: packets on the
    /// generated table's record APID carry concatenated
    /// variable-length records (telemetry-only).
    TmCcsdsSppRecords,
    /// Header-less SLIP frames over TCP (telemetry-only; fullUid
    /// from config -- the bare-instrument case).
    RawSlip,
}

impl Protocol {
    /// Parse the config string. Unknown values are a boot error --
    /// a misspelled protocol must never silently become the default.
    pub fn from_config(s: &str) -> Result<Self, String> {
        match s {
            "aproto-slip" => Ok(Protocol::AprotoSlip),
            "ccsds-spp" => Ok(Protocol::CcsdsSpp),
            "slip+ccsds-spp" => Ok(Protocol::SlipCcsdsSpp),
            "tm+ccsds-spp" => Ok(Protocol::TmCcsdsSpp),
            "tm+ccsds-spp+records" => Ok(Protocol::TmCcsdsSppRecords),
            "raw-slip" => Ok(Protocol::RawSlip),
            other => Err(format!(
                "unknown protocol '{}' (supported: aproto-slip, ccsds-spp, \
                 slip+ccsds-spp, tm+ccsds-spp, tm+ccsds-spp+records, \
                 raw-slip)",
                other
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Protocol::AprotoSlip => "aproto-slip",
            Protocol::CcsdsSpp => "ccsds-spp",
            Protocol::SlipCcsdsSpp => "slip+ccsds-spp",
            Protocol::TmCcsdsSpp => "tm+ccsds-spp",
            Protocol::TmCcsdsSppRecords => "tm+ccsds-spp+records",
            Protocol::RawSlip => "raw-slip",
        }
    }
}

/// A link's state from our side, as connect answers and logs name it.
/// A link that is neither is down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateWord {
    /// A live peer: a dialed stream, an accepted session, or a UDP
    /// target heard since bind.
    Connected,
    /// Bound on a listening carrier and waiting for its target.
    Listening,
}

/// One target's link, dispatched by protocol.
pub enum ProtocolLink {
    Aproto(AprotoClient),
    /// Every composed telemetry-only stream stack (SPP, SLIP+SPP,
    /// raw SLIP, future EPP compositions) shares one link type; the
    /// composition lives in its PipelineSpec.
    Stream(crate::core::stream_link::StreamLink),
}

impl ProtocolLink {
    pub fn protocol(&self) -> Protocol {
        match self {
            ProtocolLink::Aproto(_) => Protocol::AprotoSlip,
            ProtocolLink::Stream(s) => s.protocol(),
        }
    }

    /* ------------------- generic surface (every protocol) ------------------- */

    pub async fn connect(&mut self, host: &str, port: u16) -> Result<(), ClientError> {
        match self {
            ProtocolLink::Aproto(c) => c.connect(host, port).await,
            ProtocolLink::Stream(c) => c.connect(host, port).await,
        }
    }

    pub fn disconnect(&mut self) {
        match self {
            ProtocolLink::Aproto(c) => c.disconnect(),
            ProtocolLink::Stream(c) => c.disconnect(),
        }
    }

    pub fn is_connected(&self) -> bool {
        match self {
            ProtocolLink::Aproto(c) => c.is_connected(),
            ProtocolLink::Stream(c) => c.is_connected(),
        }
    }

    /// Bound on a listening carrier (accepting or receiving), waiting
    /// for its peer or serving it; `is_connected` says which. Dial-out
    /// links never listen.
    pub fn is_listening(&self) -> bool {
        match self {
            ProtocolLink::Aproto(_) => false,
            ProtocolLink::Stream(c) => c.is_listening(),
        }
    }

    /// Lock-free handle to the listening flag; permanently false for
    /// links that dial out.
    pub fn listening_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        match self {
            ProtocolLink::Aproto(_) => Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ProtocolLink::Stream(c) => c.listening_handle(),
        }
    }

    /// The link's state from our side: connected with a live peer,
    /// listening while bound and waiting for its target, None when it
    /// is neither (down).
    pub fn state_word(&self) -> Option<StateWord> {
        if self.is_connected() {
            Some(StateWord::Connected)
        } else if self.is_listening() {
            Some(StateWord::Listening)
        } else {
            None
        }
    }

    /// Lock-free handle to the connection flag (status endpoints must
    /// not wait on the link mutex).
    pub fn connected_handle(&self) -> Arc<AtomicBool> {
        match self {
            ProtocolLink::Aproto(c) => c.connected_handle(),
            ProtocolLink::Stream(c) => c.connected_handle(),
        }
    }

    pub fn set_metrics(&mut self, metrics: Arc<crate::core::metrics::TargetMetrics>) {
        match self {
            ProtocolLink::Aproto(c) => c.set_metrics(metrics),
            ProtocolLink::Stream(c) => c.set_metrics(metrics),
        }
    }

    /* ---------------- protocol-family surfaces (optional) ---------------- */

    /// The APROTO command/file/TPRM surface, when this target's
    /// protocol has one. Handlers that need it answer
    /// `Err(unsupported)` on None rather than pretending every target
    /// is an apex target.
    pub fn aproto(&mut self) -> Option<&mut AprotoClient> {
        match self {
            ProtocolLink::Aproto(c) => Some(c),
            ProtocolLink::Stream(_) => None,
        }
    }
}

/// The uniform "this target's protocol cannot do that" answer for
/// handlers, keeping the phrasing consistent across the surface.
pub fn unsupported_op(op: &str, protocol: Protocol) -> String {
    format!(
        "operation '{}' is not supported by this target's protocol ({})",
        op,
        protocol.name()
    )
}

/// What a connect call answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectAnswer {
    /// The link was already up from our side and the call did nothing:
    /// 200 with this status word.
    Already(&'static str),
    /// The call brought the link up: 200 with this status word, the
    /// link's state when connect returned.
    BroughtUp(&'static str),
    /// 502 with this message: the connect failed, or the link was
    /// already down again when it returned.
    Failed(String),
    /// 503: shutdown has begun. The call was not admitted, or the
    /// connect it made completed after shutdown began and the caller
    /// tears down what it brought up.
    Refused,
}

/// Decide a connect call's answer from the link's state word when the
/// call took the link (`before`), what the call left (`after`): the
/// connect's error, or the link's state word at the moment it returned,
/// and whether shutdown had begun by then (`stopping`). A link that was
/// up from our side is not connected again, so its `after` is its
/// unchanged state and the answer is `already_` that state. A connect
/// that completed after shutdown began is refused, whatever it left.
/// Otherwise the answer is the link's state when connect returns, true
/// when given, and 502 only if the link is already down again by then.
/// An arm step that fails after the answer takes the link down after a
/// 200; the status endpoints carry the settled state.
pub fn connect_answer(
    before: Option<StateWord>,
    after: Result<Option<StateWord>, String>,
    stopping: bool,
) -> ConnectAnswer {
    match (before, after) {
        (Some(StateWord::Connected), _) => ConnectAnswer::Already("already_connected"),
        (Some(StateWord::Listening), _) => ConnectAnswer::Already("already_listening"),
        (None, _) if stopping => ConnectAnswer::Refused,
        (None, Ok(Some(StateWord::Connected))) => ConnectAnswer::BroughtUp("connected"),
        (None, Ok(Some(StateWord::Listening))) => ConnectAnswer::BroughtUp("listening"),
        (None, Ok(None)) => {
            ConnectAnswer::Failed("Connect failed: the link went down during bring-up".to_string())
        }
        (None, Err(e)) => ConnectAnswer::Failed(format!("Connect failed: {e}")),
    }
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::stream_link::{Carrier, InitStep, PipelineSpec, StreamLink};

    /// Bound on a connect and on the init datagram: a regression fails
    /// the test instead of hanging it.
    const BOUND: std::time::Duration = std::time::Duration::from_secs(5);

    /// @test The state word says no more than the flags: nothing for a
    /// link that is down, listening while it waits, connected once its
    /// target is heard (the socket stays bound), nothing again after a
    /// disconnect, and nothing for a link whose connect succeeded and
    /// whose only init step then could not be sent.
    #[tokio::test]
    async fn state_word_says_no_more_than_the_flags() {
        let target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let host = target_addr.ip().to_string();
        let (push_tx, _) = tokio::sync::broadcast::channel(4);
        let udp = |bytes: Vec<u8>| {
            ProtocolLink::Stream(StreamLink::new(
                Protocol::CcsdsSpp,
                PipelineSpec::Spp {
                    apid_map: std::collections::HashMap::new(),
                },
                Carrier::Udp { listen_port: 0 },
                vec![InitStep {
                    name: "hello".into(),
                    delay_ms: 0,
                    bytes,
                }],
                push_tx.clone(),
            ))
        };
        let mut link = udp(b"HI".to_vec());
        assert_eq!(link.state_word(), None, "never connected");
        tokio::time::timeout(BOUND, link.connect(&host, target_addr.port()))
            .await
            .expect("connect returns")
            .unwrap();
        assert_eq!(link.state_word(), Some(StateWord::Listening));
        let mut buf = [0u8; 8];
        let (_, link_addr) = tokio::time::timeout(BOUND, target.recv_from(&mut buf))
            .await
            .expect("the init step arrives")
            .unwrap();
        target.send_to(&[1, 2, 3], link_addr).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while link.state_word() != Some(StateWord::Connected)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(link.state_word(), Some(StateWord::Connected));
        assert!(
            link.is_listening(),
            "connected wins while the socket stays bound"
        );
        link.disconnect();
        assert_eq!(link.state_word(), None, "disconnected");

        // Larger than any IPv4 datagram: the step cannot be sent.
        let mut failing = udp(vec![0; 65536]);
        tokio::time::timeout(BOUND, failing.connect(&host, target_addr.port()))
            .await
            .expect("connect returns")
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while failing.state_word().is_some() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            failing.state_word(),
            None,
            "a link that went down during bring-up claims no state"
        );
    }

    /// @test The connect answer, every branch: a link up from our side
    /// answers already_ its state whatever the rest says; otherwise the
    /// answer is the state when connect returned; a link already down
    /// again by then, and a connect that failed, answer 502 with their
    /// message.
    #[test]
    fn connect_answer_names_the_state_when_connect_returns() {
        use ConnectAnswer::{Already, BroughtUp, Failed};
        use StateWord::{Connected, Listening};
        assert_eq!(
            connect_answer(Some(Connected), Ok(Some(Connected)), false),
            Already("already_connected")
        );
        assert_eq!(
            connect_answer(Some(Listening), Ok(Some(Listening)), false),
            Already("already_listening")
        );
        assert_eq!(
            connect_answer(Some(Listening), Ok(None), false),
            Already("already_listening"),
            "a link found up is not connected again"
        );
        assert_eq!(
            connect_answer(None, Ok(Some(Connected)), false),
            BroughtUp("connected")
        );
        assert_eq!(
            connect_answer(None, Ok(Some(Listening)), false),
            BroughtUp("listening")
        );
        assert_eq!(
            connect_answer(None, Ok(None), false),
            Failed("Connect failed: the link went down during bring-up".to_string()),
            "down again when connect returned"
        );
        assert_eq!(
            connect_answer(None, Err("connection refused".to_string()), false),
            Failed("Connect failed: connection refused".to_string())
        );
    }

    /// @test A connect that completed after shutdown began is refused,
    /// whatever it left: brought up, down again, or failed. A link that
    /// was already up answers as before; nothing was done to it.
    #[test]
    fn connect_answer_refuses_a_connect_shutdown_crossed() {
        use ConnectAnswer::{Already, Refused};
        use StateWord::{Connected, Listening};
        assert_eq!(connect_answer(None, Ok(Some(Connected)), true), Refused);
        assert_eq!(connect_answer(None, Ok(Some(Listening)), true), Refused);
        assert_eq!(connect_answer(None, Ok(None), true), Refused);
        assert_eq!(
            connect_answer(None, Err("connection timed out".to_string()), true),
            Refused
        );
        assert_eq!(
            connect_answer(Some(Listening), Ok(Some(Listening)), true),
            Already("already_listening"),
            "a link found up was not touched"
        );
    }

    /// @test The protocol boundary, enforced: zenith's neutral modules
    /// (decoder/router, storage, metrics, and the SPP protocol module)
    /// must never reference the APROTO family -- nor any specific
    /// flight framework. Target specifics live at the definition
    /// level (per-target config); a PR that re-couples a generic
    /// layer to one protocol's types or one framework's names fails
    /// here, not in review. Comments count: a framework-shaped
    /// rationale in engine code is the first step toward a
    /// framework-shaped mechanism. transport.rs itself is the
    /// sanctioned dispatch point and is deliberately absent from
    /// this list.
    #[test]
    fn neutral_modules_never_reference_aproto() {
        const FORBIDDEN: &[(&str, &str)] = &[
            ("aproto", "the APROTO family"),
            ("apex", "the apex framework"),
            ("cfs", "the cFS framework"),
            ("to_lab", "a cFS app"),
            ("ci_lab", "a cFS app"),
            ("fprime", "the F-prime framework"),
            ("f-prime", "the F-prime framework"),
        ];
        for (name, source) in [
            ("core/telemetry.rs", include_str!("telemetry.rs")),
            (
                "storage/telemetry_db.rs",
                include_str!("../storage/telemetry_db.rs"),
            ),
            ("core/metrics.rs", include_str!("metrics.rs")),
            (
                "protocol/ccsds_spp.rs",
                include_str!("../protocol/ccsds_spp.rs"),
            ),
            ("protocol/slip.rs", include_str!("../protocol/slip.rs")),
            (
                "protocol/ccsds_tm.rs",
                include_str!("../protocol/ccsds_tm.rs"),
            ),
            ("core/stream_link.rs", include_str!("stream_link.rs")),
            ("core/config_manager.rs", include_str!("config_manager.rs")),
            ("core/registration.rs", include_str!("registration.rs")),
        ] {
            let lower = source.to_lowercase();
            for (token, what) in FORBIDDEN {
                assert!(
                    !lower.contains(token),
                    "{name} references {what} ('{token}'); generic layers \
                     stay protocol- and framework-neutral -- specifics \
                     belong in per-target config, dispatch in \
                     core/transport.rs"
                );
            }
        }
    }
}
