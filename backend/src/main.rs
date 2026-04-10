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

use crate::config::ServerConfig;
use crate::core::aproto_client::{AprotoClient, PushTelemetryPacket};
use crate::core::config_manager::StructDictionary;
use crate::core::telemetry::{self, TelemetrySample};
use crate::storage::telemetry_db::TelemetryDb;

/* ----------------------------- CLI ----------------------------- */

#[derive(Parser)]
#[command(name = "zenith", about = "Real-time operations interface for Apex CSF")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    #[arg(short, long)]
    port: Option<u16>,
}

/* ----------------------------- App State ----------------------------- */

struct TargetState {
    config: config::TargetSection,
    client: Arc<Mutex<AprotoClient>>,
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
}

type AppState = Arc<RwLock<SharedState>>;

/* ----------------------------- Response Types ----------------------------- */

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
struct TargetInfo {
    id: String,
    name: String,
    host: String,
    port: u16,
    connected: bool,
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

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn server_version() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "zenith",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Real-time operations interface for Apex CSF",
    }))
}

async fn list_targets(State(state): State<AppState>) -> Json<serde_json::Value> {
    let st = state.read().await;
    let mut targets: Vec<TargetInfo> = Vec::new();
    for (id, t) in &st.targets {
        let connected = t.client.lock().await.is_connected();
        targets.push(TargetInfo {
            id: id.clone(),
            name: t.config.name.clone(),
            host: t.config.host.clone(),
            port: t.config.port,
            connected,
        });
    }
    // Sort by ID to preserve config.toml order (target-0, target-1, ...)
    targets.sort_by(|a, b| a.id.cmp(&b.id));
    Json(serde_json::json!({ "targets": targets }))
}

async fn connect_target(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Hold the write lock across the entire connect operation to prevent
    // TOCTOU races between the is_connected check and the connect call.
    let mut st = state.write().await;
    let db_for_audit = st.db.clone();
    let target = st
        .targets
        .get_mut(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;

    let host = target.config.host.clone();
    let port = target.config.port;
    let ip_str = addr.ip().to_string();
    {
        let mut cli = target.client.lock().await;
        if cli.is_connected() {
            return Ok(Json(
                serde_json::json!({"status": "already_connected", "target": id}),
            ));
        }
        if let Err(e) = cli.connect(&host, port).await {
            let err_msg = format!("Connect failed: {}", e);
            record_audit(
                &db_for_audit,
                "operator",
                "connect_target",
                Some(&id),
                Some(&format!("{}:{}", host, port)),
                &format!("err: {}", err_msg),
                Some(&ip_str),
            );
            return Err((StatusCode::BAD_GATEWAY, err_msg));
        }
    }

    // Start telemetry router (push packets -> decoded samples)
    let push_rx = target.push_tlm_tx.subscribe();
    let handle = telemetry::spawn_router(
        id.clone(),
        push_rx,
        target.sample_tx.clone(),
        target.struct_dicts.clone(),
        target.manifest.clone(),
    );
    target._router_handle = Some(handle);

    record_audit(
        &db_for_audit,
        "operator",
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

async fn disconnect_target(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut st = state.write().await;
    let db_for_audit = st.db.clone();
    let target = st
        .targets
        .get_mut(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;

    // Abort telemetry router
    if let Some(h) = target._router_handle.take() {
        h.abort();
    }

    let mut cli = target.client.lock().await;
    cli.disconnect();
    drop(cli);
    record_audit(
        &db_for_audit,
        "operator",
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
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let mut cli = target.client.lock().await;
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
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let mut cli = target.client.lock().await;
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
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;

    let uid_clean = uid_str.trim_start_matches("0x").trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", uid_str),
        )
    })?;

    let mut cli = target.client.lock().await;
    let resp = cli
        .inspect(full_uid, query.category, query.offset, query.length)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("{}", e)))?;
    Ok(Json(to_cmd_response(resp)))
}

