//! Zenith - Real-time operations interface
//!
//! REST API + WebSocket server for commanding and monitoring Apex CSF applications.

// The server runs on the library crate's modules: they compile once, in
// the library, and their tests run in its harness. This import is what
// `crate::config` and the other `crate::` paths below name, and this
// binary's own test harness holds only tests written in this file.
use zenith::{config, core, protocol, storage};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, Request, State,
    },
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Extension, Router,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::{FifoStrategy, ServerConfig};
use crate::core::aproto_client::AprotoClient;
use crate::core::auth::Credential;
use crate::core::config_manager::{parse_num_u32, StructDictionary};
use crate::core::metrics::TargetMetrics;
use crate::core::session::{EndReason, Session, SessionPolicy, SessionStatus, SessionStore};
use crate::core::telemetry::{self, TelemetrySample};
use crate::core::telemetry_socket::{self, StreamEnd};
use crate::core::tprm;
use crate::core::transport::{
    connect_answer, unsupported_op, ConnectAnswer, Protocol, ProtocolLink, PushTelemetryPacket,
};
use crate::storage::telemetry_db::{DbError, TelemetryDb};

/* ----------------------------- CLI ----------------------------- */

#[derive(Parser)]
#[command(name = "zenith", about = "Real-time operations interface for Apex CSF")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    #[arg(short, long)]
    port: Option<u16>,
    /// Read a password from stdin, print its argon2 PHC hash for
    /// config.toml's [auth] password_hash, and exit.
    #[arg(long)]
    hash_password: bool,
    /// Ask the server this config describes whether it serves: GET
    /// /api/health from its front door, exit 0 on HTTP 200 and 1
    /// otherwise (the container image's health check).
    #[arg(long, conflicts_with = "hash_password")]
    health_check: bool,
}

/// The `--health-check` exit code. The front door is the server's own
/// listen address (`ServerSection::listen_addr`, from the config after
/// ZENITH_PORT and from `--port`), dialled over the loopback of its
/// family when it is unspecified; connect, write and read each give up
/// after two seconds.
fn health_check(cli: &Cli) -> i32 {
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let config = match config::load(&cli.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("health check: config refused: {}", e);
            return 1;
        }
    };
    let addr = config::loopback_if_unspecified(config.server.listen_addr(cli.port));
    let limit = Duration::from_secs(2);
    let status_line = TcpStream::connect_timeout(&addr, limit).and_then(|mut stream| {
        stream.set_read_timeout(Some(limit))?;
        stream.set_write_timeout(Some(limit))?;
        stream.write_all(
            b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )?;
        let mut line = String::new();
        BufReader::new(stream.take(1024)).read_line(&mut line)?;
        Ok(line)
    });
    match status_line {
        Ok(line) if line.starts_with("HTTP/1.") && line.split(' ').nth(1) == Some("200") => 0,
        Ok(line) => {
            eprintln!("health check: {} answered {:?}", addr, line.trim_end());
            1
        }
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            eprintln!("health check: {} gave no answer within {:?}", addr, limit);
            1
        }
        Err(e) => {
            eprintln!("health check: {}: {}", addr, e);
            1
        }
    }
}

/// The password a `--hash-password` stdin line holds: the line as
/// typed, only its line ending removed, since sign-in compares the
/// password exactly as it is sent, edge spaces included. None for an
/// empty or blank line.
fn password_from_line(line: &str) -> Option<&str> {
    let password = line
        .strip_suffix('\n')
        .map_or(line, |l| l.strip_suffix('\r').unwrap_or(l));
    (!password.trim().is_empty()).then_some(password)
}

/// Authenticated principal for audit attribution, inserted by the auth
/// middleware on every request: the token's sub or the browser
/// session's user when auth is on, the anonymous "operator" when it is
/// off.
#[derive(Clone)]
struct Actor(Arc<str>);

/* ----------------------------- App State ----------------------------- */

struct TargetState {
    config: config::TargetSection,
    client: Arc<Mutex<ProtocolLink>>,
    /// Lock-free view of the client's connection flag. Status endpoints
    /// read this instead of locking `client`, which a file transfer can
    /// hold for the duration of an upload.
    connected: Arc<std::sync::atomic::AtomicBool>,
    /// Lock-free view of the link's listening flag: a listening
    /// carrier bound, waiting for its peer or serving it; `connected`
    /// says which (false for dial-out).
    listening: Arc<std::sync::atomic::AtomicBool>,
    /// Pipeline counters shared by the router, DB writer, WebSocket
    /// subscribers, and the client's command accounting.
    metrics: Arc<TargetMetrics>,
    /// Push telemetry raw packets from APROTO reader
    push_tlm_tx: broadcast::Sender<PushTelemetryPacket>,
    /// Decoded telemetry samples for WebSocket subscribers
    sample_tx: broadcast::Sender<TelemetrySample>,
    /// Per-target struct dictionaries (falls back to global)
    struct_dicts: Arc<StructDictionary>,
    /// App manifest (component registry from build artifact)
    manifest: Option<Arc<crate::core::config_manager::AppManifest>>,
    /// Command definitions (quick commands + per-component commands)
    commands_config: Option<Arc<crate::core::config_manager::CommandConfig>>,
    /// Connect-time init sequence (named steps; the link runs it,
    /// this copy names the steps in connect audit records)
    connect_init: Option<Arc<crate::core::config_manager::ConnectInit>>,
    /// Record dictionary (the decode source for record-shaped
    /// telemetry; the link's RecordSpec derives from it)
    record_table: Option<Arc<crate::core::config_manager::RecordTable>>,
    _router_handle: Option<tokio::task::JoinHandle<()>>,
    /// Timed block reads feeding the telemetry stream (config `poll`);
    /// torn down wherever the router is.
    _poll_handles: Vec<tokio::task::JoinHandle<()>>,
    /// The thread writing this target's samples to the database. It
    /// ends once every sender of `sample_tx` is gone, after writing
    /// what it still holds.
    sample_writer: Option<std::thread::JoinHandle<()>>,
}

struct SharedState {
    config: ServerConfig,
    targets: HashMap<String, TargetState>,
    db: Arc<TelemetryDb>,
    struct_dicts: Arc<StructDictionary>,
    /// Per-IP token bucket for command rate limiting. Engaged only when
    /// auth is enabled (so dev mode is unconstrained).
    rate_limiter: Arc<RateLimiter>,
    /// Live storage-pressure numbers shared with /api/telemetry/stats.
    storage_vitals: Arc<StorageVitals>,
    /// Browser sessions. The store has its own lock: callers clone the
    /// Arc under a short guard and use it after dropping the guard.
    sessions: Arc<SessionStore>,
    /// Set by the shutdown snapshot, under the write guard: from then
    /// on no connect is admitted, and a connect that completes is torn
    /// down instead of registering a router or polls.
    stopping: bool,
}

/// Storage pressure measured by the maintenance loop: the configured
/// cap and the net file growth per minute (signed -- FIFO eviction and
/// vacuum legitimately shrink the file). Handlers project time-to-cap
/// from these instead of guessing.
struct StorageVitals {
    cap_bytes: u64,
    /// Retention-ladder config, echoed to the storage panel.
    tiers: crate::config::TiersSection,
    fill_bytes_per_min: std::sync::atomic::AtomicI64,
    /// File size at the previous tick; 0 means "no tick yet", which
    /// suppresses the rate until a real delta exists.
    last_size: std::sync::atomic::AtomicU64,
    /// Cumulative rows removed by the size-cap FIFO since boot.
    fifo_evicted_samples: std::sync::atomic::AtomicU64,
    /// Cumulative rows removed by time-based retention since boot.
    retention_pruned_samples: std::sync::atomic::AtomicU64,
    /// Cumulative source rows the tier ladder has converted to
    /// envelope buckets since boot.
    tiered_source_rows: std::sync::atomic::AtomicU64,
    /// Rows per age band as of the last tick: [full-res window,
    /// mid horizon, older]. Exact once the ladder has converged;
    /// briefly approximate while a backlog is still processing.
    tier_band_rows: [std::sync::atomic::AtomicU64; 3],
}

type AppState = Arc<RwLock<SharedState>>;

/* ----------------------------- Response Types ----------------------------- */

#[derive(Serialize)]
struct TargetInfo {
    id: String,
    name: String,
    host: String,
    port: u16,
    connected: bool,
    /// A listening carrier (tcp-listen or udp) is bound: true while it
    /// waits for its target and after the target dials in or is heard;
    /// `connected` says which. False for links that dial out.
    listening: bool,
    /// Command-surface capabilities the target's dictionaries declare
    /// (e.g. "readback"). Empty for older dictionary sets.
    capabilities: Vec<String>,
    /// Dashboard display policy from this target's config: which
    /// fields read as bad, and when (normalized field key, comparison,
    /// bound).
    health: Vec<config::HealthCheck>,
    /// Wire protocol this target speaks.
    protocol: String,
}

#[derive(Serialize)]
struct CommandResponse {
    status: u8,
    status_name: String,
    /// True when the command ran deferred: a QUEUED interim preceded
    /// this terminal COMPLETION frame.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    queued: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_length: Option<usize>,
}

#[derive(Deserialize)]
struct InspectQuery {
    category: u8,
    #[serde(default)]
    offset: u16,
    #[serde(default)]
    length: u16,
}

#[derive(Deserialize)]
struct SendCommandRequest {
    /// Component fullUid: "0x" hex ("0x00D000") or decimal ("53248")
    full_uid: String,
    /// Opcode: "0x" hex ("0x0100") or decimal ("256")
    opcode: String,
    /// Payload as hex string (optional, e.g., "0102030405")
    #[serde(default)]
    payload_hex: String,
}

fn to_cmd_response(resp: crate::protocol::aproto::AckResponse) -> CommandResponse {
    CommandResponse {
        status: resp.status,
        status_name: resp.status_name,
        queued: resp.queued,
        extra_hex: if resp.extra.is_empty() {
            None
        } else {
            Some(hex::encode(&resp.extra))
        },
        extra_length: if resp.extra.is_empty() {
            None
        } else {
            Some(resp.extra.len())
        },
    }
}

/* ----------------------------- Handlers ----------------------------- */

/// Liveness + readiness in one place: DB writability and per-target
/// connection state with last-sample age. status is "degraded" only
/// when the DB probe fails -- a disconnected target is operator data,
/// not instance sickness (targets are legitimately offline in normal
/// operation). Always 200, so the image's health check (`zenith
/// --health-check`) passes while the server serves; the body carries the
/// verdict.
async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let st = state.read().await;
    let db_writable = st.db.probe_writable().is_ok();

    let mut targets = serde_json::Map::new();
    for (id, t) in &st.targets {
        let last = t
            .metrics
            .last_sample_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        targets.insert(
            id.clone(),
            serde_json::json!({
                "connected": t.connected.load(std::sync::atomic::Ordering::Acquire),
                "listening": t.listening.load(std::sync::atomic::Ordering::Acquire),
                "last_sample_age_ms":
                    if last > 0 { Some(now_ms.saturating_sub(last)) } else { None },
                "db_write_failures": t
                    .metrics
                    .db_write_failures
                    .load(std::sync::atomic::Ordering::Relaxed),
            }),
        );
    }

    Json(serde_json::json!({
        "status": if db_writable { "ok" } else { "degraded" },
        "version": env!("CARGO_PKG_VERSION"),
        "db_writable": db_writable,
        "targets": targets,
    }))
}

async fn server_version() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "zenith",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Real-time operations interface for Apex CSF",
    }))
}

/// Build the link a target's declared protocol asks for.
fn build_link(
    protocol: Protocol,
    push_tlm_tx: broadcast::Sender<PushTelemetryPacket>,
    config: &config::TargetSection,
    connect_init: Option<&crate::core::config_manager::ConnectInit>,
    record_spec: Option<crate::core::stream_link::RecordSpec>,
) -> ProtocolLink {
    match protocol {
        Protocol::AprotoSlip => ProtocolLink::Aproto(AprotoClient::new(push_tlm_tx)),
        Protocol::CcsdsSpp
        | Protocol::SlipCcsdsSpp
        | Protocol::TmCcsdsSpp
        | Protocol::TmCcsdsSppRecords
        | Protocol::RawSlip => {
            use crate::core::stream_link::{PipelineSpec, StreamLink};
            // Boot already validated the carrier (see carrier_from_config
            // at startup); a bad value on the dynamic-add path degrades
            // to TCP with a warning rather than taking the server down.
            let carrier = carrier_from_config(config, protocol).unwrap_or_else(|e| {
                tracing::warn!("[{}] {e}; using tcp carrier", config.name);
                crate::core::stream_link::Carrier::Tcp
            });
            let init = init_steps(connect_init);
            let spec = match protocol {
                Protocol::CcsdsSpp => PipelineSpec::Spp {
                    apid_map: parse_apid_map(config.apid_map.as_ref(), &config.name),
                },
                Protocol::SlipCcsdsSpp => PipelineSpec::SlipSpp {
                    apid_map: parse_apid_map(config.apid_map.as_ref(), &config.name),
                },
                Protocol::TmCcsdsSpp => PipelineSpec::TmSpp {
                    apid_map: parse_apid_map(config.apid_map.as_ref(), &config.name),
                    frame_size: config.tm_frame_size,
                },
                Protocol::TmCcsdsSppRecords => PipelineSpec::TmSppRecords {
                    frame_size: config.tm_frame_size,
                    records: record_spec.expect("boot validated records_config for this protocol"),
                },
                Protocol::RawSlip => PipelineSpec::SlipRaw {
                    uid: config.raw_uid.as_deref().and_then(parse_num_u32),
                },
                Protocol::AprotoSlip => unreachable!("guarded by the outer match"),
            };
            ProtocolLink::Stream(StreamLink::new(protocol, spec, carrier, init, push_tlm_tx))
        }
    }
}

/// The generated artifacts a target's link and decoder use, loaded
/// once. Boot and the runtime add-target path resolve them through
/// this one loader, so an added target names its channels and offers
/// its command catalog as a configured one does. Curated plot layouts
/// are not held here: they live in the database, seeded at
/// registration by `core::registration`.
struct TargetArtifacts {
    dicts: Arc<StructDictionary>,
    manifest: Option<Arc<crate::core::config_manager::AppManifest>>,
    commands_config: Option<Arc<crate::core::config_manager::CommandConfig>>,
    connect_init: Option<Arc<crate::core::config_manager::ConnectInit>>,
    record_table: Option<Arc<crate::core::config_manager::RecordTable>>,
}

/// Load a target's artifacts from wherever its definition points --
/// a target directory by convention, or explicit paths. Display
/// artifacts degrade to warnings (a missing command catalog costs the
/// catalog, not the target); the connect-init sequence is load-bearing,
/// so a broken one is returned as an error for the caller to refuse on.
fn load_target_artifacts(
    tc: &config::TargetSection,
    global_dicts: &Arc<StructDictionary>,
) -> Result<TargetArtifacts, String> {
    let name = tc.name.as_str();

    let dicts = match tc.structs_path() {
        Some(dir) => match StructDictionary::load_dir(std::path::Path::new(&dir)) {
            Ok(d) => {
                tracing::info!(
                    "Loaded per-target struct dicts for {} from {} ({} components)",
                    name,
                    dir,
                    d.components.len()
                );
                for w in d.validate() {
                    tracing::warn!("  struct dict warning [{}]: {}", name, w);
                }
                Arc::new(d)
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load struct dicts for {}: {}, using global",
                    name,
                    e
                );
                global_dicts.clone()
            }
        },
        None => global_dicts.clone(),
    };

    let manifest = tc.manifest_path().and_then(|path| {
        match crate::core::config_manager::AppManifest::load(std::path::Path::new(&path)) {
            Ok(m) => {
                tracing::info!(
                    "Loaded app manifest for {}: {} ({} components)",
                    name,
                    m.application,
                    m.components.len()
                );
                for w in m.validate() {
                    tracing::warn!("  manifest warning [{}]: {}", name, w);
                }
                Some(Arc::new(m))
            }
            Err(e) => {
                tracing::warn!("Failed to load manifest for {}: {}", name, e);
                None
            }
        }
    });

    let commands_config =
        tc.commands_path().and_then(
            |path| match crate::core::config_manager::CommandConfig::load(std::path::Path::new(
                &path,
            )) {
                Ok(cc) => {
                    tracing::info!(
                        "Loaded command config for {}: {} quick commands, {} components",
                        name,
                        cc.quick_commands.len(),
                        cc.components.len()
                    );
                    Some(Arc::new(cc))
                }
                Err(e) => {
                    tracing::warn!("Failed to load command config for {}: {}", name, e);
                    None
                }
            },
        );

    // Load-bearing: a broken init sequence means every connect comes
    // up half-armed, so the caller refuses rather than degrading.
    let connect_init = match tc.connect_init_path() {
        Some(path) => {
            let ci = crate::core::config_manager::ConnectInit::load(std::path::Path::new(&path))
                .map_err(|e| format!("connect_init: {}", e))?;
            tracing::info!(
                "Loaded connect init for {}: {} step(s): {}",
                name,
                ci.on_connect.len(),
                ci.step_names()
            );
            Some(Arc::new(ci))
        }
        None => None,
    };

    // Load-bearing as well: a record table that fails to parse would
    // leave a record-stream target decoding nothing by name.
    let record_table = match tc.records_path() {
        Some(path) => {
            let table = crate::core::config_manager::RecordTable::load(std::path::Path::new(&path))
                .map_err(|e| format!("records: {}", e))?;
            tracing::info!(
                "Loaded record dictionary for {}: {} channels",
                name,
                table.records.len()
            );
            Some(Arc::new(table))
        }
        None => None,
    };

    Ok(TargetArtifacts {
        dicts,
        manifest,
        commands_config,
        connect_init,
        record_table,
    })
}

/// Resolve a target's carrier declaration against its protocol.
/// Errors are boot refusals: a misspelled carrier or a UDP listener
/// with no port must never silently become a link that hears nothing.
fn carrier_from_config(
    config: &config::TargetSection,
    protocol: Protocol,
) -> Result<crate::core::stream_link::Carrier, String> {
    use crate::core::stream_link::Carrier;
    match config.carrier.as_str() {
        "tcp" => Ok(Carrier::Tcp),
        "udp" | "tcp-listen" => {
            if protocol == Protocol::AprotoSlip {
                return Err(format!(
                    "carrier '{}' is not supported for aproto-slip (TCP-dial only)",
                    config.carrier
                ));
            }
            match config.listen_port {
                Some(port) if config.carrier == "udp" => Ok(Carrier::Udp { listen_port: port }),
                Some(port) => Ok(Carrier::TcpListen { listen_port: port }),
                None => Err(format!(
                    "carrier '{}' requires listen_port (the local port \
                     the target sends or dials to)",
                    config.carrier
                )),
            }
        }
        other => Err(format!(
            "unknown carrier '{}' (supported: tcp, udp, tcp-listen)",
            other
        )),
    }
}

/// A loaded record table as the engine consumes it: numeric
/// addresses parsed, ids mapped to (value size, uid).
fn record_spec_from_table(
    table: &crate::core::config_manager::RecordTable,
) -> crate::core::stream_link::RecordSpec {
    crate::core::stream_link::RecordSpec {
        record_apid: parse_num_u32(&table.record_apid).unwrap_or(0) as u16,
        packet_prefix: table.packet_prefix,
        id_offset: table.id_offset,
        id_size: table.id_size,
        header_size: table.header_size,
        by_id: table.records.iter().map(|r| (r.id, r.size)).collect(),
        skip_apids: table
            .skip_apids
            .iter()
            .filter_map(|a| parse_num_u32(a).map(|v| v as u16))
            .collect(),
        skip_by_id: table
            .skip
            .iter()
            .map(|s| (s.id, s.layout.clone()))
            .collect(),
    }
}

/// The link-layer init sequence from a target's loaded
/// on_connect.json (validated at load; names travel for logs and
/// audit).
fn init_steps(
    connect_init: Option<&crate::core::config_manager::ConnectInit>,
) -> Vec<crate::core::stream_link::InitStep> {
    connect_init
        .map(|ci| {
            ci.steps()
                .into_iter()
                .map(
                    |(name, delay_ms, bytes)| crate::core::stream_link::InitStep {
                        name,
                        delay_ms,
                        bytes,
                    },
                )
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the config's APID routing table. Bad entries are skipped
/// with a warning naming them; an SPP target with an empty map boots
/// (valid: watch nothing yet) but says so.
fn parse_apid_map(
    raw: Option<&std::collections::HashMap<String, String>>,
    target_name: &str,
) -> std::collections::HashMap<u16, u32> {
    let mut out = std::collections::HashMap::new();
    for (k, v) in raw.into_iter().flatten() {
        match (parse_num_u32(k), parse_num_u32(v)) {
            (Some(apid), Some(uid)) if apid <= 0x7FF => {
                out.insert(apid as u16, uid);
            }
            _ => tracing::warn!(
                "[{}] apid_map entry '{}' = '{}' is not a valid APID/fullUid pair; skipped",
                target_name,
                k,
                v
            ),
        }
    }
    if out.is_empty() {
        tracing::warn!(
            "[{}] SPP target has no APID mappings; telemetry will be dropped until apid_map is configured",
            target_name
        );
    }
    out
}

/// A component uid in a route path: hex digits, with or without a "0x"
/// prefix. The console writes a manifest fullUid there without its
/// prefix ("00D000"); request bodies use the 0x-or-decimal convention
/// instead (`parse_num_u32`).
fn path_uid(uid: &str) -> Result<u32, (StatusCode, String)> {
    let digits = uid
        .strip_prefix("0x")
        .or_else(|| uid.strip_prefix("0X"))
        .unwrap_or(uid);
    u32::from_str_radix(digits, 16)
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("Invalid fullUid: {}", uid)))
}

/// Snapshot a target's client handle and the audit DB under a short
/// read guard. Callers do target I/O only AFTER this returns: tokio's
/// RwLock is writer-preferring, so a guard held across an APROTO round
/// trip (worst case a multi-thousand-RTT file upload) parks the next
/// writer and stalls every request behind it.
async fn target_client(
    state: &AppState,
    id: &str,
) -> Result<(Arc<Mutex<ProtocolLink>>, Arc<TelemetryDb>), (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    Ok((target.client.clone(), st.db.clone()))
}

/// Connect a target's client and attach its telemetry router when the
/// call brings the link up. The answer is `connect_answer`'s, from the
/// link's state word before the call and what the call left: the
/// link's state when connect returns, so it is true when given, and a
/// 502 only if the link is already down again by then. An arm step
/// that fails after the answer takes the link down after a 200;
/// /api/targets has the settled state. Once shutdown has begun, no
/// connect is admitted, and one that completes afterwards is torn down
/// and refused (503) instead of registering a router or polls. A
/// caller that waited for the link checks again once it holds it,
/// so it never begins bring-up after shutdown began.
///
/// The state check and the connect run under the client mutex (not
/// the state guard): a concurrent connect serializes on the mutex and
/// finds the link already up. The completion is decided under the
/// state's write guard, which the shutdown snapshot takes too, with the
/// link still held; the link is held while the guard is awaited, never
/// the reverse, so reads stay responsive.
async fn do_connect_target(
    state: &AppState,
    id: &str,
) -> Result<ConnectAnswer, (StatusCode, String)> {
    let (client, host, port) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        if st.stopping {
            return Ok(ConnectAnswer::Refused);
        }
        (
            target.client.clone(),
            target.config.host.clone(),
            target.config.port,
        )
    };

    let mut cli = client.lock().await;
    // A caller that waited here for the link, behind another connect,
    // passed the entry check before shutdown may have begun: look
    // again, under a short guard released before any bring-up.
    let stopping = state.read().await.stopping;
    if stopping {
        return Ok(ConnectAnswer::Refused);
    }
    // Up from our side (a live session, or a listener waiting for one):
    // nothing to bring up and no router to replace, and the state stays
    // what it was.
    let state_before = cli.state_word();
    if state_before.is_some() {
        return Ok(connect_answer(state_before, Ok(state_before), false));
    }
    let after = cli
        .connect(&host, port)
        .await
        .map(|()| cli.state_word())
        .map_err(|e| e.to_string());

    // Either this lands before the shutdown snapshot, which then stops
    // the router and polls attached here and disconnects the link, or it
    // sees shutdown begun and leaves nothing behind.
    let mut st = state.write().await;
    let answer = connect_answer(state_before, after, st.stopping);
    match answer {
        ConnectAnswer::Refused => {
            if cli.state_word().is_some() {
                cli.disconnect();
            }
        }
        ConnectAnswer::BroughtUp(_) => {
            // Replacing the handle aborts any previous router so a
            // reconnect can't leave two subscribers decoding the same
            // push stream.
            if let Some(target) = st.targets.get_mut(id) {
                let push_rx = target.push_tlm_tx.subscribe();
                let handle = telemetry::spawn_router(
                    id.to_string(),
                    push_rx,
                    target.sample_tx.clone(),
                    target.struct_dicts.clone(),
                    target.manifest.clone(),
                    target.record_table.clone(),
                    target.metrics.clone(),
                );
                if let Some(old) = target._router_handle.replace(handle) {
                    old.abort();
                }
                for h in target._poll_handles.drain(..) {
                    h.abort();
                }
                target._poll_handles = crate::core::poll::spawn_polls(
                    &target.config.poll,
                    target.client.clone(),
                    target.push_tlm_tx.clone(),
                );
            }
        }
        _ => {}
    }
    Ok(answer)
}

