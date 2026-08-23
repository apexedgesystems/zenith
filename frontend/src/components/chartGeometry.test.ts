import { describe, expect, it } from "vitest";
import { computePadL, pixelToTime, PAD_R } from "./chartGeometry";

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
    const padL = computePadL(0, 50000); // 66 -- the case the old
    // hardcoded 60 got wrong: every readout shifted by 6px of time.
    const width = 566; // plot region exactly 490px
    expect(pixelToTime(padL, width, padL, 1000, 2000)).toBe(1000);
    expect(pixelToTime(width - PAD_R, width, padL, 1000, 2000)).toBe(2000);
    expect(pixelToTime(padL + 245, width, padL, 1000, 2000)).toBe(1500);
    expect(pixelToTime(10, width, padL, 1000, 2000)).toBeNull();
    expect(pixelToTime(width - 2, width, padL, 1000, 2000)).toBeNull();
  });

  it("regresses the hardcoded-60 defect: wrong pad shifts the timestamp", () => {
    const truePad = computePadL(0, 50000); // 66
    const width = 566;
    const atTruePad = pixelToTime(truePad + 100, width, truePad, 0, 1000);
    const atWrongPad = pixelToTime(truePad + 100, width, 60, 0, 1000);
    expect(atTruePad).not.toBeNull();
    expect(atWrongPad).not.toBeNull();
    expect(
      Math.abs((atTruePad as number) - (atWrongPad as number)),
    ).toBeGreaterThan(5);
  });
});
