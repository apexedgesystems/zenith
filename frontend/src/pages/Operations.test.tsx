import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
 * target-1's link as `link` says, the registry lists `components`, the
 * command catalog is `catalog` (404 while null), every command succeeds,
 * and every other read answers 404. */

let link = { connected: false, listening: false };
let components: object[] = [];
let catalog: object | null = null;
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
  components = [motor];
  catalog = null;
  server = stubFetch(({ method, path }) => {
    if (path === "/api/targets") {
      return json({ targets: [target({ id: "target-1", ...link })] });
    }
    if (path === "/api/targets/target-1/registry") {
      return json({ components });
    }
    if (path === "/api/targets/target-1/commands" && catalog) {
      return json(catalog);
    }
    if (method === "POST" && path === "/api/targets/target-1/command") {
      return json({ status: 0, status_name: "SUCCESS" });
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

/* Opcodes no shipped catalog uses, so a send that matches them was
 * resolved from this catalog by name. */
const OPCODES: Record<string, string> = {
  CMD_PAUSE: "0x0A01",
  CMD_RESUME: "0x0A02",
  CMD_SLEEP: "0x0A06",
  CMD_WAKE: "0x0A07",
  CMD_LOCK_COMPONENT: "0x0A14",
  CMD_UNLOCK_COMPONENT: "0x0A15",
  SET_VERBOSITY: "0x0A21",
};

/** A catalog whose executive, "Sys" at 0x00AB00, carries `names` in that
 *  order, listed after a component that cannot pause. */
function catalogWith(names: string[]) {
  return {
    components: {
      Motor: {
        fullUid: "0x00D000",
        commands: [{ name: "SET_SPEED", opcode: "0x0100" }],
      },
      Sys: {
        fullUid: "0x00AB00",
        commands: names.map((name) => ({ name, opcode: OPCODES[name] })),
      },
    },
  };
}

/** The bodies posted to the command route, in the order sent. */
const commandsSent = () =>
  server.sent
    .filter(
      (r) => r.method === "POST" && r.path === "/api/targets/target-1/command",
    )
    .map((r) => JSON.parse(r.body ?? ""));

describe("Operations executive actions", () => {
  it("resolves each action by name from the target's catalog and sends it to the executive", async () => {
    link = { connected: true, listening: false };
    catalog = catalogWith([
      "SET_VERBOSITY",
      "CMD_WAKE",
      "CMD_PAUSE",
      "CMD_SLEEP",
      "CMD_RESUME",
    ]);
    renderPage();
    const user = userEvent.setup();
    await screen.findByText("Motor #0");
    for (const [action, next] of [
      ["Pause", "Resume"],
      ["Resume", "Pause"],
      ["Sleep", "Wake"],
      ["Wake", "Sleep"],
    ]) {
      await user.click(screen.getByRole("button", { name: action }));
      await screen.findByRole("button", { name: next });
    }
    await user.click(screen.getByRole("button", { name: "Apply" }));

    await waitFor(() => expect(commandsSent()).toHaveLength(5));
    const sys = (opcode: string, payload_hex = "") => ({
      full_uid: "0x00AB00",
      opcode,
      payload_hex,
    });
    expect(commandsSent()).toEqual([
      sys("0x0A01"),
      sys("0x0A02"),
      sys("0x0A06"),
      sys("0x0A07"),
      sys("0x0A21", "03"), // the default level, one byte
    ]);
  });

  it("refuses an action the target's catalog lacks, with a notice, and sends nothing for it", async () => {
    link = { connected: true, listening: false };
    catalog = catalogWith(["CMD_PAUSE", "CMD_RESUME", "CMD_WAKE"]);
    renderPage();
    const user = userEvent.setup();
    await screen.findByText("Motor #0");
    // The catalog has landed: an action it defines is sent.
    await user.click(screen.getByRole("button", { name: "Pause" }));
    await waitFor(() => expect(commandsSent()).toHaveLength(1));

    await user.click(screen.getByRole("button", { name: "Sleep" }));
    const notice = await screen.findByRole("dialog", { name: "Unavailable" });
    expect(notice).toHaveTextContent(
      "Sleep is not in this target's command set",
    );
    expect(commandsSent()).toHaveLength(1);
  });

  it("locks and unlocks a component with the little-endian u32 of its uid", async () => {
    link = { connected: true, listening: false };
    components = [
      {
        fullUid: "0x12D0A5",
        name: "Gimbal #0",
        type: "DRIVER",
        reachable: true,
      },
    ];
    catalog = catalogWith([
      "CMD_PAUSE",
      "CMD_LOCK_COMPONENT",
      "CMD_UNLOCK_COMPONENT",
    ]);
    renderPage();
    const user = userEvent.setup();
    const row = (await screen.findByText("Gimbal #0")).closest(
      "tr",
    ) as HTMLElement;
    await user.click(within(row).getByRole("button", { name: "Lock" }));
    await within(row).findByRole("button", { name: "LOCKED" });
    await user.click(within(row).getByRole("button", { name: "Unlock" }));

    await waitFor(() => expect(commandsSent()).toHaveLength(2));
    expect(commandsSent()).toEqual([
      { full_uid: "0x00AB00", opcode: "0x0A14", payload_hex: "a5d01200" },
      { full_uid: "0x00AB00", opcode: "0x0A15", payload_hex: "a5d01200" },
    ]);
  });
});