async fn list_targets(State(state): State<AppState>) -> Json<serde_json::Value> {
    let st = state.read().await;
    let mut targets: Vec<TargetInfo> = Vec::new();
    for (id, t) in &st.targets {
        // Lock-free flag read: locking the client here would make the
        // 3s frontend poll stall behind any in-flight file transfer.
        let connected = t.connected.load(std::sync::atomic::Ordering::Acquire);
        let listening = t.listening.load(std::sync::atomic::Ordering::Acquire);
        targets.push(TargetInfo {
            id: id.clone(),
            name: t.config.name.clone(),
            host: t.config.host.clone(),
            port: t.config.port,
            connected,
            listening,
            capabilities: t.struct_dicts.capabilities(),
            health: t.config.health_checks().unwrap_or_default(),
            protocol: t.config.protocol.clone(),
        });
    }
    // Sort by ID to preserve config.toml order (target-0, target-1, ...)
    targets.sort_by(|a, b| a.id.cmp(&b.id));
    Json(serde_json::json!({ "targets": targets }))
}

async fn connect_target(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (host, port, init_names, db_for_audit) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            target.config.host.clone(),
            target.config.port,
            target.connect_init.as_ref().map(|ci| ci.step_names()),
            st.db.clone(),
        )
    };
    let ip_str = addr.ip().to_string();
    // The init sequence is part of what "connect" DOES to this
    // target, so the audit record names its steps.
    let details = match &init_names {
        Some(names) => format!("{}:{} (init: {})", host, port, names),
        None => format!("{}:{}", host, port),
    };

    // A link found up was not touched, so nothing is audited for it.
    let outcome = match do_connect_target(&state, &id).await {
        Ok(ConnectAnswer::Already(status)) => {
            return Ok(Json(serde_json::json!({"status": status, "target": id})));
        }
        Ok(ConnectAnswer::BroughtUp(status)) => Ok(status),
        Ok(ConnectAnswer::Failed(err_msg)) => Err((StatusCode::BAD_GATEWAY, err_msg)),
        Ok(ConnectAnswer::Refused) => {
            Err((StatusCode::SERVICE_UNAVAILABLE, "shutting down".to_string()))
        }
        Err(e) => Err(e),
    };
    let audit_status = match &outcome {
        Ok(_) => "ok".to_string(),
        Err((_, err_msg)) => format!("err: {}", err_msg),
    };
    record_audit(
        &db_for_audit,
        &actor.0,
        "connect_target",
        Some(&id),
        Some(&details),
        &audit_status,
        Some(&ip_str),
    );
    outcome.map(|status| Json(serde_json::json!({"status": status, "target": id})))
}

async fn disconnect_target(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Snapshot and drop the write guard before touching the client
    // mutex: an in-flight upload holds that mutex, and waiting for it
    // under the write guard would stall the whole API until it ends.
    let (client, db_for_audit) = {
        let mut st = state.write().await;
        let db = st.db.clone();
        let target = st
            .targets
            .get_mut(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        if let Some(h) = target._router_handle.take() {
            h.abort();
        }
        for h in target._poll_handles.drain(..) {
            h.abort();
        }
        (target.client.clone(), db)
    };

    let mut cli = client.lock().await;
    cli.disconnect();
    drop(cli);
    record_audit(
        &db_for_audit,
        &actor.0,
        "disconnect_target",
        Some(&id),
        None,
        "ok",
        Some(&addr.ip().to_string()),
    );
    Ok(Json(
        serde_json::json!({"status": "disconnected", "target": id}),
    ))
}

async fn target_noop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CommandResponse>, (StatusCode, String)> {
    let (client, _) = target_client(&state, &id).await?;
    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((StatusCode::NOT_IMPLEMENTED, unsupported_op("noop", proto)));
    };
    let resp = cli
        .noop()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;
    Ok(Json(to_cmd_response(resp)))
}

async fn target_health(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<CommandResponse>, (StatusCode, String)> {
    let (client, _) = target_client(&state, &id).await?;
    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("health command", proto),
        ));
    };
    let resp = cli
        .get_health()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;
    Ok(Json(to_cmd_response(resp)))
}

async fn target_inspect(
    State(state): State<AppState>,
    Path((id, uid_str)): Path<(String, String)>,
    axum::extract::Query(query): axum::extract::Query<InspectQuery>,
) -> Result<Json<CommandResponse>, (StatusCode, String)> {
    let full_uid = path_uid(&uid_str)?;

    let (client, _) = target_client(&state, &id).await?;
    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("inspect", proto),
        ));
    };
    let resp = cli
        .inspect(full_uid, query.category, query.offset, query.length)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;
    Ok(Json(to_cmd_response(resp)))
}

/// Upload a file to the target filesystem via APROTO file transfer.
async fn upload_file(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<FileUploadRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (client, db_for_audit) = target_client(&state, &id).await?;
    let max_bytes = {
        let st = state.read().await;
        st.config.server.upload_max_mb as usize * 1024 * 1024
    };

    // Path policy: relative, no parent traversal -- this string lands
    // on the target's filesystem verbatim.
    if body.remote_path.starts_with('/')
        || body.remote_path.split('/').any(|seg| seg == "..")
        || body.remote_path.is_empty()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "remote_path must be relative with no parent traversal".to_string(),
        ));
    }

    // Cap BEFORE decoding: base64 length bounds the decoded size, so an
    // oversized body is rejected without materializing it.
    if body.content_base64.len() / 4 * 3 > max_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("upload exceeds {} MB cap", max_bytes / 1024 / 1024),
        ));
    }

    // Decode base64 file content
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&body.content_base64)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)))?;

    // RTS destinations get the author-time sequence check.
    if in_rts_directory(&body.remote_path) {
        let dicts = {
            state
                .read()
                .await
                .targets
                .get(&id)
                .map(|t| t.struct_dicts.clone())
        };
        if let Some(dicts) = dicts {
            if let Err(reason) = crate::core::sequence::validate_rts_upload(&dicts, &data) {
                return Err((StatusCode::UNPROCESSABLE_ENTITY, reason));
            }
        }
    }

    let detail = format!("path={} bytes={}", body.remote_path, data.len());
    let ip_str = addr.ip().to_string();

    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("file upload", proto),
        ));
    };
    let result = cli.upload_file(&body.remote_path, &data).await;
    drop(link);

    match result {
        Ok(resp) => {
            let status = if resp.status == 0 {
                "ok".to_string()
            } else {
                format!("nak: {}", resp.status_name)
            };
            record_audit(
                &db_for_audit,
                &actor.0,
                "upload_file",
                Some(&id),
                Some(&detail),
                &status,
                Some(&ip_str),
            );
            Ok(Json(serde_json::json!({
                "status": resp.status,
                "status_name": resp.status_name,
                "remote_path": body.remote_path,
                "uploaded_bytes": data.len(),
            })))
        }
        Err(e) => {
            let err_msg = format!("Upload failed: {}", e);
            record_audit(
                &db_for_audit,
                &actor.0,
                "upload_file",
                Some(&id),
                Some(&detail),
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            Err((StatusCode::BAD_GATEWAY, err_msg))
        }
    }
}

/// Whether an upload's remote path puts the file in a directory named
/// `rts`, where the vehicle loads sequence plans from (the console's
/// destination is bank_a/rts/). A directory whose name merely ends in
/// "rts" (parts/, shirts/) is not one, nor is a file named rts.
fn in_rts_directory(remote_path: &str) -> bool {
    let mut parts = remote_path.split('/');
    parts.next_back();
    parts.any(|dir| dir == "rts")
}

#[derive(Deserialize)]
struct FileUploadRequest {
    remote_path: String,
    content_base64: String,
}

#[derive(Deserialize)]
struct SwapLibraryRequest {
    component_name: String,
    #[serde(default)]
    instance_index: u32,
    #[serde(default = "default_inactive_bank")]
    inactive_bank: String,
    content_base64: String,
}

fn default_inactive_bank() -> String {
    "bank_b".to_string()
}

/// Restart the executive on a target via RELOAD_EXECUTIVE (0x0127).
///
/// Apex defers the execv() until the ACK is on the wire, so a healthy
/// restart returns a normal SUCCESS response here; the client helper
/// then disconnects because the process image is about to be replaced
/// and the socket will drop. An error from this path is therefore a
/// real failure (the ACK never arrived), not the expected close --
/// callers reconnect after a few seconds.
async fn restart_target(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (client, db_for_audit) = target_client(&state, &id).await?;
    let ip_str = addr.ip().to_string();

    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("executive restart", proto),
        ));
    };
    let result = cli.restart_executive().await;
    drop(link);

    match result {
        Ok(resp) => {
            record_audit(
                &db_for_audit,
                &actor.0,
                "restart_executive",
                Some(&id),
                Some("execve restart, connection closed as expected"),
                "ok",
                Some(&ip_str),
            );
            Ok(Json(serde_json::json!({
                "status": resp.status,
                "status_name": resp.status_name,
                "note": "executive is restarting; reconnect after ~3 seconds",
            })))
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            record_audit(
                &db_for_audit,
                &actor.0,
                "restart_executive",
                Some(&id),
                None,
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            Err((StatusCode::BAD_GATEWAY, err_msg))
        }
    }
}

/// Hot-swap a component's shared library: lock, upload .so, reload, auto-unlock.
async fn swap_library(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path((id, uid_str)): Path<(String, String)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<SwapLibraryRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (client, db_for_audit) = target_client(&state, &id).await?;

    let full_uid = path_uid(&uid_str)?;

    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&body.content_base64)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)))?;

    if data.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty .so payload".to_string()));
    }
    let max_bytes = {
        let st = state.read().await;
        st.config.server.upload_max_mb as usize * 1024 * 1024
    };
    if data.len() > max_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("library exceeds {} MB cap", max_bytes / 1024 / 1024),
        ));
    }
    // These strings become a target filesystem path.
    if body.component_name.contains('/')
        || body.component_name.contains("..")
        || body.inactive_bank.contains('/')
        || body.inactive_bank.contains("..")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "component_name and inactive_bank must be plain names".to_string(),
        ));
    }

    let detail = format!(
        "uid=0x{:06X} comp={} idx={} bank={} bytes={}",
        full_uid,
        body.component_name,
        body.instance_index,
        body.inactive_bank,
        data.len()
    );
    let ip_str = addr.ip().to_string();

    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("library swap", proto),
        ));
    };
    let result = cli
        .swap_library(
            full_uid,
            &body.component_name,
            body.instance_index,
            &body.inactive_bank,
            &data,
        )
        .await;
    drop(link);

    match result {
        Ok(resp) => {
            let status = if resp.status == 0 {
                "ok".to_string()
            } else {
                format!("nak: {}", resp.status_name)
            };
            record_audit(
                &db_for_audit,
                &actor.0,
                "swap_library",
                Some(&id),
                Some(&detail),
                &status,
                Some(&ip_str),
            );
            Ok(Json(serde_json::json!({
                "status": resp.status,
                "status_name": resp.status_name,
                "uploaded_bytes": data.len(),
            })))
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            record_audit(
                &db_for_audit,
                &actor.0,
                "swap_library",
                Some(&id),
                Some(&detail),
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            Err((StatusCode::BAD_GATEWAY, err_msg))
        }
    }
}

async fn get_commands_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;

    match &target.commands_config {
        Some(cc) => Ok(Json(serde_json::json!({
            "quickCommands": cc.quick_commands,
            "components": cc.components,
        }))),
        None => Ok(Json(serde_json::json!({
            "quickCommands": [],
            "components": {},
        }))),
    }
}

async fn send_command(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<SendCommandRequest>,
) -> Result<Json<CommandResponse>, (StatusCode, String)> {
    let (client, db_for_audit) = target_client(&state, &id).await?;

    // "0x" hex or decimal, the convention of every number in target
    // configuration: the console sends 0x strings, and an all-digit
    // string is decimal.
    let full_uid = parse_num_u32(&body.full_uid).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", body.full_uid),
        )
    })?;
    let opcode = parse_num_u32(&body.opcode)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid opcode: {}", body.opcode),
            )
        })?;

    // Parse payload hex
    let payload = if body.payload_hex.is_empty() {
        Vec::new()
    } else {
        hex::decode(&body.payload_hex)
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid payload_hex".to_string()))?
    };

    let detail = format!(
        "uid=0x{:06X} opcode=0x{:04X} payload_len={}",
        full_uid,
        opcode,
        payload.len()
    );
    let ip_str = addr.ip().to_string();

    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("raw command", proto),
        ));
    };
    let result = cli.send_command(full_uid, opcode, &payload).await;
    drop(link);

    match result {
        Ok(resp) => {
            let status = if resp.status == 0 {
                "ok".to_string()
            } else {
                format!("nak: {}", resp.status_name)
            };
            record_audit(
                &db_for_audit,
                &actor.0,
                "send_command",
                Some(&id),
                Some(&detail),
                &status,
                Some(&ip_str),
            );
            Ok(Json(to_cmd_response(resp)))
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            record_audit(
                &db_for_audit,
                &actor.0,
                "send_command",
                Some(&id),
                Some(&detail),
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            Err((StatusCode::BAD_GATEWAY, err_msg))
        }
    }
}

/* ----------------------------- TPRM / Decoded Params ----------------------------- */

/// List all components that have TUNABLE_PARAM structs (auto-discovered from struct dicts).
async fn list_tunable_components(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let dicts = target.struct_dicts.clone();
    drop(st);

    let mut components = Vec::new();
    for dict in dicts.components.values() {
        for (sname, sdef) in &dict.structs {
            if sdef.category == "TUNABLE_PARAM" && !sdef.fields.is_empty() {
                components.push(serde_json::json!({
                    "component": dict.component,
                    "struct_name": sname,
                    "size": sdef.size,
                    "field_count": sdef.fields.len(),
                }));
            }
        }
    }

    Ok(Json(
        serde_json::json!({ "tunable_components": components }),
    ))
}

/// Find the matching TUNABLE_PARAM struct for a component.
///
/// Strategy:
/// 1. If manifest provides a component name for this UID, find that
///    component's TUNABLE_PARAM struct (ignoring size -- dict may not
///    include alignment padding)
/// 2. Fall back to exact size match across all components
fn find_tunable_struct<'a>(
    dicts: &'a StructDictionary,
    data_len: usize,
    full_uid: u32,
    manifest: Option<&crate::core::config_manager::AppManifest>,
) -> Option<(&'a str, &'a str, &'a crate::core::config_manager::StructDef)> {
    // Exact join first: the manifest's dataFile stem names THE dict
    // for this uid. Name matching survives only for manifests
    // predating the field.
    if let Some(dict) = manifest
        .and_then(|m| m.dict_key_for(full_uid))
        .and_then(|key| dicts.components.get(&key))
    {
        let mut fallback: Option<(&str, &str, &crate::core::config_manager::StructDef)> = None;
        for (sname, sdef) in &dict.structs {
            if sdef.category == "TUNABLE_PARAM" && !sdef.fields.is_empty() {
                if sdef.size == data_len {
                    return Some((&dict.component, sname, sdef));
                }
                if fallback.is_none() {
                    fallback = Some((&dict.component, sname, sdef));
                }
            }
        }
        if fallback.is_some() {
            return fallback;
        }
    }

    // Map UID to component name from the manifest
    let component_name = manifest.and_then(|m| {
        m.components.iter().find_map(|c| {
            let uid_clean = c.full_uid.trim_start_matches("0x").trim_start_matches("0X");
            let uid = u32::from_str_radix(uid_clean, 16).ok()?;
            if uid == full_uid {
                Some(c.name.as_str())
            } else {
                None
            }
        })
    });

    // Legacy name-based match (manifests without dataFile)
    if let Some(name) = component_name {
        let bn = name.to_lowercase();
        for dict in dicts.components.values() {
            let dn = dict.component.to_lowercase();
            let dict_matches = dn == bn || dn.contains(&bn) || bn.contains(&dn);

            if dict_matches {
                // Prefer size-matched struct, fall back to any TUNABLE_PARAM
                let mut fallback: Option<(&str, &str, &crate::core::config_manager::StructDef)> =
                    None;
                for (sname, sdef) in &dict.structs {
                    if sdef.category == "TUNABLE_PARAM" && !sdef.fields.is_empty() {
                        if sdef.size == data_len {
                            return Some((&dict.component, sname, sdef)); // exact size match
                        }
                        if fallback.is_none() {
                            fallback = Some((&dict.component, sname, sdef));
                        }
                    }
                }
                if let Some(fb) = fallback {
                    return Some(fb);
                }
            }
        }
    }

    // Fallback: exact size match
    for dict in dicts.components.values() {
        for (sname, sdef) in &dict.structs {
            if sdef.category == "TUNABLE_PARAM" && sdef.size == data_len && !sdef.fields.is_empty()
            {
                return Some((&dict.component, sname, sdef));
            }
        }
    }
    None
}

async fn get_params(
    State(state): State<AppState>,
    Path((id, uid_str)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (client, dicts, manifest) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            target.client.clone(),
            target.struct_dicts.clone(),
            target.manifest.clone(),
        )
    };

    let full_uid = path_uid(&uid_str)?;

    // INSPECT TUNABLE_PARAM (category=1)
    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("tunable inspect", proto),
        ));
    };
    let resp = cli
        .inspect(full_uid, 1, 0, 0)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;

    if resp.status != 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("INSPECT failed: {}", resp.status_name),
        ));
    }

    // Find matching struct definition
    let mut decoded = serde_json::json!({
        "fullUid": format!("0x{:06X}", full_uid),
        "category": "TUNABLE_PARAM",
        "raw_hex": hex::encode(&resp.extra),
        "raw_length": resp.extra.len(),
    });

    // Try variable-length decode first (header + N entries, e.g. Scheduler)
    // Only search within the component that owns this UID to avoid cross-component false matches
    let comp_name_for_uid = manifest.as_ref().and_then(|m| {
        m.components.iter().find_map(|c| {
            let uid_clean = c.full_uid.trim_start_matches("0x").trim_start_matches("0X");
            let uid = u32::from_str_radix(uid_clean, 16).ok()?;
            if uid == full_uid {
                Some(c.name.as_str())
            } else {
                None
            }
        })
    });
    if let Some((component, hdr_name, ent_name, fields)) =
        dicts.decode_variable_length_for("TUNABLE_PARAM", &resp.extra, comp_name_for_uid)
    {
        decoded["struct_name"] = serde_json::json!(format!("{} + {}[]", hdr_name, ent_name));
        decoded["component"] = serde_json::json!(component);
        decoded["fields"] = fields;
        decoded["variable_length"] = serde_json::json!(true);
    } else if let Some((component, struct_name, _sdef)) =
        find_tunable_struct(&dicts, resp.extra.len(), full_uid, manifest.as_deref())
    {
        // Single fixed-size struct
        if let Some(fields) = dicts.decode_payload(component, struct_name, &resp.extra) {
            decoded["struct_name"] = serde_json::json!(struct_name);
            decoded["component"] = serde_json::json!(component);
            decoded["fields"] = fields;
        }
    }

    Ok(Json(decoded))
}

#[derive(Deserialize)]
struct UpdateParamsRequest {
    /// Field values as JSON object (field_name -> value) -- flat params
    fields: serde_json::Map<String, serde_json::Value>,
    /// Entry array for variable-length TPRMs (header+entries)
    #[serde(default)]
    entries: Option<Vec<serde_json::Map<String, serde_json::Value>>>,
    /// Flag for variable-length mode
    #[serde(default)]
    variable_length: bool,
    /// Original binary as hex (from the initial read, avoids hidden INSPECT)
    #[serde(default)]
    raw_hex: Option<String>,
}

