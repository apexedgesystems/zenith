//! TPRM format A payload prelude (v4): stamp and verify.
//!
//! Every component payload zenith uploads carries a 28-byte
//! little-endian prelude the vehicle verifies before the body touches
//! a component:
//!
//!   magic[4]       = "APV4"
//!   version[2]     = 4
//!   reserved[2]    = 0
//!   payloadSize[8] = byte length of the payload that follows
//!   fullUid[4]     = component the payload targets
//!   layoutHash[4]  = CRC-32 of the canonical field spec
//!   payloadCrc[4]  = CRC-32 (IEEE 802.3) of the payload bytes
//!
//! Canonical field spec (offset-aware): every leaf contributes
//! `name:type:size:offset;` in emission order -- nested structs
//! flatten to their leaves at absolute offsets, arrays contribute
//! `name:array:total:offset;[elemtype:elemsizexN]` -- terminated by
//! `|size:total`. layoutHash is the CRC-32 of that ASCII string, so
//! two layouts with the same fields still hash apart when their
//! packing differs.
//!
//! Conformance: the tests below assert byte-identity against the
//! golden vectors in compat/tprm/ -- the cross-repo contract. This
//! module conforms to those vectors; it never imports another
//! implementation.
//!
//! Boundary note: this is a FORMAT module for one target family's
//! parameter payloads -- pure byte functions, no transport or client
//! coupling. Zenith's generic layers must not depend on it; callers
//! at the handler edge decide (per target) whether payloads get this
//! stamp, so targets speaking other protocols remain possible.

use std::ops::Range;

use crate::core::config_manager::{leaf_name, FieldDef};

/// Prelude magic bytes.
pub const MAGIC: &[u8; 4] = b"APV4";
/// Prelude format version.
pub const VERSION: u16 = 4;
/// Prelude length in bytes.
pub const HEADER_SIZE: usize = 28;

// Byte ranges of the prelude fields: the one layout stamp_v4 writes
// and parse_v4 reads.
const MAGIC_BYTES: Range<usize> = 0..4;
const VERSION_BYTES: Range<usize> = 4..6;
const RESERVED_BYTES: Range<usize> = 6..8;
const SIZE_BYTES: Range<usize> = 8..16;
const UID_BYTES: Range<usize> = 16..20;
const HASH_BYTES: Range<usize> = 20..24;
const CRC_BYTES: Range<usize> = 24..HEADER_SIZE;

/// Errors from the prelude and staged-verify parsers. Each prelude
/// defect is its own variant, so a refusal says which field was wrong.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TprmError {
    #[error("truncated: too short for its layout")]
    Truncated,
    #[error("bad magic \"{}\" (want \"{}\")", .found.escape_ascii(), MAGIC.escape_ascii())]
    BadMagic { found: [u8; 4] },
    #[error("unsupported version {0} (want {want})", want = VERSION)]
    BadVersion(u16),
    #[error("reserved bytes are 0x{0:04X}, not zero")]
    ReservedNonZero(u16),
    #[error("payload size mismatch (header {header}, actual {actual})")]
    SizeMismatch { header: u64, actual: usize },
    #[error("payload CRC mismatch")]
    BadCrc,
}

/// CRC-32 (IEEE 802.3): reflected polynomial 0xEDB88320, init and
/// final XOR 0xFFFFFFFF. Distinct from the CRC-32C the file-transfer
/// protocol uses -- the two must not be conflated.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// One leaf of a payload layout, in emission order. Nested structs
/// do not appear as entries: they flatten to their leaves, each
/// carrying its absolute offset.
#[derive(Debug, Clone)]
pub struct LeafSpec<'a> {
    pub name: &'a str,
    pub field_type: &'a str,
    /// Total serialized bytes of this leaf (element size x count for
    /// arrays).
    pub size: usize,
    /// Absolute byte offset within the payload.
    pub offset: usize,
    /// For arrays: (element_type, element_size, element_count).
    pub array: Option<(&'a str, usize, usize)>,
}

/// Build the canonical field spec string. Scalar leaves contribute
/// `name:type:size:offset;`; array leaves contribute
/// `name:array:totalsize:offset;[elemtype:elemsizexN]`; the string
/// ends with the `|size:total` terminator so trailing padding is part
/// of the hashed layout.
pub fn canonical_field_spec<'a, I>(fields: I, total_size: usize) -> String
where
    I: IntoIterator<Item = LeafSpec<'a>>,
{
    let mut spec = String::new();
    for leaf in fields {
        match leaf.array {
            None => {
                spec.push_str(&format!(
                    "{}:{}:{}:{};",
                    leaf.name, leaf.field_type, leaf.size, leaf.offset
                ));
            }
            Some((elem_type, elem_size, count)) => {
                spec.push_str(&format!(
                    "{}:array:{}:{};",
                    leaf.name, leaf.size, leaf.offset
                ));
                spec.push_str(&format!("[{}:{}x{}]", elem_type, elem_size, count));
            }
        }
    }
    spec.push_str(&format!("|size:{}", total_size));
    spec
}

