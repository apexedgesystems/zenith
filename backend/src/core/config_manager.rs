//! Struct dictionary and app manifest loading.
//!
//! Loads JSON struct dictionaries produced by apex_data_gen and app
//! manifests from a target's configuration directory. Provides field-level
//! type information for decoding telemetry and generating command forms.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/* ----------------------------- Types ----------------------------- */

/// A single field in a struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub offset: usize,
    pub size: usize,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(default)]
    pub element_type: Option<String>,
    #[serde(default)]
    pub dims: Option<Vec<usize>>,
}

/// A struct definition with its fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructDef {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub size: usize,
    #[serde(default)]
    pub opcode: Option<String>,
    #[serde(default)]
    pub fields: Vec<FieldDef>,
    /// Producer-stated v3 layout hash ("0x" hex), exported by
    /// apex_data_gen for spec-defined structs. When present it is THE
    /// hash the vehicle verifies -- consumers must not recompute.
    #[serde(default)]
    pub layout_hash: Option<String>,
    /// The canonical field-spec string the hash derives from
    /// (diagnostic surface; the hash is the contract).
    #[serde(default)]
    pub canonical_spec: Option<String>,
}

impl StructDef {
    /// The producer-stated layout hash as the u32 the prelude carries,
    /// when the dictionary exports one.
    pub fn layout_hash_u32(&self) -> Option<u32> {
        let s = self.layout_hash.as_deref()?;
        let hex = s.trim_start_matches("0x").trim_start_matches("0X");
        u32::from_str_radix(hex, 16).ok()
    }
}

/// A component's struct dictionary (one JSON file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentDict {
    pub component: String,
    #[serde(default)]
    pub structs: HashMap<String, StructDef>,
    #[serde(default)]
    pub enums: HashMap<String, serde_json::Value>,
}

/// All loaded struct dictionaries keyed by component name.
#[derive(Debug, Clone, Default)]
pub struct StructDictionary {
    pub components: HashMap<String, ComponentDict>,
}

/* ----------------------------- Loading ----------------------------- */

impl StructDictionary {
    /// Load all JSON struct dictionaries from a directory.
    pub fn load_dir(dir: &Path) -> Result<Self, String> {
        let mut components = HashMap::new();

        if !dir.exists() {
            return Err(format!("Directory does not exist: {}", dir.display()));
        }

        for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {}", dir.display(), e))? {
            let entry = entry.map_err(|e| format!("read_dir: {}", e))?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Skip non-struct-dict files (e.g. app_manifest.json)
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("app_") || name.starts_with("manifest") {
                    continue;
                }
            }

            let content =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?;

