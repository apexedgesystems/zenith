# TPRM conformance contract (zenith copy)

Byte-level golden vectors for the TPRM formats zenith writes and
reads. Zenith's encoder answers to these files, not to any other
implementation's source code: the backend test suite asserts
byte-identity against them on every run, so CI fails on any
divergence between zenith's writer and the contract.

This is a COPY, refreshed deliberately -- never a reference to
another repo. The vector set is the cross-repo contract; the repos
stay independent.

Source: the `compat/tprm/` directory of Apex CSF (apex_csf) at commit
5bda98c, copied unmodified apart from this README.

## Contents

| Path                  | What it is                                                                                                       |
| --------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `toml/*.toml`         | Vector sources in the authoring-template format; the intended values are readable here                           |
| `payloads/*.bin`      | Format A v4: 28-byte prelude (magic APV4, version, reserved, 64-bit size, fullUid, layout hash, CRC-32) + body |
| `payloads/*_raw.bin`  | The unstamped serialization of the same payload                                                                  |
| `archives/basic.tprm` | Format B: a packed archive (v4 component entries + a raw RTS-reserved entry)                                     |

## The v4 prelude (what zenith stamps)

28 bytes, little-endian, ahead of every component payload upload:

```
magic[4]       = "APV4"
version[2]     = 4
reserved[2]    = 0
payloadSize[8] = byte length of the payload that follows
fullUid[4]     = component the payload targets
layoutHash[4]  = CRC-32 of the canonical field spec
payloadCrc[4]  = CRC-32 (IEEE 802.3) of the payload bytes
```

Canonical field spec (offset-aware): every leaf field contributes
`name:type:size:offset;` in emission order -- nested structs flatten
to their leaves at absolute offsets, arrays contribute
`name:array:total:offset;[elemtype:elemsizexN]` -- and the string
ends with a `|size:total` terminator. A key typed `struct`
contributes no row of its own; its leaves do, at their absolute
offsets, and a plain nested table is the same grouping (the
nested_enum vector pins this). layoutHash is the CRC-32 of that ASCII
string.

The vehicle verifies the prelude on load and rejects each failure
distinctly (magic, version, size, target uid, body CRC, and the layout
hash where it knows the layout) -- an unstamped or mis-stamped upload
never reaches a component. Zenith's parser checks length, magic,
version, size and CRC in the same order and also refuses non-zero
reserved bytes.

The payload uids are part of the contract: `scalar_types` targets
0x000000, `strings_arrays` 0x00D001, `nested_enum` 0x00CA00,
`scheduler_shape` 0x000100 (the variable-length header + task-entry
shape; its hash covers header leaves then each entry's leaves in
order, no container markers, so it is entry-count dependent).

## Refreshing this copy

When the producing repo announces a contract change (relay note or a
red conformance suite after a target-config regeneration):

1. Copy the producing repo's `compat/tprm/` over this directory,
   preserving this README.
2. Update the Source line above to the commit copied, and name it in
   the refresh commit body.
3. Run `make test-backend` -- the conformance suite in
   `backend/src/core/tprm.rs` re-asserts every vector.

Proving the gate works: flip any committed vector byte and the suite
must fail.
