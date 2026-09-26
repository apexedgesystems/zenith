#!/usr/bin/env python3
"""Generate a zenith target directory from an F-prime deployment's
build-generated JSON topology dictionary.

The cleanest of the three generators: F-prime hands us a versioned,
fully-typed dictionary, and record-shaped telemetry is a
first-class zenith dictionary form -- so this is a pure transform
producing exactly two files:

    <out>/records.json         THE record dictionary: header shape,
                               byte order, and every channel's id,
                               name, type, size, annotation
    <out>/telemetry.json       curated layouts from the spec, or a
                               default chunking per component

Record layout (stock ComCcsds downlink, confirmed on a live capture):
[descriptor U16][channel id U32][time: base U16, ctx U8, secs U32,
useconds U32] = 17 bytes, then the value, big-endian throughout per
F-prime serialization.

String channels are variable-length on the wire (length-prefixed)
and are skipped with a warning: a string record's id is absent from
the table, so an aggregate's tail past one is dropped and counted.

Usage: dictgen.py <spec.json> <output-dir>
Spec: { "application": ..., "dictionary": path, "layouts": [...] }
"""

import json
import os
import sys

RECORD_HEADER = 17
DESCRIPTOR_OFFSET_OF_ID = 2
ID_SIZE = 4
TELEMETRY_APID = "0x001"
# The dictionary format this transform was written and verified
# against; a different version gets a warning, not silence.
TESTED_SPEC_VERSION = "1.0.0"
# Stock F-prime packet types deliberately not decoded (events, files,
# packetized tlm, data products, idle, params, handshake, unknown).
SKIP_APIDS = ["0x002", "0x003", "0x004", "0x005", "0x006", "0x007", "0x0FE", "0x0FF"]


def resolve_type(t: dict, typedefs: dict, depth: int = 0) -> dict | None:
    """Follow alias/enum indirection to a concrete numeric type."""
    if depth > 8:
        return None
    kind = t.get("kind")
    if kind in ("integer", "float", "bool"):
        return t
    if kind == "qualifiedIdentifier":
        td = typedefs.get(t.get("name"))
        if td is None:
            return None
        if td.get("kind") == "alias":
            return resolve_type(td.get("underlyingType", {}), typedefs, depth + 1)
        if td.get("kind") == "enum":
            return resolve_type(td.get("representationType", {}), typedefs, depth + 1)
    return None


def describe_kind(t: dict, typedefs: dict) -> str:
    """Why a channel type is not converted, by what it resolves to."""
    kind = t.get("kind")
    if kind == "string":
        return "string: variable-length on the wire"
    if kind == "qualifiedIdentifier":
        td = typedefs.get(t.get("name"), {})
        tk = td.get("kind", "unknown")
        if tk == "struct":
            return f"struct {t.get('name')}: aggregate values are not converted"
        if tk == "array":
            return f"array {t.get('name')}: aggregate values are not converted"
        if tk == "alias":
            return f"alias {t.get('name')}: resolves to a non-numeric type"
        return f"{tk} {t.get('name')}: not a numeric type"
    return f"{kind}: not a numeric type"


def zenith_type(concrete: dict) -> tuple[str, int]:
    size = concrete["size"] // 8
    if concrete["kind"] == "float":
        return "float", size
    if concrete["kind"] == "bool":
        return "uint", max(size, 1)
    return ("int" if concrete.get("signed") else "uint"), size


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    spec_path = sys.argv[1]
    spec = json.load(open(spec_path))
    # The dictionary path is relative to the spec file, which lives
    # with the deployment, not to wherever the generator is invoked.
    dict_path = os.path.join(os.path.dirname(os.path.abspath(spec_path)), spec["dictionary"])
    fdict = json.load(open(dict_path))
    out_dir = sys.argv[2]
    os.makedirs(out_dir, exist_ok=True)

    meta = fdict.get("metadata", {})
    if meta.get("dictionarySpecVersion") != TESTED_SPEC_VERSION:
        print(f"warning: dictionary spec {meta.get('dictionarySpecVersion')} is not "
              f"the tested {TESTED_SPEC_VERSION}; check the record header shape "
              f"against the framework's serialization before trusting the output",
              file=sys.stderr)
    typedefs = {t.get("qualifiedName"): t for t in fdict.get("typeDefinitions", [])}

    records = []
    skipped = []
    converted = 0
    for ch in fdict["telemetryChannels"]:
        concrete = resolve_type(ch["type"], typedefs)
        if concrete is None:
            skipped.append((ch["name"], describe_kind(ch["type"], typedefs)))
            continue
        ftype, fsize = zenith_type(concrete)
        entry = {"id": ch["id"], "channel": ch["name"],
                 "type": ftype, "size": fsize}
        if ch.get("annotation"):
            entry["annotation"] = ch["annotation"]
        records.append(entry)
        converted += 1

    with open(os.path.join(out_dir, "records.json"), "w") as f:
        json.dump({
            "_generated_by": (
                f"fprime-dictgen from {meta.get('deploymentName', '?')} "
                f"(framework {meta.get('frameworkVersion', '?')}, "
                f"dictionary spec {meta.get('dictionarySpecVersion', '?')})"
            ),
            "record_apid": TELEMETRY_APID,
            "id_offset": DESCRIPTOR_OFFSET_OF_ID,
            "id_size": ID_SIZE,
            "header_size": RECORD_HEADER,
            "byte_order": "be",
            "skip_apids": SKIP_APIDS,
            "records": records,
        }, f, indent=2)
        f.write("\n")

    layouts = spec.get("layouts")
    if not layouts:
        # Default: chunk channels per component, 8 per plot.
        plots = []
        per_comp: dict = {}
        for ch in fdict["telemetryChannels"]:
            if resolve_type(ch["type"], typedefs) is None:
                continue
            comp, _, _leaf = ch["name"].rpartition(".")
            per_comp.setdefault(comp, []).append(ch["name"])
        for comp, chans in per_comp.items():
            for i in range(0, len(chans), 8):
                n = f" ({i // 8 + 1})" if len(chans) > 8 else ""
                plots.append({"title": f"{comp}{n}", "channels": chans[i:i + 8],
                              "height": 200})
        layouts = [{"name": "Default", "plots": plots}]
    with open(os.path.join(out_dir, "telemetry.json"), "w") as f:
        json.dump({"layouts": layouts}, f, indent=2)
        f.write("\n")

    print(f"{converted} channels converted, {len(skipped)} skipped:")
    for name, why in skipped:
        print(f"  skipped {name} ({why})")
    print(f"framework {meta.get('frameworkVersion')}, "
          f"dictionary spec {meta.get('dictionarySpecVersion')}")


if __name__ == "__main__":
    main()
