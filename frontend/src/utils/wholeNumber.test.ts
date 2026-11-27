import { describe, expect, it } from "vitest";
import { parseWholeNumber, wholeNumberHint } from "./wholeNumber";

/** Each row's text reads, for a field of the row's width and sign, as
 *  the row's value. */
function expectRead(
  rows: [text: string, bits: number, signed: boolean, value: bigint][],
) {
  for (const [text, bits, signed, value] of rows) {
    const field = `${signed ? "int" : "uint"}${bits}`;
    expect(parseWholeNumber(text, bits, signed), `${field} '${text}'`).toBe(
      value,
    );
  }
}

/** Each row's text is refused for a field of the row's width and sign. */
function expectRefused(rows: [text: string, bits: number, signed: boolean][]) {
  for (const [text, bits, signed] of rows) {
    const field = `${signed ? "int" : "uint"}${bits}`;
    expect(parseWholeNumber(text, bits, signed), `${field} '${text}'`).toBe(
      null,
    );
  }
}

describe("parseWholeNumber", () => {
  it("reads decimal and 0x hex exactly, up to each width's limits", () => {
    expectRead([
      ["0", 8, false, 0n],
      ["255", 8, false, 255n],
      ["0xff", 8, false, 255n],
      ["0XFF", 8, false, 255n],
      ["-128", 8, true, -128n],
      ["127", 8, true, 127n],
      ["4294967295", 32, false, 4294967295n],
      // Past 2^53, where a double would round to ...992.
      ["9007199254740993", 64, true, 9007199254740993n],
      ["-9007199254740993", 64, true, -9007199254740993n],
      ["0x20000000000001", 64, false, 9007199254740993n],
      ["9223372036854775807", 64, true, 9223372036854775807n],
      ["-9223372036854775808", 64, true, -9223372036854775808n],
      ["0x7FFFFFFFFFFFFFFF", 64, true, 9223372036854775807n],
      ["18446744073709551615", 64, false, 18446744073709551615n],
      ["0xFFFFFFFFFFFFFFFF", 64, false, 18446744073709551615n],
    ]);
  });

  it("ignores surrounding whitespace and leading zeros", () => {
    expectRead([
      [" 7 ", 8, false, 7n],
      ["007", 16, false, 7n],
      ["-0", 64, true, 0n],
      ["0x0001", 16, false, 1n],
    ]);
  });

  it("refuses a value one past either end of the field's range", () => {
    expectRefused([
      ["256", 8, false],
      ["-1", 8, false],
      ["128", 8, true],
      ["-129", 8, true],
      ["4294967296", 32, false],
      ["9223372036854775808", 64, true],
      ["-9223372036854775809", 64, true],
      ["0x8000000000000000", 64, true],
      ["18446744073709551616", 64, false],
      ["0x10000000000000000", 64, false],
      ["-1", 64, false],
    ]);
  });

  it("refuses what is not a whole decimal or 0x hex number", () => {
    for (const text of [
      "",
      "  ",
      "1.5",
      "1.0",
      "1e3",
      "+5",
      "--1",
      "0x",
      "0x1G",
      "-0x10",
      "12abc",
      "abc",
      "Infinity",
      "0b101",
      "0o17",
    ]) {
      expectRefused([
        [text, 64, true],
        [text, 64, false],
      ]);
    }
  });
});

describe("wholeNumberHint", () => {
  it("states the field's range in the words a refusal uses", () => {
    expect(wholeNumberHint(8, false)).toBe(
      "a whole number from 0 to 255, decimal or 0x hex",
    );
    expect(wholeNumberHint(64, true)).toBe(
      "a whole number from -9223372036854775808 to 9223372036854775807, decimal or 0x hex",
    );
    expect(wholeNumberHint(64, false)).toBe(
      "a whole number from 0 to 18446744073709551615, decimal or 0x hex",
    );
  });
});