/// layoutHash over a canonical field spec.
pub fn layout_hash<'a, I>(fields: I, total_size: usize) -> u32
where
    I: IntoIterator<Item = LeafSpec<'a>>,
{
    crc32(canonical_field_spec(fields, total_size).as_bytes())
}

/// Stamp the prelude onto a payload body. The reserved bytes are
/// written zero, and the 64-bit size field holds any body length.
pub fn stamp_v4(full_uid: u32, layout_hash: u32, payload: &[u8]) -> Vec<u8> {
    let mut prelude = [0u8; HEADER_SIZE];
    prelude[MAGIC_BYTES].copy_from_slice(MAGIC);
    prelude[VERSION_BYTES].copy_from_slice(&VERSION.to_le_bytes());
    prelude[RESERVED_BYTES].copy_from_slice(&0u16.to_le_bytes());
    prelude[SIZE_BYTES].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    prelude[UID_BYTES].copy_from_slice(&full_uid.to_le_bytes());
    prelude[HASH_BYTES].copy_from_slice(&layout_hash.to_le_bytes());
    prelude[CRC_BYTES].copy_from_slice(&crc32(payload).to_le_bytes());
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.extend_from_slice(&prelude);
    out.extend_from_slice(payload);
    out
}

/// Parsed prelude fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreludeFields {
    pub version: u16,
    pub payload_size: u64,
    pub full_uid: u32,
    pub layout_hash: u32,
    pub payload_crc: u32,
}

/// One prelude field's bytes, sized by the integer they decode to.
fn field<const N: usize>(data: &[u8], bytes: Range<usize>) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&data[bytes]);
    out
}

/// Verify a stamped payload and return its prelude fields and body.
/// Checks, in order: length, magic, version, reserved bytes, declared
/// size against the body, and body CRC, each failure its own error.
/// Reserved bytes must be zero: every stamp this reads (zenith's own
/// and the producer's tools) writes zero there. The caller compares
/// full_uid and layout_hash against its expectations (they are
/// policy, not format).
pub fn parse_v4(data: &[u8]) -> Result<(PreludeFields, &[u8]), TprmError> {
    if data.len() < HEADER_SIZE {
        return Err(TprmError::Truncated);
    }
    let found: [u8; 4] = field(data, MAGIC_BYTES);
    if &found != MAGIC {
        return Err(TprmError::BadMagic { found });
    }
    let version = u16::from_le_bytes(field(data, VERSION_BYTES));
    if version != VERSION {
        return Err(TprmError::BadVersion(version));
    }
    let reserved = u16::from_le_bytes(field(data, RESERVED_BYTES));
    if reserved != 0 {
        return Err(TprmError::ReservedNonZero(reserved));
    }
    let payload_size = u64::from_le_bytes(field(data, SIZE_BYTES));
    let body = &data[HEADER_SIZE..];
    if body.len() as u64 != payload_size {
        return Err(TprmError::SizeMismatch {
            header: payload_size,
            actual: body.len(),
        });
    }
    let fields = PreludeFields {
        version,
        payload_size,
        full_uid: u32::from_le_bytes(field(data, UID_BYTES)),
        layout_hash: u32::from_le_bytes(field(data, HASH_BYTES)),
        payload_crc: u32::from_le_bytes(field(data, CRC_BYTES)),
    };
    if crc32(body) != fields.payload_crc {
        return Err(TprmError::BadCrc);
    }
    Ok((fields, body))
}

/* ----------------------------- Dictionary layouts ----------------------------- */

