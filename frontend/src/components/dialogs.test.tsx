import { describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { DialogProvider, useDialogs } from "./dialogs";

/* A confirm answers true only on OK, so the destructive action behind
 * it (remove a target, trim or delete stored telemetry) runs only then. */

function Asker({ onAnswer }: { onAnswer: (ok: boolean) => void }) {
  const { confirmDialog } = useDialogs();
  return (
    <button
      onClick={async () =>
        onAnswer(await confirmDialog("Remove this target?", "Remove target"))
      }
    >
      Ask
    </button>
  );
}

describe("confirmDialog", () => {
  it("resolves false on Cancel and true on OK, closing the dialog each time", async () => {
    const answers: boolean[] = [];
    render(
      <DialogProvider>
        <Asker onAnswer={(ok) => answers.push(ok)} />
      </DialogProvider>,
    );
    const user = userEvent.setup();

    await user.click(screen.getByRole("button", { name: "Ask" }));
    const dialog = screen.getByRole("dialog", { name: "Remove target" });
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(answers).toEqual([false]));
    expect(screen.queryByRole("dialog")).toBeNull();

    await user.click(screen.getByRole("button", { name: "Ask" }));
    await user.click(screen.getByRole("button", { name: "OK" }));
    await waitFor(() => expect(answers).toEqual([false, true]));
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});
