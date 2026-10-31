import { afterEach, describe, it, expect, vi } from "vitest";
import { readFileSync } from "node:fs";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import CommandingPage, {
  buildPayload,
  encodeField,
  type CommandFieldDef,
} from "./Commanding";

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

/** The hex an accepted field encodes to; a refusal fails the test. */
function hex(field: CommandFieldDef, value: string): string {
  const encoded = encodeField(field, value);
  if (!encoded.ok) throw new Error(`refused: ${encoded.error}`);
  return encoded.hex;
}

interface Catalog {
  components: Record<
    string,
    { commands: { name: string; fields: CommandFieldDef[] }[] }
  >;
}

function command(text: string, component: string, name: string) {
  const catalog = JSON.parse(text) as Catalog;
  const found = catalog.components[component]?.commands.find(
    (c) => c.name === name,
  );
  if (!found) throw new Error(`${component} ${name} missing from catalog`);
  return found;
}

describe("encodeField", () => {
  describe("uint8", () => {
    it("encodes decimal value as 2-char hex", () => {
      expect(hex(f("uint8"), "42")).toBe("2a");
    });

    it("encodes hex input (0x prefix)", () => {
      expect(hex(f("uint8"), "0xFF")).toBe("ff");
      expect(hex(f("uint8"), "0x0a")).toBe("0a");
    });

    it("clamps negative values to 0", () => {
      expect(hex(f("uint8"), "-5")).toBe("00");
    });

    it("masks values larger than 0xFF", () => {
      expect(hex(f("uint8"), "256")).toBe("00"); // 0x100 & 0xFF = 0
      expect(hex(f("uint8"), "257")).toBe("01"); // 0x101 & 0xFF = 1
    });

    it("returns safe zero for non-numeric input", () => {
      expect(hex(f("uint8"), "abc")).toBe("00");
      expect(hex(f("uint8"), "")).toBe("00");
    });
  });

  describe("uint16", () => {
    it("encodes as 4-char little-endian hex", () => {
      // 0x1234 LE = 34 12
      expect(hex(f("uint16"), "0x1234")).toBe("3412");
    });

    it("masks values larger than 0xFFFF", () => {
      // 0x10000 & 0xFFFF = 0
      expect(hex(f("uint16"), "65536")).toBe("0000");
    });

    it("clamps negative to 0", () => {
      expect(hex(f("uint16"), "-1")).toBe("0000");
    });

    it("encodes non-numeric input as zero at the field's width", () => {
      expect(hex(f("uint16"), "abc")).toBe("0000");
    });
  });

  describe("uint32", () => {
    it("encodes as 8-char little-endian hex", () => {
      // 0xDEADBEEF LE = ef be ad de
      expect(hex(f("uint32"), "0xDEADBEEF")).toBe("efbeadde");
    });

    it("encodes 0 cleanly", () => {
      expect(hex(f("uint32"), "0")).toBe("00000000");
    });

    it("clamps negative to 0", () => {
      expect(hex(f("uint32"), "-1")).toBe("00000000");
    });
  });

  describe("float", () => {
    it("encodes a fraction as little-endian binary32", () => {
      expect(hex(f("float"), "10.5")).toBe("00002841");
    });

    it("keeps the sign of a negative fraction", () => {
      expect(hex(f("float"), "-20.25")).toBe("0000a2c1");
      expect(hex(f("float"), "-0.375")).toBe("0000c0be");
    });

    it("encodes zero as four zero bytes", () => {
      expect(hex(f("float"), "0")).toBe("00000000");
    });

    it("refuses input that is not a finite binary32", () => {
      for (const value of ["ten", "Infinity", "NaN", "1e39"]) {
        const encoded = encodeField(f("float"), value);
        expect(encoded.ok).toBe(false);
        if (!encoded.ok) expect(encoded.error).toContain(`'${value}'`);
      }
    });
  });

  describe("double", () => {
    it("encodes as little-endian binary64", () => {
      expect(hex(f("double"), "10.5")).toBe("0000000000002540");
      expect(hex(f("double"), "-20.25")).toBe("00000000004034c0");
      expect(hex(f("double"), "0")).toBe("0000000000000000");
    });
  });

  describe("unsupported types", () => {
    it("refuses a type it cannot encode, naming the field and type", () => {
      for (const type of ["raw[16]", "int16", "uint64", "mystery"]) {
        const encoded = encodeField({ name: "data", type }, "42");
        expect(encoded.ok).toBe(false);
        if (!encoded.ok) {
          expect(encoded.error).toContain("'data'");
          expect(encoded.error).toContain(`'${type}'`);
        }
      }
    });
  });
});

