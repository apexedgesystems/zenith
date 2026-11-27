import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import TunablesPage from "./Tunables";
import { bytesToHex } from "../api/decode";
import {
  json,
  renderWithQuery,
  stubFetch,
  type FetchStub,
} from "../test/helpers";

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
/** Whether updates are held, and the release of the one held. */
let holdPost = false;
let releasePost: (() => void) | null = null;

function serve() {
  holdA = false;
  releaseA = null;
  holdPost = false;
  releasePost = null;
  return stubFetch(({ method, path }) => {
    if (method === "POST") {
      const answer = () => json({ status: 0, uploaded_bytes: 8 });
      if (!holdPost) return answer();
      return new Promise<Response>((resolve) => {
        releasePost = () => resolve(answer());
      });
    }
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

/** The components whose values were read, in the order asked. */
const reads = (server: FetchStub) =>
  server.sent
    .filter((r) => r.method === "GET" && r.path.includes("/params/"))
    .map((r) => r.path.slice(r.path.lastIndexOf("/") + 1));

/** Past the reload the page starts 500 ms after an apply. */
const PAST_RELOAD = 1_000;

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

  /** Apply A with its answer held, select B, wait for B's form, then
   *  hand the clock to the test. */
  async function applyAThenSelectB(): Promise<FetchStub> {
    const server = serve();
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    await waitFor(() => expect(shown()).toEqual(["1.5", "100"]));
    holdPost = true;
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    await waitFor(() => expect(releasePost).not.toBeNull());
    fireEvent.change(screen.getByRole("combobox"), {
      target: { value: "00B000" },
    });
    await waitFor(() => expect(shown()).toEqual(["9.25", "7"]));
    vi.useFakeTimers();
    return server;
  }

  /** Let the held answer land, and `ms` pass after it. */
  async function answerAndWait(ms: number) {
    await act(async () => {
      releasePost!();
      await vi.advanceTimersByTimeAsync(ms);
    });
  }

  it("shows nothing of an apply answered after another component was chosen", async () => {
    await applyAThenSelectB();
    await answerAndWait(0);
    expect(screen.queryByText(/updated|applied/i)).toBeNull();
    expect(shown()).toEqual(["9.25", "7"]);
  });

  it("does not read back the component left when its apply is answered late", async () => {
    const server = await applyAThenSelectB();
    await answerAndWait(PAST_RELOAD);
    expect(reads(server)).toEqual(["00A000", "00B000"]);
    expect(shown()).toEqual(["9.25", "7"]);
  });

  it("reports an apply of the component shown, then reloads it", async () => {
    const server = serve();
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    await waitFor(() => expect(shown()).toEqual(["1.5", "100"]));
    vi.useFakeTimers();
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(
      screen.getByText("Parameters updated and reloaded (8 bytes uploaded)"),
    ).toBeInTheDocument();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(PAST_RELOAD);
    });
    expect(reads(server)).toEqual(["00A000", "00A000"]);
  });

  it("reports an apply answered after a reload of the same component", async () => {
    const server = serve();
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    await waitFor(() => expect(shown()).toEqual(["1.5", "100"]));
    vi.useFakeTimers();
    const apply = () =>
      fireEvent.click(screen.getByRole("button", { name: "Apply" }));

    // The first apply is answered at once and reloads 500 ms later; the
    // second is answered only after that reload.
    apply();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    holdPost = true;
    apply();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(PAST_RELOAD);
    });
    expect(reads(server)).toEqual(["00A000", "00A000"]);
    await answerAndWait(0);
    expect(
      screen.getByText("Parameters updated and reloaded (8 bytes uploaded)"),
    ).toBeInTheDocument();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(PAST_RELOAD);
    });
    expect(reads(server)).toEqual(["00A000", "00A000", "00A000"]);
  });

  it("asks for nothing more when an apply is answered after the page closed", async () => {
    const server = serve();
    const page = renderWithQuery(<TunablesPage selectedTarget="t1" />);
    await waitFor(() => expect(shown()).toEqual(["1.5", "100"]));
    holdPost = true;
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    await waitFor(() => expect(releasePost).not.toBeNull());

    // Another target or page replaces this one before the answer lands.
    page.unmount();
    const asked = server.calls();
    vi.useFakeTimers();
    await answerAndWait(PAST_RELOAD);
    expect(server.calls()).toEqual(asked);
  });
});

/* 8-byte integers cross the wire as decimal strings: the parameter read
 * gives an int64 or uint64 field as one, since a double (what a JSON
 * number becomes here) holds integers exactly only up to 2^53, and the
 * update takes one back. The answers below are JSON text, as the server
 * sends it, and the checks read the text the page posts. */