async fn update_params(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path((id, uid_str)): Path<(String, String)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<UpdateParamsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (client, dicts, manifest, db_for_audit) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            target.client.clone(),
            target.struct_dicts.clone(),
            target.manifest.clone(),
            st.db.clone(),
        )
    };

    let full_uid = path_uid(&uid_str)?;

    // Decode the original binary from the frontend (from the initial page
    // load). No hidden INSPECT -- the user sees what they're modifying.
    // Strict decode: this buffer becomes bytes written to the target, so
    // malformed hex must be a 400, never silently patched with zeros.
    let original_binary: Vec<u8> = match body.raw_hex.as_deref() {
        Some(h) => hex::decode(h)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid raw_hex: {}", e)))?,
        None => Vec::new(),
    };

    let mut struct_size = 0usize;
    let mut struct_fields = Vec::new();

    // Build binary TPRM from edited fields, and the layout hash of
    // whatever shape gets built (the hash describes the layout the
    // vehicle will read, so the branch that lays the bytes out also
    // computes it).
    let (binary, payload_layout_hash) = if body.variable_length {
        // Variable-length: find header + entry struct pair
        let comp_name = manifest.as_ref().and_then(|m| {
            m.components.iter().find_map(|c| {
                let uid_clean = c.full_uid.trim_start_matches("0x").trim_start_matches("0X");
                let uid = u32::from_str_radix(uid_clean, 16).ok()?;
                if uid == full_uid {
                    Some(c.name.as_str())
                } else {
                    None
                }
            })
        });

        // Find the two TUNABLE_PARAM structs (header = smaller, entry
        // = larger). Exact dataFile join narrows to THE dict when the
        // manifest carries it; the name hint survives for older
        // manifests.
        let exact_key = manifest.as_ref().and_then(|m| m.dict_key_for(full_uid));
        let search_dicts: Vec<&crate::core::config_manager::ComponentDict> = match &exact_key {
            Some(key) => dicts.components.get(key).into_iter().collect(),
            None => dicts.components.values().collect(),
        };
        // Each block with its dictionary's byte order.
        let mut tunable_structs: Vec<(&crate::core::config_manager::StructDef, bool)> = Vec::new();
        for dict in search_dicts {
            if exact_key.is_none() {
                if let Some(hint) = comp_name {
                    let dn = dict.component.to_lowercase();
                    let hn = hint.to_lowercase();
                    if dn != hn && !dn.contains(&hn) && !hn.contains(&dn) {
                        continue;
                    }
                }
            }
            for sdef in dict.structs.values() {
                if sdef.category == "TUNABLE_PARAM" && !sdef.fields.is_empty() && sdef.size > 0 {
                    tunable_structs.push((sdef, dict.big_endian()));
                }
            }
        }
        tunable_structs.sort_by_key(|(s, _)| s.size);

        if tunable_structs.len() < 2 {
            return Err((
                StatusCode::BAD_REQUEST,
                "Cannot find header+entry struct pair".to_string(),
            ));
        }

        let (hdr_struct, hdr_big_endian) = tunable_structs[0]; // smaller = header
        let (ent_struct, ent_big_endian) = tunable_structs[1]; // larger = entry
        let entries = body
            .entries
            .as_ref()
            .ok_or((StatusCode::BAD_REQUEST, "Missing entries".to_string()))?;

        let mut rail_violations = constraint_violations(&hdr_struct.fields, &body.fields);
        if let Some(entries) = body.entries.as_ref() {
            for e in entries {
                rail_violations.extend(constraint_violations(&ent_struct.fields, e));
            }
        }
        if !rail_violations.is_empty() {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Constraint violation(s): {}", rail_violations.join("; ")),
            ));
        }

        let total_size = hdr_struct.size + entries.len() * ent_struct.size;
        let mut buf = vec![0u8; total_size];
        let mut unencodable: Vec<String> = Vec::new();

        // Encode header
        for field in &hdr_struct.fields {
            if let Some(value) = body.fields.get(&field.name) {
                if let Err(reason) = encode_field(&mut buf, field, value, hdr_big_endian) {
                    unencodable.push(format!("{}: {}", field.name, reason));
                }
            }
        }

        // Encode each entry
        for (i, entry_fields) in entries.iter().enumerate() {
            let base = hdr_struct.size + i * ent_struct.size;
            for field in &ent_struct.fields {
                if let Some(value) = entry_fields.get(&field.name) {
                    let mut adjusted_field = field.clone();
                    adjusted_field.offset = base + field.offset;
                    if let Err(reason) =
                        encode_field(&mut buf, &adjusted_field, value, ent_big_endian)
                    {
                        unencodable.push(format!("entry[{}].{}: {}", i, field.name, reason));
                    }
                }
            }
        }
        if !unencodable.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Cannot encode edited field(s): {}", unencodable.join("; ")),
            ));
        }

        // Variable-length layout hash, offset-aware form: header
        // leaves then each entry instance's leaves at ABSOLUTE
        // offsets, terminated by the total size -- entry-count
        // dependent by construction (contract-pinned by the
        // scheduler-shape vector).
        let entry_offsets: Vec<Vec<usize>> = (0..entries.len())
            .map(|i| {
                ent_struct
                    .fields
                    .iter()
                    .map(|f| hdr_struct.size + i * ent_struct.size + f.offset)
                    .collect()
            })
            .collect();
        let hash = tprm::layout_hash(
            tprm::dict_leaf_specs(&hdr_struct.fields).into_iter().chain(
                entry_offsets.iter().flat_map(|offs| {
                    tprm::dict_leaf_specs(&ent_struct.fields)
                        .into_iter()
                        .zip(offs.iter())
                        .map(|(mut leaf, &off)| {
                            leaf.offset = off;
                            leaf
                        })
                }),
            ),
            total_size,
        );

        (buf, hash)
    } else {
        // Flat: single struct
        let mut dict_hash: Option<u32> = None;
        let mut big_endian = false;
        if let Some((comp, _sname, sdef)) =
            find_tunable_struct(&dicts, original_binary.len(), full_uid, manifest.as_deref())
        {
            struct_size = sdef.size;
            big_endian = dicts.components.get(comp).is_some_and(|d| d.big_endian());
            // Expanded so nested fragments are editable leaf by leaf
            // (offsets payload-absolute; encode_field works unchanged).
            struct_fields = dicts
                .components
                .get(comp)
                .map(|d| crate::core::config_manager::expanded_fields(d, &sdef.fields, 0))
                .unwrap_or_else(|| sdef.fields.clone());
            dict_hash = sdef.layout_hash_u32();
        }

        if struct_size == 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "No matching TUNABLE_PARAM struct found for {} bytes",
                    original_binary.len()
                ),
            ));
        }

        let rail_violations = constraint_violations(&struct_fields, &body.fields);
        if !rail_violations.is_empty() {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Constraint violation(s): {}", rail_violations.join("; ")),
            ));
        }

        // Start with the original binary from the page load (preserves arrays, padding, etc.)
        // Then overwrite only the scalar fields the user edited.
        let mut buf = if original_binary.len() == struct_size {
            original_binary.clone()
        } else {
            vec![0u8; struct_size]
        };
        let mut unencodable: Vec<String> = Vec::new();
        for field in &struct_fields {
            if let Some(value) = body.fields.get(&field.name) {
                if let Err(reason) = encode_field(&mut buf, field, value, big_endian) {
                    unencodable.push(format!("{}: {}", field.name, reason));
                }
            }
        }
        if !unencodable.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Cannot encode edited field(s): {}", unencodable.join("; ")),
            ));
        }
        // Producer-stated hash when the dictionary exports one -- that
        // is the value the vehicle verifies. Otherwise recompute it from
        // the leaves, under the same canonical rule (a test holds the
        // recompute to the shipped dictionaries' exported hashes, except
        // structs that also list array elements as fields), and warn
        // that the dictionary carries none.
        let hash = dict_hash.unwrap_or_else(|| {
            tracing::warn!(
                "no layout_hash in dictionary for uid 0x{:06X}; recomputing from flattened fields",
                full_uid
            );
            tprm::layout_hash(tprm::dict_leaf_specs(&struct_fields), struct_size)
        });
        (buf, hash)
    };

    // Stamp the payload prelude: the vehicle verifies magic, version,
    // size, target uid, layout hash and body CRC before the payload
    // reaches a component, and rejects unstamped uploads. The layout
    // hash is the dictionary's exported one when it carries one, else
    // a recompute over the same dict fields that encoded the body
    // (dict declaration order is template emission order).
    let stamped = tprm::stamp_v4(full_uid, payload_layout_hash, &binary);

    // Upload and reload
    let detail = format!(
        "uid=0x{:06X} bytes={} stamped_bytes={} field_count={}",
        full_uid,
        binary.len(),
        stamped.len(),
        body.fields.len()
    );
    let ip_str = addr.ip().to_string();

    // Readback-capable vehicles get the staged flow: upload, cross-
    // check the staged bank's declared identity against what we sent,
    // run the vehicle's full verify, and only then apply. Anything
    // short of a verified payload never reaches RELOAD. Capability-off
    // targets keep the classic upload+reload path byte-for-byte.
    let readback = dicts.has_capability("readback");
    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("tprm update", proto),
        ));
    };
    let result = if readback {
        staged_update_flow(cli, full_uid, &stamped).await
    } else {
        cli.update_tprm(full_uid, &stamped)
            .await
            .map(|resp| (resp, serde_json::Value::Null))
    };
    drop(link);

    match result {
        Ok((resp, staged_report)) => {
            let status = if resp.status == 0 {
                "ok".to_string()
            } else {
                format!("nak: {}", resp.status_name)
            };
            record_audit(
                &db_for_audit,
                &actor.0,
                "update_tprm",
                Some(&id),
                Some(&if readback {
                    format!("{} flow=staged-verify", detail)
                } else {
                    detail.clone()
                }),
                &status,
                Some(&ip_str),
            );
            let mut out = serde_json::json!({
                "status": resp.status,
                "status_name": resp.status_name,
                "fullUid": format!("0x{:06X}", full_uid),
                "uploaded_bytes": binary.len(),
                "queued": resp.queued,
            });
            if !staged_report.is_null() {
                out["staged"] = staged_report;
            }
            // A RELOAD refusal carries the TprmPayloadCheck verdict in
            // the extra: decode it so the operator reads a named
            // reason, never a bare status number.
            if resp.status == tprm::LOAD_FAILED && !resp.extra.is_empty() {
                out["reload_verdict"] = serde_json::json!(tprm::payload_check_name(resp.extra[0]));
            }
            Ok(Json(out))
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            record_audit(
                &db_for_audit,
                &actor.0,
                "update_tprm",
                Some(&id),
                Some(&detail),
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            Err((StatusCode::BAD_GATEWAY, err_msg))
        }
    }
}

/// The verify-before-apply pipeline for readback-capable targets.
/// Returns the terminal RELOAD response plus a step-by-step report,
/// or the stopping step's response with the report explaining where
/// and why the pipeline stopped (active bytes untouched).
async fn staged_update_flow(
    cli: &mut AprotoClient,
    full_uid: u32,
    stamped: &[u8],
) -> Result<
    (crate::protocol::aproto::AckResponse, serde_json::Value),
    crate::core::aproto_client::ClientError,
> {
    use crate::protocol::aproto;

    // Our own prelude is the intent the vehicle must echo back. The
    // caller guarded the protocol: only the APROTO family stages TPRM.
    let (sent_prelude, _) = tprm::parse_v4(stamped).expect("stamp_v4 output always parses");

    // A stop is the stopping step's response named by its reason, and
    // never reads as success: a step the vehicle answered with SUCCESS
    // stops as LOAD_FAILED (nothing was loaded). The report carries
    // the step's details, so the extra is dropped and no step's payload
    // can read as a RELOAD verdict.
    let stop = |step: aproto::AckResponse, reason: &str| aproto::AckResponse {
        status: tprm::stop_status(step.status),
        status_name: reason.to_string(),
        extra: Vec::new(),
        ..step
    };

    // 1. Stage (no reload).
    let up = cli.stage_tprm(full_uid, stamped).await?;
    if up.status != 0 {
        return Ok((
            up,
            serde_json::json!({ "step": "stage", "outcome": "upload refused" }),
        ));
    }

    // 2. Digest cross-check: the row declaring our uid must declare
    // exactly the identity we sent (layout hash, payload CRC) under an
    // OK header verdict.
    let rb = cli
        .send_command(0x000000, tprm::OP_READBACK_TPRM, &[])
        .await?;
    let row = match tprm::parse_readback_page(&rb.extra) {
        Ok(page) => page.rows.into_iter().find(|r| r.full_uid == full_uid),
        Err(_) => None,
    };
    let staged = tprm::judge_staged_row(row.as_ref(), &sent_prelude);
    let readback_report = |outcome: &str| {
        serde_json::json!({
            "step": "readback",
            "outcome": outcome,
            "sent_layout_hash": format!("0x{:08X}", sent_prelude.layout_hash),
            "sent_payload_crc": format!("0x{:08X}", sent_prelude.payload_crc),
            "staged_row": row,
        })
    };
    match staged {
        tprm::StagedRow::Refused(verdict) => {
            let report = readback_report("vehicle refused the staged prelude");
            return Ok((stop(rb, tprm::payload_check_name(verdict)), report));
        }
        tprm::StagedRow::Differs => {
            let report = readback_report("staged bytes do not match intent");
            return Ok((stop(rb, "READBACK_MISMATCH"), report));
        }
        // An unattributed file goes on to VERIFY, which addresses the
        // staged file by uid and names a fault the digest could not;
        // it never reaches apply (below).
        tprm::StagedRow::Matches | tprm::StagedRow::Unattributed => {}
    }

    // 3. Vehicle-side verify (full ingest checks, no apply).
    let vr = cli
        .send_command(0x000000, tprm::OP_VERIFY_TPRM, &full_uid.to_le_bytes())
        .await?;
    let verdict = tprm::parse_verify_verdict(&vr.extra).ok();
    let verdict_ok = vr.status == 0 && verdict.as_ref().is_some_and(|v| v.ok);
    if !verdict_ok {
        let reason = verdict
            .as_ref()
            .map(|v| v.verdict_name.to_string())
            .unwrap_or_else(|| vr.status_name.clone());
        let report = serde_json::json!({
            "step": "verify",
            "outcome": "vehicle verify refused",
            "verdict": verdict,
        });
        return Ok((stop(vr, &reason), report));
    }
    if staged == tprm::StagedRow::Unattributed {
        let report = readback_report("no staged row declares this uid");
        return Ok((stop(rb, "READBACK_MISMATCH"), report));
    }

    // 4. Apply.
    let resp = cli.reload_tprm(full_uid).await?;
    let report = serde_json::json!({
        "step": "applied",
        "readback": "match",
        "verify": "OK",
    });
    Ok((resp, report))
}

/// Staged-bank digest: what the vehicle's inactive TPRM bank declares
/// (identity + header verdict per file), for the verify-before-apply
/// UI and for diffing staged state against intent.
async fn tprm_staged_digest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (client, _db) = target_client(&state, &id).await?;
    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("tprm readback", proto),
        ));
    };
    let resp = cli
        .send_command(0x000000, tprm::OP_READBACK_TPRM, &[])
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;
    drop(link);
    if resp.status != 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("READBACK_TPRM refused: {}", resp.status_name),
        ));
    }
    let page = tprm::parse_readback_page(&resp.extra)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("digest parse: {}", e)))?;
    Ok(Json(serde_json::json!({
        "total": page.total,
        "first": page.first,
        "rows": page.rows.iter().map(|r| serde_json::json!({
            "fullUid": format!("0x{:06X}", r.full_uid),
            "layout_hash": format!("0x{:08X}", r.layout_hash),
            "payload_crc": format!("0x{:08X}", r.payload_crc),
            "verdict": r.verdict,
            "verdict_name": r.verdict_name,
        })).collect::<Vec<_>>(),
    })))
}

/// Vehicle-side verify of one component's staged payload, no apply.
async fn tprm_verify_staged(
    State(state): State<AppState>,
    Path((id, uid_str)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let full_uid = path_uid(&uid_str)?;
    let (client, _db) = target_client(&state, &id).await?;
    let mut link = client.lock().await;
    let proto = link.protocol();
    let Some(cli) = link.aproto() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            unsupported_op("tprm verify", proto),
        ));
    };
    let resp = cli
        .send_command(0x000000, tprm::OP_VERIFY_TPRM, &full_uid.to_le_bytes())
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;
    drop(link);
    if resp.status != 0 {
        return Ok(Json(serde_json::json!({
            "fullUid": format!("0x{:06X}", full_uid),
            "ok": false,
            "status": resp.status,
            "status_name": resp.status_name,
        })));
    }
    let v = tprm::parse_verify_verdict(&resp.extra)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("verdict parse: {}", e)))?;
    Ok(Json(serde_json::json!({
        "fullUid": format!("0x{:06X}", full_uid),
        "ok": v.ok,
        "verdict": v.verdict,
        "verdict_name": v.verdict_name,
        "declared_layout_hash": format!("0x{:08X}", v.declared_layout_hash),
        "declared_payload_crc": format!("0x{:08X}", v.declared_payload_crc),
    })))
}

/// Guard a preference key triple. Scope is "global" or a live
/// "target:<id>"; kind and name are short slug identifiers. Bounded
/// so junk scopes cannot pile rows into the shared database file.
async fn validate_pref_key(
    state: &AppState,
    scope: &str,
    kind: &str,
    name: &str,
) -> Result<(), (StatusCode, String)> {
    let slug_ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    if !slug_ok(kind) || !slug_ok(name) {
        return Err((
            StatusCode::BAD_REQUEST,
            "kind and name must be short lowercase slugs".to_string(),
        ));
    }
    if scope == "global" {
        return Ok(());
    }
    if let Some(id) = scope.strip_prefix("target:") {
        let st = state.read().await;
        if st.targets.contains_key(id) {
            return Ok(());
        }
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    Err((
        StatusCode::BAD_REQUEST,
        "scope must be 'global' or 'target:<id>'".to_string(),
    ))
}

async fn list_prefs(
    State(state): State<AppState>,
    Path((scope, kind)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    validate_pref_key(&state, &scope, &kind, "x").await?;
    let db = { state.read().await.db.clone() };
    let rows = db
        .list_prefs(&scope, &kind)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    let mut prefs = serde_json::Map::new();
    for (name, value) in rows {
        prefs.insert(
            name,
            serde_json::from_str(&value).unwrap_or(serde_json::Value::Null),
        );
    }
    Ok(Json(serde_json::json!({ "prefs": prefs })))
}

async fn get_pref_value(
    State(state): State<AppState>,
    Path((scope, kind, name)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    validate_pref_key(&state, &scope, &kind, &name).await?;
    let db = { state.read().await.db.clone() };
    match db
        .get_pref(&scope, &kind, &name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?
    {
        Some(text) => Ok(Json(
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
        )),
        None => Err((StatusCode::NOT_FOUND, "no such preference".to_string())),
    }
}

async fn put_pref(
    State(state): State<AppState>,
    Path((scope, kind, name)): Path<(String, String, String)>,
    Json(value): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    validate_pref_key(&state, &scope, &kind, &name).await?;
    let text = serde_json::to_string(&value).unwrap_or_default();
    if text.len() > 32 * 1024 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "preference value exceeds 32KB".to_string(),
        ));
    }
    let db = { state.read().await.db.clone() };
    db.set_pref(&scope, &kind, &name, &text)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    Ok(Json(serde_json::json!({ "saved": true })))
}

async fn delete_pref(
    State(state): State<AppState>,
    Path((scope, kind, name)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    validate_pref_key(&state, &scope, &kind, &name).await?;
    let db = { state.read().await.db.clone() };
    let existed = db
        .delete_pref(&scope, &kind, &name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    Ok(Json(serde_json::json!({ "deleted": existed })))
}

/// Validate edited values against the dictionary's constraint rails
/// before anything is encoded or uploaded. Arrays apply their rails
/// per element. Returns "field: reason" strings for every violation.
fn constraint_violations(
    fields: &[crate::core::config_manager::FieldDef],
    edits: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    let mut violations = Vec::new();
    for field in fields {
        let (Some(value), Some(rails)) = (edits.get(&field.name), field.constraints.as_ref())
        else {
            continue;
        };
        let numbers: Vec<f64> = match value {
            serde_json::Value::Array(a) => a.iter().filter_map(|v| v.as_f64()).collect(),
            v => v.as_f64().into_iter().collect(),
        };
        for n in numbers {
            if let Err(reason) = rails.check(n) {
                violations.push(format!("{}: {}", field.name, reason));
            }
        }
    }
    violations
}

/// Encode one edited field into the payload buffer, in its dictionary's
/// byte order (`big_endian`). A field takes back what the parameter read
/// returned for it: a number, or, for a field the read returns as hex
/// (a string, an array, a shape it reads no number from, a float that
/// is not finite), that hex form, written as the bytes it holds, so an
/// unedited block is written back unchanged. A value the field cannot
/// hold is refused with the reason, never written: the wrong JSON type,
/// an integer outside the field's width (an unchecked cast would store
/// 300 in a uint8 as 44), a bool other than 0 or 1, a float beyond
/// float32's range for a float32, a string longer than its buffer, an
/// array of the wrong length, or a (type, size) given neither a value
/// nor its hex form. Callers fail the request naming the field: a value
/// skipped here would upload the OLD value under a SUCCESS response,
/// which reads as "applied" while changing nothing.
fn encode_field(
    buf: &mut [u8],
    field: &crate::core::config_manager::FieldDef,
    value: &serde_json::Value,
    big_endian: bool,
) -> Result<(), String> {
    let off = field.offset;
    let size = field.size;
    if off + size > buf.len() {
        return Err(format!("lies past the {}-byte payload", buf.len()));
    }
    let bits = size * 8;
    // A scalar's little-endian bytes, put in the dictionary's order.
    let mut put = |le: &[u8]| {
        let dst = &mut buf[off..off + le.len()];
        dst.copy_from_slice(le);
        if big_endian {
            dst.reverse();
        }
    };
    match (field.field_type.as_str(), size) {
        // Integers are checked against the field's width, then written
        // as their low `size` bytes, exact for any value in range.
        ("uint", 1 | 2 | 4 | 8) => {
            let max = u64::MAX >> (64 - bits);
            let v = match (value.as_u64(), value.as_i64()) {
                (Some(v), _) if v <= max => v,
                (Some(_), _) | (None, Some(_)) => {
                    return Err(format!("{value} is outside uint{bits}'s range 0..={max}"));
                }
                (None, None) => return Err(format!("{value} is not an integer")),
            };
            put(&v.to_le_bytes()[..size]);
        }
        ("int", 1 | 2 | 4 | 8) => {
            let (min, max) = (i64::MIN >> (64 - bits), i64::MAX >> (64 - bits));
            let v = match (value.as_i64(), value.as_u64()) {
                (Some(v), _) if (min..=max).contains(&v) => v,
                (Some(_), _) | (None, Some(_)) => {
                    return Err(format!(
                        "{value} is outside int{bits}'s range {min}..={max}"
                    ));
                }
                (None, None) => return Err(format!("{value} is not an integer")),
            };
            put(&v.to_le_bytes()[..size]);
        }
        ("bool", 1) => match value.as_u64() {
            Some(v @ (0 | 1)) => put(&[v as u8]),
            _ => return Err(format!("{value} is not a bool (0 or 1)")),
        },
        ("float", 4) => match (value.as_f64(), hex_form(value, 4)) {
            (Some(v), _) => {
                let v = v as f32;
                if !v.is_finite() {
                    return Err(format!("{value} is outside float32's range"));
                }
                put(&v.to_le_bytes());
            }
            (None, Some(bytes)) => buf[off..off + 4].copy_from_slice(&bytes),
            (None, None) => return Err(format!("{value} is not a number")),
        },
        ("float", 8) | ("double", 8) => match (value.as_f64(), hex_form(value, 8)) {
            (Some(v), _) => put(&v.to_le_bytes()),
            (None, Some(bytes)) => buf[off..off + 8].copy_from_slice(&bytes),
            (None, None) => return Err(format!("{value} is not a number")),
        },
        // Bounded string: fixed char buffer, NUL-padded. A value longer
        // than the field is refused, never truncated -- the packer on
        // the producing side errors the same way.
        // The hex form of its n bytes is 2n characters, which no text
        // that fits can be.
        ("string", n) => match value.as_str() {
            Some(s) if s.len() <= n => {
                buf[off..off + n].fill(0);
                buf[off..off + s.len()].copy_from_slice(s.as_bytes());
            }
            Some(s) => match hex_form(value, n) {
                Some(bytes) => buf[off..off + n].copy_from_slice(&bytes),
                None => return Err(format!("{} bytes do not fit {n}", s.len())),
            },
            None => return Err(format!("{value} is not a string")),
        },
        // Array: exact element count required; each element encodes by
        // element_type at its slot (strings recurse into the bounded-
        // string rule above).
        ("array", total) => {
            let count = field
                .dims
                .as_ref()
                .map(|d| d.iter().product::<usize>())
                .unwrap_or(0);
            let Some(arr) = value.as_array() else {
                return match hex_form(value, total) {
                    Some(bytes) => {
                        buf[off..off + total].copy_from_slice(&bytes);
                        Ok(())
                    }
                    None => Err(format!("{value} is not an array")),
                };
            };
            if count == 0 || total % count != 0 {
                return Err("the dictionary gives this array no element size".to_string());
            }
            if arr.len() != count {
                return Err(format!(
                    "{} elements where the field holds {count}",
                    arr.len()
                ));
            }
            let elem_size = total / count;
            let elem_type = field.element_type.as_deref().unwrap_or("uint");
            for (i, v) in arr.iter().enumerate() {
                let elem = crate::core::config_manager::FieldDef {
                    name: field.name.clone(),
                    field_type: elem_type.to_string(),
                    offset: off + i * elem_size,
                    size: elem_size,
                    value: serde_json::Value::Null,
                    element_type: None,
                    dims: None,
                    constraints: None,
                    struct_ref: None,
                    enum_ref: None,
                };
                encode_field(buf, &elem, v, big_endian).map_err(|e| format!("element {i}: {e}"))?;
            }
        }
        (ty, _) => match hex_form(value, size) {
            Some(bytes) => buf[off..off + size].copy_from_slice(&bytes),
            None => {
                return Err(format!(
                    "a {size}-byte {ty} field takes only the hex form it reads as"
                ))
            }
        },
    }
    Ok(())
}

/// The bytes `value` holds as a field's hex form: hex digits for exactly
/// `size` bytes, the form the parameter read returns a field in when it
/// reads no number from it.
fn hex_form(value: &serde_json::Value, size: usize) -> Option<Vec<u8>> {
    let bytes = hex::decode(value.as_str()?).ok()?;
    (bytes.len() == size).then_some(bytes)
}

/* ----------------------------- Telemetry History ----------------------------- */

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    channel: Option<String>,
    #[serde(default = "default_start")]
    start_ms: u64,
    #[serde(default = "default_end")]
    end_ms: u64,
    #[serde(default = "default_limit")]
    limit: usize,
}

/// Hard ceiling on rows any history/CSV request can materialize.
/// A request may ask for less, never more -- an unclamped limit was a
/// one-request memory exhaustion.
const HISTORY_LIMIT_MAX: usize = 200_000;

impl HistoryQuery {
    fn clamped_limit(&self) -> usize {
        self.limit.min(HISTORY_LIMIT_MAX)
    }
}

fn default_start() -> u64 {
    0
}
fn default_end() -> u64 {
    9_999_999_999_999
} // ~2286 AD
fn default_limit() -> usize {
    10000
}

/// Run a blocking TelemetryDb operation on tokio's blocking pool.
/// Range scans, CSV export, downsample, and trims touch disk for
/// milliseconds (and a contended writer can hold SQLite's busy
/// timeout for seconds); run inline they would stall the async
/// worker that serves every other request in the meantime.
async fn db_blocking<T, F>(db: Arc<TelemetryDb>, f: F) -> Result<T, (StatusCode, String)>
where
    T: Send + 'static,
    F: FnOnce(&TelemetryDb) -> Result<T, crate::storage::telemetry_db::DbError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&db))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("blocking task: {e}"),
            )
        })?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))
}

