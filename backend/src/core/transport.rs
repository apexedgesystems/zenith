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

use crate::core::aproto_client::{AprotoClient, ClientError};

/* ----------------------------- Push Telemetry ----------------------------- */

/// A raw telemetry packet from any transport: the component fullUid
/// plus raw payload bytes. Protocol-neutral by design -- every
/// transport's reader produces these, and the decoder consumes them
/// knowing nothing about the wire that carried them.
///
/// The original opcode/APID is intentionally NOT stored: zenith
/// routes purely on (fullUid, payload size), so wire addressing is
/// opaque past the transport.
#[derive(Debug, Clone)]
pub struct PushTelemetryPacket {
    pub full_uid: u32,
    pub payload: Vec<u8>,
}

/// Wire protocol a target speaks, from per-target config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// APROTO packets over SLIP framing on TCP (the apex family).
    AprotoSlip,
    /// CCSDS Space Packets over TCP (telemetry-only).
    CcsdsSpp,
}

impl Protocol {
    /// Parse the config string. Unknown values are a boot error --
    /// a misspelled protocol must never silently become the default.
    pub fn from_config(s: &str) -> Result<Self, String> {
        match s {
            "aproto-slip" => Ok(Protocol::AprotoSlip),
            "ccsds-spp" => Ok(Protocol::CcsdsSpp),
            other => Err(format!(
                "unknown protocol '{}' (supported: aproto-slip, ccsds-spp)",
                other
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Protocol::AprotoSlip => "aproto-slip",
            Protocol::CcsdsSpp => "ccsds-spp",
        }
    }
}

/// One target's link, dispatched by protocol.
pub enum ProtocolLink {
    Aproto(AprotoClient),
    CcsdsSpp(crate::core::ccsds_link::SppLink),
}

impl ProtocolLink {
    pub fn protocol(&self) -> Protocol {
        match self {
            ProtocolLink::Aproto(_) => Protocol::AprotoSlip,
            ProtocolLink::CcsdsSpp(_) => Protocol::CcsdsSpp,
        }
    }

    /* ------------------- generic surface (every protocol) ------------------- */

    pub async fn connect(&mut self, host: &str, port: u16) -> Result<(), ClientError> {
        match self {
            ProtocolLink::Aproto(c) => c.connect(host, port).await,
            ProtocolLink::CcsdsSpp(c) => c.connect(host, port).await,
        }
    }

    pub fn disconnect(&mut self) {
        match self {
            ProtocolLink::Aproto(c) => c.disconnect(),
            ProtocolLink::CcsdsSpp(c) => c.disconnect(),
        }
    }

    pub fn is_connected(&self) -> bool {
        match self {
            ProtocolLink::Aproto(c) => c.is_connected(),
            ProtocolLink::CcsdsSpp(c) => c.is_connected(),
        }
    }

    /// Lock-free handle to the connection flag (status endpoints must
    /// not wait on the link mutex).
    pub fn connected_handle(&self) -> Arc<AtomicBool> {
        match self {
            ProtocolLink::Aproto(c) => c.connected_handle(),
            ProtocolLink::CcsdsSpp(c) => c.connected_handle(),
        }
    }

    pub fn set_metrics(&mut self, metrics: Arc<crate::core::metrics::TargetMetrics>) {
        match self {
            ProtocolLink::Aproto(c) => c.set_metrics(metrics),
            ProtocolLink::CcsdsSpp(c) => c.set_metrics(metrics),
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
            ProtocolLink::CcsdsSpp(_) => None,
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

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    /// @test The protocol boundary, enforced: zenith's neutral modules
    /// (decoder/router, storage, metrics, and the SPP protocol module)
    /// must never reference the APROTO family. A PR that re-couples a
    /// generic layer to one protocol's types fails here, not in
    /// review. transport.rs itself is the sanctioned dispatch point
    /// and is deliberately absent from this list.
    #[test]
    fn neutral_modules_never_reference_aproto() {
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
        ] {
            assert!(
                !source.to_lowercase().contains("aproto"),
                "{name} references the APROTO family; generic layers must \
                 stay protocol-neutral (route through core/transport.rs)"
            );
        }
    }
}
