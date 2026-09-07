"""DWARF struct extractor -- runs inside gdb (see dictgen.py).

Walks the debug info of the already-loaded ELF for each requested
type and emits flat, payload-absolute field lists as JSON on stdout
between marker lines. Knows the cFS telemetry conventions on
purpose: this is definition-level tooling that converts one
framework's build artifacts into zenith dictionaries -- the zenith
engine itself never sees any of this.

Offset model: zenith receives a space packet's data field (the bytes
after the 6-octet primary header), while DWARF offsets start at the
struct top which includes the full message header. Every offset is
therefore shifted by -6; whatever remains of the header (the 6-byte
time secondary header) is dropped along with any other field that
would land below zero.
"""

import json
import os

import gdb

PRIMARY_HEADER = 6
SPEC = json.loads(os.environ["DICTGEN_SPEC"])

# cFS telemetry struct convention: { TelemetryHeader; Payload; }.
HEADER_NAMES = {"TelemetryHeader", "TlmHeader", "Hdr"}
PAYLOAD_NAMES = {"Payload"}


def is_signed(t):
    try:
        return t.is_signed
    except AttributeError:  # older gdb
        return "unsigned" not in str(t)


def walk(t, prefix, base_off, out, top_level):
    t = t.strip_typedefs()
    code = t.code
    if code == gdb.TYPE_CODE_STRUCT:
        for f in t.fields():
            if not hasattr(f, "bitpos") or f.bitpos is None:
                continue  # static member
            if f.bitsize and f.bitsize % 8 != 0:
                continue  # true bitfield: no byte address
            name = f.name or "anon"
            if top_level and name in HEADER_NAMES:
                continue
            if top_level and name in PAYLOAD_NAMES:
                sub = prefix  # inline the payload without a prefix
            else:
                sub = f"{prefix}.{name}" if prefix else name
            walk(f.type, sub, base_off + f.bitpos // 8, out, False)
        return
    if code == gdb.TYPE_CODE_UNION:
        return  # ambiguous bytes: skip rather than guess
    entry = {"name": prefix, "offset": base_off, "size": t.sizeof}
    if code == gdb.TYPE_CODE_ARRAY:
        elem = t.target().strip_typedefs()
        char_like = elem.code == gdb.TYPE_CODE_INT and elem.sizeof == 1
        entry["type"] = "string" if char_like else "array"
    elif code == gdb.TYPE_CODE_INT:
        entry["type"] = "int" if is_signed(t) else "uint"
    elif code == gdb.TYPE_CODE_BOOL:
        entry["type"] = "uint"
    elif code == gdb.TYPE_CODE_ENUM:
        entry["type"] = "uint"
    elif code == gdb.TYPE_CODE_FLT:
        entry["type"] = "float"
    else:
        return  # pointers etc.: not telemetry values
    out.append(entry)


results = []
for item in SPEC:
    t = gdb.lookup_type(item["type"])
    fields = []
    walk(t, "", 0, fields, True)
    shifted = []
    for f in fields:
        f["offset"] -= PRIMARY_HEADER
        if f["offset"] >= 0:
            shifted.append(f)
    results.append(
        {
            "component": item["component"],
            "struct": item["type"],
            "size": t.sizeof - PRIMARY_HEADER,
            "fields": shifted,
        }
    )

print("DICTGEN_BEGIN")
print(json.dumps(results))
print("DICTGEN_END")
