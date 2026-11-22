import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DialogProvider } from "../components/dialogs";
import OperationsPage from "./Operations";

/* What the operations page says before it has a live target, for the
 * link state the target list reports. Every other read answers 404. */

let link = { connected: false, listening: false };

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={client}>
      <DialogProvider>
        <OperationsPage selectedTarget="target-1" />
      </DialogProvider>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  link = { connected: false, listening: false };
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      if (String(input) === "/api/targets") {
        const target = {
          id: "target-1",
          name: "Target",
          host: "127.0.0.1",
          port: 1234,
          ...link,
        };
        return new Response(JSON.stringify({ targets: [target] }), {
          status: 200,
        });
      }
      return new Response("not found", { status: 404 });
    }),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("Operations page link state", () => {
  it("tells a waiting link it is listening for the target, not to connect", async () => {
    link = { connected: false, listening: true };
    renderPage();
    expect(
      await screen.findByText(
        "Listening for the target. Operations appear once the target connects.",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Connect from the Dashboard/)).toBeNull();
  });

  it("asks to connect a disconnected target", async () => {
    renderPage();
    expect(
      await screen.findByText(
        "Target not connected. Connect from the Dashboard or sidebar to issue operations.",
      ),
    ).toBeInTheDocument();
  });
});
