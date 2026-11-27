import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import MultiChart from "./MultiChart";

/* The hover readout, read off a recording 2D context: jsdom has no
 * canvas, so every drawing call is a no-op except fillText, which keeps
 * the labels drawn. */

let labels: string[] = [];

beforeEach(() => {
  labels = [];
  const ctx = new Proxy(
    {},
    {
      get: (_t, p) =>
        p === "fillText"
          ? (s: string) => labels.push(s)
          : p === "measureText"
            ? () => ({ width: 50 })
            : () => {},
      set: () => true,
    },
  );
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(
    ctx as unknown as CanvasRenderingContext2D,
  );
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({
    left: 0,
    top: 0,
    x: 0,
    y: 0,
    width: 600,
    height: 180,
    right: 600,
    bottom: 180,
    toJSON: () => ({}),
  } as DOMRect);
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
    cb(0);
    return 1;
  });
  vi.stubGlobal("cancelAnimationFrame", () => {});
});

describe("MultiChart hover readout", () => {
  it("shows no reading for a channel past its last sample", () => {
    // A.x stopped at 6 s; B.y runs to 10 s, the right edge of the window.
    const { container } = render(
      <MultiChart
        channels={["A.x", "B.y"]}
        data={{
          "A.x": [
            { t: 5000, v: 10 },
            { t: 6000, v: 20 },
          ],
          "B.y": [
            { t: 0, v: 1 },
            { t: 10000, v: 2 },
          ],
        }}
        title="p"
        timeWindowMs={10000}
        hiddenChannels={new Set()}
        onRemoveChannel={() => {}}
        onToggleChannel={() => {}}
      />,
    );
    const overlay = container.querySelectorAll("canvas")[1];
    labels = [];
    fireEvent.mouseMove(overlay, { clientX: 590 }); // the right edge: 10 s
    expect(labels.filter((s) => /^[xy]: /.test(s))).toEqual(["y: 2.0000"]);
  });
});