async fn telemetry_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st); // Release lock before DB query

    let limit = query.clamped_limit();
    let samples = db_blocking(db, move |db| {
        db.query_range(
            &id,
            query.channel.as_deref(),
            query.start_ms,
            query.end_ms,
            limit,
        )
    })
    .await?;

    Ok(Json(serde_json::json!({
        "count": samples.len(),
        "samples": samples,
    })))
}

async fn telemetry_latest(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    let samples = db
        .query_latest(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    Ok(Json(serde_json::json!({
        "channels": samples,
    })))
}

async fn telemetry_layouts(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (db, dicts, manifest, record_table) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            st.db.clone(),
            target.struct_dicts.clone(),
            target.manifest.clone(),
            target.record_table.clone(),
        )
    };

    let layouts = db
        .get_layouts(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    // Annotate each layout with channel references the current
    // dictionaries cannot produce -- after a target-config refresh,
    // saved layouts can point at renamed or removed fields, and that
    // rot should be visible in the picker instead of silently
    // rendering an empty plot.
    let uid_joins: Vec<(u32, String, Option<String>)> = manifest
        .as_ref()
        .map(|m| m.component_dict_joins())
        .unwrap_or_default();
    let uid_refs: Vec<(u32, &str, Option<&str>)> = uid_joins
        .iter()
        .map(|(u, n, k)| (*u, n.as_str(), k.as_deref()))
        .collect();
    let known: std::collections::HashSet<String> = {
        let mut d = telemetry::TelemetryDecoder::new(&dicts, &uid_refs);
        if let Some(table) = &record_table {
            d.add_record_table(table);
        }
        d.channel_names().into_iter().collect()
    };

    let annotated: Vec<serde_json::Value> = layouts
        .iter()
        .map(|l| {
            let mut v = serde_json::to_value(l).unwrap_or_default();
            let unknown: Vec<&str> = l
                .plots
                .iter()
                .flat_map(|p| p.channels.iter())
                .filter(|c| !known.contains(c.as_str()))
                .map(|c| c.as_str())
                .collect();
            v["unknown_channels"] = serde_json::json!(unknown);
            v
        })
        .collect();

    Ok(Json(serde_json::json!({ "layouts": annotated })))
}

#[derive(Deserialize)]
struct SaveLayoutRequest {
    name: String,
    #[serde(default = "default_grid")]
    grid: String,
    #[serde(default = "default_time_window")]
    time_window_s: u32,
    plots: Vec<crate::storage::telemetry_db::SavedPlot>,
}

fn default_grid() -> String {
    "1x1".to_string()
}
fn default_time_window() -> u32 {
    30
}

async fn save_telemetry_layout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SaveLayoutRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    match db.save_layout(&id, &body.name, &body.grid, body.time_window_s, &body.plots) {
        Ok(layout_id) => Ok(Json(
            serde_json::json!({ "id": layout_id, "name": body.name }),
        )),
        Err(e) => {
            // Translate name-collision-with-config to 409 so the frontend
            // can show a helpful "pick a different name" error.
            let msg = format!("{}", e);
            if msg.contains("config-sourced layout") {
                Err((StatusCode::CONFLICT, msg))
            } else {
                Err((StatusCode::INTERNAL_SERVER_ERROR, msg))
            }
        }
    }
}

async fn delete_telemetry_layout(
    State(state): State<AppState>,
    Path((id, layout_id)): Path<(String, i64)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    let deleted = db
        .delete_layout(layout_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    if deleted {
        Ok(Json(serde_json::json!({ "deleted": layout_id })))
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "Cannot delete config-sourced layout".to_string(),
        ))
    }
}

async fn export_telemetry_layouts(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    let layouts = db
        .get_layouts(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    // Convert to telemetry.json format
    let export: Vec<serde_json::Value> = layouts
        .iter()
        .map(|l| {
            serde_json::json!({
                "name": l.name,
                "grid": l.grid,
                "timeWindowS": l.time_window_s,
                "plots": l.plots.iter().map(|p| {
                    let mut plot = serde_json::json!({
                        "title": p.title,
                        "channels": p.channels,
                        "height": p.height,
                        "position": p.position,
                    });
                    if let Some(ymin) = p.y_min { plot["yMin"] = serde_json::json!(ymin); }
                    if let Some(ymax) = p.y_max { plot["yMax"] = serde_json::json!(ymax); }
                    if let Some(ref yl) = p.y_label { plot["yLabel"] = serde_json::json!(yl); }
                    if let Some(ref t) = p.thresholds { plot["thresholds"] = serde_json::json!(t); }
                    plot
                }).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "layouts": export })))
}

async fn telemetry_csv(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<HistoryQuery>,
) -> Result<
    (
        StatusCode,
        [(axum::http::header::HeaderName, String); 2],
        String,
    ),
    (StatusCode, String),
> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    // Query AND row formatting both belong on the blocking pool: at
    // the 200k-row limit the string build alone is milliseconds.
    let limit = query.clamped_limit();
    let target = id.clone();
    let csv = db_blocking(db, move |db| {
        let samples = db.query_range(
            &target,
            query.channel.as_deref(),
            query.start_ms,
            query.end_ms,
            limit,
        )?;
        // ~40 bytes/row; the clamp bounds worst case to a few MB.
        let mut csv = String::with_capacity(32 + samples.len() * 40);
        csv.push_str("timestamp_ms,channel,value\n");
        for s in &samples {
            csv.push_str(&format!("{},{},{}\n", s.timestamp_ms, s.channel, s.value));
        }
        Ok(csv)
    })
    .await?;

    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/csv".to_string()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}_telemetry.csv\"", id),
            ),
        ],
        csv,
    ))
}

async fn telemetry_stats(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let db = st.db.clone();
    let vitals = st.storage_vitals.clone();
    drop(st);

    let (main_bytes, wal_bytes) = db.file_sizes();
    let size_bytes = main_bytes + wal_bytes;
    let (count, audit_rows) = db_blocking(db, |db| Ok((db.count()?, db.audit_count()?))).await?;

    // Effective-retention picture: the cap, the measured net growth,
    // and the projection they imply. fill <= 0 (empty DB, holding at
    // the cap, or shrinking) yields no projection rather than a fake
    // one.
    let cap_bytes = vitals.cap_bytes;
    let fill_per_min = vitals
        .fill_bytes_per_min
        .load(std::sync::atomic::Ordering::Relaxed);
    let projected_secs_to_cap = if fill_per_min > 0 && size_bytes < cap_bytes {
        Some((cap_bytes - size_bytes) * 60 / fill_per_min as u64)
    } else {
        None
    };

    Ok(Json(serde_json::json!({
        "total_samples": count,
        "db_size_bytes": size_bytes,
        "db_size_mb": format!("{:.2}", size_bytes as f64 / 1_048_576.0),
        "wal_bytes": wal_bytes,
        "audit_rows": audit_rows,
        "cap_bytes": cap_bytes,
        "fill_bytes_per_min": fill_per_min,
        "projected_secs_to_cap": projected_secs_to_cap,
        "fifo_evicted_samples": vitals
            .fifo_evicted_samples
            .load(std::sync::atomic::Ordering::Relaxed),
        "retention_pruned_samples": vitals
            .retention_pruned_samples
            .load(std::sync::atomic::Ordering::Relaxed),
        "tiers": {
            "enabled": vitals.tiers.enabled,
            "full_resolution_minutes": vitals.tiers.full_resolution_minutes,
            "mid_bucket_seconds": vitals.tiers.mid_bucket_seconds,
            "mid_horizon_hours": vitals.tiers.mid_horizon_hours,
            "coarse_bucket_seconds": vitals.tiers.coarse_bucket_seconds,
            "converted_rows": vitals
                .tiered_source_rows
                .load(std::sync::atomic::Ordering::Relaxed),
            "band_rows": vitals
                .tier_band_rows
                .iter()
                .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
                .collect::<Vec<_>>(),
        },
    })))
}

async fn target_storage_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    let stats = db_blocking(db, move |db| db.target_stats(&id)).await?;
    Ok(Json(serde_json::to_value(stats).unwrap_or_default()))
}

/// Rewrite stored history: the last hour stays at full resolution,
/// everything older becomes 1-minute envelope buckets. Audited like a
/// trim, since what it converts is gone.
async fn downsample_data(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let db = st.db.clone();
    drop(st);

    let result = db_blocking(db.clone(), |db| db.downsample(3_600_000, 60_000)).await;
    let ip_str = addr.ip().to_string();
    let (detail, status) = match &result {
        Ok(r) => (
            Some(format!(
                "samples_before={} samples_after={} removed={}",
                r.samples_before, r.samples_after, r.removed
            )),
            "ok".to_string(),
        ),
        Err((_, err_msg)) => (None, format!("err: {}", err_msg)),
    };
    record_audit(
        &db,
        &actor.0,
        "downsample_telemetry",
        None,
        detail.as_deref(),
        &status,
        Some(&ip_str),
    );
    Ok(Json(serde_json::to_value(result?).unwrap_or_default()))
}

async fn delete_target_data(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    let result = {
        let db = db.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || db.delete_target(&id))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("blocking task: {e}"),
                )
            })?
    };
    let ip_str = addr.ip().to_string();
    match result {
        Ok(deleted) => {
            record_audit(
                &db,
                &actor.0,
                "delete_target_data",
                Some(&id),
                Some(&format!("deleted={}", deleted)),
                "ok",
                Some(&ip_str),
            );
            Ok(Json(
                serde_json::json!({ "deleted": deleted, "target": id }),
            ))
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            record_audit(
                &db,
                &actor.0,
                "delete_target_data",
                Some(&id),
                None,
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, err_msg))
        }
    }
}

#[derive(Deserialize)]
struct TrimRequest {
    /// Number of samples to delete (oldest first); 0 deletes none. If
    /// unset, defaults to 25% of the current sample count for this
    /// target.
    #[serde(default)]
    count: Option<usize>,
}

/// Trim the oldest N samples for a target. Used by the per-target
/// storage management UI to free space without dropping a whole target.
async fn trim_target_data(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<TrimRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    let count = match body.count {
        Some(n) => n,
        None => {
            let db = db.clone();
            let id = id.clone();
            let stats = db_blocking(db, move |db| db.target_stats(&id)).await?;
            ((stats.sample_count / 4).max(1)) as usize
        }
    };

    let ip_str = addr.ip().to_string();
    let result = {
        let db = db.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || db.delete_oldest_for_target(&id, count))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("blocking task: {e}"),
                )
            })?
    };
    match result {
        Ok(deleted) => {
            record_audit(
                &db,
                &actor.0,
                "trim_target_data",
                Some(&id),
                Some(&format!("requested={} deleted={}", count, deleted)),
                "ok",
                Some(&ip_str),
            );
            Ok(Json(serde_json::json!({
                "deleted": deleted,
                "target": id,
            })))
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            record_audit(
                &db,
                &actor.0,
                "trim_target_data",
                Some(&id),
                Some(&format!("requested={}", count)),
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, err_msg))
        }
    }
}

/* ----------------------------- Dynamic Target Management ----------------------------- */

#[derive(Deserialize)]
struct AddTargetRequest {
    name: String,
    host: String,
    #[serde(default = "default_port_9000")]
    port: u16,
    /// The target's generated artifact directory: named channels,
    /// command catalog and curated plots, found as boot finds them for
    /// a config.toml target. Add Target creates aproto-slip targets
    /// only, so a directory with a connect-init sequence is refused;
    /// stream protocols and connect-init are declared in config.toml.
    #[serde(default)]
    dir: Option<String>,
    /// Struct dictionaries alone, for a bundle that does not follow
    /// the directory convention.
    #[serde(default)]
    structs_dir: Option<String>,
}

fn default_port_9000() -> u16 {
    9000
}

async fn add_target(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<AddTargetRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if let Some(dir) = body.dir.as_deref() {
        if !std::path::Path::new(dir).is_dir() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("target directory '{}' does not exist", dir),
            ));
        }
    }

    // Build the definition first so artifact loading goes through the
    // same resolver boot uses: an aproto-slip target added here gets
    // the channels, catalog and plots one declared in config.toml gets.
    let tc = config::TargetSection {
        name: body.name.clone(),
        host: body.host.clone(),
        port: body.port,
        dir: body.dir.clone(),
        manifest: None,
        structs_dir: body.structs_dir.clone(),
        telemetry_config: None,
        commands_config: None,
        // UI-added targets speak the default protocol; config.toml is
        // where other protocols are declared.
        protocol: crate::core::transport::Protocol::AprotoSlip
            .name()
            .to_string(),
        health: crate::config::default_health_public(),
        health_nonzero_bad: None,
        apid_map: None,
        raw_uid: None,
        carrier: "tcp".to_string(),
        listen_port: None,
        udp_listen_port: None,
        tm_frame_size: 1024,
        records_config: None,
        connect_init: None,
        auto_connect: false,
        poll: Vec::new(),
    };

    // Load artifacts BEFORE taking the write guard -- these hit the
    // filesystem and must not stall every request behind a parked
    // writer.
    let global_dicts = { state.read().await.struct_dicts.clone() };
    let artifacts =
        load_target_artifacts(&tc, &global_dicts).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if artifacts.connect_init.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "connect_init is not supported on aproto-slip; declare stream protocols \
             in config.toml"
                .to_string(),
        ));
    }

    let (push_tlm_tx, _) = broadcast::channel::<PushTelemetryPacket>(4096);
    let (sample_tx, _) = broadcast::channel::<TelemetrySample>(4096);
    let telemetry_path = tc.telemetry_path();

    let mut st = state.write().await;
    let db = st.db.clone();

    // ID scheme: config-file targets are positional (target-0,
    // target-1, ...) and stable across restarts so their DB history
    // stays addressable. Dynamically added targets get a random
    // suffix: any length-derived id can collide with an id already
    // carried by another target's telemetry rows, layouts, and audit
    // history after a remove-then-add sequence.
    let id = loop {
        let candidate = format!("target-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        if !st.targets.contains_key(&candidate) {
            break candidate;
        }
    };

    let metrics = TargetMetrics::new();

    // Spawn DB writer for new target
    let sample_writer = spawn_sample_writer(
        id.clone(),
        st.db.clone(),
        sample_tx.subscribe(),
        metrics.clone(),
    );

    let mut new_client = build_link(Protocol::AprotoSlip, push_tlm_tx.clone(), &tc, None, None);
    new_client.set_metrics(metrics.clone());
    let connected = new_client.connected_handle();
    let listening = new_client.listening_handle();
    st.targets.insert(
        id.clone(),
        TargetState {
            config: tc,
            client: Arc::new(Mutex::new(new_client)),
            connected,
            listening,
            metrics,
            push_tlm_tx,
            sample_tx,
            struct_dicts: artifacts.dicts,
            manifest: artifacts.manifest,
            commands_config: artifacts.commands_config,
            connect_init: artifacts.connect_init,
            record_table: artifacts.record_table,
            _router_handle: None,
            _poll_handles: Vec::new(),
            sample_writer: Some(sample_writer),
        },
    );
    drop(st);

    // Curated plots are served from the database: register them before
    // answering, outside the write guard (file and database work), so
    // the caller's next layouts read finds them as it would for a
    // configured target.
    let layouts = crate::core::registration::register_layouts(
        &db,
        &id,
        &body.name,
        telemetry_path.as_deref(),
    );

    tracing::info!(
        "Added target: {} ({}:{}), {} layouts",
        body.name,
        body.host,
        body.port,
        layouts.map_or(0, |cfg| cfg.layouts.len())
    );
    record_audit(
        &db,
        &actor.0,
        "add_target",
        Some(&id),
        Some(&format!(
            "name={} host={}:{}",
            body.name, body.host, body.port
        )),
        "ok",
        Some(&addr.ip().to_string()),
    );

    Ok(Json(serde_json::json!({
        "id": id,
        "name": body.name,
        "host": body.host,
        "port": body.port,
    })))
}

async fn remove_target(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Remove under the write guard, then drop it before waiting on the
    // client mutex (an in-flight upload can hold that mutex for a while).
    let (target, db_for_audit) = {
        let mut st = state.write().await;
        let db = st.db.clone();
        let target = st
            .targets
            .remove(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (target, db)
    };

    let target_name = target.config.name.clone();

    // Disconnect if connected
    let mut cli = target.client.lock().await;
    cli.disconnect();
    drop(cli);

    // Abort telemetry router and any block polls
    if let Some(h) = target._router_handle {
        h.abort();
    }
    for h in target._poll_handles {
        h.abort();
    }

    tracing::info!("Removed target: {}", id);
    record_audit(
        &db_for_audit,
        &actor.0,
        "remove_target",
        Some(&id),
        Some(&format!("name={}", target_name)),
        "ok",
        Some(&addr.ip().to_string()),
    );
    Ok(Json(serde_json::json!({ "removed": id })))
}

/* ----------------------------- Configuration ----------------------------- */

async fn target_registry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (client, manifest, db) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            target.client.clone(),
            target.manifest.clone(),
            st.db.clone(),
        )
    };

    // Use app manifest (build artifact) for component registry
    let manifest_components = match manifest {
        Some(m) => m.components.clone(),
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                "No app manifest configured for this target. \
                 Set 'manifest' in config.toml to the path of app_manifest.json."
                    .to_string(),
            ));
        }
    };

    let mut link = client.lock().await;
    let connected = link.is_connected();
    // Reachability probes are an APROTO-family operation. On other
    // protocols a component is reachable when its telemetry is
    // arriving: the vehicle cannot be asked, but it can be heard.
    let mut cli = link.aproto();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    const HEARD_WITHIN_MS: u64 = 30_000;
    let heard: Vec<(String, u64)> = if connected && cli.is_none() {
        db.query_latest(&id)
            .map(|v| {
                v.into_iter()
                    .map(|s| (s.channel.to_string(), s.timestamp_ms))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut components = Vec::new();
    for comp in &manifest_components {
        let uid_clean = comp
            .full_uid
            .trim_start_matches("0x")
            .trim_start_matches("0X");
        let uid = u32::from_str_radix(uid_clean, 16).unwrap_or(0);

        // Probe reachability only if connected
        let reachable = if connected {
            match cli.as_deref_mut() {
                Some(c) => match c.send_command(uid, 0x0000, &[]).await {
                    Ok(r) => r.status == 0,
                    Err(_) => false,
                },
                None => {
                    let prefix = format!("{}.", comp.name);
                    heard.iter().any(|(ch, ts)| {
                        ch.starts_with(&prefix) && now_ms.saturating_sub(*ts) < HEARD_WITHIN_MS
                    })
                }
            }
        } else {
            false
        };

        let display_name = if let Some(idx) = comp.instance_index {
            format!("{} #{}", comp.name, idx)
        } else {
            comp.name.clone()
        };

        components.push(serde_json::json!({
            "fullUid": comp.full_uid,
            "name": display_name,
            "type": comp.comp_type,
            "reachable": reachable,
        }));
    }

    Ok(Json(serde_json::json!({ "components": components })))
}

/* ----------------------------- Struct Dictionaries ----------------------------- */

async fn get_structs(State(state): State<AppState>) -> Json<serde_json::Value> {
    let st = state.read().await;
    let dicts = &st.struct_dicts;
    let components: Vec<serde_json::Value> = dicts
        .components
        .values()
        .map(|d| {
            let structs: Vec<serde_json::Value> = d
                .structs
                .iter()
                .map(|(name, sdef)| {
                    serde_json::json!({
                        "name": name,
                        "category": sdef.category,
                        "size": sdef.size,
                        "opcode": sdef.opcode,
                        "fieldCount": sdef.fields.len(),
                    })
                })
                .collect();
            serde_json::json!({
                "component": d.component,
                "structs": structs,
            })
        })
        .collect();
    Json(serde_json::json!({ "components": components }))
}

async fn get_struct_detail(
    State(state): State<AppState>,
    Path(component): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let dict = st.struct_dicts.get_component(&component).ok_or((
        StatusCode::NOT_FOUND,
        format!("Component '{}' not found in struct dictionaries", component),
    ))?;
    Ok(Json(serde_json::to_value(dict).unwrap_or_default()))
}

/// Per-target struct dict listing. The global `/api/structs` reads from
/// `st.struct_dicts`, which is the optional fallback dict (often empty).
/// Real struct dicts live per-target so the INSPECT browser, the Dashboard
/// health card path, and anything else that wants typed decoding has to
/// hit these endpoints instead. Falls back to the global dict if the
/// per-target dict is empty.
async fn target_get_structs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let dicts = if !target.struct_dicts.components.is_empty() {
        &target.struct_dicts
    } else {
        &st.struct_dicts
    };
    let components: Vec<serde_json::Value> = dicts
        .components
        .values()
        .map(|d| {
            let structs: Vec<serde_json::Value> = d
                .structs
                .iter()
                .map(|(name, sdef)| {
                    serde_json::json!({
                        "name": name,
                        "category": sdef.category,
                        "size": sdef.size,
                        "opcode": sdef.opcode,
                        "fieldCount": sdef.fields.len(),
                    })
                })
                .collect();
            serde_json::json!({
                "component": d.component,
                "structs": structs,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "components": components })))
}

async fn target_get_struct_detail(
    State(state): State<AppState>,
    Path((id, component)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    // Try per-target first, then fall back to global
    let dict = target
        .struct_dicts
        .get_component(&component)
        .or_else(|| st.struct_dicts.get_component(&component))
        .ok_or((
            StatusCode::NOT_FOUND,
            format!(
                "Component '{}' not found in struct dictionaries for target '{}'",
                component, id
            ),
        ))?;
    Ok(Json(serde_json::to_value(dict).unwrap_or_default()))
}

/* ----------------------------- Auth ----------------------------- */

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

/// The bearer-token login. Both outcomes are audited as signing in at
/// the session door is, marked as this login; the password never is.
async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;

    if !st.config.auth.enabled {
        return Ok(Json(serde_json::json!({
            "token": "auth-disabled",
            "message": "Authentication is disabled",
        })));
    }

    // Credential check: the argon2 hash from config is the gate; the
    // signing secret is never compared against user input. Boot
    // validation guarantees password_hash is present when auth is on.
    let auth = st.config.auth.clone();
    let db = st.db.clone();
    drop(st);

    let ok = crate::core::auth::verify_credentials(
        &body.username,
        &body.password,
        &auth.username,
        &auth.password_hash,
    );
    let ip = addr.ip().to_string();
    const DETAIL: Option<&str> = Some("bearer token");

    if !ok {
        let actor = crate::core::auth::refused_sign_in_actor(&body.username, &auth.username);
        record_audit(
            &db,
            actor,
            "sign_in",
            None,
            DETAIL,
            "err: invalid credentials",
            Some(&ip),
        );
        return Err((StatusCode::UNAUTHORIZED, "Invalid credentials".to_string()));
    }
    let token =
        crate::core::auth::mint_token(&body.username, &auth.secret, chrono::Utc::now().timestamp());
    let outcome = match &token {
        Ok(_) => "ok".to_string(),
        Err(e) => format!("err: {}", e),
    };
    record_audit(
        &db,
        &body.username,
        "sign_in",
        None,
        DETAIL,
        &outcome,
        Some(&ip),
    );
    let token = token.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("JWT error: {}", e),
        )
    })?;

    Ok(Json(serde_json::json!({
        "token": token,
        "expires_in": crate::core::auth::TOKEN_TTL_SECS,
    })))
}

