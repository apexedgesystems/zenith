//! Telemetry routing from push packets to WebSocket subscribers.
//!
//! Decodes push telemetry using struct dictionaries for proper field-level
//! resolution. Each field is decoded according to its declared type (uint8,
//! uint16, float, etc.) from the struct dict, not blindly extracted as f32.
//!
//! Falls back to generic f32 extraction only for payloads that don't match
//! any known struct definition.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast;

use crate::core::aproto_client::PushTelemetryPacket;
use crate::core::config_manager::{FieldDef, StructDictionary};

/* ----------------------------- Types ----------------------------- */

/// A decoded telemetry sample for the frontend.
///
/// `target_id` and `channel` are stored as `Arc<str>` so the decoder
/// hot path can clone them in O(1) without per-sample allocations.
/// Both fields serialize as plain JSON strings.
#[derive(Debug, Clone, Serialize)]
pub struct TelemetrySample {
    pub target_id: Arc<str>,
    pub timestamp_ms: u64,
    pub channel: Arc<str>,
    pub value: f64,
}

/* ----------------------------- Struct-Aware Decoder ----------------------------- */

/// One field that the decoder will emit, with the channel name precomputed.
#[derive(Clone)]
struct CachedField {
    /// "{component_name}.{field_name}" stored once, cloned per sample.
    channel: Arc<str>,
    field: FieldDef,
}

/// Pre-built lookup table for decoding push telemetry using struct definitions.
///
/// Maps (fullUid, payload_size) -> precomputed list of fields ready to emit
/// with their channel names already in `Arc<str>` form. The decode hot path
/// does no string allocation per sample.
///
/// Public for benchmark/test access; not part of the stable API surface.
#[doc(hidden)]
pub struct TelemetryDecoder {
    /// (fullUid, payload_byte_size) -> precomputed fields
    lookup: HashMap<(u32, usize), Vec<CachedField>>,
    /// fullUid -> Arc<str> display name (for generic fallback)
    uid_names: HashMap<u32, Arc<str>>,
}

impl TelemetryDecoder {
    /// Build the decoder lookup table from a struct dictionary plus a
    /// list of `(fullUid, component_name)` pairs from the manifest.
    /// Pre-computes channel name `Arc<str>`s for every emittable field
    /// so the hot path does no string allocation per sample.
    pub fn new(dicts: &StructDictionary, known_components: &[(u32, &str)]) -> Self {
        let mut lookup: HashMap<(u32, usize), Vec<CachedField>> = HashMap::new();
        let mut uid_names: HashMap<u32, Arc<str>> = HashMap::new();
        // Track which field names are already claimed by higher-priority structs
        // to prevent duplicates (e.g. OUTPUT and STATE both having "output" field)
        let mut claimed_fields: HashMap<u32, std::collections::HashSet<String>> = HashMap::new();

        for &(uid, comp_name) in known_components {
            uid_names.insert(uid, Arc::from(comp_name));

            let base_name = comp_name.split('#').next().unwrap_or(comp_name).trim();
            let bn = base_name.to_lowercase();

            // Collect all matching structs, sorted by priority (OUTPUT first)
            let mut candidates: Vec<(u8, usize, Vec<FieldDef>)> = Vec::new();

            for dict in dicts.components.values() {
                let dn = dict.component.to_lowercase();
                if dn != bn && !dn.contains(&bn) && !bn.contains(&dn) {
                    continue;
                }

                for sdef in dict.structs.values() {
                    if sdef.fields.is_empty() || sdef.size == 0 {
                        continue;
                    }
                    let prio = category_priority(&sdef.category);
                    candidates.push((prio, sdef.size, sdef.fields.clone()));
                }
            }

            // Sort by priority descending (OUTPUT=4, STATE=3, TELEMETRY=2, etc.)
            candidates.sort_by(|a, b| b.0.cmp(&a.0));

            let claimed = claimed_fields.entry(uid).or_default();

            for (prio, size, mut fields) in candidates {
                let before = fields.len();
                // Remove fields already claimed by a higher-priority struct
                fields.retain(|f| !claimed.contains(&f.name));
                let removed = before - fields.len();

                if removed > 0 {
                    tracing::info!(
                        "Decoder: {comp_name} ({size}B prio={prio}): removed {removed} duplicate fields, keeping {}",
                        fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
                    );
                }

                if fields.is_empty() {
                    continue;
                }

                // Filter out fields that the decode loop will skip anyway:
                // arrays, strings, padding, reserved, zero-size. Doing this
                // at construction means the hot loop has fewer entries.
                let cached: Vec<CachedField> = fields
                    .into_iter()
                    .filter(|f| {
                        f.field_type != "array"
                            && f.field_type != "string"
                            && !f.name.starts_with("pad")
                            && !f.name.starts_with("reserved")
                            && f.size != 0
                    })
                    .map(|f| {
                        // Claim the field name
                        claimed.insert(f.name.clone());
                        let channel: Arc<str> =
                            Arc::from(format!("{}.{}", comp_name, f.name).as_str());
                        CachedField { channel, field: f }
                    })
                    .collect();

                if cached.is_empty() {
                    continue;
                }

                lookup.insert((uid, size), cached);
            }
        }

        Self { lookup, uid_names }
    }