/// LeafSpec views over a dictionary struct's leaves, in declaration
/// (template emission) order, as `config_manager::expanded_fields`
/// lays them out. A nested leaf hashes under its own name: the
/// canonical spec inlines a nested field's leaves at their absolute
/// offsets, once per instance, with no row for the nested field.
pub fn dict_leaf_specs(fields: &[FieldDef]) -> Vec<LeafSpec<'_>> {
    fields
        .iter()
        .map(|f| LeafSpec {
            name: leaf_name(&f.name),
            field_type: &f.field_type,
            size: f.size,
            offset: f.offset,
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

/* ----------------------------- READBACK / VERIFY ----------------------------- */

/// Executive opcodes for the staged-verify surface.
pub const OP_READBACK_TPRM: u16 = 0x0131;
pub const OP_VERIFY_TPRM: u16 = 0x0132;

/// TprmPayloadCheck fault-code names (apex TprmPayload.hpp). The
/// vocabulary the vehicle speaks for both VERIFY verdicts and
/// RELOAD-refusal extra bytes; hash-mismatch and constraint-violation
/// are distinct codes by ground contract.
pub fn payload_check_name(code: u8) -> &'static str {
    match code {
        0 => "OK",
        1 => "FILE_ERROR",
        2 => "TOO_SMALL",
        3 => "BAD_MAGIC",
        4 => "BAD_VERSION",
        5 => "SIZE_MISMATCH",
        6 => "UID_MISMATCH",
        7 => "CRC_MISMATCH",
        8 => "BODY_SIZE_MISMATCH",
        9 => "CONSTRAINT_VIOLATION",
        10 => "LAYOUT_MISMATCH",
        _ => "UNKNOWN",
    }
}

/// One staged-bank digest row: the identity a staged payload's prelude
/// declares, plus the vehicle's header verdict for the file. Corrupt
/// files are rows with non-OK verdicts, never skipped.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReadbackRow {
    pub full_uid: u32,
    pub layout_hash: u32,
    pub payload_crc: u32,
    pub verdict: u8,
    pub verdict_name: &'static str,
}

/// One READBACK_TPRM page: 8-byte header (total u16, first u16,
/// count u8, reserved[3]) + 16-byte rows.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReadbackPage {
    pub total: u16,
    pub first: u16,
    pub rows: Vec<ReadbackRow>,
}

pub fn parse_readback_page(extra: &[u8]) -> Result<ReadbackPage, TprmError> {
    if extra.len() < 8 {
        return Err(TprmError::Truncated);
    }
    let total = u16::from_le_bytes([extra[0], extra[1]]);
    let first = u16::from_le_bytes([extra[2], extra[3]]);
    let count = extra[4] as usize;
    if extra.len() < 8 + count * 16 {
        return Err(TprmError::Truncated);
    }
    let mut rows = Vec::with_capacity(count);
    for i in 0..count {
        let b = &extra[8 + i * 16..8 + (i + 1) * 16];
        let verdict = b[12];
        rows.push(ReadbackRow {
            full_uid: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            layout_hash: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            payload_crc: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            verdict,
            verdict_name: payload_check_name(verdict),
        });
    }
    Ok(ReadbackPage { total, first, rows })
}

/// A VERIFY_TPRM verdict block: verdict u8, reserved[3], then the
/// declared layoutHash and payloadCrc from the staged prelude.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyVerdict {
    pub verdict: u8,
    pub verdict_name: &'static str,
    pub ok: bool,
    pub declared_layout_hash: u32,
    pub declared_payload_crc: u32,
}

pub fn parse_verify_verdict(extra: &[u8]) -> Result<VerifyVerdict, TprmError> {
    if extra.len() < 12 {
        return Err(TprmError::Truncated);
    }
    let verdict = extra[0];
    Ok(VerifyVerdict {
        verdict,
        verdict_name: payload_check_name(verdict),
        ok: verdict == 0,
        declared_layout_hash: u32::from_le_bytes([extra[4], extra[5], extra[6], extra[7]]),
        declared_payload_crc: u32::from_le_bytes([extra[8], extra[9], extra[10], extra[11]]),
    })
}

/// Status the vehicle answers a refused RELOAD_TPRM with (the verdict
/// block follows in the extra).
pub const LOAD_FAILED: u8 = 5;

/// What the staged bank's digest says about the payload just staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedRow {
    /// The row for the uid declares the sent layout hash and payload
    /// CRC, and the vehicle's header verdict is OK.
    Matches,
    /// The row for the uid carries a refusing header verdict.
    Refused(u8),
    /// The row for the uid declares another layout hash or CRC.
    Differs,
    /// No row declares the uid. The vehicle fills a refused row with
    /// the bytes it read, so a prelude it cannot read (a foreign
    /// magic) declares no usable uid; only a verify, which addresses
    /// the staged file by uid, can name that fault.
    Unattributed,
}

