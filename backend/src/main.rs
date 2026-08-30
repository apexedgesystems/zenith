//! Zenith - Real-time operations interface
//!
//! REST API + WebSocket server for commanding and monitoring Apex CSF applications.

mod config;
mod core;
mod protocol;
mod storage;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Path, Request, State,
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex, RwLock};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::{FifoStrategy, ServerConfig};
use crate::core::aproto_client::{AprotoClient, PushTelemetryPacket};
use crate::core::config_manager::StructDictionary;
use crate::core::metrics::TargetMetrics;
use crate::core::telemetry::{self, TelemetrySample};
use crate::core::tprm;
use crate::storage::telemetry_db::TelemetryDb;

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
}

/// Authenticated principal for audit attribution, inserted by the auth
/// middleware on every request: the token's sub when auth is on, the
/// anonymous "operator" when it is off.
#[derive(Clone)]
struct Actor(Arc<str>);

/* ----------------------------- App State ----------------------------- */

struct TargetState {
    config: config::TargetSection,
    client: Arc<Mutex<AprotoClient>>,
    /// Lock-free view of the client's connection flag. Status endpoints
    /// read this instead of locking `client`, which a file transfer can
    /// hold for the duration of an upload.
    connected: Arc<std::sync::atomic::AtomicBool>,
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
    /// Telemetry display config (plot layouts from target config)
    telemetry_config: Option<Arc<crate::core::config_manager::TelemetryConfig>>,
    /// Command definitions (quick commands + per-component commands)
    commands_config: Option<Arc<crate::core::config_manager::CommandConfig>>,
    _router_handle: Option<tokio::task::JoinHandle<()>>,
}

struct SharedState {
    #[allow(dead_code)]
    config: ServerConfig,
    targets: HashMap<String, TargetState>,
    db: Arc<TelemetryDb>,
    struct_dicts: Arc<StructDictionary>,
    /// Per-IP token bucket for command rate limiting. Engaged only when
    /// auth is enabled (so dev mode is unconstrained).
    rate_limiter: Arc<RateLimiter>,
    /// Live storage-pressure numbers shared with /api/telemetry/stats.
    storage_vitals: Arc<StorageVitals>,
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
    /// Command-surface capabilities the target's dictionaries declare
    /// (e.g. "readback"). Empty for older dictionary sets.
    capabilities: Vec<String>,
}

#[derive(Serialize)]
struct CommandResponse {
    status: u8,
    status_name: String,
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
    /// Component fullUid (hex string like "0x00D000" or decimal)
    full_uid: String,
    /// Opcode (hex string like "0x0100" or decimal)
    opcode: String,
    /// Payload as hex string (optional, e.g., "0102030405")
    #[serde(default)]
    payload_hex: String,
}

fn to_cmd_response(resp: crate::protocol::aproto::AckResponse) -> CommandResponse {
    CommandResponse {
        status: resp.status,
        status_name: resp.status_name,
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
/// operation). Always 200 so the compose healthcheck validates the
/// server is serving; the body carries the verdict.
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

/// Snapshot a target's client handle and the audit DB under a short
/// read guard. Callers do target I/O only AFTER this returns: tokio's
/// RwLock is writer-preferring, so a guard held across an APROTO round
/// trip (worst case a multi-thousand-RTT file upload) parks the next
/// writer and stalls every request behind it.
async fn target_client(
    state: &AppState,
    id: &str,
) -> Result<(Arc<Mutex<AprotoClient>>, Arc<TelemetryDb>), (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    Ok((target.client.clone(), st.db.clone()))
}

/// Connect a target's client and attach its telemetry router.
/// Returns Ok(true) on a fresh connect, Ok(false) if already connected.
///
/// The connected check and the TCP connect run under the client mutex
/// (not the state guard), which gives the same TOCTOU protection the
/// old hold-the-write-lock version had: a concurrent connect serializes
/// on the mutex and sees is_connected() == true.
async fn do_connect_target(state: &AppState, id: &str) -> Result<bool, (StatusCode, String)> {
    let (client, host, port) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            target.client.clone(),
            target.config.host.clone(),
            target.config.port,
        )
    };

    {
        let mut cli = client.lock().await;
        if cli.is_connected() {
            return Ok(false);
        }
        cli.connect(&host, port)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("Connect failed: {}", e)))?;
    }

    // Attach the telemetry router under a short write guard. Replacing
    // the handle aborts any previous router so a reconnect can't leave
    // two subscribers decoding the same push stream.
    let mut st = state.write().await;
    if let Some(target) = st.targets.get_mut(id) {
        let push_rx = target.push_tlm_tx.subscribe();
        let handle = telemetry::spawn_router(
            id.to_string(),
            push_rx,
            target.sample_tx.clone(),
            target.struct_dicts.clone(),
            target.manifest.clone(),
            target.metrics.clone(),
        );
        if let Some(old) = target._router_handle.replace(handle) {
            old.abort();
        }
    }
    Ok(true)
}

