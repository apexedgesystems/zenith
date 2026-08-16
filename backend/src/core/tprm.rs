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
//! Canonical field spec: every leaf field contributes `name:type:size;`
//! in emission order; layoutHash is the CRC-32 of that ASCII string,
//! so two layouts with the same byte count still hash apart.
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

/// One entry of a payload layout, in emission order. Nested-struct
/// containers appear as `field_type = "struct"` with size 0, followed
/// by their leaves -- they carry no bytes but are part of the hashed
/// spec.
#[derive(Debug, Clone)]
pub struct LeafSpec<'a> {
    pub name: &'a str,
    pub field_type: &'a str,
    /// Total serialized bytes of this leaf (element size x count for
    /// arrays).
    pub size: usize,
    /// For arrays: (element_type, element_size, element_count).
    pub array: Option<(&'a str, usize, usize)>,
}

/// Build the canonical field spec string. Scalar leaves contribute
/// `name:type:size;`; array leaves contribute
/// `name:array:totalsize;[elemtype:elemsizexN]`.
pub fn canonical_field_spec<'a, I>(fields: I) -> String
where
    I: IntoIterator<Item = LeafSpec<'a>>,
{
    let mut spec = String::new();
    for leaf in fields {
        match leaf.array {
            None => {
                spec.push_str(&format!("{}:{}:{};", leaf.name, leaf.field_type, leaf.size));
            }
            Some((elem_type, elem_size, count)) => {
                spec.push_str(&format!("{}:array:{};", leaf.name, leaf.size));
                spec.push_str(&format!("[{}:{}x{}]", elem_type, elem_size, count));
            }
        }
    }
    spec
}

/// layoutHash over a canonical field spec.
pub fn layout_hash<'a, I>(fields: I) -> u32
where
    I: IntoIterator<Item = LeafSpec<'a>>,
{
    crc32(canonical_field_spec(fields).as_bytes())
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
        element_type: Option<String>,
        element_count: usize,
    }

    /// Parse a vector toml in the authoring-template format, returning
    /// leaves in declaration order. Line-scan on purpose: declaration
    /// order is part of the contract, and a map-based toml parse would
    /// lose it. Skips the [__enums__] section and struct containers
    /// (their nested leaves carry the layout).
    fn parse_vector_toml(name: &str) -> Vec<TomlLeaf> {
        let text = std::fs::read_to_string(contract_dir().join("toml").join(name)).unwrap();
        let mut leaves: Vec<TomlLeaf> = Vec::new();
        let mut in_leaf = false;
        for line in text.lines() {
            let line = line.trim();
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
        // Struct containers stay in the list: they carry no bytes but
        // contribute `name:struct:0;` to the canonical spec, followed
        // by their nested leaves.
        leaves
    }

    fn leaf_specs(leaves: &[TomlLeaf]) -> Vec<LeafSpec<'_>> {
        leaves
            .iter()
            .map(|l| LeafSpec {
                name: &l.name,
                field_type: &l.field_type,
                size: l.size,
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
        ] {
            let vector = read_vector(bin_name);
            let leaves = parse_vector_toml(toml_name);
            let hash = layout_hash(leaf_specs(&leaves));

            let (prelude, body) =
                parse_v3(&vector).unwrap_or_else(|e| panic!("{bin_name}: vector fails parse: {e}"));
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

    /// @test stamp_v3 refuses a body larger than the u16 size field
    /// can describe instead of silently truncating the length.
    #[test]
    fn stamp_rejects_oversized_body() {
        let big = vec![0u8; 70_000];
        assert_eq!(stamp_v3(0, 0, &big), Err(TprmError::TooLarge));
    }
}
