//! Registration-time setup a target needs beyond its link, shared by
//! boot and runtime Add Target.
//!
//! A target's curated plots are served from the database, not from its
//! telemetry.json: the layouts endpoint and the Telemetry page read the
//! seeded rows. Registering a target therefore parses the file and
//! seeds its layouts under the target's id. Both entry points call
//! [`register_layouts`], so a target added to a running server gets the
//! same plots a configured one does.

use std::path::Path;
use std::sync::Arc;

use crate::core::config_manager::TelemetryConfig;
use crate::storage::telemetry_db::TelemetryDb;

/* ----------------------------- API ----------------------------- */

/// Parse a target's telemetry.json. A display artifact: a missing,
/// unreadable or invalid file is logged and costs the target its plots,
/// never the target.
pub fn load_layouts(name: &str, path: &Path) -> Option<TelemetryConfig> {
    match TelemetryConfig::load(path) {
        Ok(cfg) => {
            tracing::info!(
                "Loaded telemetry config for {}: {} layouts",
                name,
                cfg.layouts.len()
            );
            for w in cfg.validate() {
                tracing::warn!("  telemetry config warning [{}]: {}", name, w);
            }
            Some(cfg)
        }
        Err(e) => {
            tracing::warn!("Failed to load telemetry config for {}: {}", name, e);
            None
        }
    }
}

/// Seed parsed layouts under a target id. A failed seed is logged and
/// leaves the target registered without curated plots.
pub fn seed_layouts(db: &TelemetryDb, target_id: &str, config: &TelemetryConfig) {
    match db.seed_layouts(target_id, config) {
        Ok(n) if n > 0 => tracing::info!("Seeded {} telemetry layouts for {}", n, target_id),
        Ok(_) => {}
        Err(e) => tracing::warn!("Failed to seed layouts for {}: {}", target_id, e),
    }
}

/// Register a target's curated layouts: parse its telemetry.json when
/// it has one, seed the layouts under `target_id`, and return the
/// parsed config.
pub fn register_layouts(
    db: &TelemetryDb,
    target_id: &str,
    name: &str,
    path: Option<&str>,
) -> Option<Arc<TelemetryConfig>> {
    let config = load_layouts(name, Path::new(path?))?;
    seed_layouts(db, target_id, &config);
    Some(Arc::new(config))
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_db(dir: &TempDir) -> TelemetryDb {
        TelemetryDb::open(&dir.path().join("zenith.db")).unwrap()
    }

    /// Config-sourced layout names the layouts endpoint would serve for
    /// a target, in the order it serves them.
    fn served_names(db: &TelemetryDb, target_id: &str) -> Vec<String> {
        db.get_layouts(target_id)
            .unwrap()
            .into_iter()
            .filter(|l| l.source == "config")
            .map(|l| l.name)
            .collect()
    }

    /// @test Registering a target directory seeds its telemetry.json:
    /// the layouts the endpoint reads for that target id are the
    /// file's, in file order (not name order), with their plots.
    #[test]
    fn register_layouts_seeds_the_file_in_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("telemetry.json");
        std::fs::write(
            &path,
            r#"{"layouts": [
                {"name": "Zeta", "plots": [{"title": "z", "channels": ["A.z"]}]},
                {"name": "Alpha", "plots": [{"title": "a", "channels": ["A.a", "A.b"]}]},
                {"name": "Mid", "plots": [{"title": "m", "channels": ["B.m"]}]}
            ]}"#,
        )
        .unwrap();
        let db = temp_db(&dir);

        let cfg = register_layouts(&db, "target-1a2b3c4d", "Added", path.to_str()).unwrap();

        assert_eq!(cfg.layouts.len(), 3);
        assert_eq!(
            served_names(&db, "target-1a2b3c4d"),
            ["Zeta", "Alpha", "Mid"]
        );
        let alpha = db
            .get_layouts("target-1a2b3c4d")
            .unwrap()
            .into_iter()
            .find(|l| l.name == "Alpha")
            .unwrap();
        assert_eq!(alpha.plots[0].channels, vec!["A.a", "A.b"]);
        // Seeding is per target id: nothing lands under another id.
        assert!(served_names(&db, "target-0").is_empty());
    }

    /// @test The shipped rover directory registers the curated layouts
    /// its own telemetry.json declares, in the file's order.
    #[test]
    fn register_layouts_seeds_the_rover_directory() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../targets/rover-mcu/telemetry.json"
        );
        let declared: Vec<String> = TelemetryConfig::load(Path::new(path))
            .unwrap()
            .layouts
            .into_iter()
            .map(|l| l.name)
            .collect();
        assert!(declared.len() > 1, "fixture must declare several layouts");
        let dir = TempDir::new().unwrap();
        let db = temp_db(&dir);

        register_layouts(&db, "target-5e6f7a8b", "Rover copy", Some(path)).unwrap();

        assert_eq!(served_names(&db, "target-5e6f7a8b"), declared);
    }

    /// @test A target without telemetry.json, or with one that does not
    /// parse, registers no layouts and keeps the target: nothing is
    /// seeded and no error escapes.
    #[test]
    fn register_layouts_without_a_usable_file_seeds_nothing() {
        let dir = TempDir::new().unwrap();
        let db = temp_db(&dir);
        assert!(register_layouts(&db, "target-a", "None", None).is_none());

        let missing = dir.path().join("absent.json");
        assert!(register_layouts(&db, "target-b", "Missing", missing.to_str()).is_none());

        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{ not json").unwrap();
        assert!(register_layouts(&db, "target-c", "Broken", broken.to_str()).is_none());

        for id in ["target-a", "target-b", "target-c"] {
            assert!(
                served_names(&db, id).is_empty(),
                "{id} must have no layouts"
            );
        }
    }
}
