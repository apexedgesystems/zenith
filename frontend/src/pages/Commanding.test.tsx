import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import CommandingPage, {
  buildPayload,
  encodeField,
  type CommandFieldDef,
} from "./Commanding";
import {
  json,
  renderWithQuery,
  stubFetch,
  type FetchStub,
} from "../test/helpers";

/** A shipped target's generated command catalog, as the backend serves
 *  it. Read at runtime (the base is a variable so the bundler does not
 *  turn the URL into an asset import). */
const testFile = import.meta.url;
const catalogText = (target: string): string =>
  readFileSync(
    new URL(`../../../targets/${target}/commands.json`, testFile),
    "utf8",
  );
const roverCatalogText = catalogText("rover-mcu");
const specCatalogText = catalogText("sil-spec-demo");

const f = (type: string): CommandFieldDef => ({ name: "x", type });

/** What a field of `type` encodes `value` to: its hex, or the refusal. */
function encoded(type: string, value: string): string {
  const e = encodeField(f(type), value);
  return e.ok ? e.hex : `refused: ${e.error}`;
}

/** Each row's input encodes to the row's hex. */
function expectRows(rows: [type: string, value: string, hex: string][]) {
  for (const [type, value, hex] of rows) {
    expect(encoded(type, value), `${type} '${value}'`).toBe(hex);
  }
}

/** Each row's input is refused, naming the field. */
function expectRefused(rows: [type: string, value: string][]) {
  for (const [type, value] of rows) {
    expect(encoded(type, value), `${type} '${value}'`).toMatch(
      /^refused: field 'x'/,
    );
  }
}

interface Catalog {
  components: Record<
    string,
    {
      fullUid: string;
      commands: { name: string; opcode: string; fields: CommandFieldDef[] }[];
    }
  >;
}

/** One command from a catalog, with its component's uid. */
function command(text: string, component: string, name: string) {
  const comp = (JSON.parse(text) as Catalog).components[component];
  const found = comp?.commands.find((c) => c.name === name);
  if (!found) throw new Error(`${component} ${name} missing from catalog`);
  return { ...found, fullUid: comp.fullUid };
}

describe("encodeField", () => {
  it("encodes unsigned integers little-endian at the field's width and refuses what does not fit", () => {
    expectRows([
      ["uint8", "42", "2a"],
      ["uint8", "255", "ff"],
      ["uint16", "0x1234", "3412"],
      ["uint16", "65535", "ffff"],
      ["uint32", "0xDEADBEEF", "efbeadde"],
      ["uint32", "4294967295", "ffffffff"],
      ["uint32", "0", "00000000"],
    ]);
    expectRefused([
      ["uint8", "256"],
      ["uint8", "257"],
      ["uint16", "65536"],
      ["uint32", "4294967296"],
      ["uint32", "0x100000000"],
    ]);
  });

  it("reads unsigned input as a whole decimal or 0x hex number and refuses anything else", () => {
    expectRows([
      ["uint8", "0xFF", "ff"],
      ["uint8", "0x0a", "0a"],
      ["uint8", " 7 ", "07"],
      ["uint16", "007", "0700"],
    ]);
    expectRefused([
      ["uint8", "-5"],
      ["uint16", "-1"],
      ["uint32", "-1"],
      ["uint8", "abc"],
      ["uint16", "abc"],
      ["uint8", "12abc"],
      ["uint8", "1.9"],
      ["uint8", "1e3"],
      ["uint8", "0x1G"],
      ["uint8", "0x"],
    ]);
  });

  it("encodes float and double as little-endian binary32 and binary64", () => {
    expectRows([
      ["float", "10.5", "00002841"],
      ["float", "-20.25", "0000a2c1"],
      ["float", "-0.375", "0000c0be"],
      ["float", "1999.75", "00f8f944"],
      ["float", "0", "00000000"],
      ["double", "10.5", "0000000000002540"],
      ["double", "-20.25", "00000000004034c0"],
      ["double", "0", "0000000000000000"],
    ]);
  });

  it("reads float and double input in decimal notation only", () => {
    expectRows([
      ["float", "+1.5", "0000c03f"],
      ["float", ".5", "0000003f"],
      ["float", "2.", "00000040"],
      ["float", "2.5e3", "00401c45"],
      ["double", "-1E-1", "9a9999999999b9bf"],
    ]);
    expectRefused([
      ["float", "0x10"],
      ["float", "0b101"],
      ["float", "0o17"],
      ["double", "0x10"],
      ["double", "0b101"],
      ["double", "0o17"],
      ["float", "1.2.3"],
      ["float", "1e"],
    ]);
  });

  it("refuses input that is not a finite binary32, naming the field and the value", () => {
    for (const value of ["ten", "Infinity", "NaN", "1e39"]) {
      const e = encodeField({ name: "a_m", type: "float" }, value);
      expect(e.ok, value).toBe(false);
      if (!e.ok) {
        expect(e.error).toContain("'a_m'");
        expect(e.error).toContain(`'${value}'`);
      }
    }
  });

  it("refuses a type it cannot encode, naming the field and type and pointing to Raw Mode", () => {
    for (const type of ["raw[16]", "int16", "uint64", "mystery"]) {
      const e = encodeField({ name: "data", type }, "42");
      expect(e.ok, type).toBe(false);
      if (!e.ok) {
        expect(e.error).toContain("'data'");
        expect(e.error).toContain(`'${type}'`);
        expect(e.error).toContain("Raw Mode");
      }
    }
  });
});