    /// Decode a push telemetry packet into named samples.
    ///
    /// `target_id` is taken as `&Arc<str>` so the caller can hand us
    /// a stable reference; the Arc is cloned into each emitted
    /// sample (no allocation).
    pub fn decode(
        &self,
        target_id: &Arc<str>,
        timestamp_ms: u64,
        pkt: &PushTelemetryPacket,
    ) -> Vec<TelemetrySample> {
        let data = &pkt.payload;

        // Try struct-aware decode: match by (uid, payload_size)
        if let Some(cached) = self.lookup.get(&(pkt.full_uid, data.len())) {
            return self.decode_with_struct(target_id, timestamp_ms, cached, data);
        }

        // Fallback: generic f32 extraction with component name prefix
        self.decode_generic(target_id, timestamp_ms, pkt)
    }

    /// Decode using precomputed cached fields.
    fn decode_with_struct(
        &self,
        target_id: &Arc<str>,
        timestamp_ms: u64,
        cached: &[CachedField],
        data: &[u8],
    ) -> Vec<TelemetrySample> {
        let mut samples = Vec::with_capacity(cached.len());

        for cf in cached {
            // Skip fields that extend past the payload
            if cf.field.offset + cf.field.size > data.len() {
                continue;
            }
            if let Some(v) = decode_numeric(data, &cf.field) {
                samples.push(TelemetrySample {
                    target_id: Arc::clone(target_id),
                    timestamp_ms,
                    channel: Arc::clone(&cf.channel),
                    value: v,
                });
            }
        }

        samples
    }

    /// Generic f32 extraction fallback for unknown payloads.
    fn decode_generic(
        &self,
        target_id: &Arc<str>,
        timestamp_ms: u64,
        pkt: &PushTelemetryPacket,
    ) -> Vec<TelemetrySample> {
        let mut samples = Vec::new();
        let data = &pkt.payload;
        // Resolve component name for the channel prefix
        let prefix_owned: String = match self.uid_names.get(&pkt.full_uid) {
            Some(arc) => arc.to_string(),
            None => format!("0x{:06X}", pkt.full_uid),
        };

        let mut offset = 0;
        let mut idx = 0;
        while offset + 4 <= data.len() {
            let value = f32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);

            if value.is_finite() && value.abs() < 1e12 {
                let channel: Arc<str> =
                    Arc::from(format!("{}.field{}", prefix_owned, idx).as_str());
                samples.push(TelemetrySample {
                    target_id: Arc::clone(target_id),
                    timestamp_ms,
                    channel,
                    value: value as f64,
                });
            }

            offset += 4;
            idx += 1;
        }

        samples
    }
}