async fn list_targets(State(state): State<AppState>) -> Json<serde_json::Value> {
    let st = state.read().await;
    let mut targets: Vec<TargetInfo> = Vec::new();
    for (id, t) in &st.targets {
        // Lock-free flag read: locking the client here would make the
        // 3s frontend poll stall behind any in-flight file transfer.
        let connected = t.connected.load(std::sync::atomic::Ordering::Acquire);
        targets.push(TargetInfo {
            id: id.clone(),
            name: t.config.name.clone(),
            host: t.config.host.clone(),
            port: t.config.port,
            connected,
            capabilities: t.struct_dicts.capabilities(),
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
    let (host, port, db_for_audit) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            target.config.host.clone(),
            target.config.port,
            st.db.clone(),
        )
    };
    let ip_str = addr.ip().to_string();

    match do_connect_target(&state, &id).await {
        Ok(false) => Ok(Json(
            serde_json::json!({"status": "already_connected", "target": id}),
        )),
        Ok(true) => {
            record_audit(
                &db_for_audit,
                &actor.0,
                "connect_target",
                Some(&id),
                Some(&format!("{}:{}", host, port)),
                "ok",
                Some(&ip_str),
            );
            Ok(Json(
                serde_json::json!({"status": "connected", "target": id}),
            ))
        }
        Err((code, err_msg)) => {
            record_audit(
                &db_for_audit,
                &actor.0,
                "connect_target",
                Some(&id),
                Some(&format!("{}:{}", host, port)),
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            Err((code, err_msg))
        }
    }
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
    let mut cli = client.lock().await;
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
    let mut cli = client.lock().await;
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
    let uid_clean = uid_str.trim_start_matches("0x").trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", uid_str),
        )
    })?;

    let (client, _) = target_client(&state, &id).await?;
    let mut cli = client.lock().await;
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

    let detail = format!("path={} bytes={}", body.remote_path, data.len());
    let ip_str = addr.ip().to_string();

    let mut cli = client.lock().await;
    let result = cli.upload_file(&body.remote_path, &data).await;
    drop(cli);

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

    let mut cli = client.lock().await;
    let result = cli.restart_executive().await;
    drop(cli);

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

    let uid_clean = uid_str.trim_start_matches("0x").trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", uid_str),
        )
    })?;

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

    let mut cli = client.lock().await;
    let result = cli
        .swap_library(
            full_uid,
            &body.component_name,
            body.instance_index,
            &body.inactive_bank,
            &data,
        )
        .await;
    drop(cli);

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

    // Parse fullUid
    let uid_clean = body
        .full_uid
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16)
        .or_else(|_| body.full_uid.parse::<u32>())
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid fullUid: {}", body.full_uid),
            )
        })?;

    // Parse opcode
    let opc_clean = body
        .opcode
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let opcode = u16::from_str_radix(opc_clean, 16)
        .or_else(|_| body.opcode.parse::<u16>())
        .map_err(|_| {
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

    let mut cli = client.lock().await;
    let result = cli.send_command(full_uid, opcode, &payload).await;
    drop(cli);

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

    // Try name-based match first (handles struct alignment padding differences)
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

    let uid_clean = uid_str.trim_start_matches("0x").trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", uid_str),
        )
    })?;

    // INSPECT TUNABLE_PARAM (category=1)
    let mut cli = client.lock().await;
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

    let uid_clean = uid_str.trim_start_matches("0x").trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", uid_str),
        )
    })?;

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

        // Find the two TUNABLE_PARAM structs (header = smaller, entry = larger)
        let mut tunable_structs: Vec<&crate::core::config_manager::StructDef> = Vec::new();
        for dict in dicts.components.values() {
            if let Some(hint) = comp_name {
                let dn = dict.component.to_lowercase();
                let hn = hint.to_lowercase();
                if dn != hn && !dn.contains(&hn) && !hn.contains(&dn) {
                    continue;
                }
            }
            for sdef in dict.structs.values() {
                if sdef.category == "TUNABLE_PARAM" && !sdef.fields.is_empty() && sdef.size > 0 {
                    tunable_structs.push(sdef);
                }
            }
        }
        tunable_structs.sort_by_key(|s| s.size);

        if tunable_structs.len() < 2 {
            return Err((
                StatusCode::BAD_REQUEST,
                "Cannot find header+entry struct pair".to_string(),
            ));
        }

        let hdr_struct = tunable_structs[0]; // smaller = header
        let ent_struct = tunable_structs[1]; // larger = entry
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
                if !encode_field(&mut buf, field, value) {
                    unencodable.push(field.name.clone());
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
                    if !encode_field(&mut buf, &adjusted_field, value) {
                        unencodable.push(format!("entry[{}].{}", i, field.name));
                    }
                }
            }
        }
        if !unencodable.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Cannot encode edited field(s): {}", unencodable.join(", ")),
            ));
        }

        // Variable-length layout hash (recipe confirmed by the apex
        // side, relay 2026-08-16): header leaves then each entry's
        // leaves in order, no container markers. The hash is
        // entry-count-dependent by design -- the count is part of the
        // layout the vehicle will read.
        let hash = tprm::layout_hash(
            dict_leaf_specs(&hdr_struct.fields).into_iter().chain(
                std::iter::repeat_with(|| dict_leaf_specs(&ent_struct.fields))
                    .take(entries.len())
                    .flatten(),
            ),
        );

        (buf, hash)
    } else {
        // Flat: single struct
        let mut dict_hash: Option<u32> = None;
        if let Some((_comp, _sname, sdef)) =
            find_tunable_struct(&dicts, original_binary.len(), full_uid, manifest.as_deref())
        {
            struct_size = sdef.size;
            struct_fields = sdef.fields.clone();
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
        let mut unencodable: Vec<&str> = Vec::new();
        for field in &struct_fields {
            if let Some(value) = body.fields.get(&field.name) {
                if !encode_field(&mut buf, field, value) {
                    unencodable.push(&field.name);
                }
            }
        }
        if !unencodable.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "Cannot encode edited field(s): {} -- unsupported type/size or wrong value type",
                    unencodable.join(", ")
                ),
            ));
        }
        // Producer-stated hash when the dictionary exports one -- that
        // is the value the vehicle verifies. Recompute only for
        // dictionaries predating the export, with a warning: the
        // flattened-fields recompute predates string leaves and cannot
        // be trusted once those appear.
        let hash = dict_hash.unwrap_or_else(|| {
            tracing::warn!(
                "no layout_hash in dictionary for uid 0x{:06X}; recomputing from flattened fields",
                full_uid
            );
            tprm::layout_hash(dict_leaf_specs(&struct_fields))
        });
        (buf, hash)
    };

    // Stamp the v3 prelude: the vehicle verifies magic, version, size,
    // target uid, layout hash, and body CRC before the payload reaches
    // a component, and rejects unstamped uploads. The layout hash came
    // from the same dict fields that encoded the body (dict
    // declaration order is template emission order). The dicts are
    // flattened, so a template using explicitly-typed nested-struct
    // containers would hash differently on the vehicle -- which
    // rejects with a distinct layout-hash fault rather than
    // misloading; the dict generator exporting the hash is accepted
    // apex-side work (relay thread).
    let stamped = tprm::stamp_v3(full_uid, payload_layout_hash, &binary)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("TPRM stamp failed: {}", e)))?;

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
    let result = if readback {
        staged_update_flow(&client, full_uid, &stamped).await
    } else {
        let mut cli = client.lock().await;
        let r = cli.update_tprm(full_uid, &stamped).await;
        drop(cli);
        r.map(|resp| (resp, serde_json::Value::Null))
    };

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
            if resp.status == 5 && !resp.extra.is_empty() {
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
/// or the failing step's response with the report explaining where
/// and why the pipeline stopped (active bytes untouched).
async fn staged_update_flow(
    client: &Arc<Mutex<AprotoClient>>,
    full_uid: u32,
    stamped: &[u8],
) -> Result<
    (crate::protocol::aproto::AckResponse, serde_json::Value),
    crate::core::aproto_client::ClientError,
> {
    use crate::protocol::aproto;

    // Our own prelude is the intent the vehicle must echo back.
    let (sent_prelude, _) = tprm::parse_v3(stamped).expect("stamp_v3 output always parses");

    let mut cli = client.lock().await;

    // 1. Stage (no reload).
    let up = cli.stage_tprm(full_uid, stamped).await?;
    if up.status != 0 {
        return Ok((
            up,
            serde_json::json!({ "step": "stage", "outcome": "upload refused" }),
        ));
    }

    // 2. Digest cross-check: the staged bank must declare exactly the
    // identity we sent (uid, layout hash, payload CRC).
    let rb = cli
        .send_command(0x000000, tprm::OP_READBACK_TPRM, &[])
        .await?;
    let row = match tprm::parse_readback_page(&rb.extra) {
        Ok(page) => page.rows.into_iter().find(|r| r.full_uid == full_uid),
        Err(_) => None,
    };
    let row_ok = row.as_ref().is_some_and(|r| {
        r.layout_hash == sent_prelude.layout_hash
            && r.payload_crc == sent_prelude.payload_crc
            && r.verdict == 0
    });
    if !row_ok {
        let report = serde_json::json!({
            "step": "readback",
            "outcome": "staged bytes do not match intent",
            "sent_layout_hash": format!("0x{:08X}", sent_prelude.layout_hash),
            "sent_payload_crc": format!("0x{:08X}", sent_prelude.payload_crc),
            "staged_row": row,
        });
        // Shape the refusal as a NAK-like response so callers get one
        // uniform (response, report) surface for every stop point.
        let refused = aproto::AckResponse {
            cmd_opcode: tprm::OP_READBACK_TPRM,
            cmd_sequence: rb.cmd_sequence,
            status: rb.status,
            status_name: "READBACK_MISMATCH".to_string(),
            stage: rb.stage,
            queued: rb.queued,
            extra: Vec::new(),
        };
        return Ok((refused, report));
    }

    // 3. Vehicle-side verify (full ingest checks, no apply).
    let vr = cli
        .send_command(0x000000, tprm::OP_VERIFY_TPRM, &full_uid.to_le_bytes())
        .await?;
    let verdict = tprm::parse_verify_verdict(&vr.extra).ok();
    let verdict_ok = vr.status == 0 && verdict.as_ref().is_some_and(|v| v.ok);
    if !verdict_ok {
        let report = serde_json::json!({
            "step": "verify",
            "outcome": "vehicle verify refused",
            "verdict": verdict,
        });
        let mut refused = vr;
        refused.status_name = verdict
            .as_ref()
            .map(|v| v.verdict_name.to_string())
            .unwrap_or_else(|| refused.status_name.clone());
        return Ok((refused, report));
    }

    // 4. Apply.
    let resp = cli.reload_tprm(full_uid).await?;
    drop(cli);
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
    let mut cli = client.lock().await;
    let resp = cli
        .send_command(0x000000, tprm::OP_READBACK_TPRM, &[])
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;
    drop(cli);
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
    let uid_clean = uid_str.trim_start_matches("0x").trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", uid_str),
        )
    })?;
    let (client, _db) = target_client(&state, &id).await?;
    let mut cli = client.lock().await;
    let resp = cli
        .send_command(0x000000, tprm::OP_VERIFY_TPRM, &full_uid.to_le_bytes())
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;
    drop(cli);
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

