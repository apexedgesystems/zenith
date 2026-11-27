import { beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import DashboardPage from "./Dashboard";
import { json, renderWithQuery, stubFetch, target } from "../test/helpers";

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

/* A reachable component with a TELEMETRY struct gets a health card:
 * the page asks for it on the command route, which reads full_uid and
 * opcode as strings (hex, also when all digits). */
describe("Dashboard component health", () => {
  it("asks a component for its health with the uid and opcode in the command route's hex form", async () => {
    const server = stubFetch(({ method, path }) => {
      if (method === "POST") return json({ status: 0 });
      if (path === "/api/targets/target-1/structs") {
        return json({
          components: [
            {
              component: "Motor",
              structs: [
                {
                  name: "MotorTlm",
                  category: "TELEMETRY",
                  opcode: "0x0100",
                  size: 4,
                  fieldCount: 1,
                },
              ],
            },
          ],
        });
      }
      if (path === "/api/targets/target-1/structs/Motor") {
        return json({
          structs: {
            MotorTlm: {
              fields: [{ name: "rpm", type: "uint", offset: 0, size: 4 }],
            },
          },
        });
      }
      if (path === "/api/targets/target-1/registry") {
        return json({
          components: [
            { fullUid: "0x00D000", name: "Motor #0", reachable: true },
          ],
        });
      }
      return undefined;
    });
    renderWithQuery(
      <DashboardPage
        selectedTarget="target-1"
        targets={[target({ id: "target-1", connected: true })]}
      />,
    );
    const commands = () =>
      server.sent.filter(
        (r) => r.method === "POST" && r.path.endsWith("/command"),
      );
    await waitFor(() => expect(commands().length).toBeGreaterThan(0), {
      timeout: 3_000,
    });
    expect(JSON.parse(commands()[0].body ?? "")).toEqual({
      full_uid: "0x00D000",
      opcode: "0x0100",
    });
  });
});
