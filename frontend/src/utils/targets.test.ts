import { describe, it, expect } from "vitest";
import { formatBytes, formatCount } from "./targets";

describe("formatBytes", () => {
  it("picks the unit by magnitude: bytes, KB and MB to one decimal, GB to two", () => {
    const sizes = [
      0,
      512,
      1023,
      1024,
      1536,
      2048,
      1024 ** 2,
      5 * 1024 ** 2,
      1024 ** 3,
      2.5 * 1024 ** 3,
    ];
    expect(sizes.map((n) => formatBytes(n))).toEqual([
      "0 B",
      "512 B",
      "1023 B",
      "1.0 KB",
      "1.5 KB",
      "2.0 KB",
      "1.0 MB",
      "5.0 MB",
      "1.00 GB",
      "2.50 GB",
    ]);
  });
});

describe("formatCount", () => {
  it("keeps counts below 1000 whole and shortens thousands and millions to one decimal", () => {
    const counts = [0, 999, 1000, 12500, 1_000_000, 2_500_000];
    expect(counts.map((n) => formatCount(n))).toEqual([
      "0",
      "999",
      "1.0K",
      "12.5K",
      "1.0M",
      "2.5M",
    ]);
  });
});
