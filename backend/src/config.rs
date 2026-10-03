//! Server configuration (TOML file parsing).

use std::path::Path;

use serde::{Deserialize, Serialize};

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
    /// Display policy for the dashboard health cards: which fields
    /// read as bad, and when. A bare name means "bad when nonzero";
    /// a table names the field and one comparison (eq, ne, ge, gt,
    /// le, lt) against a number. Ground-side judgement, not vehicle
    /// truth, so it lives in zenith config -- override per target to
    /// fit its components. Field names match lowercased with
    /// underscores stripped.
    #[serde(default = "default_health")]
    pub health: Vec<HealthRule>,
    /// Retired spelling of `health` (bare names only). Parsed only so
    /// a config that still uses it refuses boot by name.
    #[serde(default)]
    pub health_nonzero_bad: Option<Vec<String>>,
    /// CCSDS SPP targets: APID -> component fullUid routing table
    /// (keys and values as strings, decimal or 0x hex). Long term this
    /// arrives as generated target config alongside the manifest.
    #[serde(default)]
    pub apid_map: Option<std::collections::HashMap<String, String>>,
    /// raw-slip targets: the one component fullUid this header-less
    /// stream speaks for ("0x" hex or decimal).
    #[serde(default)]
    pub raw_uid: Option<String>,
    /// Stream targets: how bytes reach zenith. "tcp" (default)
    /// dials host:port and reads the stream; "udp" binds
    /// `listen_port` for inbound telemetry datagrams and sends
    /// outbound (init-step) datagrams to host:port; "tcp-listen"
    /// accepts the target dialing IN to `listen_port` (the
    /// push-to-ground pattern some flight stacks use). aproto-slip
    /// is TCP-dial only; validated at boot.
    #[serde(default = "default_carrier")]
    pub carrier: String,
    /// Listening carriers (udp, tcp-listen): the local port bound
    /// for inbound traffic. Required for both -- the target sends
    /// or dials to a configured port, so an OS-assigned one would
    /// never hear it.
    #[serde(default)]
    pub listen_port: Option<u16>,
    /// Retired spelling of `listen_port`. Accepted only so a config
    /// that still uses it refuses boot by name instead of silently
    /// losing its port.
    #[serde(default)]
    pub udp_listen_port: Option<u16>,
    /// tm+ccsds-spp targets: the fixed TM transfer frame length in
    /// octets (a mission constant of the producing build).
    #[serde(default = "default_tm_frame_size")]
    pub tm_frame_size: usize,
    /// Record-stage targets (tm+ccsds-spp+records): path to the
    /// generated record table (records.json in the target config
    /// directory).
    #[serde(default)]
    pub records_config: Option<String>,
    /// Path to this target's connect-time init sequence
    /// (on_connect.json): named steps of raw bytes sent, in order
    /// with optional delays, when the link comes up. Generated into
    /// the target config directory like every other artifact there.
    #[serde(default)]
    pub connect_init: Option<String>,
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
fn default_carrier() -> String {
    "tcp".to_string()
}
fn default_tm_frame_size() -> usize {
    1024
}
/// The default policy, callable from target-add paths that build a
/// TargetSection literal.
/// One health rule as written in config: a bare field name, or a
/// table with the field and exactly one comparison.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum HealthRule {
    NonzeroBad(String),
    Compare {
        field: String,
        #[serde(default)]
        eq: Option<f64>,
        #[serde(default)]
        ne: Option<f64>,
        #[serde(default)]
        ge: Option<f64>,
        #[serde(default)]
        gt: Option<f64>,
        #[serde(default)]
        le: Option<f64>,
        #[serde(default)]
        lt: Option<f64>,
    },
}

/// A health rule as the engine and the UI consume it: the normalized
/// field key, the comparison, and the bound.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthCheck {
    pub field: String,
    pub op: String,
    pub value: f64,
}

/// Normalize a field name the way the dashboard matches it.
pub fn health_field_key(name: &str) -> String {
    name.to_lowercase().replace('_', "")
}

