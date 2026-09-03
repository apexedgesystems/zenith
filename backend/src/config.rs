//! Server configuration (TOML file parsing).

use std::path::Path;

use serde::Deserialize;

/* ----------------------------- Config Types ----------------------------- */

/// Top-level zenith server configuration loaded from `config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default)]
    pub storage: StorageSection,
    #[serde(default = "Vec::new")]
    pub targets: Vec<TargetSection>,
}

/// HTTP listener configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerSection {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Ceiling for file/library upload payloads, megabytes (decoded).
    #[serde(default = "default_upload_max_mb")]
    pub upload_max_mb: u32,
    /// Origins allowed to call the API cross-origin. Empty (the
    /// default) means same-origin only -- zenith serves its own
    /// frontend, so cross-origin access is opt-in.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
}

fn default_upload_max_mb() -> u32 {
    50
}

/// Authentication and rate-limiting configuration. Disabled by default
/// for development; set `enabled = true` to require JWT bearer tokens
/// on all `/api/*` routes (except `/api/auth/login` and `/api/health`).
#[derive(Debug, Clone, Deserialize)]
pub struct AuthSection {
    #[serde(default)]
    pub enabled: bool,
    /// JWT signing key ONLY -- never a login credential. Startup
    /// refuses to boot with the default value while auth is enabled.
    #[serde(default = "default_secret")]
    pub secret: String,
    /// Login username (single-operator scheme).
    #[serde(default = "default_username")]
    pub username: String,
    /// argon2 PHC hash of the login password. Generate with
    /// `zenith --hash-password`. Required when auth is enabled.
    #[serde(default)]
    pub password_hash: String,
}

fn default_username() -> String {
    "admin".to_string()
}

/// Storage layer configuration: SQLite path, retention, FIFO trigger.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageSection {
    #[serde(default = "default_db_path")]
    pub path: String,
    #[serde(default = "default_retention")]
    pub retention_hours: u32,
    /// Audit rows older than this many days are pruned by the
    /// maintenance loop. 0 keeps the log forever (pre-existing
    /// behavior; unbounded growth inside the size-capped DB file).
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u32,
    /// Max DB size in MB before FIFO kicks in (default 2048 = 2GB)
    #[serde(default)]
    pub max_db_size_mb: Option<u32>,
    /// How the size-cap FIFO distributes evictions across targets.
    #[serde(default)]
    pub fifo_strategy: FifoStrategy,
    /// Age-based multi-resolution retention ladder (off by default).
    #[serde(default)]
    pub tiers: TiersSection,
    #[serde(default)]
    pub structs_dir: Option<String>,
}

/// The retention ladder: full resolution for the newest window, then
/// envelope buckets (mean + min/max + count) at two coarser tiers.
/// Age-triggered, not fill-triggered -- deterministic and free of
/// feedback loops with the size-cap FIFO, which stays the final
/// backstop and naturally evicts the oldest (coarsest) rows first.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct TiersSection {
    #[serde(default)]
    pub enabled: bool,
    /// Newest window kept at full resolution.
    #[serde(default = "default_full_res_minutes")]
    pub full_resolution_minutes: u32,
    /// Bucket width for the mid tier (seconds).
    #[serde(default = "default_mid_bucket_seconds")]
    pub mid_bucket_seconds: u32,
    /// Age at which mid-tier buckets re-tier to coarse.
    #[serde(default = "default_mid_horizon_hours")]
    pub mid_horizon_hours: u32,
    /// Bucket width for the coarse tier (seconds).
    #[serde(default = "default_coarse_bucket_seconds")]
    pub coarse_bucket_seconds: u32,
}

impl Default for TiersSection {
    fn default() -> Self {
        Self {
            enabled: false,
            full_resolution_minutes: default_full_res_minutes(),
            mid_bucket_seconds: default_mid_bucket_seconds(),
            mid_horizon_hours: default_mid_horizon_hours(),
            coarse_bucket_seconds: default_coarse_bucket_seconds(),
        }
    }
}