            match serde_json::from_str::<ComponentDict>(&content) {
                Ok(dict) => {
                    tracing::info!(
                        "Loaded struct dict: {} ({} structs)",
                        dict.component,
                        dict.structs.len()
                    );
                    components.insert(dict.component.clone(), dict);
                }
                Err(e) => {
                    tracing::warn!("Failed to parse {}: {}", path.display(), e);
                }
            }
        }

        tracing::info!(
            "Loaded {} component dictionaries from {}",
            components.len(),
            dir.display()
        );
        Ok(Self { components })
    }

    /// Decode a binary payload using a struct definition.
    ///
    /// Returns a JSON object with field names mapped to decoded values.
    pub fn decode_payload(
        &self,
        component: &str,
        struct_name: &str,
        data: &[u8],
    ) -> Option<serde_json::Value> {
        let dict = self.components.get(component)?;
        let sdef = dict.structs.get(struct_name)?;

        let mut fields = serde_json::Map::new();
        for field in &sdef.fields {
            if field.offset + field.size > data.len() {
                continue;
            }
            let value = decode_field(data, field);
            fields.insert(field.name.clone(), value);
        }

        Some(serde_json::Value::Object(fields))
    }

    /// Get all struct definitions for a component.
    pub fn get_component(&self, name: &str) -> Option<&ComponentDict> {
        self.components.get(name)
    }

    /// Validate struct dictionary contents. Returns list of warnings.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        for (comp_name, dict) in &self.components {
            if dict.structs.is_empty() {
                warnings.push(format!("{}: no structs defined", comp_name));
            }
            for (sname, sdef) in &dict.structs {
                if sdef.fields.is_empty() && sdef.size > 0 {
                    warnings.push(format!(
                        "{}/{}: has size {} but no fields",
                        comp_name, sname, sdef.size
                    ));
                }
                // Check field offsets don't exceed struct size
                for field in &sdef.fields {
                    if sdef.size > 0 && field.offset + field.size > sdef.size && field.size > 0 {
                        warnings.push(format!(
                            "{}/{}.{}: field extends past struct (offset {} + size {} > {})",
                            comp_name, sname, field.name, field.offset, field.size, sdef.size
                        ));
                    }
                }
            }
        }

        warnings
    }

    /// Decode variable-length payload, optionally restricted to a specific component.
    pub fn decode_variable_length_for(
        &self,
        category: &str,
        data: &[u8],
        component_hint: Option<&str>,
    ) -> Option<(String, String, String, serde_json::Value)> {
        let data_len = data.len();

        for dict in self.components.values() {
            // If component hint provided, only search matching components
            if let Some(hint) = component_hint {
                let dn = dict.component.to_lowercase();
                let hn = hint.to_lowercase();
                if dn != hn && !dn.contains(&hn) && !hn.contains(&dn) {
                    continue;
                }
            }
            let mut candidates: Vec<(&str, &StructDef)> = Vec::new();
            for (sname, sdef) in &dict.structs {
                if sdef.category == category && !sdef.fields.is_empty() && sdef.size > 0 {
                    candidates.push((sname, sdef));
                }
            }

            // Try each pair (header, entry) where header is the SMALLER struct
            // and header.size + N*entry.size == data_len
            for i in 0..candidates.len() {
                for j in 0..candidates.len() {
                    if i == j {
                        continue;
                    }
                    let (hdr_name, hdr_def) = candidates[i];
                    let (ent_name, ent_def) = candidates[j];

                    // Header must be smaller than data
                    if hdr_def.size >= data_len {
                        continue;
                    }

                    let remainder = data_len - hdr_def.size;
                    if ent_def.size == 0
                        || remainder == 0
                        || !remainder.is_multiple_of(ent_def.size)
                    {
                        continue;
                    }

                    let n_entries = remainder / ent_def.size;

                    // Decode header and validate: at least one header field
                    // should match the entry count (e.g. numTasks == n_entries).
                    // This prevents matching the wrong pair orientation.
                    let mut hdr_fields = serde_json::Map::new();
                    let mut count_matches = false;
                    for field in &hdr_def.fields {
                        if field.offset + field.size <= hdr_def.size {
                            let val = decode_field(data, field);
                            // Check if this field's value matches the entry count
                            if let Some(n) = val.as_u64() {
                                if n == n_entries as u64 {
                                    count_matches = true;
                                }
                            }
                            hdr_fields.insert(field.name.clone(), val);
                        }
                    }

                    // If no header field matches the entry count, this is likely
                    // the wrong pair orientation. Skip unless no other match found.
                    if !count_matches && candidates.len() > 1 {
                        continue;
                    }

                    let mut result = serde_json::Map::new();
                    result.insert("header".into(), serde_json::Value::Object(hdr_fields));

                    // Decode entries
                    let mut entries = Vec::with_capacity(n_entries);
                    for idx in 0..n_entries {
                        let base = hdr_def.size + idx * ent_def.size;
                        let entry_data = &data[base..base + ent_def.size];
                        let mut ent_fields = serde_json::Map::new();
                        for field in &ent_def.fields {
                            if field.offset + field.size <= ent_def.size {
                                ent_fields
                                    .insert(field.name.clone(), decode_field(entry_data, field));
                            }
                        }
                        entries.push(serde_json::Value::Object(ent_fields));
                    }
                    result.insert("entries".into(), serde_json::Value::Array(entries));

                    return Some((
                        dict.component.clone(),
                        hdr_name.to_string(),
                        ent_name.to_string(),
                        serde_json::Value::Object(result),
                    ));
                }
            }
        }

        None
    }
}

/* ----------------------------- Field Decoding ----------------------------- */

fn decode_field(data: &[u8], field: &FieldDef) -> serde_json::Value {
    let off = field.offset;
    match (field.field_type.as_str(), field.size) {
        ("uint", 1) => serde_json::json!(data[off]),
        ("uint", 2) => serde_json::json!(u16::from_le_bytes([data[off], data[off + 1]])),
        ("uint", 4) => {
            serde_json::json!(u32::from_le_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3]
            ]))
        }
        ("uint", 8) => {
            serde_json::json!(u64::from_le_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3],
                data[off + 4],
                data[off + 5],
                data[off + 6],
                data[off + 7],
            ]))
        }
        ("int", 1) => serde_json::json!(data[off] as i8),
        ("int", 2) => serde_json::json!(i16::from_le_bytes([data[off], data[off + 1]])),
        ("int", 4) => {
            serde_json::json!(i32::from_le_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3]
            ]))
        }
        ("float", 4) => {
            let v = f32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            serde_json::json!(v)
        }
        ("float", 8) | ("double", 8) => {
            let v = f64::from_le_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3],
                data[off + 4],
                data[off + 5],
                data[off + 6],
                data[off + 7],
            ]);
            serde_json::json!(v)
        }
        _ => {
            // Unknown type -- return hex
            let bytes = &data[off..off + field.size.min(data.len() - off)];
            serde_json::json!(hex::encode(bytes))
        }
    }
}

/* ----------------------------- App Manifest ----------------------------- */

/// A component entry from the application manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestComponent {
    pub name: String,
    #[serde(rename = "fullUid")]
    pub full_uid: String,
    #[serde(default, rename = "type")]
    pub comp_type: String,
    #[serde(default, rename = "instanceIndex")]
    pub instance_index: Option<u32>,
    #[serde(default, rename = "dataFile")]
    pub data_file: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Application manifest loaded from JSON build artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManifest {
    pub application: String,
    #[serde(default)]
    pub description: String,
    pub components: Vec<ManifestComponent>,
    #[serde(flatten)]
    pub extra: HashMap<String, Json>,
}

