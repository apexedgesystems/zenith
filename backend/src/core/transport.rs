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

/// Wire protocol a target speaks, from per-target config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// APROTO packets over SLIP framing on TCP (the apex family).
    AprotoSlip,
}

impl Protocol {
    /// Parse the config string. Unknown values are a boot error --
    /// a misspelled protocol must never silently become the default.
    pub fn from_config(s: &str) -> Result<Self, String> {
        match s {
            "aproto-slip" => Ok(Protocol::AprotoSlip),
            other => Err(format!(
                "unknown protocol '{}' (supported: aproto-slip)",
                other
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Protocol::AprotoSlip => "aproto-slip",
        }
    }
}

/// One target's link, dispatched by protocol.
pub enum ProtocolLink {
    Aproto(AprotoClient),
}

impl ProtocolLink {
    pub fn protocol(&self) -> Protocol {
        match self {
            ProtocolLink::Aproto(_) => Protocol::AprotoSlip,
        }
    }

    /* ------------------- generic surface (every protocol) ------------------- */

    pub async fn connect(&mut self, host: &str, port: u16) -> Result<(), ClientError> {
        match self {
            ProtocolLink::Aproto(c) => c.connect(host, port).await,
        }
    }

    pub fn disconnect(&mut self) {
        match self {
            ProtocolLink::Aproto(c) => c.disconnect(),
        }
    }

    pub fn is_connected(&self) -> bool {
        match self {
            ProtocolLink::Aproto(c) => c.is_connected(),
        }
    }

    /// Lock-free handle to the connection flag (status endpoints must
    /// not wait on the link mutex).
    pub fn connected_handle(&self) -> Arc<AtomicBool> {
        match self {
            ProtocolLink::Aproto(c) => c.connected_handle(),
        }
    }

    pub fn set_metrics(&mut self, metrics: Arc<crate::core::metrics::TargetMetrics>) {
        match self {
            ProtocolLink::Aproto(c) => c.set_metrics(metrics),
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