/// LeafSpec views over dict fields, shared by flat and variable-length
/// layout hashing. Dict declaration order is template emission order.
fn dict_leaf_specs(fields: &[crate::core::config_manager::FieldDef]) -> Vec<tprm::LeafSpec<'_>> {
    fields
        .iter()
        .map(|f| tprm::LeafSpec {
            name: &f.name,
            field_type: &f.field_type,
            size: f.size,
            array: if f.field_type == "array" {
                let count = f
                    .dims
                    .as_ref()
                    .map(|d| d.iter().product::<usize>())
                    .unwrap_or(1)
                    .max(1);
                Some((
                    f.element_type.as_deref().unwrap_or("uint"),
                    f.size / count,
                    count,
                ))
            } else {
                None
            },
        })
        .collect()
}

/// Encode one edited field into the payload buffer. Returns false when
/// the (type, size) shape is not encodable or the value has the wrong
/// JSON type -- callers must surface that as an error: a silent skip
/// here uploads the OLD value under a SUCCESS response, which reads as
/// "applied" while changing nothing (this bit u64 swap thresholds).
fn encode_field(
    buf: &mut [u8],
    field: &crate::core::config_manager::FieldDef,
    value: &serde_json::Value,
) -> bool {
    let off = field.offset;
    if off + field.size > buf.len() {
        return false;
    }
    match (field.field_type.as_str(), field.size) {
        ("uint", 1) | ("bool", 1) => value.as_u64().map(|v| buf[off] = v as u8).is_some(),
        ("uint", 2) => value
            .as_u64()
            .map(|v| buf[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes()))
            .is_some(),
        ("uint", 4) => value
            .as_u64()
            .map(|v| buf[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes()))
            .is_some(),
        ("uint", 8) => value
            .as_u64()
            .map(|v| buf[off..off + 8].copy_from_slice(&v.to_le_bytes()))
            .is_some(),
        ("int", 1) => value.as_i64().map(|v| buf[off] = v as u8).is_some(),
        ("int", 2) => value
            .as_i64()
            .map(|v| buf[off..off + 2].copy_from_slice(&(v as i16).to_le_bytes()))
            .is_some(),
        ("int", 4) => value
            .as_i64()
            .map(|v| buf[off..off + 4].copy_from_slice(&(v as i32).to_le_bytes()))
            .is_some(),
        ("int", 8) => value
            .as_i64()
            .map(|v| buf[off..off + 8].copy_from_slice(&v.to_le_bytes()))
            .is_some(),
        ("float", 4) => value
            .as_f64()
            .map(|v| buf[off..off + 4].copy_from_slice(&(v as f32).to_le_bytes()))
            .is_some(),
        ("float", 8) | ("double", 8) => value
            .as_f64()
            .map(|v| buf[off..off + 8].copy_from_slice(&v.to_le_bytes()))
            .is_some(),
        // Bounded string: fixed char buffer, NUL-padded. A value longer
        // than the field is refused, never truncated -- the packer on
        // the producing side errors the same way.
        ("string", n) => match value.as_str() {
            Some(s) if s.len() <= n => {
                buf[off..off + n].fill(0);
                buf[off..off + s.len()].copy_from_slice(s.as_bytes());
                true
            }
            _ => false,
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
                return false;
            };
            if count == 0 || arr.len() != count || total % count != 0 {
                return false;
            }
            let elem_size = total / count;
            let elem_type = field.element_type.as_deref().unwrap_or("uint");
            arr.iter().enumerate().all(|(i, v)| {
                let elem = crate::core::config_manager::FieldDef {
                    name: field.name.clone(),
                    field_type: elem_type.to_string(),
                    offset: off + i * elem_size,
                    size: elem_size,
                    value: serde_json::Value::Null,
                    element_type: None,
                    dims: None,
                    constraints: None,
                };
                encode_field(buf, &elem, v)
            })
        }
        _ => false,
    }
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
    let (db, dicts, manifest) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (
            st.db.clone(),
            target.struct_dicts.clone(),
            target.manifest.clone(),
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
    let uid_names: Vec<(u32, String)> = manifest
        .as_ref()
        .map(|m| m.component_uids())
        .unwrap_or_default();
    let uid_refs: Vec<(u32, &str)> = uid_names.iter().map(|(u, n)| (*u, n.as_str())).collect();
    let known: std::collections::HashSet<String> =
        telemetry::TelemetryDecoder::new(&dicts, &uid_refs)
            .channel_names()
            .into_iter()
            .collect();

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
    // the 100k-row limit the string build alone is milliseconds.
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

async fn downsample_data(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let db = st.db.clone();
    drop(st);

    // Keep last hour at full resolution, downsample older to 1-minute averages
    let result = db_blocking(db, |db| db.downsample(3_600_000, 60_000)).await?;
    Ok(Json(serde_json::to_value(result).unwrap_or_default()))
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
    /// Number of samples to delete (oldest first). If unset, defaults to
    /// 25% of the current sample count for this target.
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
        Some(n) if n > 0 => n,
        _ => {
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
    // Load struct dicts BEFORE taking the write guard -- load_dir hits
    // the filesystem and must not stall every request behind a parked
    // writer.
    let loaded_dicts = body
        .structs_dir
        .as_ref()
        .and_then(|dir| StructDictionary::load_dir(&std::path::PathBuf::from(dir)).ok())
        .map(Arc::new);

    let (push_tlm_tx, _) = broadcast::channel::<PushTelemetryPacket>(4096);
    let (sample_tx, _) = broadcast::channel::<TelemetrySample>(4096);

    let mut st = state.write().await;
    let db_for_audit = st.db.clone();

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

    let target_dicts = loaded_dicts.unwrap_or_else(|| st.struct_dicts.clone());

    let tc = config::TargetSection {
        name: body.name.clone(),
        host: body.host.clone(),
        port: body.port,
        manifest: None,
        structs_dir: body.structs_dir,
        telemetry_config: None,
        commands_config: None,
        auto_connect: false,
    };

    let metrics = TargetMetrics::new();

    // Spawn DB writer for new target
    spawn_sample_writer(
        id.clone(),
        st.db.clone(),
        sample_tx.subscribe(),
        metrics.clone(),
    );

    let mut new_client = AprotoClient::new(push_tlm_tx.clone());
    new_client.set_metrics(metrics.clone());
    let connected = new_client.connected_handle();
    st.targets.insert(
        id.clone(),
        TargetState {
            config: tc,
            client: Arc::new(Mutex::new(new_client)),
            connected,
            metrics,
            push_tlm_tx,
            sample_tx,
            struct_dicts: target_dicts,
            manifest: None,
            telemetry_config: None,
            commands_config: None,
            _router_handle: None,
        },
    );

    tracing::info!("Added target: {} ({}:{})", body.name, body.host, body.port);
    record_audit(
        &db_for_audit,
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

    // Abort telemetry router
    if let Some(h) = target._router_handle {
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
    let (client, manifest) = {
        let st = state.read().await;
        let target = st
            .targets
            .get(&id)
            .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
        (target.client.clone(), target.manifest.clone())
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

    let mut cli = client.lock().await;
    let connected = cli.is_connected();

    let mut components = Vec::new();
    for comp in &manifest_components {
        let uid_clean = comp
            .full_uid
            .trim_start_matches("0x")
            .trim_start_matches("0X");
        let uid = u32::from_str_radix(uid_clean, 16).unwrap_or(0);

        // Probe reachability only if connected
        let reachable = if connected {
            match cli.send_command(uid, 0x0000, &[]).await {
                Ok(r) => r.status == 0,
                Err(_) => false,
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

async fn target_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (client, _) = target_client(&state, &id).await?;
    let mut cli = client.lock().await;
    if !cli.is_connected() {
        return Err((StatusCode::BAD_GATEWAY, "Not connected".to_string()));
    }

    // Query scheduler health (fullUid=0x000100, opcode=0x0100)
    let sched = match cli.send_command(0x000100, 0x0100, &[]).await {
        Ok(r) if r.status == 0 && r.extra.len() >= 32 => {
            let tick_count = u64::from_le_bytes([
                r.extra[0], r.extra[1], r.extra[2], r.extra[3], r.extra[4], r.extra[5], r.extra[6],
                r.extra[7],
            ]);
            let task_count = u32::from_le_bytes([r.extra[8], r.extra[9], r.extra[10], r.extra[11]]);
            let violations =
                u32::from_le_bytes([r.extra[12], r.extra[13], r.extra[14], r.extra[15]]);
            let skip_count =
                u32::from_le_bytes([r.extra[16], r.extra[17], r.extra[18], r.extra[19]]);
            let freq = u16::from_le_bytes([r.extra[20], r.extra[21]]);
            let pool_count = r.extra[22];
            let sleeping = r.extra[23] != 0;

            serde_json::json!({
                "tickCount": tick_count,
                "taskCount": task_count,
                "periodViolations": violations,
                "totalSkipCount": skip_count,
                "fundamentalFreqHz": freq,
                "poolCount": pool_count,
                "sleeping": sleeping,
            })
        }
        _ => serde_json::json!({"error": "Could not query scheduler"}),
    };

    // Query executive health for additional context
    let exec = match cli.send_command(0x000000, 0x0100, &[]).await {
        Ok(r) if r.status == 0 && r.extra.len() >= 48 => {
            let clock_cycles = u64::from_le_bytes([
                r.extra[0], r.extra[1], r.extra[2], r.extra[3], r.extra[4], r.extra[5], r.extra[6],
                r.extra[7],
            ]);
            let freq = u16::from_le_bytes([r.extra[32], r.extra[33]]);
            let uptime_sec = if freq > 0 {
                clock_cycles / freq as u64
            } else {
                0
            };

            serde_json::json!({
                "clockCycles": clock_cycles,
                "clockFreqHz": freq,
                "uptimeSeconds": uptime_sec,
                "frameOverruns": u64::from_le_bytes([
                    r.extra[16], r.extra[17], r.extra[18], r.extra[19],
                    r.extra[20], r.extra[21], r.extra[22], r.extra[23],
                ]),
                "watchdogWarnings": u64::from_le_bytes([
                    r.extra[24], r.extra[25], r.extra[26], r.extra[27],
                    r.extra[28], r.extra[29], r.extra[30], r.extra[31],
                ]),
            })
        }
        _ => serde_json::json!({"error": "Could not query executive"}),
    };

    Ok(Json(serde_json::json!({
        "scheduler": sched,
        "executive": exec,
    })))
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

async fn login(
    State(state): State<AppState>,
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
    drop(st);

    let ok = crate::core::auth::verify_credentials(
        &body.username,
        &body.password,
        &auth.username,
        &auth.password_hash,
    );

    if ok {
        let token = crate::core::auth::mint_token(
            &body.username,
            &auth.secret,
            chrono::Utc::now().timestamp(),
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("JWT error: {}", e),
            )
        })?;

        Ok(Json(serde_json::json!({
            "token": token,
            "expires_in": crate::core::auth::TOKEN_TTL_SECS,
        })))
    } else {
        Err((StatusCode::UNAUTHORIZED, "Invalid credentials".to_string()))
    }
}

/// Mint a short-lived single-purpose WebSocket ticket for the caller.
/// Browsers cannot set Authorization on WebSocket upgrades, so the
/// authenticated page trades its bearer token for a 30 s ticket and
/// puts THAT on the query string -- a leaked ticket is stale before a
/// log file is ever read.
async fn ws_ticket(
    State(state): State<AppState>,
    axum::Extension(actor): axum::Extension<Actor>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (enabled, secret) = {
        let st = state.read().await;
        (st.config.auth.enabled, st.config.auth.secret.clone())
    };
    if !enabled {
        return Ok(Json(serde_json::json!({ "ticket": "auth-disabled" })));
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

/* ----------------------------- WebSocket ----------------------------- */

async fn telemetry_ws(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;

    let rx = target.sample_tx.subscribe();
    let metrics = target.metrics.clone();
    let target_id = id.clone();

    Ok(ws.on_upgrade(move |socket| handle_telemetry_ws(socket, rx, target_id, metrics)))
}

async fn handle_telemetry_ws(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<TelemetrySample>,
    target_id: String,
    metrics: Arc<TargetMetrics>,
) {
    use std::sync::atomic::Ordering;
    const LAG_WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

    tracing::info!("WebSocket client connected for target {}", target_id);
    metrics.ws_clients.fetch_add(1, Ordering::Relaxed);
    let mut last_lag_warn: Option<std::time::Instant> = None;

    loop {
        match rx.recv().await {
            Ok(sample) => {
                let json = serde_json::to_string(&sample).unwrap_or_default();
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break; // Client disconnected
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
                    last_lag_warn = Some(std::time::Instant::now());
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    metrics.ws_clients.fetch_sub(1, Ordering::Relaxed);
    tracing::info!("WebSocket client disconnected for target {}", target_id);
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

/// Validate the JWT in the Authorization header. Skips validation when
/// auth is disabled in config (the default for development).
///
/// Whitelisted paths bypass auth: `/api/auth/login` (otherwise nobody could
/// log in) and `/api/health` (used by load balancers and `make health`).
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

    // CRITICAL: read the config under a SHORT-lived guard and then drop
    // it before calling next.run(). Holding the guard across the await
    // would deadlock with any handler that takes a write lock (e.g.
    // connect_target), because tokio's RwLock is writer-preferring.
    let (auth_enabled, secret) = {
        let st = state.read().await;
        (st.config.auth.enabled, st.config.auth.secret.clone())
    };

    if !auth_enabled {
        request
            .extensions_mut()
            .insert(Actor(Arc::from("operator")));
        return Ok(next.run(request).await);
    }

    // Bearer token from the Authorization header, or -- for WebSocket
    // upgrades, which cannot set headers from a browser -- a
    // short-lived ticket on the query string. A query credential MUST
    // be a ticket (ws claim, 30 s expiry, minted via
    // POST /api/auth/ws-ticket): long-lived tokens are rejected there
    // so they can never land in request logs.
    let header_token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let query_ticket = request.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| {
            let mut parts = kv.splitn(2, '=');
            if parts.next()? == "ticket" {
                Some(parts.next()?.to_string())
            } else {
                None
            }
        })
    });
    let from_query = header_token.is_none();
    let token = header_token
        .or(query_ticket)
        .ok_or((StatusCode::UNAUTHORIZED, "missing token".to_string()))?;

    // Validation lives in core::auth (lib-testable); the subject
    // becomes the audit actor for the request.
    let sub = crate::core::auth::validate_token(&token, &secret, from_query)
        .map_err(|e| (StatusCode::UNAUTHORIZED, e))?;

    request
        .extensions_mut()
        .insert(Actor(Arc::from(sub.as_str())));
    Ok(next.run(request).await)
}

/* ----------------------------- Rate Limiting ----------------------------- */

/// A simple per-IP token bucket for command-emitting endpoints. Not a
/// production-grade solution -- the in-memory map grows unbounded if
/// many distinct IPs hit the server -- but adequate for the MVP.
/// Replace with `tower_governor` for production deployment.
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
        .route("/targets/{id}/registry", get(target_registry))
        .route("/targets/{id}/schedule", get(target_schedule))
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
        .route("/auth/ws-ticket", post(ws_ticket));

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
        // deliberately (WebSocket auth falls back to a ?token= query
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
) {
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
    });
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
        let mut password = String::new();
        eprintln!("Password (echoed):");
        if std::io::stdin().read_line(&mut password).is_err() || password.trim().is_empty() {
            eprintln!("no password read from stdin");
            std::process::exit(1);
        }
        match crate::core::auth::hash_password(password.trim()) {
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

    let config = config::load(&cli.config).unwrap_or_else(|e| {
        tracing::warn!("Config load failed ({}), using defaults", e);
        ServerConfig::default()
    });

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

    let port = cli.port.unwrap_or(config.server.port);
    // Honor the configured bind host instead of hardcoding 0.0.0.0.
    // Falls back to 0.0.0.0 if the host string fails to parse so an
    // operator typo can't accidentally make the server invisible.
    let bind_ip: std::net::IpAddr = config
        .server
        .host
        .parse()
        .unwrap_or_else(|_| std::net::IpAddr::from([0, 0, 0, 0]));
    let addr = SocketAddr::from((bind_ip, port));

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

    // Initialize targets with per-target or global struct dicts
    let mut targets = HashMap::new();
    for (i, tc) in config.targets.iter().enumerate() {
        let id = format!("target-{}", i);
        let (push_tlm_tx, _) = broadcast::channel::<PushTelemetryPacket>(4096);
        let (sample_tx, _) = broadcast::channel::<TelemetrySample>(4096);

        // Per-target struct dicts (falls back to global)
        let target_dicts = if let Some(ref dir) = tc.structs_dir {
            let path = std::path::PathBuf::from(dir);
            match StructDictionary::load_dir(&path) {
                Ok(dicts) => {
                    tracing::info!(
                        "Loaded per-target struct dicts for {} from {} ({} components)",
                        tc.name,
                        dir,
                        dicts.components.len()
                    );
                    for w in dicts.validate() {
                        tracing::warn!("  struct dict warning [{}]: {}", tc.name, w);
                    }
                    Arc::new(dicts)
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load struct dicts for {}: {}, using global",
                        tc.name,
                        e
                    );
                    global_dicts.clone()
                }
            }
        } else {
            global_dicts.clone()
        };

        // Load app manifest (build artifact describing component registry)
        let manifest = tc.manifest.as_ref().and_then(|path| {
            match crate::core::config_manager::AppManifest::load(&std::path::PathBuf::from(path)) {
                Ok(m) => {
                    tracing::info!(
                        "Loaded app manifest for {}: {} ({} components)",
                        tc.name,
                        m.application,
                        m.components.len()
                    );
                    for w in m.validate() {
                        tracing::warn!("  manifest warning [{}]: {}", tc.name, w);
                    }
                    Some(Arc::new(m))
                }
                Err(e) => {
                    tracing::warn!("Failed to load manifest for {}: {}", tc.name, e);
                    None
                }
            }
        });

        // Load telemetry display config (plot layouts)
        let telemetry_config = tc.telemetry_config.as_ref().and_then(|path| {
            match crate::core::config_manager::TelemetryConfig::load(&std::path::PathBuf::from(
                path,
            )) {
                Ok(tc_cfg) => {
                    tracing::info!(
                        "Loaded telemetry config for {}: {} layouts",
                        tc.name,
                        tc_cfg.layouts.len()
                    );
                    for w in tc_cfg.validate() {
                        tracing::warn!("  telemetry config warning [{}]: {}", tc.name, w);
                    }
                    Some(Arc::new(tc_cfg))
                }
                Err(e) => {
                    tracing::warn!("Failed to load telemetry config for {}: {}", tc.name, e);
                    None
                }
            }
        });

        // Load command definitions
        let commands_config = tc.commands_config.as_ref().and_then(|path| {
            match crate::core::config_manager::CommandConfig::load(&std::path::PathBuf::from(path))
            {
                Ok(cc) => {
                    tracing::info!(
                        "Loaded command config for {}: {} quick commands, {} components",
                        tc.name,
                        cc.quick_commands.len(),
                        cc.components.len()
                    );
                    Some(Arc::new(cc))
                }
                Err(e) => {
                    tracing::warn!("Failed to load command config for {}: {}", tc.name, e);
                    None
                }
            }
        });

        let metrics = TargetMetrics::new();
        let mut new_client = AprotoClient::new(push_tlm_tx.clone());
        new_client.set_metrics(metrics.clone());
        let connected = new_client.connected_handle();
        targets.insert(
            id,
            TargetState {
                config: tc.clone(),
                client: Arc::new(Mutex::new(new_client)),
                connected,
                metrics,
                push_tlm_tx,
                sample_tx,
                struct_dicts: target_dicts,
                manifest,
                telemetry_config,
                commands_config,
                _router_handle: None,
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
    // Each writer flushes remaining samples when the broadcast channel closes (shutdown).
    for (id, target) in targets.iter() {
        spawn_sample_writer(
            id.clone(),
            db.clone(),
            target.sample_tx.subscribe(),
            target.metrics.clone(),
        );
    }

    // Spawn periodic maintenance (every 60 seconds)
    // - Measure net fill rate for storage vitals
    // - Size-based FIFO when the DB exceeds its cap (fair waterline
    //   across targets by default; see storage.fifo_strategy)
    // - Prune samples older than retention
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
        "Storage limits: max_db_size={}MB, retention={}h",
        max_db_mb,
        config.storage.retention_hours
    );
    let retention_ms = config.storage.retention_hours as u64 * 3600 * 1000;
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
                match db.prune(retention_ms) {
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

    // Seed telemetry layouts from config files into DB
    for (id, target) in &targets {
        if let Some(ref tc) = target.telemetry_config {
            match db.seed_layouts(id, tc) {
                Ok(n) if n > 0 => tracing::info!("Seeded {} telemetry layouts for {}", n, id),
                Ok(_) => {}
                Err(e) => tracing::warn!("Failed to seed layouts for {}: {}", id, e),
            }
        }
    }

    let state: AppState = Arc::new(RwLock::new(SharedState {
        config: config.clone(),
        targets,
        db,
        struct_dicts: global_dicts,
        rate_limiter: Arc::new(RateLimiter::default()),
        storage_vitals,
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
                        Ok(true) => tracing::info!("auto_connect: {} connected", id),
                        Ok(false) => {}
                        Err((_, e)) => {
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

        // Disconnect all targets to flush telemetry and close connections
        let mut st = shutdown_state.write().await;
        for (id, target) in st.targets.iter_mut() {
            if let Some(h) = target._router_handle.take() {
                h.abort();
            }
            let mut cli = target.client.lock().await;
            if cli.is_connected() {
                cli.disconnect();
                tracing::info!("Disconnected target {} during shutdown", id);
            }
        }
    });

    if let Err(e) = server.await {
        tracing::error!("Server error: {}", e);
        std::process::exit(1);
    }

    tracing::info!("Zenith shutdown complete");
}
