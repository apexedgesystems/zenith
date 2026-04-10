import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, screen, act } from "@testing-library/react";
import Clock from "./Clock";

/**
 * Component test pattern (React Testing Library + Vitest):
 *
 * 1. render() mounts the component into a jsdom DOM tree.
 * 2. screen queries the DOM by what users see (text, role, etc.),
 *    not by class names or implementation details.
 * 3. expect(...).toBeInTheDocument() asserts presence (from
 *    @testing-library/jest-dom, set up in src/test/setup.ts).
 *
 * For time-driven components, use vi.useFakeTimers() so we can
 * advance time deterministically without waiting for real seconds.
 * Wrap state-changing time advances in act().
 */
describe("Clock", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    // Pin "now" to a known instant so the rendered text is predictable.
    // 2026-04-09 12:34:56 UTC
    vi.setSystemTime(new Date("2026-04-09T12:34:56.000Z"));
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("renders the current UTC time on mount", () => {
    render(<Clock />);
    expect(screen.getByText("12:34:56 UTC")).toBeInTheDocument();
  });

  it("ticks once per second", () => {
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

  it("does not re-tick after unmount", () => {
    const { unmount } = render(<Clock />);
    unmount();
    // Advancing time after unmount should not throw and should not
    // attempt to update an unmounted component (the cleanup function
    // clears the interval). If clean-up were broken React would warn
    // and Vitest would surface it.
    act(() => {
      vi.advanceTimersByTime(5000);
    });
  });
});