/** The bytes of a block holding an int64, a uint64 and an int32, as the
 *  read gives them: little-endian hex. */
function wideHex(iWide: string, uWide: string): string {
  const view = new DataView(new ArrayBuffer(20));
  view.setBigInt64(0, BigInt(iWide), true);
  view.setBigUint64(8, BigInt(uWide), true);
  view.setInt32(16, -70000, true);
  return bytesToHex(new Uint8Array(view.buffer));
}

/** Serve one component, uid 00C000: its block reads as the JSON text
 *  `block`, and the struct route answers `structs`, or never when it is
 *  null (the field types have not arrived). */
function serveBlock(
  component: string,
  block: string,
  structs: object | null,
): FetchStub {
  return stubFetch(({ method, path }) => {
    if (method === "POST") return json({ status: 0, uploaded_bytes: 20 });
    if (path === "/api/targets/t1/registry") {
      return json({
        components: [
          { fullUid: "0x00C000", name: `${component} #0`, reachable: true },
        ],
      });
    }
    if (path === "/api/targets/t1/params/00C000") return new Response(block);
    if (path === `/api/targets/t1/structs/${component}`) {
      return structs ? json(structs) : new Promise<Response>(() => {});
    }
    return undefined;
  });
}

const MATRIX_STRUCTS = {
  structs: {
    MatrixTprm: {
      category: "TUNABLE_PARAM",
      fields: [
        { name: "iWide", type: "int", offset: 0, size: 8 },
        { name: "uWide", type: "uint", offset: 8, size: 8 },
        { name: "iWord", type: "int", offset: 16, size: 4 },
      ],
    },
  },
};

/** Serve the block of an int64, a uint64 and an int32, with `iWide`
 *  and `uWide` as the strings the server gives 8-byte integers in. */
function serveWide(
  iWide: string,
  uWide: string,
  structs: object | null = MATRIX_STRUCTS,
): FetchStub {
  return serveBlock(
    "Matrix",
    `{"fullUid":"0x00C000","component":"Matrix","struct_name":"MatrixTprm",` +
      `"fields":{"iWide":"${iWide}","uWide":"${uWide}","iWord":-70000},` +
      `"raw_hex":"${wideHex(iWide, uWide)}","raw_length":20}`,
    structs,
  );
}

/** Open the page on that block and wait for its field types. */
async function openWide(iWide: string, uWide: string): Promise<FetchStub> {
  const server = serveWide(iWide, uWide);
  renderWithQuery(<TunablesPage selectedTarget="t1" />);
  await screen.findByText("uint64");
  return server;
}

/** The bodies of the updates posted, as sent. */
const posted = (server: FetchStub) =>
  server.sent.filter((r) => r.method === "POST").map((r) => r.body);

/** The update body for the block, as text, with these field values. */
const wideBody = (iWide: string, uWide: string, hex: string) =>
  `{"fields":{"iWide":"${iWide}","uWide":"${uWide}","iWord":-70000},` +
  `"raw_hex":"${hex}"}`;

/** The values the block is read with before an edit. */
const I0 = "-8000000000";
const U0 = "9000000000";

/** Type `text` into field `name` of the block read with I0 and U0. */
function typeInto(name: "iWide" | "uWide", text: string) {
  fireEvent.change(screen.getByDisplayValue(name === "iWide" ? I0 : U0), {
    target: { value: text },
  });
}

