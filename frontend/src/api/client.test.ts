import { describe, expect, it } from "vitest";
import { ApiError, request } from "./client";
import { json, stubFetch } from "../test/helpers";

/* request() turns each way a typed call can fail into an ApiError that
 * names the endpoint; apiFetch.test.ts covers its 401. */

/** The endpoint, status and message of the ApiError `call` fails with. */
async function failure(call: Promise<unknown>) {
  const e = await call.then(
    () => null,
    (err: unknown) => err,
  );
  expect(e).toBeInstanceOf(ApiError);
  const { endpoint, status, message } = e as ApiError;
  return { endpoint, status, message };
}

describe("request", () => {
  it("fails with an ApiError for a shape mismatch, a body that is not JSON and an unreachable server", async () => {
    const targetsList = (v: unknown) =>
      Array.isArray((v as { targets?: unknown }).targets)
        ? null
        : "targets: expected array";

    stubFetch(() => json({ targets: "none" }));
    expect(await failure(request("/targets", targetsList))).toEqual({
      endpoint: "/targets",
      status: 200,
      message: "/targets: shape mismatch: targets: expected array",
    });

    stubFetch(() => new Response("<html>"));
    expect(await failure(request("/targets", () => null))).toEqual({
      endpoint: "/targets",
      status: 200,
      message: "/targets: response is not JSON",
    });

    stubFetch(() => {
      throw new TypeError("Failed to fetch");
    });
    expect(await failure(request("/targets", () => null))).toEqual({
      endpoint: "/targets",
      status: 0,
      message: "/targets: network error: TypeError: Failed to fetch",
    });
  });
});
