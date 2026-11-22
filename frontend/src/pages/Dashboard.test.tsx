import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import DashboardPage from "./Dashboard";

/* The dashboard's header and empty state for each link state. Every
 * data read answers 404, so the link state alone decides what shows. */

const target = (connected: boolean, listening: boolean) => ({
  id: "target-1",
  name: "Target",
  host: "127.0.0.1",
  port: 1234,
  connected,
  listening,
});

function renderDashboard(t: ReturnType<typeof target>) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={client}>
      <DashboardPage selectedTarget="target-1" targets={[t]} />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response("not found", { status: 404 })),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("Dashboard link state", () => {
  it("shows a waiting link as listening and does not ask to connect it", () => {
    renderDashboard(target(false, true));
    expect(screen.getByText("Listening for the target")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Disconnect" }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Connect" })).toBeNull();
    expect(
      screen.getByText("Health data appears once the target connects"),
    ).toBeInTheDocument();
    expect(screen.queryByText("Connect to view health data")).toBeNull();
  });

  it("asks to connect a disconnected target", () => {
    renderDashboard(target(false, false));
    expect(screen.getByText("Disconnected")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Connect" })).toBeInTheDocument();
    expect(screen.getByText("Connect to view health data")).toBeInTheDocument();
  });

  it("reads a heard target as connected while its socket stays bound", () => {
    renderDashboard(target(true, true));
    expect(screen.getByText("Connected")).toBeInTheDocument();
    expect(screen.queryByText("Listening for the target")).toBeNull();
    expect(
      screen.getByRole("button", { name: "Disconnect" }),
    ).toBeInTheDocument();
  });
});
