import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import TunablesPage from "./Tunables";
import { json, renderWithQuery, stubFetch } from "../test/helpers";

/* Two instances of one component type, answered as the backend answers
 * GET /params/{uid}. The form shows, and Apply sends, only the values
 * read from the selected component, whatever order the reads land in. */

const motor = (uid: string, gain: number, limit: number, raw_hex: string) => ({
  fullUid: `0x${uid}`,
  component: "Motor",
  struct_name: "MotorTprm",
  fields: { gain, limit },
  raw_hex,
  raw_length: 8,
});
const A = motor("00A000", 1.5, 100, "0000c03f64000000");
const B = motor("00B000", 9.25, 7, "0000144107000000");

/** Whether A's reads are held, and the release of the one held. */
let holdA = false;
let releaseA: (() => void) | null = null;

function serve() {
  holdA = false;
  releaseA = null;
  return stubFetch(({ method, path }) => {
    if (method === "POST") return json({ status: 0, uploaded_bytes: 8 });
    if (path === "/api/targets/t1/registry") {
      return json({
        components: [
          { fullUid: "0x00A000", name: "Motor #0", reachable: true },
          { fullUid: "0x00B000", name: "Motor #1", reachable: true },
        ],
      });
    }
    if (path === "/api/targets/t1/params/00A000") {
      if (!holdA) return json(A);
      return new Promise<Response>((resolve) => {
        releaseA = () => resolve(json(A));
      });
    }
    if (path === "/api/targets/t1/params/00B000") return json(B);
    if (path === "/api/targets/t1/structs/Motor") {
      return json({
        structs: {
          MotorTprm: {
            category: "TUNABLE_PARAM",
            fields: [
              { name: "gain", type: "float", offset: 0, size: 4 },
              { name: "limit", type: "uint", offset: 4, size: 4 },
            ],
          },
        },
      });
    }
    return undefined;
  });
}

/** The values the form shows, in field order. */
const shown = () =>
  (screen.queryAllByRole("spinbutton") as HTMLInputElement[]).map(
    (i) => i.value,
  );

describe("Tunables", () => {
  it("shows and applies only the selected component's values when an earlier read lands late", async () => {
    const server = serve();
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    await waitFor(() => expect(shown()).toEqual(["1.5", "100"]));

    // A is read again, slowly; meanwhile the operator selects B.
    holdA = true;
    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(releaseA).not.toBeNull());
    fireEvent.change(screen.getByRole("combobox"), {
      target: { value: "00B000" },
    });
    expect(shown()).toEqual([]); // nothing of A under B while B loads
    await waitFor(() => expect(shown()).toEqual(["9.25", "7"]));

    // A's answer lands after B's.
    await act(async () => {
      releaseA!();
      await new Promise((r) => setTimeout(r, 50));
    });
    expect(shown()).toEqual(["9.25", "7"]);
    expect(screen.getByText(B.raw_hex)).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    const posts = () => server.sent.filter((r) => r.method === "POST");
    await waitFor(() => expect(posts()).toHaveLength(1));
    expect(posts()[0].path).toBe("/api/targets/t1/params/00B000/update");
    expect(JSON.parse(posts()[0].body ?? "")).toEqual({
      fields: B.fields,
      raw_hex: B.raw_hex,
    });
  });
});