/// Upload a file to the target filesystem via APROTO file transfer.
async fn upload_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<FileUploadRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let db_for_audit = st.db.clone();

    // Decode base64 file content
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&body.content_base64)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)))?;

    let detail = format!("path={} bytes={}", body.remote_path, data.len());
    let ip_str = addr.ip().to_string();

    let mut cli = target.client.lock().await;
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
                "operator",
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
                "operator",
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
/// Apex closes the connection before sending an ACK because the
/// executive calls execv() inside the command handler. The
/// `AprotoClient::restart_executive()` helper catches the expected
/// connection-closed error and returns a synthetic SUCCESS, so the
/// audit log gets a clean "ok" entry instead of the misleading
/// "err: connection closed by remote" the generic /command path
/// produced before this dedicated endpoint existed.
async fn restart_target(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let db_for_audit = st.db.clone();
    let ip_str = addr.ip().to_string();

    let mut cli = target.client.lock().await;
    let result = cli.restart_executive().await;
    drop(cli);

    match result {
        Ok(resp) => {
            record_audit(
                &db_for_audit,
                "operator",
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
                "operator",
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
    Path((id, uid_str)): Path<(String, String)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<SwapLibraryRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let db_for_audit = st.db.clone();

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
    if data.len() > 50 * 1024 * 1024 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "library exceeds 50MB cap".to_string(),
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

    let mut cli = target.client.lock().await;
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
                "operator",
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
                "operator",
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
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<SendCommandRequest>,
) -> Result<Json<CommandResponse>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let db_for_audit = st.db.clone();

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

    let mut cli = target.client.lock().await;
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
                "operator",
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
                "operator",
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
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let dicts = target.struct_dicts.clone();
    let manifest = target.manifest.clone();

    let uid_clean = uid_str.trim_start_matches("0x").trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", uid_str),
        )
    })?;

    // INSPECT TUNABLE_PARAM (category=1)
    let mut cli = target.client.lock().await;
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
    Path((id, uid_str)): Path<(String, String)>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<UpdateParamsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;
    let dicts = target.struct_dicts.clone();
    let manifest = target.manifest.clone();
    let db_for_audit = st.db.clone();

    let uid_clean = uid_str.trim_start_matches("0x").trim_start_matches("0X");
    let full_uid = u32::from_str_radix(uid_clean, 16).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid fullUid: {}", uid_str),
        )
    })?;

    // Decode the original binary from the frontend (from the initial page load).
    // No hidden INSPECT -- the user sees what they're modifying.
    let original_binary: Vec<u8> = body
        .raw_hex
        .as_ref()
        .map(|hex| {
            hex.as_bytes()
                .chunks(2)
                .filter_map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap_or("00"), 16).ok())
                .collect()
        })
        .unwrap_or_default();

    let mut struct_size = 0usize;
    let mut struct_fields = Vec::new();

    // Build binary TPRM from edited fields
    let binary = if body.variable_length {
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

        let total_size = hdr_struct.size + entries.len() * ent_struct.size;
        let mut buf = vec![0u8; total_size];

        // Encode header
        for field in &hdr_struct.fields {
            if let Some(value) = body.fields.get(&field.name) {
                encode_field(&mut buf, field, value);
            }
        }

        // Encode each entry
        for (i, entry_fields) in entries.iter().enumerate() {
            let base = hdr_struct.size + i * ent_struct.size;
            for field in &ent_struct.fields {
                if let Some(value) = entry_fields.get(&field.name) {
                    let mut adjusted_field = field.clone();
                    adjusted_field.offset = base + field.offset;
                    encode_field(&mut buf, &adjusted_field, value);
                }
            }
        }

        buf
    } else {
        // Flat: single struct
        if let Some((_comp, _sname, sdef)) =
            find_tunable_struct(&dicts, original_binary.len(), full_uid, manifest.as_deref())
        {
            struct_size = sdef.size;
            struct_fields = sdef.fields.clone();
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

        // Start with the original binary from the page load (preserves arrays, padding, etc.)
        // Then overwrite only the scalar fields the user edited.
        let mut buf = if original_binary.len() == struct_size {
            original_binary.clone()
        } else {
            vec![0u8; struct_size]
        };
        for field in &struct_fields {
            if let Some(value) = body.fields.get(&field.name) {
                encode_field(&mut buf, field, value);
            }
        }
        buf
    };

    // Upload and reload
    let detail = format!(
        "uid=0x{:06X} bytes={} field_count={}",
        full_uid,
        binary.len(),
        body.fields.len()
    );
    let ip_str = addr.ip().to_string();

    let mut cli = target.client.lock().await;
    let result = cli.update_tprm(full_uid, &binary).await;
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
                "operator",
                "update_tprm",
                Some(&id),
                Some(&detail),
                &status,
                Some(&ip_str),
            );
            Ok(Json(serde_json::json!({
                "status": resp.status,
                "status_name": resp.status_name,
                "fullUid": format!("0x{:06X}", full_uid),
                "uploaded_bytes": binary.len(),
            })))
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            record_audit(
                &db_for_audit,
                "operator",
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

fn encode_field(
    buf: &mut [u8],
    field: &crate::core::config_manager::FieldDef,
    value: &serde_json::Value,
) {
    let off = field.offset;
    if off + field.size > buf.len() {
        return;
    }
    match (field.field_type.as_str(), field.size) {
        ("uint", 1) => {
            if let Some(v) = value.as_u64() {
                buf[off] = v as u8;
            }
        }
        ("uint", 2) => {
            if let Some(v) = value.as_u64() {
                buf[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes());
            }
        }
        ("uint", 4) => {
            if let Some(v) = value.as_u64() {
                buf[off..off + 4].copy_from_slice(&(v as u32).to_le_bytes());
            }
        }
        ("float", 4) => {
            if let Some(v) = value.as_f64() {
                buf[off..off + 4].copy_from_slice(&(v as f32).to_le_bytes());
            }
        }
        ("float", 8) | ("double", 8) => {
            if let Some(v) = value.as_f64() {
                buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        _ => {}
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

fn default_start() -> u64 {
    0
}
fn default_end() -> u64 {
    9_999_999_999_999
} // ~2286 AD
fn default_limit() -> usize {
    10000
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

    let samples = db
        .query_range(
            &id,
            query.channel.as_deref(),
            query.start_ms,
            query.end_ms,
            query.limit,
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

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
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    let layouts = db
        .get_layouts(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    Ok(Json(serde_json::json!({ "layouts": layouts })))
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

    let samples = db
        .query_range(
            &id,
            query.channel.as_deref(),
            query.start_ms,
            query.end_ms,
            query.limit,
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    let mut csv = String::from("timestamp_ms,channel,value\n");
    for s in &samples {
        csv.push_str(&format!("{},{},{}\n", s.timestamp_ms, s.channel, s.value));
    }

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
    drop(st);

    let count = db
        .count()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    let size_bytes = db.db_size_bytes().unwrap_or(0);
    Ok(Json(serde_json::json!({
        "total_samples": count,
        "db_size_bytes": size_bytes,
        "db_size_mb": format!("{:.2}", size_bytes as f64 / 1_048_576.0),
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

    let stats = db
        .target_stats(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    Ok(Json(serde_json::to_value(stats).unwrap_or_default()))
}

async fn downsample_data(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    let db = st.db.clone();
    drop(st);

    // Keep last hour at full resolution, downsample older to 1-minute averages
    let result = db
        .downsample(3_600_000, 60_000)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    Ok(Json(serde_json::to_value(result).unwrap_or_default()))
}

async fn delete_target_data(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let st = state.read().await;
    if !st.targets.contains_key(&id) {
        return Err((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)));
    }
    let db = st.db.clone();
    drop(st);

    let result = db.delete_target(&id);
    let ip_str = addr.ip().to_string();
    match result {
        Ok(deleted) => {
            record_audit(
                &db,
                "operator",
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
                "operator",
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
            let stats = db
                .target_stats(&id)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
            ((stats.sample_count / 4).max(1)) as usize
        }
    };

    let ip_str = addr.ip().to_string();
    let result = db.delete_oldest_for_target(&id, count);
    match result {
        Ok(deleted) => {
            record_audit(
                &db,
                "operator",
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
                "operator",
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
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<AddTargetRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut st = state.write().await;
    let db_for_audit = st.db.clone();

    // Generate ID
    let id = format!("target-{}", st.targets.len());

    // Load struct dicts if specified
    let target_dicts = if let Some(ref dir) = body.structs_dir {
        match StructDictionary::load_dir(&std::path::PathBuf::from(dir)) {
            Ok(dicts) => Arc::new(dicts),
            Err(_) => st.struct_dicts.clone(),
        }
    } else {
        st.struct_dicts.clone()
    };

    let (push_tlm_tx, _) = broadcast::channel::<PushTelemetryPacket>(4096);
    let (sample_tx, _) = broadcast::channel::<TelemetrySample>(4096);

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

    // Spawn DB writer for new target
    let db_clone = st.db.clone();
    let mut rx = sample_tx.subscribe();
    tokio::spawn(async move {
        let mut batch = Vec::with_capacity(64);
        loop {
            match rx.recv().await {
                Ok(sample) => {
                    batch.push(sample);
                    if batch.len() >= 50 || rx.is_empty() {
                        let _ = db_clone.insert_batch(&batch);
                        batch.clear();
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    if !batch.is_empty() {
                        let _ = db_clone.insert_batch(&batch);
                    }
                    break;
                }
            }
        }
    });

    st.targets.insert(
        id.clone(),
        TargetState {
            config: tc,
            client: Arc::new(Mutex::new(AprotoClient::new(push_tlm_tx.clone()))),
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
        "operator",
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
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut st = state.write().await;
    let db_for_audit = st.db.clone();
    let target = st
        .targets
        .remove(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;

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
        "operator",
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
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;

    // Use app manifest (build artifact) for component registry
    let manifest_components = match &target.manifest {
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

    let mut cli = target.client.lock().await;
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
    let st = state.read().await;
    let target = st
        .targets
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("Target '{}' not found", id)))?;

    let mut cli = target.client.lock().await;
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

    // Simple admin check (production would use a user database)
    if body.username == "admin" && body.password == st.config.auth.secret {
        let claims = serde_json::json!({
            "sub": body.username,
            "exp": chrono::Utc::now().timestamp() + 86400, // 24 hours
        });
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(st.config.auth.secret.as_bytes()),
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("JWT error: {}", e),
            )
        })?;

        Ok(Json(serde_json::json!({
            "token": token,
            "expires_in": 86400,
        })))
    } else {
        Err((StatusCode::UNAUTHORIZED, "Invalid credentials".to_string()))
    }
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
    let target_id = id.clone();

    Ok(ws.on_upgrade(move |socket| handle_telemetry_ws(socket, rx, target_id)))
}

async fn handle_telemetry_ws(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<TelemetrySample>,
    target_id: String,
) {
    tracing::info!("WebSocket client connected for target {}", target_id);

    loop {
        match rx.recv().await {
            Ok(sample) => {
                let json = serde_json::to_string(&sample).unwrap_or_default();
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break; // Client disconnected
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!("WebSocket lagged by {} messages", n);
            }
            Err(broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    tracing::info!("WebSocket client disconnected for target {}", target_id);
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
    let entries = db
        .query_audit(query.limit.min(1000), query.offset)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
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
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    // Whitelist
    let path = request.uri().path();
    if path == "/api/auth/login" || path == "/api/health" {
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
        return Ok(next.run(request).await);
    }

    // Extract bearer token from Authorization header.
    // For WebSocket upgrades the browser can't set Authorization, so we
    // also accept ?token=... on the query string as a fallback.
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| {
            request.uri().query().and_then(|q| {
                q.split('&').find_map(|kv| {
                    let mut parts = kv.splitn(2, '=');
                    if parts.next()? == "token" {
                        Some(parts.next()?.to_string())
                    } else {
                        None
                    }
                })
            })
        });

    let token = token.ok_or((StatusCode::UNAUTHORIZED, "missing token".to_string()))?;

    // Validate JWT against the configured secret. Default validation
    // (which checks `exp` if present and rejects expired tokens) is fine.
    let mut validation = jsonwebtoken::Validation::default();
    validation.validate_exp = true;
    validation.required_spec_claims.clear(); // sub is recommended but not required for this minimal scheme
    let _decoded = jsonwebtoken::decode::<serde_json::Value>(
        &token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|e| (StatusCode::UNAUTHORIZED, format!("invalid token: {}", e)))?;

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

fn build_router(state: AppState) -> Router {
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
        .route("/auth/login", post(login));

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
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let config = config::load(&cli.config).unwrap_or_else(|e| {
        tracing::warn!("Config load failed ({}), using defaults", e);
        ServerConfig::default()
    });

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

        targets.insert(
            id,
            TargetState {
                config: tc.clone(),
                client: Arc::new(Mutex::new(AprotoClient::new(push_tlm_tx.clone()))),
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
    let db_path = std::path::PathBuf::from(&config.storage.path);
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
    for target in targets.values() {
        let db_clone = db.clone();
        let mut rx = target.sample_tx.subscribe();
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(64);
            loop {
                match rx.recv().await {
                    Ok(sample) => {
                        batch.push(sample);
                        // Flush in batches of 50 or when channel is empty
                        if batch.len() >= 50 {
                            let _ = db_clone.insert_batch(&batch);
                            batch.clear();
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }

                // Also flush if no more pending and batch has data
                if !batch.is_empty() && rx.is_empty() {
                    let _ = db_clone.insert_batch(&batch);
                    batch.clear();
                }
            }

            // Flush any remaining samples on channel close
            if !batch.is_empty() {
                let _ = db_clone.insert_batch(&batch);
            }
        });
    }

    // Spawn periodic maintenance (every 5 minutes)
    // - Prune samples older than retention
    // - Downsample: keep last hour at full resolution, older at 1-minute averages
    // - Size-based FIFO: when DB exceeds max size, delete oldest 10%
    // - Clean up stale telemetry_latest entries and old user layouts
    // - WAL checkpoint after large deletions
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
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;

            // Size-based FIFO: if DB exceeds max size, delete enough to free space
            if let Ok(stats) = db_maint.global_stats() {
                if stats.db_size_bytes > max_db_bytes && stats.total_samples > 0 {
                    // Calculate bytes to free, estimate samples to delete
                    let bytes_over = stats.db_size_bytes - max_db_bytes;
                    let avg_bytes_per_sample = stats.db_size_bytes / stats.total_samples.max(1);
                    // Delete 120% of estimated needed to account for index/overhead
                    let to_delete = ((bytes_over / avg_bytes_per_sample.max(1)) * 6 / 5).max(100);
                    if let Ok(n) = db_maint.delete_oldest(to_delete as usize) {
                        tracing::info!(
                            "DB size {:.0}MB > {:.0}MB limit, FIFO deleted {} oldest samples (~{:.0}MB)",
                            stats.db_size_bytes as f64 / 1_048_576.0,
                            max_db_bytes as f64 / 1_048_576.0,
                            n,
                            (n as u64 * avg_bytes_per_sample) as f64 / 1_048_576.0
                        );
                        // Reclaim freed pages immediately
                        let _ = db_maint.incremental_vacuum();
                    }
                }
            }

            // Time-based retention as a safety net
            match db_maint.prune(retention_ms) {
                Ok(n) if n > 0 => tracing::info!("Pruned {} samples beyond retention", n),
                _ => {}
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
                    let mut st = state_for_autoconnect.write().await;
                    if let Some(target) = st.targets.get_mut(&id) {
                        let host = target.config.host.clone();
                        let port = target.config.port;
                        let mut cli = target.client.lock().await;
                        match cli.connect(&host, port).await {
                            Ok(()) => {
                                tracing::info!(
                                    "auto_connect: {} connected to {}:{}",
                                    id,
                                    host,
                                    port
                                );
                                drop(cli);
                                let push_rx = target.push_tlm_tx.subscribe();
                                let handle = telemetry::spawn_router(
                                    id.clone(),
                                    push_rx,
                                    target.sample_tx.clone(),
                                    target.struct_dicts.clone(),
                                    target.manifest.clone(),
                                );
                                target._router_handle = Some(handle);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "auto_connect: {} failed to connect to {}:{}: {}",
                                    id,
                                    host,
                                    port,
                                    e
                                );
                            }
                        }
                    }
                }
            });
        }
    }

    let app = build_router(state.clone());

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