fn default_full_res_minutes() -> u32 {
    60
}
fn default_mid_bucket_seconds() -> u32 {
    1
}
fn default_mid_horizon_hours() -> u32 {
    24
}
fn default_coarse_bucket_seconds() -> u32 {
    60
}

/// Eviction distribution for the size-cap FIFO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FifoStrategy {
    /// Waterline the largest holders down to a common level so one
    /// chatty target cannot evict a quiet target's history.
    #[default]
    Fair,
    /// Oldest rows go first regardless of owner.
    Global,
}

/// One target's connection details and per-target config artifact paths.
/// One `[[targets]]` array entry in `config.toml` produces one of these.
#[derive(Debug, Clone, Deserialize)]
pub struct TargetSection {
    pub name: String,
    pub host: String,
    #[serde(default = "default_target_port")]
    pub port: u16,
    /// Wire protocol this target speaks. Validated at boot against
    /// the registered transports; a typo refuses to boot rather than
    /// silently defaulting.
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// Display policy: telemetry field names (lowercased, underscores
    /// stripped) flagged as bad when nonzero on the dashboard health
    /// cards. Ground-side judgement, not vehicle truth, so it lives in
    /// zenith config -- override per target to fit its components.
    #[serde(default = "default_health_nonzero_bad")]
    pub health_nonzero_bad: Vec<String>,
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default)]
    pub structs_dir: Option<String>,
    #[serde(default)]
    pub telemetry_config: Option<String>,
    #[serde(default)]
    pub commands_config: Option<String>,
    #[serde(default)]
    pub auto_connect: bool,
}

/* ----------------------------- Defaults ----------------------------- */

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    8080
}
fn default_secret() -> String {
    "change-me-in-production".to_string()
}
fn default_db_path() -> String {
    "./data/zenith.db".to_string()
}
fn default_audit_retention_days() -> u32 {
    90
}

fn default_retention() -> u32 {
    24
}
fn default_target_port() -> u16 {
    9000
}
fn default_protocol() -> String {
    "aproto-slip".to_string()
}
/// The default policy, callable from target-add paths that build a
/// TargetSection literal.
pub fn default_health_nonzero_bad_public() -> Vec<String> {
    default_health_nonzero_bad()
}

fn default_health_nonzero_bad() -> Vec<String> {
    [
        "overruns",
        "frameoverruns",
        "watchdogwarnings",
        "watchdogwarns",
        "totalperiodviolations",
        "violationsthistick",
        "totalskipcount",
        "packetsinvalid",
        "framingerrors",
        "cmdqueueoverflows",
        "tlmqueueoverflows",
        "internalcommandsfailed",
        "warncount",
        "critcount",
    ]
    .map(String::from)
    .to_vec()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSection {
                host: default_host(),
                port: default_port(),
                upload_max_mb: default_upload_max_mb(),
                cors_allowed_origins: Vec::new(),
            },
            auth: AuthSection {
                enabled: false,
                secret: default_secret(),
                username: default_username(),
                password_hash: String::new(),
            },
            storage: StorageSection {
                path: default_db_path(),
                retention_hours: default_retention(),
                audit_retention_days: default_audit_retention_days(),
                max_db_size_mb: None,
                fifo_strategy: FifoStrategy::default(),
                tiers: TiersSection::default(),
                structs_dir: None,
            },
            targets: Vec::new(),
        }
    }
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            upload_max_mb: default_upload_max_mb(),
            cors_allowed_origins: Vec::new(),
        }
    }
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            enabled: false,
            secret: default_secret(),
            username: default_username(),
            password_hash: String::new(),
        }
    }
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            path: default_db_path(),
            retention_hours: default_retention(),
            audit_retention_days: default_audit_retention_days(),
            max_db_size_mb: None,
            fifo_strategy: FifoStrategy::default(),
            tiers: TiersSection::default(),
            structs_dir: None,
        }
    }
}

/* ----------------------------- Loading ----------------------------- */

/// Parse a TOML config file from disk into a `ServerConfig`. Returns
/// a human-readable error string on parse or I/O failure.
pub fn load(path: &Path) -> Result<ServerConfig, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    toml::from_str(&content).map_err(|e| format!("parse error: {}", e))
}
