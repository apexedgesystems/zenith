# TPRM conformance contract (zenith copy)

Byte-level golden vectors for the TPRM formats zenith writes and
reads. Zenith's encoder answers to these files, not to any other
implementation's source code: the backend test suite asserts
byte-identity against them on every run, so CI fails on any
divergence between zenith's writer and the contract.

This is a COPY, refreshed deliberately -- never a reference to
another repo. The vector set is the cross-repo contract; the repos
stay independent.

## Contents

| Path                  | What it is                                                                                    |
| --------------------- | --------------------------------------------------------------------------------------------- |
| `toml/*.toml`         | Vector sources in the authoring-template format; the intended values are readable here        |
| `payloads/*.bin`      | Format A v3: 20-byte prelude (magic APV3, version, size, fullUid, layout hash, CRC-32) + body |
| `payloads/*_raw.bin`  | The unstamped serialization of the same payload                                               |
| `archives/basic.tprm` | Format B: a packed archive (v3 component entries + a raw RTS-reserved entry)                  |

## The v3 prelude (what zenith stamps)

20 bytes, little-endian, ahead of every component payload upload:

```
magic[4]       = "APV3"
version[2]     = 3
payloadSize[2] = byte length of the payload that follows
fullUid[4]     = component the payload targets
layoutHash[4]  = CRC-32 of the canonical field spec
payloadCrc[4]  = CRC-32 (IEEE 802.3) of the payload bytes
```

Canonical field spec: every leaf field contributes `name:type:size;`
in emission order; layoutHash is the CRC-32 of that ASCII string. The
vehicle verifies all six prelude properties on load and rejects each
failure distinctly -- an unstamped or mis-stamped upload never
reaches a component.

The payload uids are part of the contract: `scalar_types` targets
0x000000, `strings_arrays` 0x00D001, `nested_enum` 0x00CA00.

## Refreshing this copy

When the producing repo announces a contract change (relay note or a
red conformance suite after a target-config regeneration):

1. Copy the producing repo's `compat/tprm/` over this directory,
   preserving this README.
2. Run `make test-backend` -- the conformance suite in
   `backend/src/core/tprm.rs` re-asserts every vector.
3. Record the source commit of the copy in the refresh commit body.

Proving the gate works: flip any committed vector byte and the suite
must fail.
