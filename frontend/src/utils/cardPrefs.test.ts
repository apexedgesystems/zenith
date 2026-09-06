import { describe, expect, it } from "vitest";
import { arrangeCards, moveTitle, toggleHidden } from "./cardPrefs";

const cards = ["A", "B", "C", "D"].map((t) => ({ title: t }));

describe("arrangeCards", () => {
  it("returns natural order without a preference", () => {
    expect(arrangeCards(cards, null, false).map((c) => c.title)).toEqual([
      "A",
      "B",
      "C",
      "D",
    ]);
  });

  it("orders ranked cards first and appends unknown new cards after", () => {
    const pref = { hidden: [], order: ["C", "A"] };
    expect(arrangeCards(cards, pref, false).map((c) => c.title)).toEqual([
      "C",
      "A",
      "B",
      "D",
    ]);
  });

  it("filters hidden cards unless customizing", () => {
    const pref = { hidden: ["B"], order: [] };
    expect(arrangeCards(cards, pref, false).map((c) => c.title)).toEqual([
      "A",
      "C",
      "D",
    ]);
    expect(arrangeCards(cards, pref, true)).toHaveLength(4);
  });
});

describe("moveTitle", () => {
  it("swaps within the materialized order and clamps at edges", () => {
    const all = ["A", "B", "C"];
    expect(moveTitle([], all, "B", -1)).toEqual(["B", "A", "C"]);
    expect(moveTitle(["C"], all, "C", 1)).toEqual(["A", "C", "B"]);
    expect(moveTitle([], all, "A", -1)).toEqual(["A", "B", "C"]);
  });
});

describe("toggleHidden", () => {
  it("adds and removes", () => {
    expect(toggleHidden([], "X")).toEqual(["X"]);
    expect(toggleHidden(["X"], "X")).toEqual([]);
  });
});