impl AppManifest {
    /// Load manifest from a JSON file path.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        serde_json::from_str(&content).map_err(|e| format!("{}: {}", path.display(), e))
    }

    /// Validate manifest contents. Returns list of warnings (non-fatal).
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        if self.application.is_empty() {
            warnings.push("Missing 'application' name".to_string());
        }
        if self.components.is_empty() {
            warnings.push("No components defined".to_string());
        }

        // Check for Executive (UID 0x000000) -- the one guaranteed component
        let has_executive = self.components.iter().any(|c| {
            let uid = c.full_uid.trim_start_matches("0x").trim_start_matches("0X");
            uid == "000000"
        });
        if !has_executive {
            warnings.push("No Executive component (fullUid 0x000000) found".to_string());
        }

        // Check for duplicate UIDs
        let mut seen = std::collections::HashSet::new();
        for c in &self.components {
            if !seen.insert(&c.full_uid) {
                warnings.push(format!("Duplicate fullUid: {}", c.full_uid));
            }
        }

        // Validate UID format
        for c in &self.components {
            let uid_clean = c.full_uid.trim_start_matches("0x").trim_start_matches("0X");
            if u32::from_str_radix(uid_clean, 16).is_err() {
                warnings.push(format!(
                    "Invalid fullUid '{}' for component '{}'",
                    c.full_uid, c.name
                ));
            }
        }

        warnings
    }

    /// Get component list as (fullUid_u32, display_name) pairs for telemetry routing.
    pub fn component_uids(&self) -> Vec<(u32, String)> {
        self.components
            .iter()
            .filter_map(|c| {
                let uid_clean = c.full_uid.trim_start_matches("0x").trim_start_matches("0X");
                let uid = u32::from_str_radix(uid_clean, 16).ok()?;
                let display = if let Some(idx) = c.instance_index {
                    format!("{}#{}", c.name, idx)
                } else {
                    c.name.clone()
                };
                Some((uid, display))
            })
            .collect()
    }
}

/* ----------------------------- Telemetry Config ----------------------------- */

/// A single plot definition within a telemetry layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlotDef {
    pub title: String,
    pub channels: Vec<String>,
    #[serde(default = "default_plot_height")]
    pub height: u16,
}

fn default_plot_height() -> u16 {
    180
}

/// A named collection of plots (a "layout" or "screen").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryLayout {
    pub name: String,
    pub plots: Vec<PlotDef>,
}

/// Telemetry display configuration loaded from a target's telemetry.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub layouts: Vec<TelemetryLayout>,
}

/* ----------------------------- Command Config ----------------------------- */

/// A field in a command definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandFieldDef {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

/// A single command definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub name: String,
    pub opcode: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub fields: Vec<CommandFieldDef>,
}

/// Commands for a component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentCommands {
    #[serde(rename = "fullUid")]
    pub full_uid: String,
    pub commands: Vec<CommandDef>,
}

/// Quick command button.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickCommand {
    pub label: String,
    #[serde(rename = "fullUid")]
    pub full_uid: String,
    pub opcode: String,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub payload: Option<String>,
}

/// Command configuration loaded from commands.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandConfig {
    #[serde(default, rename = "quickCommands")]
    pub quick_commands: Vec<QuickCommand>,
    #[serde(default)]
    pub components: HashMap<String, ComponentCommands>,
}

impl CommandConfig {
    /// Parse a `commands.json` file from disk into a `CommandConfig`.
    /// Returns a human-readable error string on parse or I/O failure.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        serde_json::from_str(&content).map_err(|e| format!("{}: {}", path.display(), e))
    }
}

impl TelemetryConfig {
    /// Load from a JSON file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        serde_json::from_str(&content).map_err(|e| format!("{}: {}", path.display(), e))
    }

    /// Validate layout definitions. Returns list of warnings.
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for layout in &self.layouts {
            if layout.name.is_empty() {
                warnings.push("Layout has empty name".to_string());
            }
            if layout.plots.is_empty() {
                warnings.push(format!("Layout '{}' has no plots", layout.name));
            }
            for plot in &layout.plots {
                if plot.channels.is_empty() {
                    warnings.push(format!(
                        "Plot '{}' in layout '{}' has no channels",
                        plot.title, layout.name
                    ));
                }
            }
        }
        warnings
    }
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    /// @test A dictionary entry carrying the producer-stated layout
    /// hash exposes the u32 the prelude carries; entries without one
    /// (dictionaries predating the export) return None.
    #[test]
    fn struct_def_layout_hash_parses() {
        let json = r#"{
            "category": "TUNABLE_PARAM",
            "size": 80,
            "fields": [],
            "layout_hash": "0xC93CD892",
            "canonical_spec": "a:uint:1;"
        }"#;
        let sdef: StructDef = serde_json::from_str(json).unwrap();
        assert_eq!(sdef.layout_hash_u32(), Some(0xC93C_D892));

        let bare: StructDef = serde_json::from_str(r#"{"size": 4}"#).unwrap();
        assert_eq!(bare.layout_hash_u32(), None);
    }
}
