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
    expect(decodeField(view([0xfb]), f({ type: "int", size: 1 }))).toBe(-5);
    expect(decodeField(view([0xef, 0xbe]), f({ type: "uint", size: 2 }))).toBe(
      0xbeef,
    );
    expect(
      decodeField(view([0xef, 0xbe, 0xad, 0xde]), f({ type: "uint", size: 4 })),
    ).toBe(0xdeadbeef);
    expect(
      decodeField(view([0, 0, 0x40, 0x40]), f({ type: "float", size: 4 })),
    ).toBe(3.0);
  });

  it("preserves 64-bit precision as bigint beyond the safe range", () => {
    const big = view([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
    const v = decodeField(big, f({ type: "uint", size: 8 }));
    expect(typeof v).toBe("bigint");
    expect(v).toBe(0x7fffffffffffffffn);
    // Small u64 stays a plain number for ergonomic display
    const small = view([42, 0, 0, 0, 0, 0, 0, 0]);
    expect(decodeField(small, f({ type: "uint", size: 8 }))).toBe(42);
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
