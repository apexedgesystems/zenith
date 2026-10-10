import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";

/* The telemetry page's live stream, driven through a fake WebSocket:
 * a close with the session-end code is not a dropped link and must not
 * start the reconnect loop; any other close is one, and must. Each test
 * loads a fresh module graph, so each gets its own session owner. */

class FakeSocket {
  static opened: FakeSocket[] = [];
  url: string;
  onopen: (() => void) | null = null;
  onmessage: ((e: MessageEvent) => void) | null = null;
  onclose: ((e: CloseEvent) => void) | null = null;
  constructor(url: string) {
    this.url = url;
    FakeSocket.opened.push(this);
  }
  close() {}
  /** The server closes the stream with `code`. */
  serverClose(code: number) {
    this.onclose?.({ code } as CloseEvent);
  }
}

const json = (v: unknown) => new Response(JSON.stringify(v), { status: 200 });
let calls: string[] = [];
let stop: (() => void) | null = null;

async function renderPage() {
  vi.resetModules();
  const { session } = await import("../api/session");
  const { DialogProvider } = await import("../components/dialogs");
  const { default: TelemetryPage } = await import("./Telemetry");
  stop = session.start(new EventTarget());
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0); // the session answers: auth off
  });
  render(
    <DialogProvider>
      <TelemetryPage selectedTarget="target-0" />
    </DialogProvider>,
  );
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
}

const sessionReads = () =>
  calls.filter((c) => c === "GET /api/auth/session").length;

beforeEach(() => {
  vi.useFakeTimers();
  FakeSocket.opened = [];
  calls = [];
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input);
      calls.push(`${init?.method ?? "GET"} ${path}`);
      if (path === "/api/auth/session") return json({ auth_enabled: false });
      if (path.endsWith("/telemetry/layouts")) return json({ layouts: [] });
      return new Response("not found", { status: 404 });
    }),
  );
});

afterEach(() => {
  stop?.();
  stop = null;
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("Telemetry page live stream", () => {
  it("does not reopen a stream the server closed because its session ended", async () => {
    await renderPage();
    expect(FakeSocket.opened).toHaveLength(1);
    const readsBefore = sessionReads();
    await act(async () => {
      FakeSocket.opened[0].serverClose(1008);
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(FakeSocket.opened).toHaveLength(1);
    // The page asks the session owner to re-read the session instead.
    expect(sessionReads()).toBe(readsBefore + 1);
  });

  it("reconnects after any other close, as after a dropped link", async () => {
    await renderPage();
    const first = FakeSocket.opened[0];
    expect(first.url).toMatch(/\/api\/targets\/target-0\/telemetry\/live$/);
    for (const code of [1006, 1011]) {
      const before = FakeSocket.opened.length;
      await act(async () => {
        FakeSocket.opened[before - 1].serverClose(code);
        await vi.advanceTimersByTimeAsync(5_000);
      });
      expect(FakeSocket.opened).toHaveLength(before + 1);
      expect(FakeSocket.opened[before].url).toBe(first.url);
    }
  });
});
