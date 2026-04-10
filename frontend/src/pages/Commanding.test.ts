import { describe, it, expect } from "vitest";
import { encodeField, type CommandFieldDef } from "./Commanding";

const f = (type: string): CommandFieldDef => ({ name: "x", type });

describe("encodeField", () => {
  describe("uint8", () => {
    it("encodes decimal value as 2-char hex", () => {
      expect(encodeField(f("uint8"), "42")).toBe("2a");
    });

    it("encodes hex input (0x prefix)", () => {
      expect(encodeField(f("uint8"), "0xFF")).toBe("ff");
      expect(encodeField(f("uint8"), "0x0a")).toBe("0a");
    });

    it("clamps negative values to 0", () => {
      expect(encodeField(f("uint8"), "-5")).toBe("00");
    });

    it("masks values larger than 0xFF", () => {
      expect(encodeField(f("uint8"), "256")).toBe("00"); // 0x100 & 0xFF = 0
      expect(encodeField(f("uint8"), "257")).toBe("01"); // 0x101 & 0xFF = 1
    });

    it("returns safe zero for non-numeric input", () => {
      expect(encodeField(f("uint8"), "abc")).toBe("00");
      expect(encodeField(f("uint8"), "")).toBe("00");
    });
  });

  describe("uint16", () => {
    it("encodes as 4-char little-endian hex", () => {
      // 0x1234 LE = 34 12
      expect(encodeField(f("uint16"), "0x1234")).toBe("3412");
    });

    it("masks values larger than 0xFFFF", () => {
      // 0x10000 & 0xFFFF = 0
      expect(encodeField(f("uint16"), "65536")).toBe("0000");
    });

    it("clamps negative to 0", () => {
      expect(encodeField(f("uint16"), "-1")).toBe("0000");
    });
  });

  describe("uint32", () => {
    it("encodes as 8-char little-endian hex", () => {
      // 0xDEADBEEF LE = ef be ad de
      expect(encodeField(f("uint32"), "0xDEADBEEF")).toBe("efbeadde");
    });

    it("encodes 0 cleanly", () => {
      expect(encodeField(f("uint32"), "0")).toBe("00000000");
    });

    it("clamps negative to 0", () => {
      expect(encodeField(f("uint32"), "-1")).toBe("00000000");
    });
  });

  describe("unknown type", () => {
    it("falls back to single uint8 byte", () => {
      expect(encodeField(f("mystery"), "42")).toBe("2a");
    });
  });
});