impl HealthRule {
    /// The rule in its consumable form; an error names what is wrong
    /// (no comparison, more than one, or an empty field name).
    pub fn check(&self) -> Result<HealthCheck, String> {
        match self {
            HealthRule::NonzeroBad(name) => {
                if name.trim().is_empty() {
                    return Err("health rule with an empty field name".to_string());
                }
                Ok(HealthCheck {
                    field: health_field_key(name),
                    op: "ne".to_string(),
                    value: 0.0,
                })
            }
            HealthRule::Compare {
                field,
                eq,
                ne,
                ge,
                gt,
                le,
                lt,
            } => {
                if field.trim().is_empty() {
                    return Err("health rule with an empty field name".to_string());
                }
                let given: Vec<(&str, f64)> = [
                    ("eq", eq),
                    ("ne", ne),
                    ("ge", ge),
                    ("gt", gt),
                    ("le", le),
                    ("lt", lt),
                ]
                .into_iter()
                .filter_map(|(op, v)| v.map(|v| (op, v)))
                .collect();
                match given.as_slice() {
                    [(op, value)] => Ok(HealthCheck {
                        field: health_field_key(field),
                        op: op.to_string(),
                        value: *value,
                    }),
                    [] => Err(format!(
                        "health rule for '{}' has no comparison (eq, ne, ge, gt, le, lt)",
                        field
                    )),
                    many => Err(format!(
                        "health rule for '{}' has {} comparisons; give one",
                        field,
                        many.len()
                    )),
                }
            }
        }
    }
}

impl TargetSection {
    /// Every health rule in consumable form, or the first error.
    pub fn health_checks(&self) -> Result<Vec<HealthCheck>, String> {
        self.health.iter().map(HealthRule::check).collect()
    }
}

pub fn default_health_public() -> Vec<HealthRule> {
    default_health()
}

fn default_health() -> Vec<HealthRule> {
    default_health_names()
        .into_iter()
        .map(HealthRule::NonzeroBad)
        .collect()
}

fn default_health_names() -> Vec<String> {
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

/// Find the first local listen port claimed by two target
/// definitions of the same transport kind (TCP and UDP port spaces
/// are distinct). Distinct targets binding one port cannot both
/// hear their telemetry, and the loser would only find out at
/// connect time -- so this is a boot refusal, same discipline as
/// protocol and carrier typos.
pub fn duplicate_listen_port(targets: &[TargetSection]) -> Option<(u16, &str, &str)> {
    let mut seen: Vec<(&str, u16, &str)> = Vec::new();
    for t in targets {
        if t.carrier != "udp" && t.carrier != "tcp-listen" {
            continue;
        }
        let Some(port) = t.listen_port else {
            continue;
        };
        if let Some((_, _, first)) = seen
            .iter()
            .find(|(c, p, _)| *c == t.carrier.as_str() && *p == port)
        {
            return Some((port, first, t.name.as_str()));
        }
        seen.push((t.carrier.as_str(), port, t.name.as_str()));
    }
    None
}

/// A retired key must not be ignored: serde drops unknown keys, so
/// the retired spelling is still parsed and refused with the new
/// name, which is the only way the operator learns why last week's
/// config stopped booting.
pub fn retired_key(t: &TargetSection) -> Option<String> {
    if let Some(p) = t.udp_listen_port {
        return Some(format!(
            "udp_listen_port = {p} was renamed: use listen_port = {p}"
        ));
    }
    if t.health_nonzero_bad.is_some() {
        return Some(
            "health_nonzero_bad was renamed: use health = [...] (bare names keep the \
             nonzero-bad meaning; tables add comparisons)"
                .to_string(),
        );
    }
    None
}

/// A TM-framed target's frame length must hold at least a header,
/// one minimal packet and the trailer, or the deframer could never
/// emit anything -- a length that cannot hold one refuses boot like
/// every other definition error instead of clamping silently.
pub fn invalid_tm_frame_size(t: &TargetSection) -> Option<String> {
    use crate::protocol::ccsds_tm::MIN_FRAME;
    if !t.protocol.starts_with("tm+") || t.tm_frame_size >= MIN_FRAME {
        return None;
    }
    Some(format!(
        "tm_frame_size {} is below the {}-octet minimum (header + one idle \
         packet + trailer)",
        t.tm_frame_size, MIN_FRAME
    ))
}

/* ----------------------------- Loading ----------------------------- */

/// Parse a TOML config file from disk into a `ServerConfig`. Every
/// failure (unreadable file, parse error, bad environment override)
/// is an error naming the file; there is no default configuration to
/// fall back to, and the boot path exits on the error.
pub fn load(path: &Path) -> Result<ServerConfig, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let cfg: ServerConfig =
        toml::from_str(&content).map_err(|e| format!("{}: parse error: {}", path.display(), e))?;
    apply_env_overrides(cfg, |k| std::env::var(k).ok())
}

