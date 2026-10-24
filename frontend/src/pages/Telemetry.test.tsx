import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";

/* The telemetry page's live stream, driven through a fake WebSocket:
 * a close with the session-end code is not a dropped link and must not
 * start the reconnect loop; any other close is one, and must. The
 * status beside the controls reads the link and the stream together.
 * Each test loads a fresh module graph, so each gets its own session
 * owner. */

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
  /** The server sends one sample on the stream. */
  serverSend(sample: unknown) {
    this.onmessage?.({ data: JSON.stringify(sample) } as MessageEvent);
  }
}

const json = (v: unknown) => new Response(JSON.stringify(v), { status: 200 });
let calls: string[] = [];
let stop: (() => void) | null = null;
/** The link state the target list reports for target-0; null answers
 *  the target list read with 404, so the link reads as down. */
let linkConnected: boolean | null = null;

async function renderPage() {
  vi.resetModules();
  const { session } = await import("../api/session");
  const { DialogProvider } = await import("../components/dialogs");
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { default: TelemetryPage } = await import("./Telemetry");
  // The page reads the target list through a query hook; the fake
  // server answers it from `linkConnected`.
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  stop = session.start(new EventTarget());
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0); // the session answers: auth off
  });
  render(
    <QueryClientProvider client={queryClient}>
      <DialogProvider>
        <TelemetryPage selectedTarget="target-0" />
      </DialogProvider>
    </QueryClientProvider>,
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
  linkConnected = null;
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input);
      calls.push(`${init?.method ?? "GET"} ${path}`);
      if (path === "/api/auth/session") return json({ auth_enabled: false });
      if (path.endsWith("/telemetry/layouts")) return json({ layouts: [] });
      if (path === "/api/targets" && linkConnected !== null) {
        return json({
          targets: [
            {
              id: "target-0",
              name: "Target",
              host: "127.0.0.1",
              port: 9000,
              connected: linkConnected,
            },
          ],
        });
      }
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

describe("Telemetry page status", () => {
  const sample = {
    target_id: "target-0",
    channel: "A.x",
    timestamp_ms: 1_000,
    value: 1.5,
  };

  it("reads No data while the link is up and no sample has arrived", async () => {
    linkConnected = true;
    await renderPage();
    expect(screen.getByText("No data")).toBeInTheDocument();
  });

  it("reads Live from the first sample on the stream", async () => {
    linkConnected = true;
    await renderPage();
    expect(screen.getByText("No data")).toBeInTheDocument();
    await act(async () => {
      FakeSocket.opened[0].serverSend(sample);
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(screen.getByText("Live")).toBeInTheDocument();
    expect(screen.queryByText("No data")).not.toBeInTheDocument();
  });

  it("reads Paused while paused, and Live again on resume", async () => {
    linkConnected = true;
    await renderPage();
    await act(async () => {
      FakeSocket.opened[0].serverSend(sample);
      await vi.advanceTimersByTimeAsync(0);
    });
    fireEvent.click(screen.getByRole("button", { name: "Pause" }));
    expect(screen.getByText("Paused")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Resume" }));
    expect(screen.getByText("Live")).toBeInTheDocument();
  });

  it("reads Off while the link is down", async () => {
    linkConnected = false;
    await renderPage();
    expect(screen.getByText("Off")).toBeInTheDocument();
    expect(screen.queryByText("No data")).not.toBeInTheDocument();
  });
});
