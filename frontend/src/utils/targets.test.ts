import { describe, it, expect } from "vitest";
import { targetsEqual, formatBytes, formatCount, type Target } from "./targets";

const t = (id: string, connected = false): Target => ({
  id,
  name: id,
  host: "10.0.0.1",
  port: 9000,
  connected,
});

describe("targetsEqual", () => {
  it("returns true for two empty arrays", () => {
    expect(targetsEqual([], [])).toBe(true);
  });

  it("returns true for two identical single-target arrays", () => {
    expect(targetsEqual([t("a")], [t("a")])).toBe(true);
  });

  it("returns false when lengths differ", () => {
    expect(targetsEqual([t("a")], [])).toBe(false);
    expect(targetsEqual([t("a")], [t("a"), t("b")])).toBe(false);
  });

  it("returns false when any field differs", () => {
    expect(targetsEqual([t("a")], [t("b")])).toBe(false);
    expect(targetsEqual([t("a", false)], [t("a", true)])).toBe(false);
    expect(
      targetsEqual(
        [{ ...t("a"), host: "1.1.1.1" }],
        [{ ...t("a"), host: "2.2.2.2" }],
      ),
    ).toBe(false);
    expect(
      targetsEqual([{ ...t("a"), port: 9000 }], [{ ...t("a"), port: 9001 }]),
    ).toBe(false);
  });

  it("returns true for two arrays of multiple identical targets", () => {
    const a = [t("a"), t("b"), t("c", true)];
    const b = [t("a"), t("b"), t("c", true)];
    expect(targetsEqual(a, b)).toBe(true);
  });

  it("does not consider order changes equal (order-sensitive by design)", () => {
    expect(targetsEqual([t("a"), t("b")], [t("b"), t("a")])).toBe(false);
  });
});

describe("formatBytes", () => {
  it("formats sub-KB as bytes", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1023)).toBe("1023 B");
  });
  it("formats KB range with one decimal", () => {
    expect(formatBytes(1024)).toBe("1.0 KB");
    expect(formatBytes(2048)).toBe("2.0 KB");
    expect(formatBytes(1536)).toBe("1.5 KB");
  });
  it("formats MB range with one decimal", () => {
    expect(formatBytes(1024 * 1024)).toBe("1.0 MB");
    expect(formatBytes(5 * 1024 * 1024)).toBe("5.0 MB");
  });
  it("formats GB range with two decimals", () => {
    expect(formatBytes(1024 * 1024 * 1024)).toBe("1.00 GB");
    expect(formatBytes(2.5 * 1024 * 1024 * 1024)).toBe("2.50 GB");
  });
});

describe("formatCount", () => {
  it("returns plain integer below 1000", () => {
    expect(formatCount(0)).toBe("0");
    expect(formatCount(999)).toBe("999");
  });
  it("formats thousands as Kx.x", () => {
    expect(formatCount(1000)).toBe("1.0K");
    expect(formatCount(12500)).toBe("12.5K");
  });
  it("formats millions as Mx.x", () => {
    expect(formatCount(1_000_000)).toBe("1.0M");
    expect(formatCount(2_500_000)).toBe("2.5M");
  });
});
