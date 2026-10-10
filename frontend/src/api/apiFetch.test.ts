import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { apiFetch, setRefusalListener } from "./apiFetch";
import { ApiError, request } from "./client";

function respond(status: number, body: string | null = "{}") {
  return vi.fn(async () => new Response(body, { status }));
}

describe("apiFetch", () => {
  let refused: number[];
  let generation: number;

  beforeEach(() => {
    refused = [];
    generation = 7;
    setRefusalListener({
      generation: () => generation,
      refused: (sentUnder) => refused.push(sentUnder),
    });
  });

  afterEach(() => {
    setRefusalListener(null);
    vi.unstubAllGlobals();
  });

  it("sends the request once, as given, and returns the response unchanged", async () => {
    const fetchMock = respond(200, '{"ok":true}');
    vi.stubGlobal("fetch", fetchMock);
    const init = { method: "POST", body: "x" };
    const resp = await apiFetch("/api/targets/t/connect", init);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledWith("/api/targets/t/connect", init);
    expect(await resp.json()).toEqual({ ok: true });
    expect(refused).toEqual([]);
  });

  it("reports a 401 with the session generation the request was sent under", async () => {
    let release: (r: Response) => void = () => {};
    vi.stubGlobal(
      "fetch",
      vi.fn(() => new Promise<Response>((res) => (release = res))),
    );
    const pending = apiFetch("/api/targets");
    generation = 8; // a re-sign-in while the request is in flight
    release(new Response("session ended", { status: 401 }));
    const resp = await pending;
    expect(resp.status).toBe(401);
    expect(await resp.text()).toBe("session ended");
    expect(refused).toEqual([7]);
  });

  it("reports nothing for answers other than 401", async () => {
    for (const status of [200, 204, 403, 404, 429, 500, 502]) {
      vi.stubGlobal("fetch", respond(status, status === 204 ? null : "x"));
      await apiFetch("/api/x");
    }
    expect(refused).toEqual([]);
  });

  it("never retries: one fetch per call whatever the outcome", async () => {
    for (const status of [401, 500, 503]) {
      const fetchMock = respond(status);
      vi.stubGlobal("fetch", fetchMock);
      await apiFetch("/api/targets/t/command", { method: "POST" });
      expect(fetchMock).toHaveBeenCalledTimes(1);
    }
    const failing = vi.fn(async () => {
      throw new TypeError("network down");
    });
    vi.stubGlobal("fetch", failing);
    await expect(apiFetch("/api/targets")).rejects.toThrow("network down");
    expect(failing).toHaveBeenCalledTimes(1);
  });

  it("works with no listener installed", async () => {
    setRefusalListener(null);
    vi.stubGlobal("fetch", respond(401));
    expect((await apiFetch("/api/targets")).status).toBe(401);
  });

  it("carries the typed request() wrapper: its 401 reaches the listener and still throws", async () => {
    const fetchMock = respond(401, "missing token or session");
    vi.stubGlobal("fetch", fetchMock);
    const err = await request("/targets", () => null).catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(401);
    expect(fetchMock).toHaveBeenCalledWith("/api/targets", undefined);
    expect(refused).toEqual([7]);
  });
});