describe("buildPayload with the shipped catalogs", () => {
  for (const name of ["SET_TARGET_REL", "SET_TARGET_ABS"]) {
    describe(`rover GroundVehicle ${name}`, () => {
      const cmd = command(roverCatalogText, "GroundVehicle", name);

      it("declares two float coordinates", () => {
        expect(cmd.fields.map((x) => [x.name, x.type])).toEqual([
          ["a_m", "float"],
          ["b_m", "float"],
        ]);
      });

      it("encodes 10.5 and -20.25 as two little-endian float32s", () => {
        expect(
          buildPayload(cmd.fields, { a_m: "10.5", b_m: "-20.25" }),
        ).toEqual({ ok: true, hex: "000028410000a2c1" });
      });

      it("encodes negative, fractional and zero coordinates", () => {
        expect(
          buildPayload(cmd.fields, { a_m: "-0.375", b_m: "1999.75" }),
        ).toEqual({ ok: true, hex: "0000c0be00f8f944" });
        expect(buildPayload(cmd.fields, { a_m: "0", b_m: "0" })).toEqual({
          ok: true,
          hex: "0000000000000000",
        });
        expect(buildPayload(cmd.fields, {})).toEqual({
          ok: true,
          hex: "0000000000000000",
        });
      });

      it("refuses a coordinate that is not a number", () => {
        const payload = buildPayload(cmd.fields, { a_m: "ten", b_m: "1" });
        expect(payload.ok).toBe(false);
        if (!payload.ok) expect(payload.error).toContain("'a_m'");
      });
    });
  }

  it("refuses a command whose fields include a type it cannot encode", () => {
    const cmd = command(specCatalogText, "SpecBusDriver #0", "SendFrame");
    const payload = buildPayload(cmd.fields, { length: "4" });
    expect(payload.ok).toBe(false);
    if (!payload.ok) {
      expect(payload.error).toContain("'reserved'");
      expect(payload.error).toContain("'raw[3]'");
      expect(payload.error).toContain("Raw Mode");
    }
  });
});

/* The command form end to end: the page loads a shipped catalog, the
 * operator picks a command and fills its fields, and Send either posts
 * the encoded payload or shows the refusal and posts nothing. */
describe("command form", () => {
  let posts: string[] = [];

  function renderPage(catalog: string) {
    posts = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const path = String(input);
        if (path.endsWith("/commands")) {
          return new Response(catalog, { status: 200 });
        }
        if (path.endsWith("/structs")) {
          return new Response(JSON.stringify({ components: [] }), {
            status: 200,
          });
        }
        if (path.endsWith("/command")) {
          posts.push(String(init?.body));
          return new Response(
            JSON.stringify({ status: 0, status_name: "SUCCESS" }),
            { status: 200 },
          );
        }
        return new Response("not found", { status: 404 });
      }),
    );
    const client = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    render(
      <QueryClientProvider client={client}>
        <CommandingPage
          selectedTarget="target-7"
          targets={[{ id: "target-7", name: "Target" }]}
        />
      </QueryClientProvider>,
    );
  }

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

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("posts SET_TARGET_REL's coordinates as two float32s", async () => {
    renderPage(roverCatalogText);
    const user = await choose("GroundVehicle", "SET_TARGET_REL");
    await user.type(screen.getByPlaceholderText("a_m"), "10.5");
    await user.type(screen.getByPlaceholderText("b_m"), "-20.25");
    await user.click(screen.getByRole("button", { name: "Send" }));

    await waitFor(() => expect(posts).toHaveLength(1));
    expect(JSON.parse(posts[0])).toEqual({
      full_uid: "0x00DE00",
      opcode: "0x0104",
      payload_hex: "000028410000a2c1",
    });
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("shows the refusal and posts nothing for a field it cannot encode", async () => {
    renderPage(specCatalogText);
    const user = await choose("SpecBusDriver #0", "SendFrame");
    await user.click(screen.getByRole("button", { name: "Send" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(
      "Not sent: field 'reserved' has type 'raw[3]'",
    );
    expect(posts).toHaveLength(0);
  });
});
