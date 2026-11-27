import { beforeEach, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import { DialogProvider } from "../components/dialogs";
import OperationsPage from "./Operations";
import { json, renderWithQuery, stubFetch, target } from "../test/helpers";

/* What the operations page says before it has a live target, for the
 * link state the target list reports. Every other read answers 404. */

let link = { connected: false, listening: false };

function renderPage() {
  renderWithQuery(
    <DialogProvider>
      <OperationsPage selectedTarget="target-1" />
    </DialogProvider>,
  );
}

beforeEach(() => {
  link = { connected: false, listening: false };
  stubFetch(({ path }) =>
    path === "/api/targets"
      ? json({ targets: [target({ id: "target-1", ...link })] })
      : undefined,
  );
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