/// Mint a short-lived single-purpose WebSocket ticket for the caller.
/// Browsers cannot set Authorization on WebSocket upgrades, so a client
/// holding a bearer token trades it for a 30 s ticket and puts THAT on
/// the query string -- a leaked ticket is stale before a log file is
/// ever read. A signed-in browser opens the socket with its session
/// cookie instead and is refused a ticket: a ticket socket is tied to
/// no session, so it would outlive the session that minted it.
async fn ws_ticket(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
    session: Option<Extension<Session>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (enabled, secret) = {
        let st = state.read().await;
        (st.config.auth.enabled, st.config.auth.secret.clone())
    };
    if !enabled {
        return Ok(Json(serde_json::json!({ "ticket": "auth-disabled" })));
    }
    if session.is_some() {
        return Err((
            StatusCode::FORBIDDEN,
            "tickets are for bearer-token clients; a signed-in browser opens the \
             telemetry socket with its session"
                .to_string(),
        ));
    }
    let ticket =
        crate::core::auth::mint_ws_ticket(&actor.0, &secret, chrono::Utc::now().timestamp())
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("JWT error: {}", e),
                )
            })?;
    Ok(Json(serde_json::json!({
        "ticket": ticket,
        "expires_in": crate::core::auth::WS_TICKET_TTL_SECS,
    })))
}

/* ----------------------------- Browser Sessions ----------------------------- */

#[derive(Deserialize)]
struct SignInRequest {
    username: String,
    password: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The caller's browser session: user, both deadlines and the server
/// clock. 401 without a live session (bearer and ticket callers have
/// none); `{"auth_enabled": false}` when auth is off.
async fn session_status(
    State(state): State<AppState>,
    session: Option<Extension<Session>>,
) -> Result<Json<SessionStatus>, (StatusCode, String)> {
    if !state.read().await.config.auth.enabled {
        return Ok(Json(SessionStatus::disabled()));
    }
    let Some(Extension(s)) = session else {
        return Err((
            StatusCode::UNAUTHORIZED,
            "no browser session (bearer and ticket credentials carry none)".to_string(),
        ));
    };
    Ok(Json(SessionStatus::live(&s, now_ms())))
}

/// Sign in: the configured credential check, then a new session whose
/// id goes into the cookie and nowhere else. Both outcomes are
/// audited; the password never is.
async fn session_sign_in(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<SignInRequest>,
) -> Result<Response, (StatusCode, String)> {
    let (auth, sessions, db) = {
        let st = state.read().await;
        (st.config.auth.clone(), st.sessions.clone(), st.db.clone())
    };
    if !auth.enabled {
        return Ok(Json(SessionStatus::disabled()).into_response());
    }
    let ip = addr.ip().to_string();
    let cookie_secure = auth.cookie_secure;
    // argon2 verification and the row insert both block; neither runs
    // on a runtime worker.
    let created = tokio::task::spawn_blocking(move || {
        let ok = crate::core::auth::verify_credentials(
            &body.username,
            &body.password,
            &auth.username,
            &auth.password_hash,
        );
        if !ok {
            let actor = crate::core::auth::refused_sign_in_actor(&body.username, &auth.username);
            record_audit(
                &db,
                actor,
                "sign_in",
                None,
                None,
                "err: invalid credentials",
                Some(&ip),
            );
            return Ok(None);
        }
        let now = now_ms();
        let status = match sessions.create(&body.username, now) {
            Ok((id, session)) => Ok(Some((id, SessionStatus::live(&session, now)))),
            Err(e) => Err(e),
        };
        let outcome = match &status {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("err: {}", e),
        };
        record_audit(
            &db,
            &body.username,
            "sign_in",
            None,
            None,
            &outcome,
            Some(&ip),
        );
        status
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("sign-in failed: {}", e),
        )
    })?
    .map_err(|e: DbError| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("session not stored: {}", e),
        )
    })?;
    let Some((id, status)) = created else {
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials".to_string()));
    };
    Ok((
        [(
            header::SET_COOKIE,
            crate::core::session::set_cookie(&id, cookie_secure),
        )],
        Json(status),
    )
        .into_response())
}

/// The keep-alive: restarts the idle limit, never past the absolute
/// one. Only this endpoint extends a session; the UI calls it after
/// operator input, never on a timer.
async fn session_refresh(
    State(state): State<AppState>,
    session: Option<Extension<Session>>,
) -> Result<Json<SessionStatus>, (StatusCode, String)> {
    let (enabled, sessions) = {
        let st = state.read().await;
        (st.config.auth.enabled, st.sessions.clone())
    };
    if !enabled {
        return Ok(Json(SessionStatus::disabled()));
    }
    let Some(Extension(s)) = session else {
        return Err((
            StatusCode::UNAUTHORIZED,
            "no browser session to keep alive".to_string(),
        ));
    };
    let now = now_ms();
    let refreshed = tokio::task::spawn_blocking(move || sessions.refresh(&s.key, now))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("keep-alive failed: {}", e),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("keep-alive not stored: {}", e),
            )
        })?;
    match refreshed {
        Some(s) => Ok(Json(SessionStatus::live(&s, now))),
        None => Err((StatusCode::UNAUTHORIZED, "session ended".to_string())),
    }
}

/// Sign out: end the caller's session (its sockets close) and clear
/// the cookie. 204 whether or not the session was still alive, so a
/// browser holding a dead cookie can still clear it; an end that
/// cannot be recorded is a 500 and leaves the session live.
async fn session_sign_out(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    session: Option<Extension<Session>>,
) -> Result<Response, (StatusCode, String)> {
    let (enabled, cookie_secure, sessions, db) = {
        let st = state.read().await;
        (
            st.config.auth.enabled,
            st.config.auth.cookie_secure,
            st.sessions.clone(),
            st.db.clone(),
        )
    };
    if !enabled {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    if let Some(Extension(s)) = session {
        let ip = addr.ip().to_string();
        tokio::task::spawn_blocking(move || {
            if let Some(user) = sessions.end(&s.key, EndReason::SignedOut)? {
                record_audit(&db, &user, "sign_out", None, None, "ok", Some(&ip));
            }
            Ok::<(), DbError>(())
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("sign-out failed: {}", e),
            )
        })?
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("sign-out not recorded, the session is still live: {}", e),
            )
        })?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            crate::core::session::clear_cookie(cookie_secure),
        )],
    )
        .into_response())
}

/* ----------------------------- WebSocket ----------------------------- */

/// A telemetry socket opened with a browser session closes when that
/// session ends, and the close is audited under the session's user.
struct SessionTie {
    ended: watch::Receiver<Option<EndReason>>,
    user: Arc<str>,
    db: Arc<TelemetryDb>,
    source_ip: String,
}

async fn telemetry_ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    session: Option<Extension<Session>>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let (rx, metrics, sessions, db) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            target.sample_tx.subscribe(),
            target.metrics.clone(),
            st.sessions.clone(),
            st.db.clone(),
        )
    };
    // Token and ticket sockets have no session; a session socket waits
    // on its session's end. A session that ended since the middleware
    // looked it up is refused here.
    let tie = match session {
        Some(Extension(s)) => Some(SessionTie {
            ended: sessions
                .subscribe(&s.key)
                .ok_or((StatusCode::UNAUTHORIZED, "session ended".to_string()))?,
            user: s.user.clone(),
            db,
            source_ip: addr.ip().to_string(),
        }),
        None => None,
    };

    Ok(telemetry_socket::accept(ws, move |socket| {
        handle_telemetry_ws(socket, rx, id, metrics, tie)
    }))
}

/// Run the socket (core::telemetry_socket owns the loop) and audit a
/// close that its session's end caused.
async fn handle_telemetry_ws(
    socket: WebSocket,
    rx: broadcast::Receiver<TelemetrySample>,
    target_id: String,
    metrics: Arc<TargetMetrics>,
    tie: Option<SessionTie>,
) {
    let (end_signal, auditor) = match tie {
        Some(t) => (Some(t.ended), Some((t.user, t.db, t.source_ip))),
        None => (None, None),
    };
    let end = telemetry_socket::stream_samples(socket, rx, &target_id, &metrics, end_signal).await;
    if let (StreamEnd::SessionEnded(reason), Some((user, db, source_ip))) = (end, auditor) {
        let _ = tokio::task::spawn_blocking(move || {
            record_audit(
                &db,
                &user,
                "close_telemetry_socket",
                Some(&target_id),
                Some(telemetry_socket::close_text(reason)),
                "ok",
                Some(&source_ip),
            )
        })
        .await;
    }
}

/* ----------------------------- Metrics ----------------------------- */

/// Per-target pipeline counters. The numbers answer "did we lose data,
/// and at which stage": decoded vs dedup-dropped vs lag-dropped vs
/// written vs failed, plus command round-trip accounting. Every stage
/// that can drop or fail reports here; nothing in the pipeline is
/// allowed to lose data uncounted.
async fn get_metrics(State(state): State<AppState>) -> Json<serde_json::Value> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let st = state.read().await;
    let mut targets = serde_json::Map::new();
    for (id, t) in &st.targets {
        let mut snap = t.metrics.snapshot(now_ms);
        snap["connected"] =
            serde_json::json!(t.connected.load(std::sync::atomic::Ordering::Acquire));
        snap["listening"] =
            serde_json::json!(t.listening.load(std::sync::atomic::Ordering::Acquire));
        targets.insert(id.clone(), snap);
    }
    Json(serde_json::json!({ "targets": targets }))
}

/* ----------------------------- Audit Log ----------------------------- */

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default = "default_audit_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_audit_limit() -> usize {
    100
}

async fn list_audit(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let db = st.db.clone();
    drop(st);
    let limit = query.limit.min(1000);
    let offset = query.offset;
    let entries = db_blocking(db, move |db| db.query_audit(limit, offset)).await?;
    Ok(Json(serde_json::json!({ "entries": entries })))
}

/// Best-effort audit logger. Errors are logged but don't propagate -- an
/// audit failure must not block the operator action.
fn record_audit(
    db: &Arc<TelemetryDb>,
    actor: &str,
    action: &str,
    target_id: Option<&str>,
    detail: Option<&str>,
    status: &str,
    source_ip: Option<&str>,
) {
    if let Err(e) = db.log_audit(actor, action, target_id, detail, status, source_ip) {
        tracing::warn!("audit log write failed: {}", e);
    }
}

/* ----------------------------- Auth Middleware ----------------------------- */

/// Authenticate the request and record who it is for. Skips everything
/// when auth is disabled in config (the default for development).
///
/// Whitelisted paths bypass auth: `/api/auth/login` (otherwise nobody could
/// log in) and `/api/health` (used by load balancers and the image's
/// health check, `zenith --health-check`).
/// Signing in and out at `/api/auth/session` need no live credential.
async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    // Whitelist
    let path = request.uri().path();
    // The middleware is layered on the nested /api router, so the
    // prefix is already stripped from the path it sees.
    if path == "/auth/login" || path == "/health" {
        request
            .extensions_mut()
            .insert(Actor(Arc::from("operator")));
        return Ok(next.run(request).await);
    }
    let session_door =
        path == "/auth/session" && matches!(*request.method(), Method::POST | Method::DELETE);
    let socket = opens_telemetry_socket(path);

    // CRITICAL: read the config under a SHORT-lived guard and then drop
    // it before calling next.run(). Holding the guard across the await
    // would deadlock with any handler that takes a write lock (e.g.
    // connect_target), because tokio's RwLock is writer-preferring.
    let (auth_enabled, secret, sessions) = {
        let st = state.read().await;
        (
            st.config.auth.enabled,
            st.config.auth.secret.clone(),
            st.sessions.clone(),
        )
    };

    if !auth_enabled {
        request
            .extensions_mut()
            .insert(Actor(Arc::from("operator")));
        return Ok(next.run(request).await);
    }

    // The first credential present decides (core::auth::credential):
    // a bearer token from the Authorization header; else -- for
    // WebSocket upgrades from API clients -- a short-lived ticket on
    // the query string, which MUST be a ticket (ws claim, 30 s expiry,
    // minted via POST /api/auth/ws-ticket) so long-lived tokens never
    // land in request logs, and opens the telemetry socket only; else
    // the browser's session cookie, which must come from the console's
    // own origin for anything that is not a plain read. Validation
    // lives in core (lib-testable); the subject becomes the audit actor
    // for the request.
    let query = request.uri().query().map(str::to_owned);
    let user: Option<Arc<str>> = match crate::core::auth::credential(&headers, query.as_deref()) {
        Credential::Bearer(token) => Some(Arc::from(
            crate::core::auth::validate_token(token, &secret, false)
                .map_err(|e| (StatusCode::UNAUTHORIZED, e))?
                .as_str(),
        )),
        Credential::Ticket(_) if !socket => {
            return Err((
                StatusCode::UNAUTHORIZED,
                "a socket ticket opens the telemetry socket only \
                 (the request was not carried out)"
                    .to_string(),
            ));
        }
        Credential::Ticket(ticket) => Some(Arc::from(
            crate::core::auth::validate_token(ticket, &secret, true)
                .map_err(|e| (StatusCode::UNAUTHORIZED, e))?
                .as_str(),
        )),
        Credential::Cookie(id) => {
            if !crate::core::session::same_origin_allows(request.method(), &headers) {
                return Err((
                    StatusCode::FORBIDDEN,
                    "cross-origin request with a session cookie refused \
                     (the request was not carried out)"
                        .to_string(),
                ));
            }
            match sessions.lookup(id, now_ms()) {
                Some(session) => {
                    let user = session.user.clone();
                    request.extensions_mut().insert(session);
                    Some(user)
                }
                None if session_door => None,
                None => {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "session ended or unknown: sign in again \
                         (the request was not carried out)"
                            .to_string(),
                    ))
                }
            }
        }
        Credential::None => None,
    };

    match user {
        Some(user) => {
            request.extensions_mut().insert(Actor(user));
            Ok(next.run(request).await)
        }
        None if session_door => Ok(next.run(request).await),
        None => Err((
            StatusCode::UNAUTHORIZED,
            "missing token or session (the request was not carried out)".to_string(),
        )),
    }
}

