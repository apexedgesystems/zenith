import { describe, expect, it } from "vitest";
import {
  fillPercent,
  fillRateLabel,
  formatDuration,
  timeToCapLabel,
} from "./storage";

describe("fillPercent", () => {
  it("scales to the cap and clamps at 100", () => {
    expect(fillPercent(512, 1024)).toBe(50);
    expect(fillPercent(2048, 1024)).toBe(100);
    expect(fillPercent(100, 0)).toBe(0);
  });
});

describe("fillRateLabel", () => {
  it("reads sub-KB rates as steady and signs real trends", () => {
    expect(fillRateLabel(0)).toBe("steady");
    expect(fillRateLabel(900)).toBe("steady");
    expect(fillRateLabel(-900)).toBe("steady");
    expect(fillRateLabel(300 * 1024)).toBe("+300 KB/min");
    expect(fillRateLabel(-2.5 * 1048576)).toBe("-2.5 MB/min");
  });
});

describe("formatDuration", () => {
  it("picks the unit by magnitude", () => {
    expect(formatDuration(42)).toBe("42s");
    expect(formatDuration(720)).toBe("12m");
    expect(formatDuration(3.5 * 3600)).toBe("3.5h");
    expect(formatDuration(2.1 * 86400)).toBe("2.1d");
  });
});

describe("timeToCapLabel", () => {
  it("distinguishes at-cap, projected, shrinking, and steady", () => {
    expect(timeToCapLabel(null, 0, 1024, 1024)).toBe("at cap (FIFO active)");
    expect(timeToCapLabel(7200, 5000, 100, 1024 * 1024)).toBe("~2.0h to cap");
    expect(timeToCapLabel(null, -50_000, 100, 1024 * 1024)).toBe("shrinking");
    expect(timeToCapLabel(null, 0, 100, 1024 * 1024)).toBe("holding steady");
  });
});