/// Decode a single numeric field from raw bytes, returning f64.
fn decode_numeric(data: &[u8], field: &FieldDef) -> Option<f64> {
    let off = field.offset;
    match (field.field_type.as_str(), field.size) {
        ("uint", 1) => Some(data[off] as f64),
        ("uint", 2) => Some(u16::from_le_bytes([data[off], data[off + 1]]) as f64),
        ("uint", 4) => {
            Some(
                u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as f64,
            )
        }
        ("uint", 8) => Some(u64::from_le_bytes([
            data[off],
            data[off + 1],
            data[off + 2],
            data[off + 3],
            data[off + 4],
            data[off + 5],
            data[off + 6],
            data[off + 7],
        ]) as f64),
        ("int", 1) => Some(data[off] as i8 as f64),
        ("int", 2) => Some(i16::from_le_bytes([data[off], data[off + 1]]) as f64),
        ("int", 4) => {
            Some(
                i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as f64,
            )
        }
        ("float", 4) => {
            let v = f32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            if v.is_finite() {
                Some(v as f64)
            } else {
                None
            }
        }
        ("float", 8) => {
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
            if v.is_finite() {
                Some(v)
            } else {
                None
            }
        }
        ("bool", 1) => Some(if data[off] != 0 { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Priority for struct category when multiple structs match the same size.
/// Higher = preferred for push telemetry decoding.
fn category_priority(category: &str) -> u8 {
    match category {
        "OUTPUT" => 4,
        "STATE" => 3,
        "TELEMETRY" => 2,
        "TUNABLE_PARAM" => 1,
        _ => 0,
    }
}

/* ----------------------------- Router ----------------------------- */

/// Spawn a task that routes PushTelemetryPackets into decoded TelemetrySamples.
pub fn spawn_router(
    target_id: String,
    push_rx: broadcast::Receiver<PushTelemetryPacket>,
    sample_tx: broadcast::Sender<TelemetrySample>,
    dicts: Arc<StructDictionary>,
    manifest: Option<Arc<crate::core::config_manager::AppManifest>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let target_arc: Arc<str> = Arc::from(target_id.as_str());
        let uid_names: Vec<(u32, String)> = manifest
            .as_ref()
            .map(|m| m.component_uids())
            .unwrap_or_default();
        let uid_refs: Vec<(u32, &str)> = uid_names.iter().map(|(u, n)| (*u, n.as_str())).collect();
        let decoder = TelemetryDecoder::new(&dicts, &uid_refs);
        let mut push_rx = push_rx;
        // Dedup: track last timestamp per channel to filter duplicate writes.
        // If the same channel gets a sample within MIN_INTERVAL_MS of the last,
        // skip it. This prevents multiple push sources (e.g. TelemetryManager
        // running at 50Hz instead of 10Hz) from flooding the DB.
        // Keyed by Arc<str> -- hashes the pointed-at content, no clones.
        let mut last_ts: HashMap<Arc<str>, u64> = HashMap::new();
        const MIN_INTERVAL_MS: u64 = 15; // ~66Hz max per channel

        loop {
            match push_rx.recv().await {
                Ok(pkt) => {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;

                    let samples = decoder.decode(&target_arc, now_ms, &pkt);
                    for sample in samples {
                        // Skip if this channel was written too recently.
                        // Use the Entry API: one hash lookup, single Arc clone
                        // only on first insert.
                        use std::collections::hash_map::Entry;
                        match last_ts.entry(Arc::clone(&sample.channel)) {
                            Entry::Occupied(mut e) => {
                                if sample.timestamp_ms.saturating_sub(*e.get()) < MIN_INTERVAL_MS {
                                    continue;
                                }
                                *e.get_mut() = sample.timestamp_ms;
                            }
                            Entry::Vacant(e) => {
                                e.insert(sample.timestamp_ms);
                            }
                        }
                        let _ = sample_tx.send(sample);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!("Telemetry router lagged by {} packets", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
        tracing::info!("Telemetry router stopped for {}", target_id);
    })
}
/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config_manager::{ComponentDict, FieldDef, StructDef};
    use std::collections::HashMap;

    /// Build a tiny WaveGen-like component dictionary with OUTPUT (8B,
    /// fields {output, phase}) and STATE (16B, fields {output, phase,
    /// cycleCount, errorCount}). The decoder should: pick OUTPUT for
    /// 8B payloads, pick STATE for 16B payloads, and the STATE struct
    /// should have its `output`/`phase` fields removed by the priority
    /// dedup since OUTPUT already claims them.
    fn make_wavegen_dict() -> StructDictionary {
        let f = |name: &str, ty: &str, off: usize, sz: usize| FieldDef {
            name: name.to_string(),
            field_type: ty.to_string(),
            offset: off,
            size: sz,
            value: serde_json::Value::Null,
            element_type: None,
            dims: None,
        };
        let comp = ComponentDict {
            component: "WaveGenerator".to_string(),
            structs: HashMap::from([
                (
                    "Output".to_string(),
                    StructDef {
                        category: "OUTPUT".to_string(),
                        size: 8,
                        opcode: None,
                        fields: vec![f("output", "float", 0, 4), f("phase", "float", 4, 4)],
                    },
                ),
                (
                    "State".to_string(),
                    StructDef {
                        category: "STATE".to_string(),
                        size: 16,
                        opcode: None,
                        fields: vec![
                            f("output", "float", 0, 4),
                            f("phase", "float", 4, 4),
                            f("cycleCount", "uint", 8, 4),
                            f("errorCount", "uint", 12, 4),
                        ],
                    },
                ),
            ]),
            enums: HashMap::new(),
        };
        StructDictionary {
            components: HashMap::from([("WaveGenerator".to_string(), comp)]),
        }
    }

    fn target_id() -> Arc<str> {
        Arc::from("test-target")
    }

    /// @test 8-byte WaveGen OUTPUT struct decodes into the two
    /// expected float samples (output, phase) with correct values.
    #[test]
    fn decode_8b_output_emits_two_samples() {
        let dict = make_wavegen_dict();
        let decoder = TelemetryDecoder::new(&dict, &[(0x00D000, "WaveGen#0")]);
        let pkt = PushTelemetryPacket {
            full_uid: 0x00D000,
            // 0.5_f32, 1.0_f32 little-endian
            payload: vec![0x00, 0x00, 0x00, 0x3F, 0x00, 0x00, 0x80, 0x3F],
        };
        let samples = decoder.decode(&target_id(), 1000, &pkt);
        assert_eq!(samples.len(), 2);
        assert_eq!(&*samples[0].channel, "WaveGen#0.output");
        assert!((samples[0].value - 0.5).abs() < 1e-6);
        assert_eq!(&*samples[1].channel, "WaveGen#0.phase");
        assert!((samples[1].value - 1.0).abs() < 1e-6);
    }

    /// @test STATE struct fields that share names with the
    /// higher-priority OUTPUT struct are skipped to prevent
    /// duplicate channels.
    #[test]
    fn decode_16b_state_skips_priority_duplicated_fields() {
        // STATE struct includes output+phase, but OUTPUT already claimed them.
        // The 16B STATE decode should only emit cycleCount and errorCount.
        let dict = make_wavegen_dict();
        let decoder = TelemetryDecoder::new(&dict, &[(0x00D000, "WaveGen#0")]);
        let mut payload = vec![0u8; 16];
        // cycleCount = 42, errorCount = 7
        payload[8..12].copy_from_slice(&42u32.to_le_bytes());
        payload[12..16].copy_from_slice(&7u32.to_le_bytes());
        let pkt = PushTelemetryPacket {
            full_uid: 0x00D000,
            payload,
        };
        let samples = decoder.decode(&target_id(), 2000, &pkt);
        let names: Vec<&str> = samples.iter().map(|s| &*s.channel).collect();
        assert!(
            !names.contains(&"WaveGen#0.output"),
            "output should be claimed by OUTPUT struct"
        );
        assert!(
            !names.contains(&"WaveGen#0.phase"),
            "phase should be claimed by OUTPUT struct"
        );
        assert!(names.contains(&"WaveGen#0.cycleCount"));
        assert!(names.contains(&"WaveGen#0.errorCount"));
        let cc = samples
            .iter()
            .find(|s| &*s.channel == "WaveGen#0.cycleCount")
            .unwrap();
        assert_eq!(cc.value, 42.0);
        let ec = samples
            .iter()
            .find(|s| &*s.channel == "WaveGen#0.errorCount")
            .unwrap();
        assert_eq!(ec.value, 7.0);
    }

    /// @test Payloads with no matching struct dict entry fall back to
    /// generic f32 extraction with synthetic field names.
    #[test]
    fn decode_unknown_payload_falls_back_to_generic() {
        // Empty struct dict -> decoder has no lookup entries -> generic fallback
        let dict = StructDictionary::default();
        let decoder = TelemetryDecoder::new(&dict, &[(0x00DEAD, "Unknown")]);
        // Payload: 1.0_f32, 2.0_f32, 3.0_f32
        let mut payload = vec![0u8; 12];
        payload[0..4].copy_from_slice(&1.0f32.to_le_bytes());
        payload[4..8].copy_from_slice(&2.0f32.to_le_bytes());
        payload[8..12].copy_from_slice(&3.0f32.to_le_bytes());
        let pkt = PushTelemetryPacket {
            full_uid: 0x00DEAD,
            payload,
        };
        let samples = decoder.decode(&target_id(), 3000, &pkt);
        assert_eq!(samples.len(), 3);
        assert_eq!(&*samples[0].channel, "Unknown.field0");
        assert_eq!(samples[0].value, 1.0);
        assert_eq!(samples[2].value, 3.0);
    }

    /// @test Fields named "padding" or "reserved" are filtered out
    /// at decoder construction so they never appear in samples.
    #[test]
    fn decode_filters_padding_and_reserved() {
        let f = |name: &str, ty: &str, off: usize, sz: usize| FieldDef {
            name: name.to_string(),
            field_type: ty.to_string(),
            offset: off,
            size: sz,
            value: serde_json::Value::Null,
            element_type: None,
            dims: None,
        };
        let comp = ComponentDict {
            component: "X".to_string(),
            structs: HashMap::from([(
                "S".to_string(),
                StructDef {
                    category: "OUTPUT".to_string(),
                    size: 8,
                    opcode: None,
                    fields: vec![
                        f("real", "float", 0, 4),
                        f("padding", "uint", 4, 2),
                        f("reserved0", "uint", 6, 2),
                    ],
                },
            )]),
            enums: HashMap::new(),
        };
        let dict = StructDictionary {
            components: HashMap::from([("X".to_string(), comp)]),
        };
        let decoder = TelemetryDecoder::new(&dict, &[(0x1, "X")]);
        let pkt = PushTelemetryPacket {
            full_uid: 0x1,
            payload: vec![0u8; 8],
        };
        let samples = decoder.decode(&target_id(), 0, &pkt);
        assert_eq!(samples.len(), 1, "padding and reserved should be skipped");
        assert_eq!(&*samples[0].channel, "X.real");
    }

    /// @test Repeated decode() calls reuse the same Arc<str> channel
    /// name allocation rather than creating fresh strings, proving
    /// the decoder channel-name cache is working.
    #[test]
    fn decode_returns_arc_clones_not_new_strings() {
        // The decoder caches channel name Arcs. Two decode calls should
        // produce samples whose channel Arcs point at the SAME allocation
        // (verifiable via Arc::strong_count rising and Arc::ptr_eq).
        let dict = make_wavegen_dict();
        let decoder = TelemetryDecoder::new(&dict, &[(0x00D000, "WaveGen#0")]);
        let pkt = PushTelemetryPacket {
            full_uid: 0x00D000,
            payload: vec![0u8; 8],
        };
        let s1 = decoder.decode(&target_id(), 0, &pkt);
        let s2 = decoder.decode(&target_id(), 1, &pkt);
        assert!(
            Arc::ptr_eq(&s1[0].channel, &s2[0].channel),
            "channel name should be the same Arc allocation across calls"
        );
    }

    /// @test The category priority ordering is OUTPUT > STATE >
    /// TELEMETRY > TUNABLE_PARAM > anything-else, used to break
    /// duplicate-field ties at decoder construction.
    #[test]
    fn category_priority_ordering() {
        assert!(category_priority("OUTPUT") > category_priority("STATE"));
        assert!(category_priority("STATE") > category_priority("TELEMETRY"));
        assert!(category_priority("TELEMETRY") > category_priority("TUNABLE_PARAM"));
        assert_eq!(category_priority("UNKNOWN"), 0);
    }
}
