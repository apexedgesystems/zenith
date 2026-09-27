import { describe, it, expect } from "vitest";
import { healthKey, isBadValue, type HealthRule } from "./health";

describe("healthKey", () => {
  it("lowercases and strips underscores", () => {
    expect(healthKey("Last_Cmd_Result")).toBe("lastcmdresult");
  });
});

describe("isBadValue", () => {
  const rules: HealthRule[] = [
    { field: "isslipping", op: "ne", value: 0 },
    { field: "lastcmdresult", op: "ge", value: 2 },
    { field: "boardlink", op: "eq", value: 2 },
    { field: "margin", op: "lt", value: 10 },
    { field: "temp", op: "gt", value: 85 },
    { field: "fuel", op: "le", value: 5 },
  ];
  it("treats a bare-name rule as nonzero-bad", () => {
    expect(isBadValue("is_slipping", 0, rules)).toBe(false);
    expect(isBadValue("is_slipping", 1, rules)).toBe(true);
  });
  it("evaluates each comparison at its boundary", () => {
    expect(isBadValue("last_cmd_result", 1, rules)).toBe(false);
    expect(isBadValue("last_cmd_result", 2, rules)).toBe(true);
    expect(isBadValue("board_link", 1, rules)).toBe(false);
    expect(isBadValue("board_link", 2, rules)).toBe(true);
    expect(isBadValue("margin", 10, rules)).toBe(false);
    expect(isBadValue("margin", 9, rules)).toBe(true);
    expect(isBadValue("temp", 85, rules)).toBe(false);
    expect(isBadValue("temp", 86, rules)).toBe(true);
    expect(isBadValue("fuel", 5, rules)).toBe(true);
    expect(isBadValue("fuel", 6, rules)).toBe(false);
  });
  it("ignores fields without a rule and unknown operators", () => {
    expect(isBadValue("other", 99, rules)).toBe(false);
    expect(isBadValue("x", 1, [{ field: "x", op: "between", value: 1 }])).toBe(
      false,
    );
  });
});
