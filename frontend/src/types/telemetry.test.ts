import { describe, it, expect } from "vitest";
import { fieldName, groupChannels } from "./telemetry";

describe("fieldName", () => {
  it("is what follows the first dot, or the whole name without a dot past its first character", () => {
    const names = [
      "WaveGen#0.output",
      "Executive.cycleCount",
      "noDot",
      "a.b.c",
      ".leading",
    ];
    expect(names.map((n) => fieldName(n))).toEqual([
      "output",
      "cycleCount",
      "noDot",
      "b.c",
      ".leading",
    ]);
  });
});

describe("groupChannels", () => {
  it("groups channels by component prefix in input order, bare names under Other", () => {
    expect(
      groupChannels([
        "WaveGen#0.output",
        "WaveGen#0.phase",
        "WaveGen#1.output",
        "Executive.cycleCount",
      ]),
    ).toEqual({
      "WaveGen#0": ["WaveGen#0.output", "WaveGen#0.phase"],
      "WaveGen#1": ["WaveGen#1.output"],
      Executive: ["Executive.cycleCount"],
    });
    expect(groupChannels(["bare", "WaveGen#0.output"])).toEqual({
      Other: ["bare"],
      "WaveGen#0": ["WaveGen#0.output"],
    });
    expect(groupChannels([])).toEqual({});
  });
});