/// The few settings a deployment sets from outside the file: the
/// front-door port, the database path, and the token-signing secret.
/// Everything else is what the file says, so a container can ship a
/// complete config and still take these from its environment.
///
/// - `ZENITH_PORT`: `[server] port`
/// - `ZENITH_DB_PATH`: `[storage] path`
/// - `ZENITH_AUTH_SECRET`: `[auth] secret` (keeps the secret out of
///   a mounted file and out of the image)
///
/// A set-but-unparseable value refuses to load: a wrong port is a
/// misconfiguration, not a hint to fall back to the file.
pub fn apply_env_overrides(
    mut cfg: ServerConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ServerConfig, String> {
    if let Some(v) = lookup("ZENITH_PORT") {
        cfg.server.port = v
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("ZENITH_PORT '{}' is not a port number", v))?;
    }
    if let Some(v) = lookup("ZENITH_DB_PATH") {
        if v.trim().is_empty() {
            return Err("ZENITH_DB_PATH is set but empty".to_string());
        }
        cfg.storage.path = v;
    }
    if let Some(v) = lookup("ZENITH_AUTH_SECRET") {
        cfg.auth.secret = v;
    }
    Ok(cfg)
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, carrier: &str, listen: Option<u16>) -> TargetSection {
        TargetSection {
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port: 9000,
            protocol: "ccsds-spp".to_string(),
            health: Vec::new(),
            health_nonzero_bad: None,
            apid_map: None,
            raw_uid: None,
            carrier: carrier.to_string(),
            listen_port: listen,
            udp_listen_port: None,
            tm_frame_size: default_tm_frame_size(),
            records_config: None,
            connect_init: None,
            manifest: None,
            structs_dir: None,
            telemetry_config: None,
            commands_config: None,
            auto_connect: false,
        }
    }

    /// @test Two UDP targets on one listen port are named in the
    /// refusal; distinct ports, TCP targets, and portless entries
    /// (caught separately at carrier validation) all pass.
    /// @test Health rules parse in both forms and normalize to one
    /// check each: a bare name is nonzero-bad, a table carries its one
    /// comparison; no comparison, two comparisons, an empty name, and
    /// the retired health_nonzero_bad key are all refused by name.
    #[test]
    fn health_rules_normalize_and_validate() {
        let t: TargetSection = toml::from_str(
            r#"
            name = "t"
            host = "h"
            port = 1
            health = [
              "is_slipping",
              { field = "last_cmd_result", ge = 2 },
              { field = "Board_Link", eq = 2 },
            ]
            "#,
        )
        .unwrap();
        let checks = t.health_checks().unwrap();
        assert_eq!(
            checks,
            vec![
                HealthCheck {
                    field: "isslipping".into(),
                    op: "ne".into(),
                    value: 0.0
                },
                HealthCheck {
                    field: "lastcmdresult".into(),
                    op: "ge".into(),
                    value: 2.0
                },
                HealthCheck {
                    field: "boardlink".into(),
                    op: "eq".into(),
                    value: 2.0
                },
            ]
        );
        for (rules, why) in [
            (r#"[{ field = "x" }]"#, "no comparison"),
            (r#"[{ field = "x", ge = 1, lt = 5 }]"#, "2 comparisons"),
            (r#"[""]"#, "empty field name"),
        ] {
            let t: TargetSection = toml::from_str(&format!(
                "name = \"t\"\nhost = \"h\"\nport = 1\nhealth = {rules}\n"
            ))
            .unwrap();
            let e = t.health_checks().unwrap_err();
            assert!(e.contains(why), "{e}");
        }
        let old: TargetSection =
            toml::from_str("name = \"t\"\nhost = \"h\"\nport = 1\nhealth_nonzero_bad = [\"x\"]\n")
                .unwrap();
        assert!(retired_key(&old)
            .unwrap()
            .contains("health_nonzero_bad was renamed"));
    }

    /// @test The retired udp_listen_port key refuses boot naming the
    /// new key and carrying the value over, instead of being dropped.
    #[test]
    fn retired_listen_port_key_is_refused_by_name() {
        let mut t = target("old", "udp", None);
        t.udp_listen_port = Some(2234);
        let msg = retired_key(&t).expect("retired key must refuse");
        assert!(msg.contains("listen_port = 2234"), "{msg}");
        t.udp_listen_port = None;
        assert!(retired_key(&t).is_none());
    }

    /// @test An undersized TM frame length is refused by name for the
    /// TM stacks only; other protocols ignore the field.
    #[test]
    fn undersized_tm_frames_are_refused() {
        let mut t = target("tm", "tcp-listen", Some(50050));
        t.protocol = "tm+ccsds-spp".to_string();
        t.tm_frame_size = 8;
        let msg = invalid_tm_frame_size(&t).expect("8 octets cannot hold a frame");
        assert!(msg.contains("tm_frame_size 8 "), "{msg}");
        t.tm_frame_size = 1024;
        assert!(invalid_tm_frame_size(&t).is_none());
        t.protocol = "ccsds-spp".to_string();
        t.tm_frame_size = 1;
        assert!(invalid_tm_frame_size(&t).is_none(), "field is unused here");
    }

    /// @test Environment overrides reach exactly the three settings a
    /// deployment sets from outside the file, an unset variable leaves
    /// the file's value, and a set-but-invalid port refuses to load
    /// by name.
    #[test]
    fn env_overrides_cover_port_db_path_and_secret() {
        let base = || -> ServerConfig { toml::from_str("[server]\nport = 8080\n").unwrap() };
        let vars = std::collections::HashMap::from([
            ("ZENITH_PORT", "9090"),
            ("ZENITH_DB_PATH", "/srv/zenith/zenith.db"),
            ("ZENITH_AUTH_SECRET", "a-signing-secret-of-length"),
        ]);
        let cfg = apply_env_overrides(base(), |k| vars.get(k).map(|v| v.to_string())).unwrap();
        assert_eq!(cfg.server.port, 9090);
        assert_eq!(cfg.storage.path, "/srv/zenith/zenith.db");
        assert_eq!(cfg.auth.secret, "a-signing-secret-of-length");

        let untouched = apply_env_overrides(base(), |_| None).unwrap();
        assert_eq!(untouched.server.port, 8080);
        assert_eq!(untouched.storage.path, base().storage.path);

        let e = apply_env_overrides(base(), |k| (k == "ZENITH_PORT").then(|| "http".to_string()))
            .unwrap_err();
        assert!(e.contains("ZENITH_PORT 'http'"), "{e}");
        let e =
            apply_env_overrides(base(), |k| (k == "ZENITH_DB_PATH").then(String::new)).unwrap_err();
        assert!(e.contains("ZENITH_DB_PATH"), "{e}");
    }

    #[test]
    fn duplicate_listen_ports_are_refused_by_name() {
        let dup = [
            target("a", "udp", Some(2234)),
            target("b", "tcp", None),
            target("c", "udp", Some(2235)),
            target("d", "udp", Some(2234)),
        ];
        assert_eq!(duplicate_listen_port(&dup), Some((2234, "a", "d")));

        let ok = [
            target("a", "udp", Some(2234)),
            target("b", "udp", Some(2235)),
            // TCP targets share nothing local; a port collision with
            // a UDP listener is impossible across protocols here.
            target("c", "tcp", None),
            target("d", "udp", None),
        ];
        assert_eq!(duplicate_listen_port(&ok), None);
    }

    /// @test A config that cannot be read or parsed is an error that
    /// names the file; there is no default to fall back to. A readable
    /// file still loads.
    #[test]
    fn unreadable_or_malformed_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("absent.toml");
        let err = load(&missing).unwrap_err();
        assert!(err.contains("absent.toml"), "{err}");

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[server]\nport = 8080\nthis line is not toml\n").unwrap();
        let err = load(&bad).unwrap_err();
        assert!(err.contains("bad.toml"), "{err}");
        assert!(err.contains("line 3"), "{err}");

        let good = dir.path().join("good.toml");
        std::fs::write(&good, "[server]\nport = 8080\n").unwrap();
        assert!(load(&good).is_ok());
    }
}
