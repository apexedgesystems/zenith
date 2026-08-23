/** THE binary decoder for struct-dict-described payloads.
 *
 *  Single implementation by design: multiple per-page copies drift in
 *  type coverage and formatting until the same struct displays
 *  differently per page. Every consumer imports from here; do not
 *  fork it into a component.
 */

export interface FieldDef {
  name: string;
  type: string;
  offset: number;
  size: number;
  element_type?: string;
  dims?: number[];
  constraints?: {
    min?: number;
    max?: number;
    step?: number;
    allowed?: number[];
  };
}

/** Strict hex -> bytes. Returns null on odd length or non-hex input
 *  instead of silently mis-parsing (one legacy copy threw on empty
 *  input, another zero-filled). */
export function hexToBytes(hex: string): Uint8Array | null {
  if (hex.length % 2 !== 0 || /[^0-9a-fA-F]/.test(hex)) return null;
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

export function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** Decoded field value: numbers for numerics (bigint for 64-bit ints
 *  beyond safe range), string for strings, arrays element-wise. */
export type DecodedValue = number | bigint | string | DecodedValue[] | null;

export function decodeField(view: DataView, field: FieldDef): DecodedValue {
  const off = field.offset;
  if (off + field.size > view.byteLength) return null;

  const scalar = (type: string, at: number, size: number): DecodedValue => {
    switch (`${type}:${size}`) {
      case "uint:1":
      case "bool:1":
        return view.getUint8(at);
      case "uint:2":
        return view.getUint16(at, true);
      case "uint:4":
        return view.getUint32(at, true);
      case "uint:8": {
        // Preserve precision: cycle counters exceed 2^53. Callers
        // format bigint; never silently Number() it.
        const v = view.getBigUint64(at, true);
        return v <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(v) : v;
      }
      case "int:1":
        return view.getInt8(at);
      case "int:2":
        return view.getInt16(at, true);
      case "int:4":
        return view.getInt32(at, true);
      case "int:8": {
        const v = view.getBigInt64(at, true);
        return v >= BigInt(Number.MIN_SAFE_INTEGER) &&
          v <= BigInt(Number.MAX_SAFE_INTEGER)
          ? Number(v)
          : v;
      }
      case "float:4":
        return view.getFloat32(at, true);
      case "double:8":
      case "float:8":
        return view.getFloat64(at, true);
      default:
        return null;
    }
  };

  if (field.type === "string") {
    const bytes = new Uint8Array(
      view.buffer,
      view.byteOffset + off,
      field.size,
    );
    const nul = bytes.indexOf(0);
    return new TextDecoder().decode(nul >= 0 ? bytes.slice(0, nul) : bytes);
  }

  if (field.type === "array") {
    const count = (field.dims ?? []).reduce((a, b) => a * b, 1);
    if (count <= 0 || field.size % count !== 0) return null;
    const elemSize = field.size / count;
    const elemType = field.element_type ?? "uint";
    const out: DecodedValue[] = [];
    for (let i = 0; i < count; i++) {
      out.push(
        elemType === "string"
          ? decodeField(view, {
              ...field,
              type: "string",
              offset: off + i * elemSize,
              size: elemSize,
            })
          : scalar(elemType, off + i * elemSize, elemSize),
      );
    }
    return out;
  }

  return scalar(field.type, off, field.size);
}

/** Uniform display formatting so every page renders the same value
 *  the same way. */
export function formatValue(v: DecodedValue, field?: FieldDef): string {
  if (v === null) return "--";
  if (typeof v === "bigint") return v.toString();
  if (typeof v === "string") return v;
  if (Array.isArray(v)) return `[${v.map((e) => formatValue(e)).join(", ")}]`;
  if (field?.type === "float" || field?.type === "double") {
    return Number.isInteger(v) ? v.toFixed(1) : String(v);
  }
  return v >= 100000 ? v.toLocaleString() : String(v);
}