describe("Tunables 8-byte integers", () => {
  it.each([
    ["9007199254740993", "9007199254740993"],
    ["-9007199254740993", "9223372036854775807"],
    ["9223372036854775807", "18446744073709551615"],
    ["-9223372036854775808", "18446744073709551615"],
  ])(
    "shows int64 %s and uint64 %s as read and applies them unedited as the same strings",
    async (iWide, uWide) => {
      const server = await openWide(iWide, uWide);
      for (const v of [iWide, uWide]) {
        expect(screen.getAllByDisplayValue(v).length).toBeGreaterThan(0);
      }
      fireEvent.click(screen.getByRole("button", { name: "Apply" }));
      await waitFor(() => expect(posted(server)).toHaveLength(1));
      expect(posted(server)[0]).toBe(
        wideBody(iWide, uWide, wideHex(iWide, uWide)),
      );
    },
  );

  it.each([
    ["iWide", "9007199254740993", "9007199254740993"],
    ["iWide", "-9007199254740993", "-9007199254740993"],
    ["iWide", "0x20000000000001", "9007199254740993"],
    ["iWide", "9223372036854775807", "9223372036854775807"],
    ["iWide", "-9223372036854775808", "-9223372036854775808"],
    ["uWide", "18446744073709551615", "18446744073709551615"],
    ["uWide", "0xFFFFFFFFFFFFFFFF", "18446744073709551615"],
    ["uWide", "0", "0"],
  ] as const)(
    "applies %s typed as %s as the decimal string %s",
    async (name, typed, sent) => {
      const server = await openWide(I0, U0);
      typeInto(name, typed);
      fireEvent.click(screen.getByRole("button", { name: "Apply" }));
      await waitFor(() => expect(posted(server)).toHaveLength(1));
      const values = { iWide: I0, uWide: U0, [name]: sent };
      expect(posted(server)[0]).toBe(
        wideBody(values.iWide, values.uWide, wideHex(I0, U0)),
      );
    },
  );

  it.each([
    ["iWide", "9223372036854775808"],
    ["iWide", "-9223372036854775809"],
    ["uWide", "18446744073709551616"],
    ["uWide", "-1"],
  ] as const)(
    "refuses %s typed as %s, one past its range, naming the field, and sends nothing",
    async (name, typed) => {
      const server = await openWide(I0, U0);
      typeInto(name, typed);
      fireEvent.click(screen.getByRole("button", { name: "Apply" }));
      expect(
        await screen.findByText(new RegExp(`field '${name}'.*'${typed}'`)),
      ).toBeInTheDocument();
      expect(posted(server)).toHaveLength(0);
    },
  );

  it("takes field types from the struct the block was read with, of several", async () => {
    // A component may define more than one TUNABLE_PARAM struct; the
    // read names the one it decoded the block with, and the struct
    // route lists them by name, here with the other one first.
    const server = stubFetch(({ method, path }) => {
      if (method === "POST") return json({ status: 0, uploaded_bytes: 10 });
      if (path === "/api/targets/t1/registry") {
        return json({
          components: [
            { fullUid: "0x000000", name: "Executive", reachable: true },
          ],
        });
      }
      if (path === "/api/targets/t1/params/000000") {
        return new Response(
          `{"fullUid":"0x000000","component":"Executive",` +
            `"struct_name":"ExecutiveTunableParams",` +
            `"fields":{"clockFrequencyHz":100,"shutdownAtCycle":"0"},` +
            `"raw_hex":"64000000000000000000","raw_length":10}`,
        );
      }
      if (path === "/api/targets/t1/structs/Executive") {
        return json({
          structs: {
            ExecutiveThreadConfigTprm: {
              category: "TUNABLE_PARAM",
              fields: [
                { name: "startup", type: "struct", offset: 0, size: 11 },
              ],
            },
            ExecutiveTunableParams: {
              category: "TUNABLE_PARAM",
              fields: [
                { name: "clockFrequencyHz", type: "uint", offset: 0, size: 2 },
                { name: "shutdownAtCycle", type: "uint", offset: 2, size: 8 },
              ],
            },
          },
        });
      }
      return undefined;
    });
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    expect(await screen.findByText("uint64")).toBeInTheDocument();
    expect(screen.getByText("uint16")).toBeInTheDocument();

    fireEvent.change(screen.getByDisplayValue("0"), {
      target: { value: "18446744073709551615" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    await waitFor(() => expect(posted(server)).toHaveLength(1));
    expect(posted(server)[0]).toBe(
      `{"fields":{"clockFrequencyHz":100,` +
        `"shutdownAtCycle":"18446744073709551615"},` +
        `"raw_hex":"64000000000000000000"}`,
    );
  });
});

/* A field whose type the form does not know: any field before the
 * struct read answers, a nested leaf (the read expands a nested field
 * into its leaves, the struct gives the field), a variable-length
 * block's field. A typed integer past 2^53 goes as the text typed, as
 * a number it would arrive rounded; anything else as before. */
describe("Tunables fields of unknown type", () => {
  const apply = () =>
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));

  it.each([
    "9007199254740993",
    "-9007199254740993",
    "9007199254740993.0",
    "9.007199254740993e15",
  ])(
    "applies %s typed before the field types arrive as that text",
    async (typed) => {
      const server = serveWide(I0, U0, null);
      renderWithQuery(<TunablesPage selectedTarget="t1" />);
      await screen.findByDisplayValue(I0);
      expect(screen.queryByText("int64")).toBeNull();
      typeInto("iWide", typed);
      apply();
      await waitFor(() => expect(posted(server)).toHaveLength(1));
      expect(posted(server)[0]).toBe(wideBody(typed, U0, wideHex(I0, U0)));
    },
  );

  it("applies a typed 9007199254740993 to a nested leaf as that text", async () => {
    const server = serveBlock(
      "Rig",
      `{"fullUid":"0x00C000","component":"Rig","struct_name":"RigTprm",` +
        `"fields":{"rate":10,"limits.lo":"-8000000000"},` +
        `"raw_hex":"0a0000b02923feffffff","raw_length":10}`,
      {
        structs: {
          RigTprm: {
            category: "TUNABLE_PARAM",
            fields: [
              { name: "rate", type: "uint", offset: 0, size: 2 },
              {
                name: "limits",
                type: "nested",
                struct: "RigLimits",
                offset: 2,
                size: 8,
              },
            ],
          },
          RigLimits: {
            category: "STRUCT",
            fields: [{ name: "lo", type: "int", offset: 0, size: 8 }],
          },
        },
      },
    );
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    await screen.findByText("uint16");
    fireEvent.change(screen.getByDisplayValue("-8000000000"), {
      target: { value: "9007199254740993" },
    });
    apply();
    await waitFor(() => expect(posted(server)).toHaveLength(1));
    expect(posted(server)[0]).toBe(
      `{"fields":{"rate":10,"limits.lo":"9007199254740993"},` +
        `"raw_hex":"0a0000b02923feffffff"}`,
    );
  });

  /** Serve a variable-length block: a header count and one entry of a
   *  slot and an 8-byte integer. */
  const serveVarLen = () =>
    serveBlock(
      "Sched",
      `{"fullUid":"0x00C000","component":"Sched",` +
        `"struct_name":"SchedHeader + SchedEntry[]","variable_length":true,` +
        `"fields":{"header":{"count":1},` +
        `"entries":[{"slot":3,"wide":"-8000000000"}]},` +
        `"raw_hex":"010300b02923feffffff","raw_length":10}`,
      { structs: {} },
    );

  it("applies a typed 9007199254740993 to a variable-length block's entry as that text", async () => {
    const server = serveVarLen();
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    fireEvent.change(await screen.findByDisplayValue("-8000000000"), {
      target: { value: "9007199254740993" },
    });
    apply();
    await waitFor(() => expect(posted(server)).toHaveLength(1));
    expect(posted(server)[0]).toBe(
      `{"fields":{"count":1},` +
        `"entries":[{"slot":3,"wide":"9007199254740993"}],` +
        `"variable_length":true,"raw_hex":"010300b02923feffffff"}`,
    );
  });

  it("still applies a typed 1.5 or 42 as a number before the field types arrive", async () => {
    const server = serveWide(I0, U0, null);
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    await screen.findByDisplayValue(I0);
    fireEvent.change(screen.getByDisplayValue("-70000"), {
      target: { value: "1.5" },
    });
    typeInto("uWide", "42");
    apply();
    await waitFor(() => expect(posted(server)).toHaveLength(1));
    expect(posted(server)[0]).toBe(
      `{"fields":{"iWide":"${I0}","uWide":42,"iWord":1.5},` +
        `"raw_hex":"${wideHex(I0, U0)}"}`,
    );
  });

  it("still applies a typed 42 or 1.5 to a variable-length block as a number", async () => {
    const server = serveVarLen();
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    fireEvent.change(await screen.findByDisplayValue("1"), {
      target: { value: "42" },
    });
    fireEvent.change(screen.getByDisplayValue("3"), {
      target: { value: "1.5" },
    });
    apply();
    await waitFor(() => expect(posted(server)).toHaveLength(1));
    expect(posted(server)[0]).toBe(
      `{"fields":{"count":42},` +
        `"entries":[{"slot":1.5,"wide":"-8000000000"}],` +
        `"variable_length":true,"raw_hex":"010300b02923feffffff"}`,
    );
  });

  it("converts a value typed into a field known to be a float to a number, past 2^53 too", async () => {
    const server = serveBlock(
      "Gauge",
      `{"fullUid":"0x00C000","component":"Gauge","struct_name":"GaugeTprm",` +
        `"fields":{"ratio":0.5},"raw_hex":"0000003f","raw_length":4}`,
      {
        structs: {
          GaugeTprm: {
            category: "TUNABLE_PARAM",
            fields: [{ name: "ratio", type: "float", offset: 0, size: 4 }],
          },
        },
      },
    );
    renderWithQuery(<TunablesPage selectedTarget="t1" />);
    await screen.findByText("float32");
    fireEvent.change(screen.getByDisplayValue("0.5"), {
      target: { value: "9007199254740993" },
    });
    apply();
    await waitFor(() => expect(posted(server)).toHaveLength(1));
    expect(posted(server)[0]).toBe(
      `{"fields":{"ratio":9007199254740992},"raw_hex":"0000003f"}`,
    );
  });
});
