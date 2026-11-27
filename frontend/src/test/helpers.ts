/* Shared test fixtures: a JSON answer, a routed fetch stub, a render
 * wrapper whose query cache never retries, and a target as the target
 * list serves it. */

import { createElement, type ReactElement } from "react";
import { vi, type Mock } from "vitest";
import { render, type RenderResult } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { Target } from "../utils/targets";

/** A JSON answer with `status`. */
export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status });
}

/** One request the fetch stub received. */
export interface Sent {
  method: string;
  path: string;
  body: string | null;
}

/** How the stub answers a request: a response, or undefined for a 404.
 *  A route can also throw (the server cannot be reached) or return a
 *  promise the test settles later (a slow answer). */
export type Route = (req: Sent) => Response | undefined | Promise<Response>;

export interface FetchStub {
  /** Every request, in the order sent. */
  sent: Sent[];
  /** The requests as "METHOD path", in the order sent. */
  calls: () => string[];
  /** The stub itself, for call assertions. */
  fetch: Mock<
    (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>
  >;
}

/** Stub the global fetch with `route`; a request it does not answer
 *  gets a 404. The setup file removes the stub after each test. */
export function stubFetch(route: Route = () => undefined): FetchStub {
  const sent: Sent[] = [];
  const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const req: Sent = {
      method: init?.method ?? "GET",
      path: String(input),
      body: init?.body == null ? null : String(init.body),
    };
    sent.push(req);
    return (await route(req)) ?? new Response("not found", { status: 404 });
  });
  vi.stubGlobal("fetch", fetch);
  return { sent, calls: () => sent.map((r) => `${r.method} ${r.path}`), fetch };
}

/** Render `ui` under a fresh query cache that never retries, so a
 *  refused read shows at once. */
export function renderWithQuery(ui: ReactElement): RenderResult {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(createElement(QueryClientProvider, { client }, ui));
}

/** A target as GET /api/targets lists it: target-0, down, unless
 *  `over` says otherwise. */
export function target(over: Partial<Target> = {}): Target {
  return {
    id: "target-0",
    name: "Target",
    host: "127.0.0.1",
    port: 9000,
    connected: false,
    listening: false,
    ...over,
  };
}
