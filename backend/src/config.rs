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
}

/// Authentication and rate-limiting configuration. Disabled by default
/// for development; set `enabled = true` to require JWT bearer tokens
/// on all `/api/*` routes (except `/api/auth/login` and `/api/health`).
#[derive(Debug, Clone, Deserialize)]
pub struct AuthSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_secret")]
    pub secret: String,
}

/// Storage layer configuration: SQLite path, retention, FIFO trigger.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageSection {
    #[serde(default = "default_db_path")]
    pub path: String,
    #[serde(default = "default_retention")]
    pub retention_hours: u32,
    /// Max DB size in MB before FIFO kicks in (default 2048 = 2GB)
    #[serde(default)]
    pub max_db_size_mb: Option<u32>,
    #[serde(default)]
    pub structs_dir: Option<String>,
}

/// One target's connection details and per-target config artifact paths.
/// One `[[targets]]` array entry in `config.toml` produces one of these.
#[derive(Debug, Clone, Deserialize)]
pub struct TargetSection {
    pub name: String,
    pub host: String,
    #[serde(default = "default_target_port")]
    pub port: u16,
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
fn default_retention() -> u32 {
    24
}
fn default_target_port() -> u16 {
    9000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: ServerSection {
                host: default_host(),
                port: default_port(),
            },
            auth: AuthSection {
                enabled: false,
                secret: default_secret(),
            },
            storage: StorageSection {
                path: default_db_path(),
                retention_hours: default_retention(),
                max_db_size_mb: None,
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
        }
    }
}

impl Default for AuthSection {
    fn default() -> Self {
        Self {
            enabled: false,
            secret: default_secret(),
        }
    }
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            path: default_db_path(),
            retention_hours: default_retention(),
            max_db_size_mb: None,
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
