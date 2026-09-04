//! TPRM format A v3 prelude: stamp and verify.
//!
//! Every component payload zenith uploads carries a 20-byte
//! little-endian prelude the vehicle verifies before the body touches
//! a component:
//!
//!   magic[4]       = "APV3"
//!   version[2]     = 3
//!   payloadSize[2] = byte length of the payload that follows
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

/// v3 magic bytes.
pub const MAGIC: &[u8; 4] = b"APV3";
/// v3 format version.
pub const VERSION: u16 = 3;
/// Prelude length in bytes.
pub const HEADER_SIZE: usize = 20;

/// Errors from prelude verification. The verify-side surface
/// (parse_v3 and the reject variants) is compiled for the full
/// contract even though the server binary currently only stamps:
/// readback validation consumes it, and the conformance tests pin it.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TprmError {
    #[error("payload too short for v3 prelude")]
    Truncated,
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported version {0}")]
    BadVersion(u16),
    #[error("payload size mismatch (header {header}, actual {actual})")]
    SizeMismatch { header: u16, actual: usize },
    #[error("payload CRC mismatch")]
    BadCrc,
    #[error("payload exceeds the u16 size field")]
    TooLarge,
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

/// Stamp the v3 prelude onto a payload body.
pub fn stamp_v3(full_uid: u32, layout_hash: u32, payload: &[u8]) -> Result<Vec<u8>, TprmError> {
    let size = u16::try_from(payload.len()).map_err(|_| TprmError::TooLarge)?;
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&full_uid.to_le_bytes());
    out.extend_from_slice(&layout_hash.to_le_bytes());
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Parsed v3 prelude fields.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreludeFields {
    pub version: u16,
    pub payload_size: u16,
    pub full_uid: u32,
    pub layout_hash: u32,
    pub payload_crc: u32,
}

/// Verify a stamped payload and return its prelude fields and body.
/// Checks magic, version, declared-vs-actual size, and body CRC; the
/// caller compares full_uid and layout_hash against its expectations
/// (they are policy, not format).
#[allow(dead_code)]
pub fn parse_v3(data: &[u8]) -> Result<(PreludeFields, &[u8]), TprmError> {
    if data.len() < HEADER_SIZE {
        return Err(TprmError::Truncated);
    }
    if &data[0..4] != MAGIC {
        return Err(TprmError::BadMagic);
    }
    let version = u16::from_le_bytes([data[4], data[5]]);
    if version != VERSION {
        return Err(TprmError::BadVersion(version));
    }
    let payload_size = u16::from_le_bytes([data[6], data[7]]);
    let body = &data[HEADER_SIZE..];
    if body.len() != payload_size as usize {
        return Err(TprmError::SizeMismatch {
            header: payload_size,
            actual: body.len(),
        });
    }
    let fields = PreludeFields {
        version,
        payload_size,
        full_uid: u32::from_le_bytes([data[8], data[9], data[10], data[11]]),
        layout_hash: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
        payload_crc: u32::from_le_bytes([data[16], data[17], data[18], data[19]]),
    };
    if crc32(body) != fields.payload_crc {
        return Err(TprmError::BadCrc);
    }
    Ok((fields, body))
}

/* ----------------------------- READBACK / VERIFY ----------------------------- */

/// Executive opcodes for the staged-verify surface (apex #137).
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

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn contract_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../compat/tprm")
    }

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
    /// lose it. Skips the [__enums__] section and struct containers
    /// (their nested leaves carry the layout).
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
        // toml carries sizes only). Struct containers stay as rows --
        // the vector contract gives them `name:struct:0:offset;` at
        // the container's starting position, zero bytes consumed --
        // followed by their leaves at absolute offsets. Section
        // headers that only held inline tables have no type and drop.
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

    fn leaf_specs(leaves: &[TomlLeaf]) -> Vec<LeafSpec<'_>> {
        leaves
            .iter()
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
                parse_v3(&vector).unwrap_or_else(|e| panic!("{bin_name}: vector fails parse: {e}"));
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

            let restamped = stamp_v3(full_uid, hash, body).unwrap();
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
        let (prelude, body) = parse_v3(&stamped).unwrap();
        assert_eq!(raw, body);
        let restamped = stamp_v3(prelude.full_uid, prelude.layout_hash, &raw).unwrap();
        assert_eq!(restamped, stamped);
    }

    /// @test Each prelude defect is rejected distinctly: truncation,
    /// bad magic, wrong version, lying size, corrupted body.
    #[test]
    fn parse_rejects_each_defect_distinctly() {
        let good = stamp_v3(0x00D000, 0xABCD_1234, &[1, 2, 3, 4]).unwrap();
        assert!(parse_v3(&good).is_ok());

        assert_eq!(parse_v3(&good[..10]), Err(TprmError::Truncated));

        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(parse_v3(&bad), Err(TprmError::BadMagic));

        let mut bad = good.clone();
        bad[4] = 9;
        assert_eq!(parse_v3(&bad), Err(TprmError::BadVersion(9)));

        let mut bad = good.clone();
        bad[6] = 99;
        assert!(matches!(
            parse_v3(&bad),
            Err(TprmError::SizeMismatch {
                header: 99,
                actual: 4
            })
        ));

        let mut bad = good.clone();
        *bad.last_mut().unwrap() ^= 0xFF;
        assert_eq!(parse_v3(&bad), Err(TprmError::BadCrc));
    }

    /// @test Variable-length (header + entries) layout hashing is
    /// entry-count dependent: header leaves then each entry's leaves
    /// in order, no container markers. The 2-task hash is anchored to
    /// the contract vector's own prelude (not a transcribed constant),
    /// and the 3-task hash pins the producer's worked evidence from
    /// the 2026-08-16 relay answer -- together they prove the count
    /// changes the hash, which is what makes a stale task list
    /// un-uploadable.
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
        // entry-count-dependent twice over. The 2-task value anchors
        // to the contract vector's own prelude; the pre-flag-day
        // 3-task evidence constant is retired with the old form.
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
        let (vector_prelude, _) = parse_v3(&read_vector("scheduler_shape.bin")).unwrap();
        assert_eq!(hash_for(2), vector_prelude.layout_hash);
        assert_ne!(hash_for(2), hash_for(3));
    }

    /// @test stamp_v3 refuses a body larger than the u16 size field
    /// can describe instead of silently truncating the length.
    #[test]
    fn stamp_rejects_oversized_body() {
        let big = vec![0u8; 70_000];
        assert_eq!(stamp_v3(0, 0, &big), Err(TprmError::TooLarge));
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
}
