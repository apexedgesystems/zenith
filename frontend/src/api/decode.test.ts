import { describe, expect, it } from "vitest";
import {
  bytesToHex,
  decodeField,
  formatValue,
  hexToBytes,
  type FieldDef,
} from "./decode";

const view = (bytes: number[]) => new DataView(new Uint8Array(bytes).buffer);
const f = (over: Partial<FieldDef>): FieldDef => ({
  name: "x",
  type: "uint",
  offset: 0,
  size: 1,
  ...over,
});

describe("hexToBytes", () => {
  it("round-trips and rejects malformed input instead of guessing", () => {
    expect(Array.from(hexToBytes("deadBEEF")!)).toEqual([
      0xde, 0xad, 0xbe, 0xef,
    ]);
    expect(bytesToHex(hexToBytes("00ff10")!)).toBe("00ff10");
    expect(hexToBytes("abc")).toBeNull(); // odd length
    expect(hexToBytes("zz")).toBeNull(); // non-hex
    expect(Array.from(hexToBytes("")!)).toEqual([]);
  });
});

describe("decodeField", () => {
  it("decodes every scalar width little-endian", () => {
    const rows: [type: string, bytes: number[], value: number][] = [
      ["uint", [0xfe], 254],
      ["bool", [0x01], 1],
      ["uint", [0xef, 0xbe], 0xbeef],
      ["uint", [0xef, 0xbe, 0xad, 0xde], 0xdeadbeef],
      ["int", [0xfb], -5],
      ["int", [0x18, 0xfc], -1000],
      ["int", [0xeb, 0x32, 0xa4, 0xf8], -123456789],
      ["float", [0, 0, 0x40, 0x40], 3.0],
      ["double", [0, 0, 0, 0, 0, 0, 0x04, 0xc0], -2.5],
      // An 8-byte float is a double.
      ["float", [0, 0, 0, 0, 0, 0, 0x04, 0x40], 2.5],
    ];
    for (const [type, bytes, value] of rows) {
      const field = f({ type, size: bytes.length });
      expect(decodeField(view(bytes), field), `${type}:${bytes.length}`).toBe(
        value,
      );
    }
  });

  it("keeps 64-bit integers exact: a number in the safe range, a bigint beyond it", () => {
    const big = view([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
    const v = decodeField(big, f({ type: "uint", size: 8 }));
    expect(typeof v).toBe("bigint");
    expect(v).toBe(0x7fffffffffffffffn);
    // Small u64 stays a plain number for ergonomic display
    const small = view([42, 0, 0, 0, 0, 0, 0, 0]);
    expect(decodeField(small, f({ type: "uint", size: 8 }))).toBe(42);
    // Signed: both ends of the range
    const minusTwo = view([0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    expect(decodeField(minusTwo, f({ type: "int", size: 8 }))).toBe(-2);
    const min = view([0, 0, 0, 0, 0, 0, 0, 0x80]);
    expect(decodeField(min, f({ type: "int", size: 8 }))).toBe(
      -0x8000000000000000n,
    );
  });

  it("returns null for a type or a width it does not know", () => {
    expect(
      decodeField(view([1, 2, 3, 4]), f({ type: "mystery", size: 4 })),
    ).toBeNull();
    expect(
      decodeField(view([1, 2, 3]), f({ type: "int", size: 3 })),
    ).toBeNull();
  });

  it("trims bounded strings at the first NUL", () => {
    const bytes = [90, 69, 78, 0, 0, 0]; // "ZEN\0\0\0"
    expect(decodeField(view(bytes), f({ type: "string", size: 6 }))).toBe(
      "ZEN",
    );
  });

  it("decodes arrays element-wise including string elements", () => {
    const u16s = view([1, 0, 2, 0, 3, 0]);
    expect(
      decodeField(
        u16s,
        f({ type: "array", size: 6, element_type: "uint", dims: [3] }),
      ),
    ).toEqual([1, 2, 3]);

    const tags = view([65, 0, 0, 66, 67, 0]); // "A\0\0" "BC\0"
    expect(
      decodeField(
        tags,
        f({ type: "array", size: 6, element_type: "string", dims: [2] }),
      ),
    ).toEqual(["A", "BC"]);
  });

  it("returns null past the buffer instead of throwing mid-render", () => {
    expect(decodeField(view([1, 2]), f({ type: "uint", size: 4 }))).toBeNull();
  });
});

describe("formatValue", () => {
  it("formats each decoded kind consistently", () => {
    expect(formatValue(null)).toBe("--");
    expect(formatValue(123n)).toBe("123");
    expect(formatValue("ZEN")).toBe("ZEN");
    expect(formatValue([1, 2])).toBe("[1, 2]");
    expect(formatValue(250000)).toBe((250000).toLocaleString());
  });
});

describe("enum labels", () => {
  const field = {
    name: "mode",
    type: "uint",
    offset: 0,
    size: 1,
    enum: "DriveMode",
  };
  const enums = {
    DriveMode: {
      underlying_type: "uint8_t",
      values: { HOLD: 0, TRAJECTORY: 1, WAYPOINT: 2 },
    },
  };
  it("names a value the enum knows and keeps the number", () => {
    expect(formatValue(1, field, enums)).toBe("TRAJECTORY (1)");
  });
  it("falls back to the number outside the enum, without a table, or without a ref", () => {
    expect(formatValue(7, field, enums)).toBe("7");
    expect(formatValue(1, field)).toBe("1");
    expect(formatValue(1, { ...field, enum: undefined }, enums)).toBe("1");
  });
});
