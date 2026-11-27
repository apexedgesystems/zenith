import { beforeEach, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import DashboardPage from "./Dashboard";
import { renderWithQuery, stubFetch, target } from "../test/helpers";

/* The dashboard's header and empty state for each link state. Every
 * data read answers 404, so the link state alone decides what shows. */

function renderDashboard(connected: boolean, listening: boolean) {
  renderWithQuery(
    <DashboardPage
      selectedTarget="target-1"
      targets={[target({ id: "target-1", connected, listening })]}
    />,
  );
}

beforeEach(() => {
  stubFetch();
});

describe("Dashboard link state", () => {
  it("shows a waiting link as listening and does not ask to connect it", () => {
    renderDashboard(false, true);
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
    renderDashboard(false, false);
    expect(screen.getByText("Disconnected")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Connect" })).toBeInTheDocument();
    expect(screen.getByText("Connect to view health data")).toBeInTheDocument();
  });

  it("reads a heard target as connected while its socket stays bound", () => {
    renderDashboard(true, true);
    expect(screen.getByText("Connected")).toBeInTheDocument();
    expect(screen.queryByText("Listening for the target")).toBeNull();
    expect(
      screen.getByRole("button", { name: "Disconnect" }),
    ).toBeInTheDocument();
  });
});
