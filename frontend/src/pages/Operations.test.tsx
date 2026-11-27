import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { DialogProvider } from "../components/dialogs";
import OperationsPage from "./Operations";
import {
  json,
  renderWithQuery,
  stubFetch,
  target,
  type FetchStub,
} from "../test/helpers";

/* The operations page against a fake server. The target list reports
 * target-1's link as `link` says; the registry lists one reachable
 * component, and every other read answers 404. */

let link = { connected: false, listening: false };
let server: FetchStub;

const motor = {
  fullUid: "0x00D000",
  name: "Motor #0",
  type: "SW_MODEL",
  reachable: true,
};

function renderPage() {
  return renderWithQuery(
    <DialogProvider>
      <OperationsPage selectedTarget="target-1" />
    </DialogProvider>,
  );
}

beforeEach(() => {
  link = { connected: false, listening: false };
  server = stubFetch(({ path }) => {
    if (path === "/api/targets") {
      return json({ targets: [target({ id: "target-1", ...link })] });
    }
    if (path === "/api/targets/target-1/registry") {
      return json({ components: [motor] });
    }
    return undefined;
  });
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

  it("asks to connect once the target list reports the link down", async () => {
    vi.useFakeTimers();
    link = { connected: true, listening: false };
    renderPage();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(screen.getByText("System Controls")).toBeInTheDocument();
    link = { connected: false, listening: false };
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3_100); // past the next target list read
    });
    expect(
      screen.getByText(
        "Target not connected. Connect from the Dashboard or sidebar to issue operations.",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText("System Controls")).toBeNull();
  });
});

/** A FileReader whose every read fails, as when the picked file changed
 *  or went away on disk after it was chosen. */
class UnreadableFileReader {
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  result: string | null = null;
  error = new DOMException("The file could not be read", "NotReadableError");
  readAsDataURL() {
    setTimeout(() => this.onerror?.(), 0);
  }
}

describe("Operations library swap", () => {
  it("marks the swap failed in the log, and sends nothing, when its file cannot be read", async () => {
    vi.stubGlobal("FileReader", UnreadableFileReader);
    link = { connected: true, listening: false };
    const { container } = renderPage();
    await screen.findByRole("option", { name: "Motor #0 (0x00D000)" });
    fireEvent.change(screen.getByDisplayValue("-- select --"), {
      target: { value: "00D000" },
    });
    fireEvent.change(
      container.querySelector('input[type="file"]') as HTMLInputElement,
      { target: { files: [new File(["abc"], "motor.so")] } },
    );
    fireEvent.click(screen.getByRole("button", { name: "Hot-Swap Library" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm Swap" }));

    const row = (await screen.findByText("Swap Library")).closest(
      "tr",
    ) as HTMLElement;
    await waitFor(() =>
      expect(within(row).getByText("Failed to read file")).toBeInTheDocument(),
    );
    expect(server.sent.filter((r) => r.method === "POST")).toEqual([]);
  });
});