/// Whether a request opens a target's telemetry socket, the one route
/// a socket ticket authenticates: `/targets/{id}/telemetry/live`, as
/// the middleware sees it under the /api prefix.
fn opens_telemetry_socket(path: &str) -> bool {
    path.strip_prefix("/targets/")
        .and_then(|rest| rest.strip_suffix("/telemetry/live"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/* ----------------------------- Rate Limiting ----------------------------- */

/// A per-IP token bucket for every POST under /api while auth is on: a
/// burst of RATE_LIMIT_BURST, refilled at RATE_LIMIT_PER_SEC. Once the
/// map holds 4096 addresses, each check first evicts the buckets idle
/// long enough to be full again, so it outgrows that only while more
/// addresses than that are active at once.
#[derive(Default)]
struct RateLimiter {
    buckets: tokio::sync::Mutex<HashMap<std::net::IpAddr, TokenBucket>>,
}

struct TokenBucket {
    tokens: f64,
    last_refill: std::time::Instant,
}

const RATE_LIMIT_PER_SEC: f64 = 10.0;
const RATE_LIMIT_BURST: f64 = 30.0;

impl RateLimiter {
    /// Try to consume one token. Returns true if allowed, false if rate-limited.
    async fn check(&self, ip: std::net::IpAddr) -> bool {
        let mut buckets = self.buckets.lock().await;
        let now = std::time::Instant::now();
        // Lazy eviction: a bucket idle long enough to be full again
        // carries no state worth keeping. Bounds the map against
        // address churn without a background task.
        if buckets.len() >= 4096 {
            buckets.retain(|_, b| {
                now.duration_since(b.last_refill).as_secs_f64()
                    < RATE_LIMIT_BURST / RATE_LIMIT_PER_SEC
            });
        }
        let bucket = buckets.entry(ip).or_insert_with(|| TokenBucket {
            tokens: RATE_LIMIT_BURST,
            last_refill: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * RATE_LIMIT_PER_SEC).min(RATE_LIMIT_BURST);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Middleware that enforces RateLimiter on POST endpoints under `/api/`.
/// Skipped when auth is disabled (dev mode) so local development isn't
/// hampered by bursty UI testing.
async fn rate_limit_middleware(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    if request.method() != axum::http::Method::POST {
        return Ok(next.run(request).await);
    }
    // Same fix as auth_middleware: snapshot needed values from state and
    // drop the guard before calling next.run(). Holding it across the
    // await would deadlock with handlers that take a write lock.
    let (enabled, limiter) = {
        let st = state.read().await;
        (st.config.auth.enabled, st.rate_limiter.clone())
    };
    if !enabled {
        return Ok(next.run(request).await);
    }
    if !limiter.check(addr.ip()).await {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "rate limited (max {} req/sec per IP)",
                RATE_LIMIT_PER_SEC as u32
            ),
        ));
    }
    Ok(next.run(request).await)
}

/* ----------------------------- Server ----------------------------- */

fn build_router(state: AppState, cors_origins: &[String], upload_max_mb: u32) -> Router {
    // Same-origin only unless origins are explicitly configured:
    // zenith serves its own frontend, and a permissive policy would
    // let any site drive a stolen token against an API that can
    // restart executives and write target filesystems.
    let cors = if cors_origins.is_empty() {
        CorsLayer::new()
    } else {
        let origins: Vec<axum::http::HeaderValue> =
            cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    };

    let api = Router::new()
        .route("/health", get(health))
        .route("/version", get(server_version))
        .route("/targets", get(list_targets))
        .route("/targets/{id}/connect", post(connect_target))
        .route("/targets/{id}/disconnect", post(disconnect_target))
        .route("/targets/{id}/noop", post(target_noop))
        .route("/targets/{id}/health", get(target_health))
        .route("/targets/{id}/inspect/{uid}", get(target_inspect))
        .route("/targets/{id}/command", post(send_command))
        .route("/targets/{id}/upload", post(upload_file))
        .route("/targets/{id}/restart", post(restart_target))
        .route("/targets/{id}/components/{uid}/library", post(swap_library))
        .route("/targets/{id}/commands", get(get_commands_config))
        .route("/targets/{id}/params", get(list_tunable_components))
        .route("/targets/{id}/params/{uid}", get(get_params))
        .route("/targets/{id}/params/{uid}/update", post(update_params))
        .route(
            "/targets/{id}/params/{uid}/verify",
            post(tprm_verify_staged),
        )
        .route("/targets/{id}/tprm/staged", get(tprm_staged_digest))
        .route("/prefs/{scope}/{kind}", get(list_prefs))
        .route(
            "/prefs/{scope}/{kind}/{name}",
            get(get_pref_value).put(put_pref).delete(delete_pref),
        )
        .route("/targets/{id}/registry", get(target_registry))
        .route("/targets/{id}/telemetry/live", get(telemetry_ws))
        .route("/targets/{id}/telemetry/history", get(telemetry_history))
        .route("/targets/{id}/telemetry/latest", get(telemetry_latest))
        .route("/targets/{id}/telemetry/layouts", get(telemetry_layouts))
        .route(
            "/targets/{id}/telemetry/layouts/save",
            post(save_telemetry_layout),
        )
        .route(
            "/targets/{id}/telemetry/layouts/{layout_id}",
            delete(delete_telemetry_layout),
        )
        .route(
            "/targets/{id}/telemetry/layouts/export",
            get(export_telemetry_layouts),
        )
        .route("/targets/{id}/telemetry/csv", get(telemetry_csv))
        .route("/targets/{id}/storage", get(target_storage_stats))
        .route("/targets/{id}/storage/delete", post(delete_target_data))
        .route("/targets/{id}/storage/trim", post(trim_target_data))
        .route("/targets/add", post(add_target))
        .route("/targets/{id}/remove", post(remove_target))
        .route("/telemetry/stats", get(telemetry_stats))
        .route("/telemetry/downsample", post(downsample_data))
        .route("/structs", get(get_structs))
        .route("/structs/{component}", get(get_struct_detail))
        .route("/targets/{id}/structs", get(target_get_structs))
        .route(
            "/targets/{id}/structs/{component}",
            get(target_get_struct_detail),
        )
        .route("/audit", get(list_audit))
        .route("/metrics", get(get_metrics))
        .route("/auth/login", post(login))
        .route("/auth/ws-ticket", post(ws_ticket))
        .route(
            "/auth/session",
            get(session_status)
                .post(session_sign_in)
                .delete(session_sign_out),
        )
        .route("/auth/session/refresh", post(session_refresh));

    let static_dir = if std::path::Path::new("/usr/local/share/zenith/static/index.html").exists() {
        "/usr/local/share/zenith/static"
    } else {
        "./frontend/dist"
    };

    // Wrap /api with auth + rate-limit middleware. Both are no-ops when
    // auth.enabled = false (the default), so dev mode is unaffected.
    let api = api
        .layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .nest("/api", api)
        .fallback_service(tower_http::services::ServeDir::new(static_dir).fallback(
            tower_http::services::ServeFile::new(format!("{}/index.html", static_dir)),
        ))
        // Body ceiling: the configured upload cap plus base64+JSON
        // overhead. axum's 2 MB default silently made any library
        // upload near the documented cap impossible; the handler-level
        // decoded-size cap stays as the precise gate.
        .layer(axum::extract::DefaultBodyLimit::max(
            upload_max_mb as usize * 1024 * 1024 * 3 / 2,
        ))
        .layer(cors)
        // Span carries method + path only: query strings are excluded
        // deliberately (a socket ticket rides the ?ticket= query
        // parameter, which must never reach the request logs).
        .layer(TraceLayer::new_for_http().make_span_with(
            |req: &axum::http::Request<axum::body::Body>| {
                tracing::info_span!("http", method = %req.method(), path = %req.uri().path())
            },
        ))
        .with_state(state)
}

/// Per-target sample writer: subscribes to the target's decoded-sample
/// broadcast and batches rows into insert_batch. A failed batch is
/// dropped, but never silently: failures log at error level
/// (rate-limited) and recovery logs how many batches were lost.
fn spawn_sample_writer(
    target_id: String,
    db: Arc<TelemetryDb>,
    mut rx: broadcast::Receiver<TelemetrySample>,
    metrics: Arc<TargetMetrics>,
) -> std::thread::JoinHandle<()> {
    use std::sync::atomic::Ordering;

    fn flush(
        db: &TelemetryDb,
        target_id: &str,
        batch: &mut Vec<TelemetrySample>,
        metrics: &TargetMetrics,
        failed_batches: &mut u64,
        last_err_log: &mut Option<std::time::Instant>,
    ) {
        // One error line per interval, not one per failed batch: at
        // ingest rates a wedged DB would otherwise flood the log.
        const ERR_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

        if batch.is_empty() {
            return;
        }
        match db.insert_batch(batch) {
            Ok(()) => {
                metrics
                    .db_written_samples
                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
                if *failed_batches > 0 {
                    tracing::warn!(
                        "[{}] telemetry writes recovered; {} batches dropped during the outage",
                        target_id,
                        failed_batches
                    );
                    *failed_batches = 0;
                    *last_err_log = None;
                }
            }
            Err(e) => {
                *failed_batches += 1;
                metrics.db_write_failures.fetch_add(1, Ordering::Relaxed);
                metrics
                    .db_failed_samples
                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
                if last_err_log.is_none_or(|t| t.elapsed() >= ERR_LOG_EVERY) {
                    tracing::error!(
                        "[{}] telemetry batch insert failed ({} failed batches so far): {}",
                        target_id,
                        failed_batches,
                        e
                    );
                    *last_err_log = Some(std::time::Instant::now());
                }
            }
        }
        batch.clear();
    }

    // A dedicated OS thread, not a tokio task: every iteration ends in
    // a blocking SQLite write, and a contended writer (FIFO delete,
    // vacuum, checkpoint) can hold it for the full busy_timeout. On a
    // runtime worker that would stall unrelated request handling; on
    // its own thread it stalls nothing but this target's ingest.
    std::thread::spawn(move || {
        const LAG_WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
        let mut batch: Vec<TelemetrySample> = Vec::with_capacity(64);
        let mut failed_batches: u64 = 0;
        let mut last_err_log: Option<std::time::Instant> = None;
        let mut last_lag_warn: Option<std::time::Instant> = None;
        loop {
            match rx.blocking_recv() {
                Ok(sample) => {
                    batch.push(sample);
                    // Flush in batches of 50 or when channel is empty
                    if batch.len() >= 50 {
                        flush(
                            &db,
                            &target_id,
                            &mut batch,
                            &metrics,
                            &mut failed_batches,
                            &mut last_err_log,
                        );
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    metrics.db_writer_lag_drops.fetch_add(n, Ordering::Relaxed);
                    if last_lag_warn.is_none_or(|t| t.elapsed() >= LAG_WARN_EVERY) {
                        tracing::warn!(
                            "[{}] DB writer lagged, {} samples dropped ({} total)",
                            target_id,
                            n,
                            metrics.db_writer_lag_drops.load(Ordering::Relaxed)
                        );
                        last_lag_warn = Some(std::time::Instant::now());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }

            // Also flush if no more pending and batch has data
            if !batch.is_empty() && rx.is_empty() {
                flush(
                    &db,
                    &target_id,
                    &mut batch,
                    &metrics,
                    &mut failed_batches,
                    &mut last_err_log,
                );
            }
        }

        // Flush any remaining samples on channel close
        flush(
            &db,
            &target_id,
            &mut batch,
            &metrics,
            &mut failed_batches,
            &mut last_err_log,
        );
    })
}

/// How long shutdown waits for the sample writers to write what they
/// hold: a writer blocked on a busy database gives up after SQLite's
/// busy timeout (5 s) anyway, and the rest of the shutdown must fit the
/// container's stop timeout (10 s).
const SAMPLE_WRITER_FLUSH_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Take every target down at the stop signal: close connections, then
/// let each sample writer write what it holds. The write guard covers
/// only what needs it (closing connect admission, stopping the routers
/// and polls, closing each target's sample channel, collecting each link
/// and writer); it is dropped before waiting on any link mutex, which a
/// connect in its dial or an upload can hold for seconds while every
/// request needs the state's guard. A connect that completes after this
/// sees `stopping` and tears its link down.
async fn shut_down_targets(state: &AppState) {
    let mut links = Vec::new();
    let mut routers = Vec::new();
    let mut writers = Vec::new();
    {
        let mut st = state.write().await;
        st.stopping = true;
        for (id, target) in st.targets.iter_mut() {
            if let Some(h) = target._router_handle.take() {
                h.abort();
                routers.push(h);
            }
            for h in target._poll_handles.drain(..) {
                h.abort();
            }
            // A writer ends once every sender of its channel is gone:
            // this one, replaced here, and the router's, which goes with
            // the router.
            target.sample_tx = broadcast::channel(1).0;
            writers.extend(target.sample_writer.take());
            links.push((id.clone(), target.client.clone()));
        }
    }
    for (id, client) in links {
        // Up from our side, peer or no peer: a listening link's
        // socket and tasks end here too.
        let mut cli = client.lock().await;
        if cli.state_word().is_some() {
            cli.disconnect();
            tracing::info!("Disconnected target {} during shutdown", id);
        }
    }
    for router in routers {
        let _ = router.await;
    }
    let flushed = tokio::time::timeout(SAMPLE_WRITER_FLUSH_LIMIT, async {
        for writer in writers {
            let _ = tokio::task::spawn_blocking(move || writer.join()).await;
        }
    })
    .await;
    if flushed.is_err() {
        tracing::warn!(
            "sample writers still writing after {:?}; their last samples may be lost",
            SAMPLE_WRITER_FLUSH_LIMIT
        );
    }
}

/// Time-based retention, the maintenance loop's safety net under the
/// size cap: telemetry older than `retention_hours` goes, with its
/// latest values. 0 keeps telemetry forever, as `audit_retention_days
/// = 0` keeps the audit log. User layouts beyond each target's newest
/// 100 go either way. Returns the samples deleted.
fn prune_telemetry(db: &TelemetryDb, retention_hours: u32) -> Result<usize, DbError> {
    db.prune((retention_hours > 0).then(|| u64::from(retention_hours) * 3_600_000))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    if cli.hash_password {
        let mut line = String::new();
        eprintln!("Password (echoed):");
        let password = match std::io::stdin().read_line(&mut line) {
            Ok(_) => password_from_line(&line),
            Err(_) => None,
        };
        let Some(password) = password else {
            eprintln!("no password read from stdin");
            std::process::exit(1);
        };
        match crate::core::auth::hash_password(password) {
            Ok(hash) => {
                println!("{}", hash);
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("hash failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    if cli.health_check {
        std::process::exit(health_check(&cli));
    }

    // A config that cannot be read or parsed refuses boot. The
    // alternative, a default server with no targets and auth off,
    // answers health checks and looks fine while serving nothing.
    let config = match config::load(&cli.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("config refused: {}", e);
            std::process::exit(1);
        }
    };

    // Refuse to boot with unusable auth: a default signing secret or a
    // missing password hash while auth is enabled is a misconfiguration
    // that must fail loudly at startup, not quietly at first login.
    let auth_errors = crate::core::auth::boot_errors(&config.auth);
    if !auth_errors.is_empty() {
        for e in &auth_errors {
            tracing::error!("auth misconfiguration: {}", e);
        }
        std::process::exit(1);
    }

    // The configured bind host and port, --port over the port; the
    // health check dials the same address (ServerSection::listen_addr).
    let addr = config.server.listen_addr(cli.port);

    // Load global struct dictionaries (fallback for targets without their own)
    let global_dicts = if let Some(ref dir) = config.storage.structs_dir {
        let path = std::path::PathBuf::from(dir);
        match StructDictionary::load_dir(&path) {
            Ok(dicts) => Arc::new(dicts),
            Err(e) => {
                tracing::warn!("Failed to load struct dicts from {}: {}", dir, e);
                Arc::new(StructDictionary::default())
            }
        }
    } else {
        tracing::info!("No structs_dir configured, struct dictionaries not loaded");
        Arc::new(StructDictionary::default())
    };

    // A target path the server cannot read for want of permission would
    // otherwise read as absent: refuse, naming the path and the error.
    if let Some((name, path, e)) = config::unreadable_target_path(&config.targets) {
        eprintln!(
            "FATAL: target '{}': cannot read '{}': {}",
            name,
            path.display(),
            e
        );
        std::process::exit(1);
    }

    // A mistyped target directory would otherwise produce a silently
    // featureless target: connected, decoding nothing by name.
    if let Some((name, dir)) = config::missing_target_dir(&config.targets) {
        eprintln!("FATAL: target '{}': dir '{}' is not a directory", name, dir);
        std::process::exit(1);
    }

    if let Some((name, reason)) = config::invalid_poll(&config.targets) {
        eprintln!("FATAL: target '{}': {}", name, reason);
        std::process::exit(1);
    }

    // Cross-target definition check: two targets binding one local
    // listen port cannot both hear their telemetry, and the loser
    // would only find out at connect time. Boot refusal instead.
    if let Some((port, first, second)) = config::duplicate_listen_port(&config.targets) {
        eprintln!(
            "FATAL: targets '{}' and '{}' both declare listen_port {}; \
             distinct targets cannot share a local listen port",
            first, second, port
        );
        std::process::exit(1);
    }

    // Initialize targets with per-target or global struct dicts
    let mut targets = HashMap::new();
    for (i, tc) in config.targets.iter().enumerate() {
        let id = format!("target-{}", i);
        let (push_tlm_tx, _) = broadcast::channel::<PushTelemetryPacket>(4096);
        let (sample_tx, _) = broadcast::channel::<TelemetrySample>(4096);

        // Every generated artifact this target has, from its
        // directory or its explicit paths. One loader, shared with the
        // runtime add-target path, so an aproto-slip target added to a
        // running zenith finds its artifacts as one declared here does.
        let artifacts = load_target_artifacts(tc, &global_dicts).unwrap_or_else(|e| {
            eprintln!("FATAL: target '{}': {}", tc.name, e);
            std::process::exit(1);
        });
        let target_dicts = artifacts.dicts;
        let manifest = artifacts.manifest;
        let commands_config = artifacts.commands_config;
        let connect_init = artifacts.connect_init;
        let record_table = artifacts.record_table;

        let metrics = TargetMetrics::new();
        // Boot refusal on an unknown protocol: a typo in config.toml
        // must never silently become the default transport.
        let protocol = Protocol::from_config(&tc.protocol).unwrap_or_else(|e| {
            eprintln!("FATAL: target '{}': {}", tc.name, e);
            std::process::exit(1);
        });
        // Same refusal discipline for the carrier axis: a typo or a
        // portless UDP listener is a config bug, not a degraded mode.
        if let Err(e) = carrier_from_config(tc, protocol) {
            eprintln!("FATAL: target '{}': {}", tc.name, e);
            std::process::exit(1);
        }
        if let Some(e) = config::invalid_tm_frame_size(tc) {
            eprintln!("FATAL: target '{}': {}", tc.name, e);
            std::process::exit(1);
        }
        if let Some(e) = config::retired_key(tc) {
            eprintln!("FATAL: target '{}': {}", tc.name, e);
            std::process::exit(1);
        }
        if let Err(e) = tc.health_checks() {
            eprintln!("FATAL: target '{}': {}", tc.name, e);
            std::process::exit(1);
        }
        // Connect-time init sequence: stream links only, for now.
        // The APROTO family's connect-time behavior belongs to its
        // command machine (catalog-named commands with real ACK
        // handling), not raw bytes injected under it -- until that
        // exists, declaring the file there must refuse rather than
        // load-log-audit a sequence that never gets sent.
        if protocol == Protocol::AprotoSlip && connect_init.is_some() {
            eprintln!(
                "FATAL: target '{}': connect_init is not supported on aproto-slip \
                 (stream protocols only; the aproto command path owns connect-time behavior)",
                tc.name
            );
            std::process::exit(1);
        }
        // Record-stage protocols need their generated table; a
        // missing one is a definition bug -> boot refusal (a broken
        // one already refused in the loader).
        if protocol == Protocol::TmCcsdsSppRecords && record_table.is_none() {
            eprintln!(
                "FATAL: target '{}': protocol '{}' requires records.json in the target \
                 directory (or records_config)",
                tc.name, tc.protocol
            );
            std::process::exit(1);
        }
        let record_spec = record_table.as_ref().map(|t| record_spec_from_table(t));
        let mut new_client = build_link(
            protocol,
            push_tlm_tx.clone(),
            tc,
            connect_init.as_deref(),
            record_spec,
        );
        new_client.set_metrics(metrics.clone());
        let connected = new_client.connected_handle();
        let listening = new_client.listening_handle();
        targets.insert(
            id,
            TargetState {
                config: tc.clone(),
                client: Arc::new(Mutex::new(new_client)),
                connected,
                listening,
                metrics,
                push_tlm_tx,
                sample_tx,
                struct_dicts: target_dicts,
                manifest,
                commands_config,
                connect_init,
                record_table,
                _router_handle: None,
                _poll_handles: Vec::new(),
                sample_writer: None,
            },
        );
    }

    // Open telemetry database
    // Resolve to an absolute path before opening so the startup log
    // states unambiguously where the DB lives -- a relative config path
    // resolves against the process working directory, which in the
    // container is not the persistent volume.
    let db_path = std::path::absolute(std::path::PathBuf::from(&config.storage.path))
        .unwrap_or_else(|_| std::path::PathBuf::from(&config.storage.path));
    let db = match TelemetryDb::open(&db_path) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            tracing::error!(
                "Failed to open telemetry database at {}: {}",
                db_path.display(),
                e
            );
            std::process::exit(1);
        }
    };

    // Spawn background writer: subscribes to each target's sample_tx and writes to DB.
    // Each writer flushes remaining samples when the broadcast channel closes;
    // shutdown closes the channels and waits for the writers (shut_down_targets).
    for (id, target) in targets.iter_mut() {
        target.sample_writer = Some(spawn_sample_writer(
            id.clone(),
            db.clone(),
            target.sample_tx.subscribe(),
            target.metrics.clone(),
        ));
    }

    // Spawn periodic maintenance (every 60 seconds)
    // - Measure net fill rate for storage vitals
    // - Size-based FIFO when the DB exceeds its cap (fair waterline
    //   across targets by default; see storage.fifo_strategy)
    // - Prune samples older than retention (retention_hours = 0 keeps
    //   them all)
    // - Prune audit rows beyond their retention
    // - Clean up stale telemetry_latest entries and old user layouts
    // - WAL checkpoint after large deletions
    // Downsampling is operator-triggered only (POST /api/telemetry/
    // downsample); nothing here rewrites history automatically.
    let db_maint = db.clone();
    let max_db_mb = config
        .storage
        .max_db_size_mb
        .unwrap_or(2048)
        .clamp(256, 102_400); // 256MB min, 100GB max
    let max_db_bytes: u64 = max_db_mb as u64 * 1024 * 1024;
    tracing::info!(
        "Storage limits: max_db_size={}MB, retention={}",
        max_db_mb,
        match config.storage.retention_hours {
            0 => "forever".to_string(),
            hours => format!("{hours}h"),
        }
    );
    if let Some(warning) = config::coarse_tier_stays_empty(&config.storage) {
        tracing::warn!("{warning}");
    }
    let retention_hours = config.storage.retention_hours;
    let audit_retention_days = config.storage.audit_retention_days;
    let fifo_strategy = config.storage.fifo_strategy;
    let storage_vitals = Arc::new(StorageVitals {
        cap_bytes: max_db_bytes,
        tiers: config.storage.tiers,
        fill_bytes_per_min: std::sync::atomic::AtomicI64::new(0),
        last_size: std::sync::atomic::AtomicU64::new(0),
        fifo_evicted_samples: std::sync::atomic::AtomicU64::new(0),
        retention_pruned_samples: std::sync::atomic::AtomicU64::new(0),
        tiered_source_rows: std::sync::atomic::AtomicU64::new(0),
        tier_band_rows: Default::default(),
    });
    let tiers = config.storage.tiers;
    let vitals_maint = storage_vitals.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;

            // The whole tick is blocking DB work -- FIFO deletes and
            // vacuum can hold the writer for seconds on a full table --
            // so it runs on the blocking pool, never on a runtime worker.
            let db = db_maint.clone();
            let vitals = vitals_maint.clone();
            let tick = tokio::task::spawn_blocking(move || {
                // Net fill rate: delta of pre-eviction file size across
                // ticks (the interval is 60s, so a delta IS per-minute).
                // Signed on purpose: a capped database hovering at its
                // limit reports ~0, which is the demo's success signal.
                let size_now = db.db_size_bytes().unwrap_or(0);
                let prev = vitals
                    .last_size
                    .swap(size_now, std::sync::atomic::Ordering::Relaxed);
                if prev > 0 {
                    vitals.fill_bytes_per_min.store(
                        size_now as i64 - prev as i64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                // Size-based FIFO: if DB exceeds max size, delete enough to free space
                if let Ok(stats) = db.global_stats() {
                    if stats.db_size_bytes > max_db_bytes && stats.total_samples > 0 {
                        // Calculate bytes to free, estimate samples to delete
                        let bytes_over = stats.db_size_bytes - max_db_bytes;
                        let avg_bytes_per_sample = stats.db_size_bytes / stats.total_samples.max(1);
                        // Delete 120% of estimated needed to account for index/overhead
                        let to_delete =
                            ((bytes_over / avg_bytes_per_sample.max(1)) * 6 / 5).max(100);

                        // Fair strategy waterlines the largest holders so
                        // a chatty target cannot evict a quiet target's
                        // only history; global deletes oldest regardless
                        // of owner (config escape hatch).
                        let deleted: usize = match fifo_strategy {
                            FifoStrategy::Fair => match db.target_sample_counts() {
                                Ok(per_target) => {
                                    let allocs = crate::storage::telemetry_db::allocate_evictions(
                                        &per_target,
                                        to_delete,
                                    );
                                    let mut total = 0usize;
                                    for (target, n) in &allocs {
                                        match db.delete_oldest_for_target(target, *n as usize) {
                                            Ok(d) => {
                                                total += d;
                                                tracing::info!(
                                                    "FIFO(fair) evicted {} oldest samples from {}",
                                                    d,
                                                    target
                                                );
                                            }
                                            Err(e) => tracing::warn!(
                                                "FIFO(fair) eviction failed for {}: {}",
                                                target,
                                                e
                                            ),
                                        }
                                    }
                                    total
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "per-target counts unavailable ({}), falling back to global FIFO",
                                        e
                                    );
                                    db.delete_oldest(to_delete as usize).unwrap_or(0)
                                }
                            },
                            FifoStrategy::Global => db.delete_oldest(to_delete as usize).unwrap_or(0),
                        };

                        if deleted > 0 {
                            vitals
                                .fifo_evicted_samples
                                .fetch_add(deleted as u64, std::sync::atomic::Ordering::Relaxed);
                            tracing::info!(
                                "DB size {:.0}MB > {:.0}MB limit, FIFO deleted {} oldest samples (~{:.0}MB)",
                                stats.db_size_bytes as f64 / 1_048_576.0,
                                max_db_bytes as f64 / 1_048_576.0,
                                deleted,
                                (deleted as u64 * avg_bytes_per_sample) as f64 / 1_048_576.0
                            );
                            // Reclaim freed pages immediately
                            let _ = db.incremental_vacuum();
                        }
                    }
                }

                // Retention ladder: convert aged rows to envelope
                // buckets, one bounded slice per tier per tick. The
                // FIFO above stays the size backstop and naturally
                // evicts the oldest (coarsest) rows first.
                if tiers.enabled {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    const SLICE_MS: u64 = 600_000;
                    let full_cutoff =
                        now_ms.saturating_sub(tiers.full_resolution_minutes as u64 * 60_000);
                    let mid_cutoff =
                        now_ms.saturating_sub(tiers.mid_horizon_hours as u64 * 3_600_000);
                    for (tier, cutoff, bucket_ms) in [
                        (
                            crate::storage::telemetry_db::MID_TIER,
                            full_cutoff,
                            tiers.mid_bucket_seconds as u64 * 1_000,
                        ),
                        (
                            crate::storage::telemetry_db::COARSE_TIER,
                            mid_cutoff,
                            tiers.coarse_bucket_seconds as u64 * 1_000,
                        ),
                    ] {
                        match db.tier_pass(tier, cutoff, bucket_ms, SLICE_MS) {
                            Ok(pass) if pass.source_rows > 0 => {
                                vitals.tiered_source_rows.fetch_add(
                                    pass.source_rows,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                tracing::info!(
                                    "tier {}: {} rows -> {} envelope buckets",
                                    tier,
                                    pass.source_rows,
                                    pass.bucket_rows
                                );
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("tier {} pass failed: {}", tier, e),
                        }
                    }
                    // Age-band populations for the storage panel
                    // (index-only range counts).
                    let bands = [
                        db.count_range(full_cutoff, i64::MAX as u64).unwrap_or(0),
                        db.count_range(mid_cutoff, full_cutoff).unwrap_or(0),
                        db.count_range(0, mid_cutoff).unwrap_or(0),
                    ];
                    for (slot, count) in vitals.tier_band_rows.iter().zip(bands) {
                        slot.store(count, std::sync::atomic::Ordering::Relaxed);
                    }
                }

                // Time-based retention as a safety net
                match prune_telemetry(&db, retention_hours) {
                    Ok(n) if n > 0 => {
                        vitals
                            .retention_pruned_samples
                            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        tracing::info!("Pruned {} samples beyond retention", n)
                    }
                    _ => {}
                }

                // Audit retention (0 = keep forever)
                if audit_retention_days > 0 {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let cutoff = now_ms.saturating_sub(audit_retention_days as u64 * 86_400_000);
                    match db.prune_audit(cutoff) {
                        Ok(n) if n > 0 => {
                            tracing::info!("Pruned {} audit rows beyond retention", n)
                        }
                        _ => {}
                    }
                }
            })
            .await;
            if let Err(e) = tick {
                tracing::warn!("maintenance tick panicked: {}", e);
            }
        }
    });

    // Register each target's curated layouts with the database, the
    // same helper runtime Add Target uses.
    for (id, target) in &targets {
        crate::core::registration::register_layouts(
            &db,
            id,
            &target.config.name,
            target.config.telemetry_path().as_deref(),
        );
    }

    // Browser sessions live in the database so a restart keeps them;
    // the store loads the live ones and drops those that ended while
    // the server was down.
    let sessions = match SessionStore::open(
        db.clone(),
        SessionPolicy::from_config(&config.auth),
        now_ms(),
    ) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            tracing::error!("Failed to load browser sessions: {}", e);
            std::process::exit(1);
        }
    };
    // The sweep ends sessions at their deadlines and closes their
    // telemetry sockets, whether or not a request arrives.
    if config.auth.enabled {
        let sweeper = sessions.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(
                crate::core::session::SWEEP_EVERY_MS,
            ));
            loop {
                interval.tick().await;
                let store = sweeper.clone();
                match tokio::task::spawn_blocking(move || store.sweep(now_ms())).await {
                    Ok(ended) => {
                        for (user, reason) in ended {
                            tracing::info!("browser session of {}: {}", user, reason.describe());
                        }
                    }
                    Err(e) => tracing::warn!("session sweep panicked: {}", e),
                }
            }
        });
    }

    let state: AppState = Arc::new(RwLock::new(SharedState {
        config: config.clone(),
        targets,
        db,
        struct_dicts: global_dicts,
        rate_limiter: Arc::new(RateLimiter::default()),
        storage_vitals,
        sessions,
        stopping: false,
    }));

    // Honor `auto_connect = true` on each target. We dispatch the
    // connect calls in the background so a slow or unreachable target
    // doesn't block server startup.
    {
        let state_for_autoconnect = state.clone();
        let auto_ids: Vec<String> = config
            .targets
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                if t.auto_connect {
                    Some(format!("target-{}", i))
                } else {
                    None
                }
            })
            .collect();
        if !auto_ids.is_empty() {
            tokio::spawn(async move {
                for id in auto_ids {
                    // Same snapshot-then-connect path as the HTTP handler:
                    // a slow target must not hold the state write guard
                    // (5 s TCP timeout each) while the API serves requests.
                    match do_connect_target(&state_for_autoconnect, &id).await {
                        Ok(ConnectAnswer::BroughtUp(status)) => {
                            tracing::info!("auto_connect: {} {}", id, status)
                        }
                        Ok(ConnectAnswer::Already(_)) => {}
                        Ok(ConnectAnswer::Refused) => {
                            tracing::warn!("auto_connect: {} refused: shutting down", id);
                        }
                        Ok(ConnectAnswer::Failed(e)) | Err((_, e)) => {
                            tracing::warn!("auto_connect: {} failed: {}", id, e);
                        }
                    }
                }
            });
        }
    }

    let app = build_router(
        state.clone(),
        &config.server.cors_allowed_origins,
        config.server.upload_max_mb,
    );

    tracing::info!("Zenith v{} starting on {}", env!("CARGO_PKG_VERSION"), addr);
    tracing::info!("Telemetry database: {}", db_path.display());
    tracing::info!(
        "Configured targets: {}",
        config
            .targets
            .iter()
            .map(|t| format!("{}({}:{})", t.name, t.host, t.port))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind {}: {}", addr, e);
            std::process::exit(1);
        }
    };

    // Graceful shutdown: wait for SIGINT/SIGTERM, then clean up.
    // `into_make_service_with_connect_info` is required so the
    // rate-limit middleware can extract the client IP via ConnectInfo.
    let shutdown_state = state.clone();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        #[cfg(unix)]
        let terminate = sigterm.recv();
        #[cfg(not(unix))]
        let terminate = std::future::pending::<Option<()>>();

        tokio::select! {
            _ = ctrl_c => tracing::info!("Received SIGINT, shutting down"),
            _ = terminate => tracing::info!("Received SIGTERM, shutting down"),
        }
        shut_down_targets(&shutdown_state).await;
    });

    if let Err(e) = server.await {
        tracing::error!("Server error: {}", e);
        std::process::exit(1);
    }

    tracing::info!("Zenith shutdown complete");
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use serde_json::json;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::core::config_manager::{ComponentDict, FieldDef};
    use crate::core::transport::StateWord;
    use crate::storage::telemetry_db::AuditEntry;

    /// Where every test request comes from.
    const CLIENT: SocketAddr =
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 40_000);

    /// One apex target. Its link is built and never dialed by these
    /// tests, so every request that reaches it is refused "not
    /// connected".
    const APEX_TARGET: &str = r#"
        [[targets]]
        name = "apex"
        host = "127.0.0.1"
        port = 9
    "#;

    /// A server as boot assembles it, over a temporary database: the
    /// config is config.toml text, and each `[[targets]]` entry becomes
    /// a target under its positional id, its link built but not
    /// connected, with the given dictionaries.
    struct Server {
        state: AppState,
        db: Arc<TelemetryDb>,
        dir: TempDir,
    }

    impl Server {
        fn new(config: &str) -> Self {
            Self::with_dicts(config, StructDictionary::default())
        }

        fn with_dicts(config: &str, dicts: StructDictionary) -> Self {
            let config: ServerConfig = toml::from_str(config).unwrap();
            let dir = TempDir::new().unwrap();
            let db = Arc::new(TelemetryDb::open(&dir.path().join("zenith.db")).unwrap());
            let dicts = Arc::new(dicts);
            let targets = config
                .targets
                .iter()
                .enumerate()
                .map(|(i, tc)| (format!("target-{i}"), target_state(tc, &dicts)))
                .collect();
            let sessions = SessionStore::open(
                db.clone(),
                SessionPolicy::from_config(&config.auth),
                now_ms(),
            )
            .unwrap();
            let storage_vitals = StorageVitals {
                cap_bytes: 0,
                tiers: config.storage.tiers,
                fill_bytes_per_min: Default::default(),
                last_size: Default::default(),
                fifo_evicted_samples: Default::default(),
                retention_pruned_samples: Default::default(),
                tiered_source_rows: Default::default(),
                tier_band_rows: Default::default(),
            };
            let state = Arc::new(RwLock::new(SharedState {
                config,
                targets,
                db: db.clone(),
                struct_dicts: Arc::new(StructDictionary::default()),
                rate_limiter: Arc::new(RateLimiter::default()),
                storage_vitals: Arc::new(storage_vitals),
                sessions: Arc::new(sessions),
                stopping: false,
            }));
            Self { state, db, dir }
        }

        /// One request answered in process by the router the server
        /// runs: its status and body text.
        async fn call(&self, request: axum::http::Request<Body>) -> (StatusCode, String) {
            let app = build_router(self.state.clone(), &[], 50).layer(MockConnectInfo(CLIENT));
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        }

        async fn get(&self, uri: &str) -> (StatusCode, String) {
            self.call(axum::http::Request::get(uri).body(Body::empty()).unwrap())
                .await
        }

        async fn post(&self, uri: &str, body: serde_json::Value) -> (StatusCode, String) {
            self.call(
                axum::http::Request::post(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
        }

        /// The audit log, oldest entry first.
        fn audit(&self) -> Vec<AuditEntry> {
            let mut entries = self.db.query_audit(1000, 0).unwrap();
            entries.reverse();
            entries
        }

        async fn link(&self, id: &str) -> Arc<Mutex<ProtocolLink>> {
            self.state.read().await.targets[id].client.clone()
        }
    }

    /// A request with the given headers and, when there is one, a JSON
    /// body.
    fn request(
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        match body {
            Some(json) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string())),
            None => builder.body(Body::empty()),
        }
        .unwrap()
    }

    /// One target as boot builds it, its link not connected.
    fn target_state(tc: &config::TargetSection, dicts: &Arc<StructDictionary>) -> TargetState {
        let (push_tlm_tx, _) = broadcast::channel(64);
        let (sample_tx, _) = broadcast::channel(64);
        let protocol = Protocol::from_config(&tc.protocol).unwrap();
        let metrics = TargetMetrics::new();
        let mut link = build_link(protocol, push_tlm_tx.clone(), tc, None, None);
        link.set_metrics(metrics.clone());
        TargetState {
            config: tc.clone(),
            connected: link.connected_handle(),
            listening: link.listening_handle(),
            client: Arc::new(Mutex::new(link)),
            metrics,
            push_tlm_tx,
            sample_tx,
            struct_dicts: dicts.clone(),
            manifest: None,
            commands_config: None,
            connect_init: None,
            record_table: None,
            _router_handle: None,
            _poll_handles: Vec::new(),
            sample_writer: None,
        }
    }

    fn temp_db() -> (TempDir, TelemetryDb) {
        let dir = TempDir::new().unwrap();
        let db = TelemetryDb::open(&dir.path().join("zenith.db")).unwrap();
        (dir, db)
    }

    fn sample(channel: &str, timestamp_ms: u64, value: f64) -> TelemetrySample {
        TelemetrySample {
            target_id: Arc::from("target-0"),
            timestamp_ms,
            channel: Arc::from(channel),
            value,
            envelope: None,
        }
    }

    /* ---- Retention ---- */

    /// @test retention_hours = 0 keeps telemetry forever, as
    /// audit_retention_days = 0 keeps the audit log: the retention step
    /// deletes no sample and no latest value, however old. A nonzero
    /// retention deletes what is older than its window, latest values
    /// with it, and keeps the rest.
    #[test]
    fn zero_retention_hours_keeps_telemetry_forever() {
        const HOUR: u64 = 3_600_000;
        let (_dir, db) = temp_db();
        let now = now_ms();
        db.insert_batch(&[
            sample("Old.x", now - 30 * HOUR, 1.0),
            sample("New.y", now - 25 * HOUR, 2.0),
            sample("New.y", now - 5_000, 3.0),
        ])
        .unwrap();
        let latest = |db: &TelemetryDb| {
            let mut channels: Vec<String> = db
                .query_latest("target-0")
                .unwrap()
                .into_iter()
                .map(|s| s.channel.to_string())
                .collect();
            channels.sort();
            channels
        };

        assert_eq!(
            prune_telemetry(&db, 0).unwrap(),
            0,
            "retention_hours = 0 deleted telemetry"
        );
        assert_eq!(db.count().unwrap(), 3);
        assert_eq!(latest(&db), ["New.y", "Old.x"]);

        assert_eq!(prune_telemetry(&db, 24).unwrap(), 2);
        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(latest(&db), ["New.y"]);
    }

    /* ---- Tunable encoding ---- */

    fn field(field_type: &str, size: usize) -> FieldDef {
        serde_json::from_value(json!({
            "name": "f", "type": field_type, "offset": 0, "size": size,
        }))
        .unwrap()
    }

    /// `value` encoded into `field` alone: the bytes, or the refusal.
    fn encoded(field: &FieldDef, value: &serde_json::Value) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; field.size];
        encode_field(&mut buf, field, value, false).map(|()| buf)
    }

    /// @test A tunable value its field cannot hold is refused, never
    /// written: an integer outside its field's width in either
    /// direction, a bool other than 0 or 1, a fraction for an integer, a
    /// float32 beyond its range, and an array element out of range. The
    /// refusal names the value and the range; each width's own limits
    /// encode exactly, little-endian.
    #[test]
    fn tunable_values_outside_their_field_are_refused() {
        // A field, a value, and its bytes or a fragment of its refusal.
        type Row = (FieldDef, serde_json::Value, Result<Vec<u8>, &'static str>);
        let ff = |n: usize| Ok(vec![0xFF; n]);
        let rows: Vec<Row> = vec![
            (field("uint", 1), json!(255), ff(1)),
            (
                field("uint", 1),
                json!(300),
                Err("300 is outside uint8's range 0..=255"),
            ),
            (
                field("uint", 1),
                json!(-1),
                Err("-1 is outside uint8's range 0..=255"),
            ),
            (field("uint", 2), json!(65_535), ff(2)),
            (
                field("uint", 2),
                json!(70_000),
                Err("70000 is outside uint16's range"),
            ),
            (field("uint", 4), json!(4_294_967_295u64), ff(4)),
            (
                field("uint", 4),
                json!(5_000_000_000u64),
                Err("outside uint32's range"),
            ),
            (field("uint", 8), json!(u64::MAX), ff(8)),
            (
                field("uint", 8),
                json!(-1),
                Err("-1 is outside uint64's range"),
            ),
            (field("int", 1), json!(-128), Ok(vec![0x80])),
            (field("int", 1), json!(127), Ok(vec![0x7F])),
            (
                field("int", 1),
                json!(200),
                Err("200 is outside int8's range -128..=127"),
            ),
            (
                field("int", 1),
                json!(-200),
                Err("-200 is outside int8's range -128..=127"),
            ),
            (field("int", 2), json!(-32_768), Ok(vec![0x00, 0x80])),
            (
                field("int", 2),
                json!(32_768),
                Err("32768 is outside int16's range"),
            ),
            (field("int", 4), json!(i32::MIN), Ok(vec![0, 0, 0, 0x80])),
            (
                field("int", 4),
                json!(1u64 << 31),
                Err("2147483648 is outside int32's range"),
            ),
            (
                field("int", 8),
                json!(i64::MIN),
                Ok(vec![0, 0, 0, 0, 0, 0, 0, 0x80]),
            ),
            (
                field("int", 8),
                json!(u64::MAX),
                Err("is outside int64's range"),
            ),
            (field("bool", 1), json!(0), Ok(vec![0])),
            (field("bool", 1), json!(1), Ok(vec![1])),
            (field("bool", 1), json!(2), Err("2 is not a bool (0 or 1)")),
            (
                field("bool", 1),
                json!(-1),
                Err("-1 is not a bool (0 or 1)"),
            ),
            (field("uint", 1), json!(1.5), Err("1.5 is not an integer")),
            (
                field("float", 4),
                json!(f32::MAX),
                Ok(f32::MAX.to_le_bytes().to_vec()),
            ),
            (
                field("float", 4),
                json!(3.5e38),
                Err("3.5e+38 is outside float32's range"),
            ),
            (
                field("float", 4),
                json!(-3.5e38),
                Err("-3.5e+38 is outside float32's range"),
            ),
        ];
        let pair: FieldDef = serde_json::from_value(json!({
            "name": "f", "type": "array", "offset": 0, "size": 2,
            "element_type": "uint", "dims": [2],
        }))
        .unwrap();
        let rows = rows.into_iter().chain([
            (pair.clone(), json!([1, 2]), Ok(vec![1, 2])),
            (
                pair,
                json!([1, 256]),
                Err("element 1: 256 is outside uint8's range"),
            ),
        ]);

        let mut written = Vec::new();
        let mut refusals = Vec::new();
        for (field, value, want) in rows {
            let shape = format!("{}{} <- {}", field.field_type, field.size * 8, value);
            match (want, encoded(&field, &value)) {
                (Ok(bytes), got) => assert_eq!(got, Ok(bytes), "{shape}"),
                (Err(_), Ok(bytes)) => written.push(format!("{shape} wrote {bytes:?}")),
                (Err(reason), Err(said)) => refusals.push((shape, reason, said)),
            }
        }
        assert!(
            written.is_empty(),
            "values their field cannot hold were written: {written:?}"
        );
        for (shape, reason, said) in refusals {
            assert!(said.contains(reason), "{shape}: refused as {said:?}");
        }
    }

    /// Dictionaries with one tunable block: uint8 `level`, bool
    /// `enabled` and int16 `trim`, four bytes.
    fn tunable_dicts() -> StructDictionary {
        let dict: ComponentDict = serde_json::from_value(json!({
            "component": "Gain",
            "structs": { "GainParams": {
                "category": "TUNABLE_PARAM", "size": 4,
                "fields": [
                    { "name": "level", "type": "uint", "offset": 0, "size": 1 },
                    { "name": "enabled", "type": "bool", "offset": 1, "size": 1 },
                    { "name": "trim", "type": "int", "offset": 2, "size": 2 },
                ],
            }},
        }))
        .unwrap();
        StructDictionary {
            components: HashMap::from([("Gain".to_string(), dict)]),
        }
    }

    /// @test A tunable update carrying a value its field cannot hold
    /// fails before anything is sent: a 400 naming the field, the value
    /// and the range, and no update reaches the link, so none is
    /// audited. The same update in range goes on to the upload, which
    /// the unconnected link refuses.
    #[tokio::test]
    async fn an_unholdable_tunable_fails_the_update_before_any_upload() {
        let server = Server::with_dicts(APEX_TARGET, tunable_dicts());
        let update = |level: i64| {
            json!({
                "fields": { "level": level, "enabled": 1, "trim": -5 },
                "raw_hex": "00000000",
            })
        };
        let uri = "/api/targets/target-0/params/00D000/update";

        let (status, body) = server.post(uri, update(300)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("level: 300 is outside uint8's range 0..=255"),
            "{body}"
        );
        assert!(server.audit().is_empty(), "the update reached the link");

        let (status, body) = server.post(uri, update(200)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        let audit = server.audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(
            (audit[0].action.as_str(), audit[0].status.as_str()),
            ("update_tprm", "err: not connected")
        );
    }

    /* ---- Request numbers ---- */

    /// @test The command route reads a number as hex only with a 0x
    /// prefix, and as decimal otherwise: the console's 0x body, its
    /// upper-case spelling and the same uid and opcode in decimal are
    /// one command at the link, each attempt audited as uid 0x00D000,
    /// opcode 0x0100. Bare hex digits, an opcode past 16 bits and an
    /// empty uid are refused before the link.
    #[tokio::test]
    async fn command_numbers_read_as_hex_with_0x_and_decimal_otherwise() {
        let server = Server::new(APEX_TARGET);
        let command = |uid: &str, opcode: &str| json!({ "full_uid": uid, "opcode": opcode });
        let uri = "/api/targets/target-0/command";

        for (uid, opcode) in [
            ("0x00D000", "0x0100"),
            ("0X00d000", "0X100"),
            ("53248", "256"),
        ] {
            let (status, body) = server.post(uri, command(uid, opcode)).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{uid} {opcode}: {body}");
        }
        let details: Vec<String> = server
            .audit()
            .into_iter()
            .map(|e| e.detail.unwrap_or_default())
            .collect();
        assert_eq!(details, ["uid=0x00D000 opcode=0x0100 payload_len=0"; 3]);

        for (uid, opcode, refusal) in [
            ("D000", "0x0100", "Invalid fullUid: D000"),
            ("0x00D000", "65536", "Invalid opcode: 65536"),
            ("0x00D000", "0x10000", "Invalid opcode: 0x10000"),
            ("", "0x0100", "Invalid fullUid: "),
        ] {
            let (status, body) = server.post(uri, command(uid, opcode)).await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::BAD_REQUEST, refusal),
                "{uid} {opcode}"
            );
        }
        assert_eq!(
            server.audit().len(),
            3,
            "a refused command reached the link"
        );
    }

    /// @test A uid in a route path is hex digits, with or without the 0x
    /// prefix, as the console writes it there (a manifest fullUid
    /// without its prefix): each form reaches the link, and a uid that
    /// is not hex is refused before it.
    #[tokio::test]
    async fn path_uids_read_as_hex() {
        let server = Server::new(APEX_TARGET);
        for uid in ["00D000", "0x00D000", "d000"] {
            let (status, body) = server
                .get(&format!("/api/targets/target-0/inspect/{uid}?category=1"))
                .await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{uid}: {body}");
        }
        let (status, body) = server
            .get("/api/targets/target-0/inspect/0xZZ?category=1")
            .await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::BAD_REQUEST, "Invalid fullUid: 0xZZ")
        );
    }

    /* ---- Connect admission and shutdown ---- */

    /// A context whose waker does nothing: the admission tests poll a
    /// connect by hand, so each step it takes is one the test chose.
    fn cx() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    /// A server with one apex target dialing `peer`, a loopback listener
    /// nobody accepts on, and one poll row, so a connect that brings the
    /// link up also starts a poll.
    fn dialing_server() -> (std::net::TcpListener, Server) {
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        peer.set_nonblocking(true).unwrap();
        let port = peer.local_addr().unwrap().port();
        let server = Server::new(&format!(
            r#"
            [[targets]]
            name = "apex"
            host = "127.0.0.1"
            port = {port}
            [[targets.poll]]
            uid = "0x00D000"
            category = 4
            rate_hz = 1.0
            "#
        ));
        (peer, server)
    }

    /// Whether the target dialed `peer`: a connection its link opened is
    /// queued for accept within moments of the connect returning.
    async fn dialed(peer: &std::net::TcpListener) -> bool {
        for _ in 0..50 {
            match peer.accept() {
                Ok(_) => return true,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(1)).await
                }
                Err(e) => panic!("accept: {e}"),
            }
        }
        false
    }

    /// @test Once shutdown has begun no connect is admitted: the entry
    /// check refuses at once, without waiting for the link (held here,
    /// as an upload holds it), and the target is never dialed.
    #[tokio::test]
    async fn a_connect_asked_for_once_shutdown_began_is_refused_at_entry() {
        let (peer, server) = dialing_server();
        server.state.write().await.stopping = true;
        let link = server.link("target-0").await;
        let held = link.lock().await;

        let answer = pin!(do_connect_target(&server.state, "target-0")).poll(&mut cx());
        assert!(
            matches!(answer, Poll::Ready(Ok(ConnectAnswer::Refused))),
            "{answer:?}"
        );
        drop(held);
        assert!(!dialed(&peer).await, "a refused connect dialed the target");
    }

    /// @test A connect that waited for the link behind another caller
    /// looks again once it holds it: shutdown began while it waited, so
    /// it is refused and never dials, although it passed the entry check.
    #[tokio::test]
    async fn a_connect_that_waited_for_the_link_is_refused_once_shutdown_began() {
        let (peer, server) = dialing_server();
        let link = server.link("target-0").await;
        let held = link.lock().await;

        let mut connect = pin!(do_connect_target(&server.state, "target-0"));
        assert!(
            connect.as_mut().poll(&mut cx()).is_pending(),
            "the connect did not wait for the link"
        );
        server.state.write().await.stopping = true;
        drop(held);

        assert_eq!(connect.await, Ok(ConnectAnswer::Refused));
        assert!(
            !dialed(&peer).await,
            "the connect dialed once shutdown began"
        );
        assert_eq!(link.lock().await.state_word(), None);
    }

    /// @test A connect that completes after shutdown began is torn down
    /// and refused: it passed both checks before shutdown began and
    /// dialed, shutdown began before it finished, and it finds that under
    /// the write guard, so it takes the link it brought up down again
    /// and registers no router and no poll.
    #[tokio::test]
    async fn a_connect_completing_after_shutdown_began_is_torn_down_and_refused() {
        let (peer, server) = dialing_server();
        let state = &server.state;
        let link = server.link("target-0").await;

        // Park the connect at its second check: the link is held while
        // it passes the entry check, the state's write guard while it
        // takes the link.
        let held = link.lock().await;
        let mut connect = pin!(do_connect_target(state, "target-0"));
        assert!(connect.as_mut().poll(&mut cx()).is_pending());
        let guard = state.write().await;
        drop(held);
        assert!(connect.as_mut().poll(&mut cx()).is_pending());

        // Release the second check with a writer queued behind it, the
        // shutdown snapshot's stand-in: the connect reads shutdown not
        // begun and dials, and the writer gets the guard before the
        // connect's completion asks for it.
        drop(guard);
        let mut shutdown = pin!(state.write());
        assert!(shutdown.as_mut().poll(&mut cx()).is_pending());
        assert!(connect.as_mut().poll(&mut cx()).is_pending());
        let Poll::Ready(mut st) = shutdown.as_mut().poll(&mut cx()) else {
            panic!("the shutdown stand-in never got the guard");
        };
        st.stopping = true;
        drop(st);

        assert_eq!(connect.await, Ok(ConnectAnswer::Refused));
        assert!(dialed(&peer).await, "the connect never dialed");
        assert_eq!(
            link.lock().await.state_word(),
            None,
            "the link it brought up is still up"
        );
        let st = state.read().await;
        let target = &st.targets["target-0"];
        assert!(target._router_handle.is_none(), "a router was registered");
        assert!(target._poll_handles.is_empty(), "a poll was registered");
    }

    /// A task standing in for a router or a poll, and a receiver that
    /// resolves once the task is cancelled and dropped.
    fn stand_in() -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (alive, gone) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _alive = alive;
            std::future::pending::<()>().await
        });
        (task, gone)
    }

    /// @test The shutdown snapshot closes connect admission and stops
    /// every target's router and polls under the write guard, then takes
    /// each link that is up from our side down, waiting for a link an
    /// upload holds without holding the guard, so requests are served
    /// meanwhile. A listening link ends; a link never brought up stays
    /// down; a connect asked for afterwards is refused.
    #[tokio::test]
    async fn shutdown_closes_admission_and_takes_every_link_down() {
        let server = Server::new(
            r#"
            [[targets]]
            name = "listener"
            host = "127.0.0.1"
            port = 9
            protocol = "ccsds-spp"
            carrier = "udp"
            listen_port = 0
            [[targets]]
            name = "apex"
            host = "127.0.0.1"
            port = 9
            "#,
        );
        let state = &server.state;
        let listener = server.link("target-0").await;
        listener.lock().await.connect("127.0.0.1", 9).await.unwrap();
        assert_eq!(
            listener.lock().await.state_word(),
            Some(StateWord::Listening)
        );
        let (router, router_gone) = stand_in();
        let (poll, poll_gone) = stand_in();
        {
            let mut st = state.write().await;
            let target = st.targets.get_mut("target-0").unwrap();
            target._router_handle = Some(router);
            target._poll_handles.push(poll);
        }

        let apex = server.link("target-1").await;
        let held = apex.lock().await;
        let mut shutdown = pin!(shut_down_targets(state));
        assert!(shutdown.as_mut().poll(&mut cx()).is_pending());
        {
            let st = state
                .try_read()
                .expect("the shutdown holds the state's guard while it waits for a link");
            assert!(st.stopping);
            assert!(st
                .targets
                .values()
                .all(|t| t._router_handle.is_none() && t._poll_handles.is_empty()));
        }
        drop(held);
        shutdown.await;

        for (task, gone) in [("router", router_gone), ("poll", poll_gone)] {
            let dropped = tokio::time::timeout(Duration::from_secs(1), gone).await;
            assert!(matches!(dropped, Ok(Err(_))), "the {task} was not stopped");
        }
        assert_eq!(listener.lock().await.state_word(), None);
        assert_eq!(apex.lock().await.state_word(), None);
        assert_eq!(
            do_connect_target(state, "target-1").await,
            Ok(ConnectAnswer::Refused)
        );
    }

    /* ---- Auth middleware ---- */

    const SECRET: &str = "a-test-secret-of-decent-length";

    /// Auth on, one apex target.
    fn auth_on() -> String {
        format!(
            r#"
            [auth]
            enabled = true
            secret = "{SECRET}"
            username = "ops"
            {APEX_TARGET}
            "#
        )
    }

    /// @test The auth middleware, through the server's router with auth
    /// on. The open routes answer without a credential: health, the
    /// bearer login, and signing in and out at the session door (the
    /// session's status is no door). A protected route refuses no
    /// credential, a malformed bearer token, a bearer token sent as a
    /// query ticket, an unknown session cookie, and a bad bearer token
    /// even beside a live cookie (the first credential present decides).
    /// A bearer token, a session cookie (writing only from the console's
    /// own origin) and a socket ticket on the telemetry socket pass, and
    /// each write is audited under its credential's subject.
    #[tokio::test]
    async fn the_auth_middleware_admits_each_credential_only_where_it_belongs() {
        let server = Server::new(&auth_on());
        let now = chrono::Utc::now().timestamp();
        let token = crate::core::auth::mint_token("api-client", SECRET, now).unwrap();
        let bearer = format!("Bearer {token}");
        let ticket = crate::core::auth::mint_ws_ticket("api-client", SECRET, now).unwrap();
        let sessions = server.state.read().await.sessions.clone();
        let (id, _) = sessions.create("ops", now_ms()).unwrap();
        let cookie = format!("{}={id}", crate::core::session::COOKIE_NAME);
        let upgrade = [
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "AAAAAAAAAAAAAAAAAAAAAA=="),
        ];
        let socket =
            |credential: &str| format!("/api/targets/target-0/telemetry/live?ticket={credential}");
        let nobody = || Some(json!({ "username": "nobody", "password": "x" }));
        let disconnect = "/api/targets/target-0/disconnect";
        let (get, post) = (Method::GET, Method::POST);

        type Row = (
            &'static str,
            axum::http::Request<Body>,
            StatusCode,
            &'static str,
        );
        let rows: Vec<Row> = vec![
            (
                "health is open",
                request(get.clone(), "/api/health", &[], None),
                StatusCode::OK,
                "\"status\"",
            ),
            (
                "the bearer login is open",
                request(post.clone(), "/api/auth/login", &[], nobody()),
                StatusCode::UNAUTHORIZED,
                "Invalid credentials",
            ),
            (
                "signing in needs no session",
                request(post.clone(), "/api/auth/session", &[], nobody()),
                StatusCode::UNAUTHORIZED,
                "invalid credentials",
            ),
            (
                "signing out needs no session",
                request(Method::DELETE, "/api/auth/session", &[], None),
                StatusCode::NO_CONTENT,
                "",
            ),
            (
                "the session's status is no door",
                request(get.clone(), "/api/auth/session", &[], None),
                StatusCode::UNAUTHORIZED,
                "missing token or session",
            ),
            (
                "no credential",
                request(get.clone(), "/api/targets", &[], None),
                StatusCode::UNAUTHORIZED,
                "missing token or session",
            ),
            (
                "a malformed bearer token",
                request(
                    get.clone(),
                    "/api/targets",
                    &[("authorization", "Bearer not-a-token")],
                    None,
                ),
                StatusCode::UNAUTHORIZED,
                "invalid token",
            ),
            (
                "a bearer token as a query ticket",
                request(get.clone(), &socket(&token), &upgrade, None),
                StatusCode::UNAUTHORIZED,
                "query credentials must be ws tickets",
            ),
            (
                "an unknown session cookie",
                request(
                    get.clone(),
                    "/api/targets",
                    &[("cookie", "zenith_session=unknown")],
                    None,
                ),
                StatusCode::UNAUTHORIZED,
                "session ended or unknown",
            ),
            (
                "a bad bearer token beside a live cookie",
                request(
                    get.clone(),
                    "/api/targets",
                    &[("authorization", "Bearer not-a-token"), ("cookie", &cookie)],
                    None,
                ),
                StatusCode::UNAUTHORIZED,
                "invalid token",
            ),
            (
                "a bearer token",
                request(
                    get.clone(),
                    "/api/targets",
                    &[("authorization", &bearer)],
                    None,
                ),
                StatusCode::OK,
                "\"targets\"",
            ),
            (
                "a session cookie",
                request(get.clone(), "/api/targets", &[("cookie", &cookie)], None),
                StatusCode::OK,
                "\"targets\"",
            ),
            (
                "a session write from another site",
                request(
                    post.clone(),
                    disconnect,
                    &[("cookie", &cookie), ("sec-fetch-site", "cross-site")],
                    None,
                ),
                StatusCode::FORBIDDEN,
                "cross-origin request with a session cookie refused",
            ),
            (
                "a session write from the console",
                request(
                    post.clone(),
                    disconnect,
                    &[("cookie", &cookie), ("sec-fetch-site", "same-origin")],
                    None,
                ),
                StatusCode::OK,
                "disconnected",
            ),
            (
                "a bearer write",
                request(
                    post.clone(),
                    disconnect,
                    &[("authorization", &bearer)],
                    None,
                ),
                StatusCode::OK,
                "disconnected",
            ),
            (
                "a ticket on the telemetry socket",
                request(get.clone(), &socket(&ticket), &upgrade, None),
                StatusCode::UPGRADE_REQUIRED,
                "",
            ),
        ];
        for (label, request, status, fragment) in rows {
            let (got, body) = server.call(request).await;
            assert_eq!(got, status, "{label}: {body}");
            assert!(body.contains(fragment), "{label}: {body}");
        }

        let audited: Vec<(String, String, String)> = server
            .audit()
            .into_iter()
            .map(|e| (e.actor, e.action, e.status))
            .collect();
        let expected = [
            ("(unknown user)", "sign_in", "err: invalid credentials"),
            ("(unknown user)", "sign_in", "err: invalid credentials"),
            ("ops", "disconnect_target", "ok"),
            ("api-client", "disconnect_target", "ok"),
        ]
        .map(|(actor, action, status)| (actor.into(), action.into(), status.into()));
        assert_eq!(audited, expected);
    }

    /// @test With auth off the middleware passes every request as the
    /// anonymous operator: no credential is asked for, and a write is
    /// audited under "operator".
    #[tokio::test]
    async fn with_auth_off_every_request_is_the_operator() {
        let server = Server::new(APEX_TARGET);
        let (status, body) = server
            .call(request(
                Method::POST,
                "/api/targets/target-0/disconnect",
                &[],
                None,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let audit = server.audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].actor, "operator");
    }

    /* ---- Byte order, passwords, uploads, audit, tickets, writers, trim ---- */

    /// A component with one tunable block and the given byte-order stamp.
    fn block_dicts(
        byte_order: Option<&str>,
        fields: serde_json::Value,
        size: usize,
    ) -> StructDictionary {
        let dict: ComponentDict = serde_json::from_value(json!({
            "component": "Block",
            "byte_order": byte_order,
            "structs": { "BlockParams": {
                "category": "TUNABLE_PARAM", "size": size, "fields": fields,
            }},
        }))
        .unwrap();
        StructDictionary {
            components: HashMap::from([("Block".to_string(), dict)]),
        }
    }

    /// @test A tunable value is written in its dictionary's byte order:
    /// a big-endian field gets its most significant byte first, array
    /// elements each in turn, and every scalar a block decodes to is
    /// written back to the block's own bytes in either order, so an
    /// unedited Apply rewrites what was read.
    #[test]
    fn tunable_values_encode_in_the_dictionary_byte_order() {
        let mut buf = [0u8; 2];
        encode_field(&mut buf, &field("uint", 2), &json!(0x1234), true).unwrap();
        assert_eq!(buf, [0x12, 0x34], "big-endian uint16");
        let pair: FieldDef = serde_json::from_value(json!({
            "name": "f", "type": "array", "offset": 0, "size": 4,
            "element_type": "int", "dims": [2],
        }))
        .unwrap();
        let mut buf = [0u8; 4];
        encode_field(&mut buf, &pair, &json!([-2, 0x0102]), true).unwrap();
        assert_eq!(buf, [0xFF, 0xFE, 0x01, 0x02], "big-endian int16 elements");

        let fields = json!([
            { "name": "wide", "type": "int", "offset": 0, "size": 8 },
            { "name": "flag", "type": "bool", "offset": 8, "size": 1 },
            { "name": "tiny", "type": "int", "offset": 9, "size": 1 },
            { "name": "short", "type": "uint", "offset": 10, "size": 2 },
            { "name": "word", "type": "int", "offset": 12, "size": 4 },
            { "name": "ratio", "type": "float", "offset": 16, "size": 4 },
            { "name": "precise", "type": "double", "offset": 20, "size": 8 },
        ]);
        // The block's values, each field's bytes in the given order.
        let block = |big_endian: bool| {
            let mut out = Vec::new();
            for le in [
                &(-2i64).to_le_bytes()[..],
                &[1],
                &(-5i8).to_le_bytes(),
                &0x1234u16.to_le_bytes(),
                &(-100_000i32).to_le_bytes(),
                &1.5f32.to_le_bytes(),
                &(-0.25f64).to_le_bytes(),
            ] {
                let mut bytes = le.to_vec();
                if big_endian {
                    bytes.reverse();
                }
                out.extend(bytes);
            }
            out
        };
        for (stamp, big_endian) in [(None, false), (Some("le"), false), (Some("be"), true)] {
            let bytes = block(big_endian);
            let dicts = block_dicts(stamp, fields.clone(), 28);
            let decoded = dicts
                .decode_payload("Block", "BlockParams", &bytes)
                .unwrap();
            let mut written = vec![0u8; 28];
            for field in &dicts.components["Block"].structs["BlockParams"].fields {
                encode_field(&mut written, field, &decoded[&field.name], big_endian)
                    .unwrap_or_else(|e| panic!("{stamp:?} {}: {e}", field.name));
            }
            assert_eq!(written, bytes, "{stamp:?}: {decoded}");
        }
    }

    /// @test `--hash-password` hashes the password as typed, only the
    /// line ending removed, since sign-in compares the password exactly
    /// as it is sent: edge spaces stay, a CRLF or no line ending at all
    /// reads the same, and an empty or blank line holds no password.
    #[test]
    fn a_hashed_password_keeps_its_edge_spaces() {
        assert_eq!(
            password_from_line("  pass phrase \n"),
            Some("  pass phrase ")
        );
        assert_eq!(password_from_line("secret\r\n"), Some("secret"));
        assert_eq!(password_from_line("secret"), Some("secret"));
        assert_eq!(password_from_line("\n"), None);
        assert_eq!(password_from_line(" \t \n"), None);
    }

    /// Dictionaries describing a sequence plan, the vehicle's shape in
    /// miniature: a 32-byte plan of an 8-byte header and two 12-byte
    /// steps, each with its delay and timeout in cycles.
    fn sequence_dicts() -> StructDictionary {
        let dict: ComponentDict = serde_json::from_value(json!({
            "component": "Action",
            "structs": {
                "StandaloneSequenceTprm": { "category": "PROTOCOL", "size": 32, "fields": [
                    { "name": "sequenceId", "type": "uint", "offset": 0, "size": 2 },
                    { "name": "stepCount", "type": "uint", "offset": 4, "size": 1 },
                    { "name": "type", "type": "uint", "offset": 6, "size": 1 },
                    { "name": "steps", "type": "array", "offset": 8, "size": 24 },
                ]},
                "StandaloneStepTprm": { "category": "STRUCT", "size": 12, "fields": [
                    { "name": "actionType", "type": "uint", "offset": 0, "size": 1 },
                    { "name": "delayCycles", "type": "uint", "offset": 4, "size": 4 },
                    { "name": "timeoutCycles", "type": "uint", "offset": 8, "size": 4 },
                ]},
            },
        }))
        .unwrap();
        StructDictionary {
            components: HashMap::from([("Action".to_string(), dict)]),
        }
    }

    /// @test An upload is checked as an RTS sequence only when it goes
    /// into a directory named rts: a plan the vehicle would refuse at
    /// load is refused (422) on its way to bank_a/rts/ or rts/, while
    /// the same bytes to parts/, to shirts/ or to a file named rts go on
    /// to the link.
    #[tokio::test]
    async fn only_uploads_into_an_rts_directory_are_checked_as_sequences() {
        use base64::Engine;
        let server = Server::with_dicts(APEX_TARGET, sequence_dicts());
        // One RTS step whose timeout expires within its own delay.
        let mut plan = vec![0u8; 32];
        plan[4] = 1;
        plan[12..16].copy_from_slice(&30u32.to_le_bytes());
        plan[16..20].copy_from_slice(&30u32.to_le_bytes());
        let plan = base64::engine::general_purpose::STANDARD.encode(&plan);
        let upload = |path: &str| json!({ "remote_path": path, "content_base64": plan });
        let uri = "/api/targets/target-0/upload";

        for path in ["bank_a/rts/seq1.bin", "rts/seq1.bin"] {
            let (status, body) = server.post(uri, upload(path)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {body}");
            assert!(
                body.contains("TIMEOUT_SHORTER_THAN_DELAY"),
                "{path}: {body}"
            );
        }
        for path in ["parts/blob.bin", "bank_a/shirts/seq1.bin", "bank_a/rts"] {
            let (status, body) = server.post(uri, upload(path)).await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::BAD_GATEWAY, "Upload failed: not connected"),
                "{path}"
            );
        }
    }

    /// @test Downsampling rewrites stored history, so it is audited like
    /// a trim: under the actor, with the rows it converted.
    #[tokio::test]
    async fn downsampling_is_audited() {
        let server = Server::new(APEX_TARGET);
        // Two samples in one minute bucket, two hours old.
        let minute = (now_ms() - 2 * 3_600_000) / 60_000 * 60_000;
        server
            .db
            .insert_batch(&[
                sample("A.x", minute, 1.0),
                sample("A.x", minute + 1_000, 3.0),
            ])
            .unwrap();

        let (status, body) = server
            .call(request(
                Method::POST,
                "/api/telemetry/downsample",
                &[],
                None,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let audit = server.audit();
        assert_eq!(audit.len(), 1, "downsampling wrote no audit row");
        assert_eq!(
            (
                audit[0].actor.as_str(),
                audit[0].action.as_str(),
                audit[0].status.as_str()
            ),
            ("operator", "downsample_telemetry", "ok")
        );
        assert_eq!(
            audit[0].detail.as_deref(),
            Some("samples_before=2 samples_after=1 removed=1")
        );
    }

    /// @test With auth on the bearer login is audited as signing in at
    /// the session door is: a refusal under "(unknown user)" for a name
    /// that is not the configured one, never the name submitted, and a
    /// success under the user, both marked as the bearer login.
    #[tokio::test]
    async fn the_bearer_login_is_audited_like_a_sign_in() {
        let hash = crate::core::auth::hash_password("pass phrase").unwrap();
        let server = Server::new(&format!(
            r#"
            [auth]
            enabled = true
            secret = "{SECRET}"
            username = "ops"
            password_hash = "{hash}"
            {APEX_TARGET}
            "#
        ));
        let login = |user: &str, password: &str| {
            request(
                Method::POST,
                "/api/auth/login",
                &[],
                Some(json!({ "username": user, "password": password })),
            )
        };

        let (status, body) = server.call(login("hunter2", "x")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let (status, body) = server.call(login("ops", "pass phrase")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"token\""), "{body}");

        let audited: Vec<(String, String, Option<String>, String)> = server
            .audit()
            .into_iter()
            .map(|e| (e.actor, e.action, e.detail, e.status))
            .collect();
        let bearer = Some("bearer token".to_string());
        assert_eq!(
            audited,
            [
                (
                    "(unknown user)".to_string(),
                    "sign_in".to_string(),
                    bearer.clone(),
                    "err: invalid credentials".to_string()
                ),
                (
                    "ops".to_string(),
                    "sign_in".to_string(),
                    bearer,
                    "ok".to_string()
                ),
            ]
        );
    }

    /// @test A socket ticket opens the telemetry socket and nothing else:
    /// on another route's query string, for a read or a write, or in any
    /// route's Authorization header, it is refused, and nothing is done;
    /// on the socket's query string it passes.
    #[tokio::test]
    async fn a_socket_ticket_opens_only_the_telemetry_socket() {
        let server = Server::new(&auth_on());
        let now = chrono::Utc::now().timestamp();
        let ticket = crate::core::auth::mint_ws_ticket("api-client", SECRET, now).unwrap();
        let as_bearer = format!("Bearer {ticket}");
        let upgrade = [
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "AAAAAAAAAAAAAAAAAAAAAA=="),
        ];
        let mut upgrade_as_bearer = upgrade.to_vec();
        upgrade_as_bearer.push(("authorization", as_bearer.as_str()));
        let socket = format!("/api/targets/target-0/telemetry/live?ticket={ticket}");

        for (label, request) in [
            (
                "a read",
                request(
                    Method::GET,
                    &format!("/api/targets?ticket={ticket}"),
                    &[],
                    None,
                ),
            ),
            (
                "a write",
                request(
                    Method::POST,
                    &format!("/api/targets/target-0/disconnect?ticket={ticket}"),
                    &[],
                    None,
                ),
            ),
            (
                "a header",
                request(
                    Method::GET,
                    "/api/targets",
                    &[("authorization", &as_bearer)],
                    None,
                ),
            ),
            (
                "a header on the socket",
                request(
                    Method::GET,
                    "/api/targets/target-0/telemetry/live",
                    &upgrade_as_bearer,
                    None,
                ),
            ),
        ] {
            let (status, body) = server.call(request).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{label}: {body}");
        }
        assert!(
            server.audit().is_empty(),
            "a refused ticket's write was carried out"
        );

        let (status, body) = server
            .call(request(Method::GET, &socket, &upgrade, None))
            .await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "{body}");
    }

    /// @test Shutdown waits for each target's sample writer to write what
    /// it holds: the samples broadcast before the stop signal are in the
    /// database when shutdown returns, though another connection held
    /// the database's write lock when it began.
    #[tokio::test]
    async fn shutdown_waits_for_the_sample_writers() {
        let server = Server::new(APEX_TARGET);
        let blocker = rusqlite::Connection::open(server.dir.path().join("zenith.db")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        {
            let mut st = server.state.write().await;
            let target = st.targets.get_mut("target-0").unwrap();
            target.sample_writer = Some(spawn_sample_writer(
                "target-0".to_string(),
                server.db.clone(),
                target.sample_tx.subscribe(),
                target.metrics.clone(),
            ));
            for i in 0..10u64 {
                target
                    .sample_tx
                    .send(sample("A.x", 1_000 + i, i as f64))
                    .unwrap();
            }
        }
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            blocker.execute_batch("ROLLBACK").unwrap();
        });

        shut_down_targets(&server.state).await;
        assert_eq!(
            server.db.count().unwrap(),
            10,
            "shutdown returned before the writer wrote its samples"
        );
        release.join().unwrap();
    }

    /// @test A trim deletes the number of samples asked for, oldest
    /// first: none for a count of 0, and a quarter of the target's
    /// samples only when no count is given.
    #[tokio::test]
    async fn a_trim_deletes_what_it_is_asked_for() {
        let server = Server::new(APEX_TARGET);
        let samples: Vec<TelemetrySample> = (0..8u64)
            .map(|i| sample("A.x", 1_000 + i, i as f64))
            .collect();
        server.db.insert_batch(&samples).unwrap();
        let uri = "/api/targets/target-0/storage/trim";

        for (body, deleted, left) in [
            (json!({ "count": 0 }), 0, 8),
            (json!({}), 2, 6),
            (json!({ "count": 3 }), 3, 3),
        ] {
            let (status, answer) = server.post(uri, body.clone()).await;
            assert_eq!(status, StatusCode::OK, "{body}: {answer}");
            let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
            assert_eq!(answer["deleted"], json!(deleted), "{body}");
            assert_eq!(server.db.count().unwrap(), left, "{body}");
        }
    }

    /* ---- What the read returns, the update takes back ---- */

    /// @test Every field a tunable block reads takes the read's value
    /// back and writes the block's own bytes: numbers as numbers, and a
    /// field the read returns as hex (a string, an array, a shape it
    /// reads no number from, a float that is not finite) as that hex,
    /// so an unedited Apply rewrites the block unchanged in either byte
    /// order, junk after a string's NUL included. Edits still go in as
    /// values: text for a string, elements for an array.
    #[test]
    fn every_field_takes_back_what_the_read_returned() {
        let fields = json!([
            { "name": "count", "type": "uint", "offset": 0, "size": 2 },
            { "name": "tag", "type": "string", "offset": 2, "size": 6 },
            { "name": "taps", "type": "array", "offset": 8, "size": 8,
              "element_type": "float", "dims": [2] },
            { "name": "names", "type": "array", "offset": 16, "size": 8,
              "element_type": "string", "dims": [2] },
            { "name": "odd", "type": "uint", "offset": 24, "size": 3 },
            { "name": "blob", "type": "nested", "offset": 27, "size": 5 },
            { "name": "gain", "type": "float", "offset": 32, "size": 4 },
            { "name": "scale", "type": "double", "offset": 36, "size": 8 },
        ]);
        // The block in the given byte order: each scalar's bytes, and
        // each element's, ordered as a big-endian writer would.
        let block = |big_endian: bool| {
            let order = |le: &[u8]| {
                let mut bytes = le.to_vec();
                if big_endian {
                    bytes.reverse();
                }
                bytes
            };
            let mut out = order(&0x0102u16.to_le_bytes());
            out.extend(b"ab\0\x7f\x01\x02");
            out.extend(order(&1.5f32.to_le_bytes()));
            out.extend(order(&(-2.0f32).to_le_bytes()));
            out.extend(b"x\0\0\0yz\0\0");
            out.extend([1, 2, 3, 9, 8, 7, 6, 5]);
            out.extend(order(&f32::NAN.to_le_bytes()));
            out.extend(order(&f64::INFINITY.to_le_bytes()));
            out
        };

        let mut refused = Vec::new();
        for (stamp, big_endian) in [(None, false), (Some("le"), false), (Some("be"), true)] {
            let bytes = block(big_endian);
            assert_eq!(bytes.len(), 44);
            let dicts = block_dicts(stamp, fields.clone(), 44);
            let decoded = dicts
                .decode_payload("Block", "BlockParams", &bytes)
                .unwrap();
            let mut written = vec![0u8; 44];
            for field in &dicts.components["Block"].structs["BlockParams"].fields {
                let value = &decoded[&field.name];
                if let Err(e) = encode_field(&mut written, field, value, big_endian) {
                    refused.push(format!("{stamp:?} {}: {e}", field.name));
                }
            }
            if refused.is_empty() {
                assert_eq!(written, bytes, "{stamp:?}: {decoded}");
            }
        }
        assert!(refused.is_empty(), "read values refused: {refused:#?}");

        let dicts = block_dicts(None, fields, 44);
        let fields = &dicts.components["Block"].structs["BlockParams"].fields;
        let mut edited = block(false);
        encode_field(&mut edited, &fields[1], &json!("new"), false).unwrap();
        assert_eq!(&edited[2..8], b"new\0\0\0");
        encode_field(&mut edited, &fields[2], &json!([0.5, 4.0]), false).unwrap();
        assert_eq!(&edited[8..12], 0.5f32.to_le_bytes());
        assert_eq!(&edited[12..16], 4.0f32.to_le_bytes());
        for (field, wrong) in [
            (&fields[1], json!("0102030405")),
            (&fields[2], json!("abcd")),
            (&fields[4], json!("01")),
        ] {
            assert!(
                encode_field(&mut edited, field, &wrong, false).is_err(),
                "{}: {wrong} taken",
                field.name
            );
        }
    }

    /// @test An unedited Apply of the shipped SpecProtoMax tunable block
    /// (float, string and integer arrays, a bounded string, scalars of
    /// every width) is accepted: every field goes back as the read
    /// returned it, and the update goes on to the upload, which the
    /// unconnected link refuses.
    #[tokio::test]
    async fn an_unedited_apply_of_the_shipped_spec_block_goes_to_the_upload() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../targets/sil-ops-demo/structs/SpecProtoMax.json"
        ))
        .unwrap();
        let dict: ComponentDict = serde_json::from_str(&text).unwrap();
        let dicts = StructDictionary {
            components: HashMap::from([(dict.component.clone(), dict)]),
        };
        let mut raw = vec![0u8; 80];
        raw[0..8].copy_from_slice(&2.5f64.to_le_bytes());
        raw[24..28].copy_from_slice(&0.5f32.to_le_bytes());
        for (i, tap) in [1.5f32, -2.0, 0.25].iter().enumerate() {
            raw[36 + 4 * i..40 + 4 * i].copy_from_slice(&tap.to_le_bytes());
        }
        raw[52..55].copy_from_slice(b"abc");
        raw[60..62].copy_from_slice(b"ab");
        raw[64] = b'c';
        raw[68..72].copy_from_slice(&[1, 2, 3, 4]);
        raw[72] = 50;
        raw[73] = (-3i8) as u8;
        raw[74] = 1;
        let fields = dicts
            .decode_payload("SpecProtoMax", "SpecProtoMaxTunableParams", &raw)
            .unwrap();

        let server = Server::with_dicts(APEX_TARGET, dicts);
        let (status, body) = server
            .post(
                "/api/targets/target-0/params/00D000/update",
                json!({ "fields": fields, "raw_hex": hex::encode(&raw) }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        let audit = server.audit();
        assert_eq!(
            (audit[0].action.as_str(), audit[0].status.as_str()),
            ("update_tprm", "err: not connected")
        );
    }
}
