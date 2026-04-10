import { describe, it, expect } from "vitest";
import { fieldName, groupChannels } from "./telemetry";

describe("fieldName", () => {
  it("returns the part after the first dot", () => {
    expect(fieldName("WaveGen#0.output")).toBe("output");
    expect(fieldName("Executive.cycleCount")).toBe("cycleCount");
  });

  it("returns the original string when no dot is present", () => {
    expect(fieldName("noDot")).toBe("noDot");
  });

  it("returns only what follows the first dot when multiple dots exist", () => {
    // Component names should never have dots, but if they do we keep
    // everything after the first.
    expect(fieldName("a.b.c")).toBe("b.c");
  });

  it("returns the original string when starting with a dot", () => {
    // Edge case: leading dot. indexOf(".") = 0, so dot > 0 is false.
    expect(fieldName(".leading")).toBe(".leading");
  });
});

describe("groupChannels", () => {
  it("groups channels by component prefix", () => {
    const grouped = groupChannels([
      "WaveGen#0.output",
      "WaveGen#0.phase",
      "WaveGen#1.output",
      "Executive.cycleCount",
    ]);
    expect(Object.keys(grouped).sort()).toEqual([
      "Executive",
      "WaveGen#0",
      "WaveGen#1",
    ]);
    expect(grouped["WaveGen#0"]).toEqual([
      "WaveGen#0.output",
      "WaveGen#0.phase",
    ]);
    expect(grouped["Executive"]).toEqual(["Executive.cycleCount"]);
  });

  it("places channels without a dot in 'Other'", () => {
    const grouped = groupChannels(["bare", "WaveGen#0.output"]);
    expect(grouped["Other"]).toEqual(["bare"]);
    expect(grouped["WaveGen#0"]).toEqual(["WaveGen#0.output"]);
  });

  it("returns an empty object for an empty input", () => {
    expect(groupChannels([])).toEqual({});
  });
});
