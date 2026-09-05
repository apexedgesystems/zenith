//! Sequence-plan validation shared with the vehicle's load rules.
//!
//! The vehicle refuses defective sequence plans at load; the checks
//! here mirror those rules dictionary-driven so authors get the
//! refusal at upload time with the same wording. Targets whose
//! dictionaries do not describe the sequence structs skip every
//! check: the vehicle is always the backstop, never bypassed.

/// Pre-validate an RTS sequence upload against the vehicle's own
/// load rule, dictionary-driven: a step whose timeoutCycles would
/// expire within its own delay is refused at upload time with the
/// step named, instead of at vehicle load. Mirrors the producer's
/// check exactly (RTS type only -- ATS delays are not comparable --
/// and only when both cycle counts are nonzero). Targets whose
/// dictionaries do not describe the sequence structs skip the check:
/// the vehicle stays the backstop.
pub fn validate_rts_upload(
    dicts: &crate::core::config_manager::StructDictionary,
    bytes: &[u8],
) -> Result<(), String> {
    // Sequence files may arrive v3-stamped; validate the body either way.
    let body = match crate::core::tprm::parse_v3(bytes) {
        Ok((_, b)) => b,
        Err(_) => bytes,
    };

    let Some((seq, step)) = dicts.components.values().find_map(|d| {
        Some((
            d.structs.get("StandaloneSequenceTprm")?,
            d.structs.get("StandaloneStepTprm")?,
        ))
    }) else {
        return Ok(());
    };
    let field_of = |sdef: &crate::core::config_manager::StructDef, name: &str| {
        sdef.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| (f.offset, f.size))
    };
    let (Some((type_off, _)), Some((count_off, _)), Some((steps_off, _))) = (
        field_of(seq, "type"),
        field_of(seq, "stepCount"),
        field_of(seq, "steps"),
    ) else {
        return Ok(());
    };
    let (Some((delay_off, _)), Some((timeout_off, _))) = (
        field_of(step, "delayCycles"),
        field_of(step, "timeoutCycles"),
    ) else {
        return Ok(());
    };
    if body.len() < seq.size || step.size == 0 {
        return Ok(()); // not a sequence payload; the vehicle decides
    }

    const RTS_TYPE: u8 = 0;
    if body[type_off] != RTS_TYPE {
        return Ok(()); // ATS exempt by design
    }
    let step_count = body[count_off] as usize;
    let u32_at = |base: usize| -> Option<u32> {
        body.get(base..base + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    for i in 0..step_count {
        let base = steps_off + i * step.size;
        let (Some(delay), Some(timeout)) = (u32_at(base + delay_off), u32_at(base + timeout_off))
        else {
            break;
        };
        if timeout > 0 && delay > 0 && timeout <= delay {
            return Err(format!(
                "step {}: timeoutCycles {} expires within the step's own {}-cycle delay                  (TIMEOUT_SHORTER_THAN_DELAY; the vehicle would refuse this at load)",
                i, timeout, delay
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config_manager::{ComponentDict, FieldDef, StructDef, StructDictionary};
    use std::collections::HashMap as Map;

    fn f(name: &str, off: usize, sz: usize) -> FieldDef {
        FieldDef {
            name: name.to_string(),
            field_type: "uint".to_string(),
            offset: off,
            size: sz,
            value: serde_json::Value::Null,
            element_type: None,
            dims: None,
            constraints: None,
            struct_ref: None,
        }
    }

    fn sequence_dicts() -> StructDictionary {
        // Miniature sequence shape: 8-byte header + 2 x 12-byte steps.
        let seq = StructDef {
            category: "PROTOCOL".to_string(),
            size: 32,
            opcode: None,
            fields: vec![
                f("sequenceId", 0, 2),
                f("stepCount", 4, 1),
                f("type", 6, 1),
                f("steps", 8, 24),
            ],
            layout_hash: None,
            canonical_spec: None,
            packed: None,
        };
        let step = StructDef {
            category: "STRUCT".to_string(),
            size: 12,
            opcode: None,
            fields: vec![
                f("actionType", 0, 1),
                f("delayCycles", 4, 4),
                f("timeoutCycles", 8, 4),
            ],
            layout_hash: None,
            canonical_spec: None,
            packed: None,
        };
        StructDictionary {
            components: Map::from([(
                "Action".to_string(),
                ComponentDict {
                    component: "Action".to_string(),
                    structs: Map::from([
                        ("StandaloneSequenceTprm".to_string(), seq),
                        ("StandaloneStepTprm".to_string(), step),
                    ]),
                    enums: Map::new(),
                    capabilities: Vec::new(),
                },
            )]),
        }
    }

    fn sequence_bytes(seq_type: u8, steps: &[(u32, u32)]) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[4] = steps.len() as u8;
        b[6] = seq_type;
        for (i, (delay, timeout)) in steps.iter().enumerate() {
            let base = 8 + i * 12;
            b[base + 4..base + 8].copy_from_slice(&delay.to_le_bytes());
            b[base + 8..base + 12].copy_from_slice(&timeout.to_le_bytes());
        }
        b
    }

    /// @test The vehicle's RTS load rule, mirrored at upload time: a
    /// step whose timeout expires within its own delay refuses with
    /// the step index and both cycle counts named; valid plans,
    /// zero-cycle steps, and ATS sequences pass untouched; targets
    /// whose dictionaries lack the sequence structs skip the check.
    #[test]
    fn rts_upload_validation_mirrors_vehicle_rule() {
        let dicts = sequence_dicts();

        // Valid: timeout exceeds delay.
        assert!(validate_rts_upload(&dicts, &sequence_bytes(0, &[(10, 20), (5, 6)])).is_ok());
        // Zero timeout or zero delay: exempt (no timeout policy set).
        assert!(validate_rts_upload(&dicts, &sequence_bytes(0, &[(10, 0), (0, 5)])).is_ok());

        // Violation on step 1 names it with both values.
        let err =
            validate_rts_upload(&dicts, &sequence_bytes(0, &[(10, 20), (30, 30)])).unwrap_err();
        assert!(err.contains("step 1"), "{err}");
        assert!(err.contains("30"), "{err}");

        // ATS exempt by design.
        assert!(validate_rts_upload(&dicts, &sequence_bytes(1, &[(30, 30)])).is_ok());

        // A v3-stamped sequence validates its body.
        let stamped =
            crate::core::tprm::stamp_v3(0x000500, 0, &sequence_bytes(0, &[(30, 30)])).unwrap();
        assert!(validate_rts_upload(&dicts, &stamped).is_err());

        // No sequence structs in the dictionaries: skip, vehicle decides.
        let empty = StructDictionary::default();
        assert!(validate_rts_upload(&empty, &sequence_bytes(0, &[(30, 30)])).is_ok());

        // Short payload: not a sequence; skip.
        assert!(validate_rts_upload(&dicts, &[0u8; 4]).is_ok());
    }
}