/// Judge the digest row declaring the staged uid (None when no row
/// declares it) against the prelude zenith sent. A refusing header
/// verdict outranks the declared identity: the vehicle did not trust
/// the bytes it read.
pub fn judge_staged_row(row: Option<&ReadbackRow>, sent: &PreludeFields) -> StagedRow {
    match row {
        None => StagedRow::Unattributed,
        Some(r) if r.verdict != 0 => StagedRow::Refused(r.verdict),
        Some(r) if r.layout_hash == sent.layout_hash && r.payload_crc == sent.payload_crc => {
            StagedRow::Matches
        }
        Some(_) => StagedRow::Differs,
    }
}

/// The status a staged-pipeline stop reports: the stopping step's own
/// status when the vehicle refused that command, LOAD_FAILED when it
/// answered the step with SUCCESS (nothing was loaded). A stop never
/// reads as a success.
pub fn stop_status(step_status: u8) -> u8 {
    if step_status == 0 {
        LOAD_FAILED
    } else {
        step_status
    }
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn contract_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../compat/tprm")
    }

    /// Whether a container declared `type = "struct"` contributes its
    /// own `name:struct:0:offset;` row ahead of its leaves. By the
    /// contract it does not: a key typed struct is a grouping whose
    /// leaves carry the rows, as for a plain nested table, and the
    /// nested_enum vector hashes its five leaves only.
    const STRUCT_CONTAINER_ROWS: bool = false;

    /// One parsed vector-toml leaf (owned strings; tests build
    /// LeafSpec views over these).
    #[derive(Debug, Default)]
    struct TomlLeaf {
        name: String,
        field_type: String,
        size: usize,
        offset: usize,
        element_type: Option<String>,
        element_count: usize,
    }

    /// Parse a vector toml in the authoring-template format, returning
    /// leaves in declaration order. Line-scan on purpose: declaration
    /// order is part of the contract, and a map-based toml parse would
    /// lose it. Skips the [__enums__] section; a struct container is
    /// returned as its own zero-size leaf ahead of its nested leaves,
    /// and leaf_specs decides whether it hashes (STRUCT_CONTAINER_ROWS).
    ///
    /// Two section styles appear in the contract: fixed-shape vectors
    /// declare one `[Section.leaf]` per leaf with key = value lines,
    /// and the variable-length scheduler vector declares inline-table
    /// leaves (`name = { type = ..., size = ... }`) under plain
    /// `[Header]` / `[[entry]]` sections. Array-of-tables sections
    /// contribute no container marker -- the variable-length recipe is
    /// flat by contract -- and a section header that turns out to hold
    /// inline tables (no type of its own) is dropped at the end.
    fn parse_vector_toml(name: &str) -> Vec<TomlLeaf> {
        let text = std::fs::read_to_string(contract_dir().join("toml").join(name)).unwrap();
        let mut leaves: Vec<TomlLeaf> = Vec::new();
        let mut in_leaf = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with("[[") {
                // Array-of-tables entry: its inline leaves follow.
                in_leaf = true;
                continue;
            }
            if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_leaf = header != "__enums__";
                if in_leaf {
                    leaves.push(TomlLeaf {
                        // Leaf name is the last path segment
                        // ([Struct.inner.fields.inner_a] -> inner_a).
                        name: header.rsplit('.').next().unwrap().to_string(),
                        ..Default::default()
                    });
                }
            } else if in_leaf {
                if let Some((key, inline)) = line.split_once(" = {") {
                    // Inline-table leaf. `type` is the first key in the
                    // authoring template, so the first "type = " match
                    // is the field type (never element_type); the
                    // byte-identity assertion would catch any drift in
                    // that layout.
                    let field = |tag: &str| {
                        inline.split_once(tag).map(|(_, rest)| {
                            rest.split([',', '}'])
                                .next()
                                .unwrap()
                                .trim()
                                .trim_matches('"')
                                .to_string()
                        })
                    };
                    leaves.push(TomlLeaf {
                        name: key.trim().to_string(),
                        field_type: field("type = ").unwrap_or_default(),
                        size: field("size = ").and_then(|s| s.parse().ok()).unwrap_or(0),
                        offset: 0,
                        element_type: field("element_type = "),
                        element_count: 0,
                    });
                    continue;
                }
                let entry = leaves.last_mut().unwrap();
                if let Some(v) = line.strip_prefix("type = ") {
                    entry.field_type = v.trim_matches('"').to_string();
                } else if let Some(v) = line.strip_prefix("size = ") {
                    entry.size = v.parse().unwrap();
                } else if let Some(v) = line.strip_prefix("element_type = ") {
                    entry.element_type = Some(v.trim_matches('"').to_string());
                } else if let Some(v) = line.strip_prefix("value = ") {
                    if v.starts_with('[') {
                        entry.element_count = v.matches(',').count() + 1;
                    }
                }
            }
        }
        // Offset assignment by declaration-order accumulation (the
        // toml carries sizes only). A struct container takes the
        // offset where its leaves start and consumes no bytes; its
        // leaves follow at absolute offsets. Section headers that
        // only held inline tables have no type and drop.
        leaves.retain(|l| !l.field_type.is_empty());
        let mut off = 0usize;
        for l in &mut leaves {
            l.offset = off;
            off += l.size;
        }
        leaves
    }

    /// Total payload size implied by a vector's leaves.
    fn total_size(leaves: &[TomlLeaf]) -> usize {
        leaves.last().map(|l| l.offset + l.size).unwrap_or(0)
    }

    /// The leaf list the layout hash covers, in declaration order.
    fn leaf_specs(leaves: &[TomlLeaf]) -> Vec<LeafSpec<'_>> {
        leaves
            .iter()
            .filter(|l| STRUCT_CONTAINER_ROWS || l.field_type != "struct")
            .map(|l| LeafSpec {
                name: &l.name,
                field_type: &l.field_type,
                size: l.size,
                offset: l.offset,
                array: l
                    .element_type
                    .as_deref()
                    .map(|et| (et, l.size / l.element_count.max(1), l.element_count)),
            })
            .collect()
    }

    fn read_vector(name: &str) -> Vec<u8> {
        std::fs::read(contract_dir().join("payloads").join(name)).unwrap()
    }

    /// nested_enum stamped with the 20-byte APV3 prelude: the bytes a
    /// parser of this format must refuse by their magic.
    const APV3_NESTED_ENUM: &[u8] = b"APV3\x03\x00\x0d\x00\x00\xca\x00\x00b.\x9f\xe4\x93X\xf4\xb9\
        \x07\xee\xff\xc0\x00\x02\x01\x00\x00`@\xfe\xff";

    /// @test CRC-32 (IEEE 802.3) matches the standard check vector.
    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0x0000_0000);
    }

    /// @test For every golden payload vector: the layout hash computed
    /// from the toml source's declaration-order field spec matches the
    /// committed prelude, and stamping the vector's own body with that
    /// hash reproduces the committed bytes exactly. This pins the
    /// prelude packing, the CRC, and the canonical-spec recipe against
    /// the contract in one assertion per vector.
    #[test]
    fn stamped_vectors_reproduce_byte_identically() {
        for (toml_name, bin_name, full_uid) in [
            ("scalar_types.toml", "scalar_types.bin", 0x000000u32),
            ("strings_arrays.toml", "strings_arrays.bin", 0x00D001),
            ("nested_enum.toml", "nested_enum.bin", 0x00CA00),
            // Variable-length shape: flat header + entry leaves, so the
            // toml's declaration order IS the recipe's expansion order.
            ("scheduler_shape.toml", "scheduler_shape.bin", 0x000100),
        ] {
            let vector = read_vector(bin_name);
            let leaves = parse_vector_toml(toml_name);
            let total = total_size(&leaves);
            let hash = layout_hash(leaf_specs(&leaves), total);

            let (prelude, body) =
                parse_v4(&vector).unwrap_or_else(|e| panic!("{bin_name}: vector fails parse: {e}"));
            assert_eq!(
                total,
                body.len(),
                "{bin_name}: accumulated leaf sizes disagree with the body"
            );
            assert_eq!(
                prelude.layout_hash, hash,
                "{bin_name}: layout-hash recipe diverges from contract"
            );
            assert_eq!(prelude.full_uid, full_uid, "{bin_name}: contract uid");

            let restamped = stamp_v4(full_uid, hash, body);
            assert_eq!(
                restamped, vector,
                "{bin_name}: stamped bytes diverge from contract"
            );
        }
    }

    /// @test The raw (unstamped) vector is exactly the stamped
    /// vector's body: stamping raw reproduces the stamped file.
    #[test]
    fn raw_vector_stamps_to_committed_bytes() {
        let raw = read_vector("nested_enum_raw.bin");
        let stamped = read_vector("nested_enum.bin");
        let (prelude, body) = parse_v4(&stamped).unwrap();
        assert_eq!(raw, body);
        let restamped = stamp_v4(prelude.full_uid, prelude.layout_hash, &raw);
        assert_eq!(restamped, stamped);
    }

    /// @test Every prelude field reads back from its own bytes, and
    /// each defect is rejected distinctly at the field that carries
    /// it: truncation one byte short of the prelude, bad magic (with
    /// the bytes found), wrong version, non-zero reserved bytes, a
    /// size that lies in the low or the high byte of the 64-bit
    /// field, a corrupted CRC field, a corrupted body.
    #[test]
    fn parse_rejects_each_defect_distinctly() {
        let body = [1u8, 2, 3, 4];
        let good = stamp_v4(0x00D000, 0xABCD_1234, &body);
        assert_eq!(good.len(), 28 + body.len());
        let (prelude, parsed_body) = parse_v4(&good).unwrap();
        assert_eq!(
            prelude,
            PreludeFields {
                version: 4,
                payload_size: 4,
                full_uid: 0x00D000,
                layout_hash: 0xABCD_1234,
                payload_crc: crc32(&body),
            }
        );
        assert_eq!(parsed_body, body);

        assert_eq!(parse_v4(&good[..27]), Err(TprmError::Truncated));

        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(parse_v4(&bad), Err(TprmError::BadMagic { found: *b"XPV4" }));

        let mut bad = good.clone();
        bad[4] = 9;
        assert_eq!(parse_v4(&bad), Err(TprmError::BadVersion(9)));

        let mut bad = good.clone();
        bad[7] = 0x80;
        assert_eq!(parse_v4(&bad), Err(TprmError::ReservedNonZero(0x8000)));

        let mut bad = good.clone();
        bad[8] = 99;
        assert_eq!(
            parse_v4(&bad),
            Err(TprmError::SizeMismatch {
                header: 99,
                actual: 4
            })
        );

        let mut bad = good.clone();
        bad[15] = 1;
        assert_eq!(
            parse_v4(&bad),
            Err(TprmError::SizeMismatch {
                header: (1 << 56) | 4,
                actual: 4
            })
        );

        let mut bad = good.clone();
        bad[24] ^= 0xFF;
        assert_eq!(parse_v4(&bad), Err(TprmError::BadCrc));

        let mut bad = good.clone();
        *bad.last_mut().unwrap() ^= 0xFF;
        assert_eq!(parse_v4(&bad), Err(TprmError::BadCrc));
    }

    /// @test A payload stamped with the 20-byte APV3 prelude is refused
    /// by its magic, and the refusal names both the magic it found and
    /// the magic this format wants.
    #[test]
    fn apv3_stamp_is_refused_by_its_magic() {
        let err = parse_v4(APV3_NESTED_ENUM).unwrap_err();
        assert_eq!(err, TprmError::BadMagic { found: *b"APV3" });
        assert_eq!(err.to_string(), "bad magic \"APV3\" (want \"APV4\")");
    }

    /// @test A body longer than 16 bits can count is stamped with its
    /// full length in the 64-bit size field and parses back intact.
    #[test]
    fn stamp_carries_a_body_beyond_u16() {
        let big: Vec<u8> = (0..70_000u32).map(|i| i as u8).collect();
        let stamped = stamp_v4(0x00D000, 0, &big);
        assert_eq!(stamped[8..16], 70_000u64.to_le_bytes());
        let (prelude, body) = parse_v4(&stamped).unwrap();
        assert_eq!(prelude.payload_size, 70_000);
        assert_eq!(body, &big[..]);
    }

    /// @test Variable-length (header + entries) layout hashing is
    /// entry-count dependent: header leaves then each entry's leaves
    /// in order, no container markers. The 2-task hash is anchored to
    /// the contract vector's own prelude (not a transcribed constant)
    /// and differs from the 3-task hash: the count changes the hash,
    /// which is what makes a stale task list un-uploadable.
    #[test]
    fn variable_length_hash_is_entry_count_dependent() {
        let hdr = [
            ("numPools", "uint", 1usize),
            ("workersPerPool", "uint", 1),
            ("numTasks", "uint", 1),
        ];
        let entry = [
            ("fullUid", "uint", 4usize),
            ("taskUid", "uint", 1),
            ("poolIndex", "uint", 1),
            ("freqN", "uint", 2),
            ("freqD", "uint", 2),
            ("offset", "uint", 2),
            ("priority", "int", 1),
            ("seqGroup", "uint", 1),
            ("seqPhase", "uint", 1),
        ];
        // Offset-aware form: absolute offsets accumulate across the
        // header and every entry; the total terminator makes the hash
        // entry-count-dependent twice over.
        let hash_for = |entry_count: usize| {
            let mut off = 0usize;
            let leaves: Vec<(&str, &str, usize, usize)> = hdr
                .iter()
                .chain((0..entry_count).flat_map(|_| entry.iter()))
                .map(|&(name, field_type, size)| {
                    let o = off;
                    off += size;
                    (name, field_type, size, o)
                })
                .collect();
            let total = off;
            layout_hash(
                leaves
                    .iter()
                    .map(|&(name, field_type, size, offset)| LeafSpec {
                        name,
                        field_type,
                        size,
                        offset,
                        array: None,
                    }),
                total,
            )
        };
        let (vector_prelude, _) = parse_v4(&read_vector("scheduler_shape.bin")).unwrap();
        assert_eq!(hash_for(2), vector_prelude.layout_hash);
        assert_ne!(hash_for(2), hash_for(3));
    }

    /// @test A READBACK page parses per the vehicle's emit layout
    /// (8-byte header + 16-byte rows, little-endian), including a
    /// corrupt file surfacing as a row with a non-OK verdict; short
    /// buffers are Truncated, never a partial parse.
    #[test]
    fn readback_page_round_trip() {
        let mut page = Vec::new();
        page.extend_from_slice(&3u16.to_le_bytes()); // total
        page.extend_from_slice(&1u16.to_le_bytes()); // first
        page.push(2); // count
        page.extend_from_slice(&[0; 3]);
        for (uid, hash, crc, verdict) in [
            (0x00D000u32, 0xAABBCCDD_u32, 0x11223344_u32, 0u8),
            (0x00D001, 0, 0, 7),
        ] {
            page.extend_from_slice(&uid.to_le_bytes());
            page.extend_from_slice(&hash.to_le_bytes());
            page.extend_from_slice(&crc.to_le_bytes());
            page.push(verdict);
            page.extend_from_slice(&[0; 3]);
        }

        let parsed = parse_readback_page(&page).unwrap();
        assert_eq!(parsed.total, 3);
        assert_eq!(parsed.first, 1);
        assert_eq!(parsed.rows.len(), 2);
        assert_eq!(parsed.rows[0].full_uid, 0x00D000);
        assert_eq!(parsed.rows[0].layout_hash, 0xAABBCCDD);
        assert_eq!(parsed.rows[0].verdict_name, "OK");
        assert_eq!(parsed.rows[1].verdict, 7);
        assert_eq!(parsed.rows[1].verdict_name, "CRC_MISMATCH");

        assert_eq!(parse_readback_page(&page[..7]), err_truncated());
        assert_eq!(parse_readback_page(&page[..20]), err_truncated());
    }

    fn err_truncated() -> Result<ReadbackPage, TprmError> {
        Err(TprmError::Truncated)
    }

    /// @test A VERIFY verdict block parses its 12-byte layout with the
    /// TprmPayloadCheck vocabulary; OK maps to ok=true and the two
    /// declared hashes come through little-endian.
    #[test]
    fn verify_verdict_round_trip() {
        let mut block = vec![9u8, 0, 0, 0]; // CONSTRAINT_VIOLATION
        block.extend_from_slice(&0xFEF9BC60u32.to_le_bytes());
        block.extend_from_slice(&0x600DF00Du32.to_le_bytes());
        let v = parse_verify_verdict(&block).unwrap();
        assert!(!v.ok);
        assert_eq!(v.verdict_name, "CONSTRAINT_VIOLATION");
        assert_eq!(v.declared_layout_hash, 0xFEF9BC60);
        assert_eq!(v.declared_payload_crc, 0x600DF00D);

        block[0] = 0;
        assert!(parse_verify_verdict(&block).unwrap().ok);
        assert!(matches!(
            parse_verify_verdict(&block[..11]),
            Err(TprmError::Truncated)
        ));
    }

    /// @test The digest row for the staged uid is judged against the
    /// prelude zenith sent: the sent identity with an OK verdict goes
    /// on; a refusing header verdict is that verdict even when the
    /// identity matches; another layout hash or CRC differs; and no
    /// row for the uid is unattributed.
    #[test]
    fn staged_row_is_judged_against_the_sent_prelude() {
        let (sent, _) = parse_v4(&stamp_v4(0x00CA00, 0xE49F_2E62, &[7, 8, 9])).unwrap();
        let row = |verdict: u8, layout_hash: u32, payload_crc: u32| ReadbackRow {
            full_uid: 0x00CA00,
            layout_hash,
            payload_crc,
            verdict,
            verdict_name: payload_check_name(verdict),
        };
        let (hash, crc) = (sent.layout_hash, sent.payload_crc);
        let judge = |r: ReadbackRow| judge_staged_row(Some(&r), &sent);
        assert_eq!(judge(row(0, hash, crc)), StagedRow::Matches);
        assert_eq!(judge(row(5, hash, crc)), StagedRow::Refused(5));
        assert_eq!(judge(row(3, hash ^ 1, crc)), StagedRow::Refused(3));
        assert_eq!(judge(row(0, hash ^ 1, crc)), StagedRow::Differs);
        assert_eq!(judge(row(0, hash, crc ^ 1)), StagedRow::Differs);
        assert_eq!(judge_staged_row(None, &sent), StagedRow::Unattributed);
    }

    /// @test A staged-pipeline stop never reports SUCCESS: a step the
    /// vehicle answered with SUCCESS stops as LOAD_FAILED (5, the
    /// vehicle's refused-RELOAD status), and a step it refused keeps
    /// its own status.
    #[test]
    fn a_staged_stop_never_reports_success() {
        assert_eq!(LOAD_FAILED, 5);
        assert_eq!(stop_status(0), LOAD_FAILED);
        assert_eq!(stop_status(4), 4);
        assert_eq!(stop_status(LOAD_FAILED), LOAD_FAILED);
    }

    /// @test For every struct that exports a layout hash in the struct
    /// dictionaries of config.toml's targets, the fallback recompute
    /// (the struct's fields expanded to leaves, then dict_leaf_specs)
    /// equals the exported hash. A nested field inlines its fragment's
    /// leaves under their own names, once per instance, with no row of
    /// its own; the shipped dictionaries carry such structs, so the
    /// rule is exercised, not assumed. The rover demo's dictionaries
    /// also list some arrays' elements as fields of their own (`a[1]`
    /// beside the array `a`), a shape the recompute does not take; its
    /// structs export their hashes, so the fallback does not run for
    /// them. Such a struct is skipped in that target only, the target
    /// must still have one, and anywhere else it fails the test.
    #[test]
    fn dictionary_recompute_matches_exported_layout_hashes() {
        use crate::core::config_manager::{expanded_fields, ComponentDict};
        const ELEMENT_VIEWS: [&str; 1] = ["Rover MCU"];
        let is_view = |leaf: &FieldDef, leaves: &[FieldDef]| {
            leaf.name
                .strip_suffix(']')
                .and_then(|n| n.rsplit_once('['))
                .is_some_and(|(a, i)| {
                    i.parse::<usize>().is_ok()
                        && leaves
                            .iter()
                            .any(|f| f.name == a && f.field_type == "array")
                })
        };
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let text = std::fs::read_to_string(repo.join("config.toml")).unwrap();
        let cfg: crate::config::ServerConfig = toml::from_str(&text).unwrap();
        let mut mismatches = Vec::new();
        let mut nested_checked = 0;
        let targets = repo.join("targets");
        for (t, structs) in cfg
            .targets
            .iter()
            .filter_map(|t| crate::config::repo_structs_path(t, &repo).map(|structs| (t, structs)))
        {
            let dir = structs.strip_prefix(&targets).unwrap_or(&structs).display();
            let views_allowed = ELEMENT_VIEWS.contains(&t.name.as_str());
            let mut views_skipped = 0;
            let mut files: Vec<PathBuf> = std::fs::read_dir(&structs)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            files.sort();
            for path in files {
                let text = std::fs::read_to_string(&path).unwrap();
                let dict: ComponentDict = serde_json::from_str(&text).unwrap();
                for (name, sdef) in &dict.structs {
                    let Some(want) = sdef.layout_hash_u32() else {
                        continue;
                    };
                    let leaves = expanded_fields(&dict, &sdef.fields, 0);
                    if leaves.iter().any(|l| is_view(l, &leaves)) {
                        if views_allowed {
                            views_skipped += 1;
                            continue;
                        }
                        mismatches.push(format!(
                            "{dir}/{}: {name} lists array elements as fields",
                            path.file_name().unwrap().to_string_lossy()
                        ));
                        continue;
                    }
                    let got = layout_hash(dict_leaf_specs(&leaves), sdef.size);
                    if sdef.fields.iter().any(|f| f.field_type == "nested") {
                        nested_checked += 1;
                    }
                    if got != want {
                        mismatches.push(format!(
                            "{dir}/{}: {name} exports 0x{want:08X}, recompute 0x{got:08X}",
                            path.file_name().unwrap().to_string_lossy()
                        ));
                    }
                }
            }
            assert!(
                !views_allowed || views_skipped > 0,
                "{}: no struct lists array elements as fields; drop it from ELEMENT_VIEWS",
                t.name
            );
        }
        assert!(
            mismatches.is_empty(),
            "the recompute disagrees with exported layout hashes:\n{}",
            mismatches.join("\n")
        );
        assert!(
            nested_checked > 0,
            "no exported hash covers a struct with a nested field"
        );
    }
}