describe("buildPayload", () => {
  it("refuses a blank field rather than sending zero", () => {
    for (const type of ["uint8", "uint16", "uint32", "float", "double"]) {
      for (const value of ["", "  "]) {
        const e = encodeField({ name: "a_m", type }, value);
        expect(e.ok, `${type} '${value}'`).toBe(false);
        if (!e.ok) expect(e.error).toContain("'a_m'");
      }
    }
    const fields = [
      { name: "a_m", type: "float" },
      { name: "b_m", type: "float" },
    ];
    expect(buildPayload(fields, { a_m: "1.5" })).toEqual({
      ok: false,
      error: "field 'b_m' is blank",
    });
  });
});

/* The command form end to end: the page loads a shipped catalog, the
 * operator picks a command and fills its fields, and Send either posts
 * the encoded payload or shows the refusal and posts nothing. */
describe("command form", () => {
  function renderPage(catalog: string): FetchStub {
    const server = stubFetch(({ method, path }) => {
      if (path.endsWith("/commands")) return new Response(catalog);
      if (path.endsWith("/structs")) return json({ components: [] });
      if (method === "POST" && path.endsWith("/command")) {
        return json({ status: 0, status_name: "SUCCESS" });
      }
      return undefined;
    });
    renderWithQuery(
      <CommandingPage
        selectedTarget="target-7"
        targets={[{ id: "target-7", name: "Target" }]}
      />,
    );
    return server;
  }

  /** The bodies the page posted to the command route. */
  const commandsSent = (server: FetchStub) =>
    server.sent
      .filter((r) => r.method === "POST" && r.path.endsWith("/command"))
      .map((r) => JSON.parse(r.body ?? ""));

  async function choose(component: string, commandName: string) {
    const user = userEvent.setup();
    await screen.findByRole("option", { name: new RegExp(`^${component} `) });
    const [componentSelect, commandSelect] = screen.getAllByRole("combobox");
    await user.selectOptions(componentSelect, component);
    await screen.findByRole("option", {
      name: new RegExp(`^${commandName} `),
    });
    await user.selectOptions(commandSelect, commandName);
    return user;
  }

  it("posts SET_TARGET_REL's coordinates as two float32s", async () => {
    const server = renderPage(roverCatalogText);
    const cmd = command(roverCatalogText, "GroundVehicle", "SET_TARGET_REL");
    const user = await choose("GroundVehicle", "SET_TARGET_REL");
    await user.type(screen.getByPlaceholderText("a_m"), "10.5");
    await user.type(screen.getByPlaceholderText("b_m"), "-20.25");
    await user.click(screen.getByRole("button", { name: "Send" }));

    await waitFor(() => expect(commandsSent(server)).toHaveLength(1));
    expect(commandsSent(server)[0]).toEqual({
      full_uid: cmd.fullUid,
      opcode: cmd.opcode,
      payload_hex: "000028410000a2c1",
    });
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("shows the refusal and posts nothing for a field it cannot encode", async () => {
    const server = renderPage(specCatalogText);
    const user = await choose("SpecBusDriver #0", "SendFrame");
    await user.type(screen.getByPlaceholderText(/^Frame length/), "4");
    await user.click(screen.getByRole("button", { name: "Send" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(
      "Not sent: field 'reserved' has type 'raw[3]'",
    );
    expect(commandsSent(server)).toHaveLength(0);
  });
});
