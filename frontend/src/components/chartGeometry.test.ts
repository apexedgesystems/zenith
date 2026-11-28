import { describe, expect, it } from "vitest";
import { computePadL, pixelToTime, PAD_R, readoutAt } from "./chartGeometry";

describe("computePadL", () => {
  it("widens with label magnitude", () => {
    expect(computePadL(0, 5)).toBe(52); // 6 chars
    expect(computePadL(0, 50000)).toBe(66); // 8 chars
    expect(computePadL(-2000000, 0)).toBe(80); // 10 chars
    expect(computePadL(0, 0.5)).toBe(59); // sub-unit: 7 chars
  });
});

describe("pixelToTime", () => {
  it("maps plot-region pixels linearly and rejects the gutters", () => {
    const padL = computePadL(0, 50000); // 66: an 8-character label
    const width = 566; // plot region exactly 490px
    expect(pixelToTime(padL, width, padL, 1000, 2000)).toBe(1000);
    expect(pixelToTime(width - PAD_R, width, padL, 1000, 2000)).toBe(2000);
    expect(pixelToTime(padL + 245, width, padL, 1000, 2000)).toBe(1500);
    expect(pixelToTime(10, width, padL, 1000, 2000)).toBeNull();
    expect(pixelToTime(width - 2, width, padL, 1000, 2000)).toBeNull();
  });
});

describe("readoutAt", () => {
  const pts = [
    { t: 5000, v: 10 },
    { t: 6000, v: 20 },
    { t: 7000, v: 15 },
  ];

  it("reads linearly between samples and nothing before the first or after the last", () => {
    const at = (t: number) => readoutAt(pts, t)?.v ?? null;
    expect([5000, 5500, 6500, 7000].map(at)).toEqual([10, 15, 17.5, 15]);
    expect([0, 4999, 7001, 10000].map(at)).toEqual([null, null, null, null]);
    expect(readoutAt([], 5000)).toBeNull();
    // One sample reads only at its own instant.
    expect(readoutAt([{ t: 5000, v: 3 }], 5000)?.v).toBe(3);
    expect(readoutAt([{ t: 5000, v: 3 }], 5001)).toBeNull();
  });

  it("names the nearer sample, whose envelope a tiered bucket shows", () => {
    const tiered = [
      { t: 0, v: 1, lo: 0, hi: 2 },
      { t: 1000, v: 5, lo: 4, hi: 6 },
    ];
    expect(readoutAt(tiered, 400)?.near).toBe(tiered[0]);
    expect(readoutAt(tiered, 600)?.near).toBe(tiered[1]);
  });
});
