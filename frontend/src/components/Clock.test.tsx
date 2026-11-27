import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen, act } from "@testing-library/react";
import Clock from "./Clock";

describe("Clock", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-04-09T12:34:56.000Z"));
  });

  it("shows the UTC time on mount and ticks once per second", () => {
    render(<Clock />);
    expect(screen.getByText("12:34:56 UTC")).toBeInTheDocument();

    act(() => {
      vi.advanceTimersByTime(1000);
    });
    expect(screen.getByText("12:34:57 UTC")).toBeInTheDocument();

    act(() => {
      vi.advanceTimersByTime(2000);
    });
    expect(screen.getByText("12:34:59 UTC")).toBeInTheDocument();
  });
});
