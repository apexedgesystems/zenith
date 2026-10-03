import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { apiFetch, setRefusalListener } from "./apiFetch";
import {
  createSession,
  isSessionStatus,
  sessionNotice,
  signsInInPlace,
  type Session,
  type SessionState,
} from "./session";

/* A fake server with the backend's session rules: deadlines derived
 * from sign-in and the last keep-alive, the keep-alive clamped at the
 * absolute deadline, reads that never extend, and a clock that may
 * differ from the browser's. */

const MIN = 60_000;
const T0 = Date.UTC(2026, 9, 3, 12, 0, 0);

interface Fake {
  authEnabled: boolean;
  idleMs: number;
  absMs: number;
  /** Server clock minus browser clock. */
  offsetMs: number;
  live: { created: number; refreshed: number } | null;
  /** false: the browser drops the cookie (Secure over plain HTTP). */
  keepCookie: boolean;
  deleteStatus: number;
  /** Requests held until released (path -> resolver). */
  held: Map<string, (r: Response) => void>;
  calls: string[];
}

function fake(over: Partial<Fake> = {}): Fake {
  return {
    authEnabled: true,
    idleMs: 30 * MIN,
    absMs: 600 * MIN,
    offsetMs: 0,
    live: null,
    keepCookie: true,
    deleteStatus: 204,
    held: new Map(),
    calls: [],
    ...over,
  };
}

const serverNow = (f: Fake) => Date.now() + f.offsetMs;

function statusOf(f: Fake, live: { created: number; refreshed: number }) {
  const abs = live.created + f.absMs;
  const idle = f.idleMs > 0 ? Math.min(live.refreshed + f.idleMs, abs) : null;
  return {
    auth_enabled: true,
    user: "ops",
    idle_deadline_ms: idle,
    absolute_deadline_ms: abs,
    server_time_ms: serverNow(f),
  };
}

function alive(f: Fake): boolean {
  if (!f.live) return false;
  const s = statusOf(f, f.live);
  const now = serverNow(f);
  return (
    now < s.absolute_deadline_ms &&
    (s.idle_deadline_ms === null || now < s.idle_deadline_ms)
  );
}

const json = (v: unknown) => new Response(JSON.stringify(v), { status: 200 });
const refusal = () => new Response("session ended", { status: 401 });

function install(f: Fake) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const method = init?.method ?? "GET";
      const path = String(input);
      f.calls.push(`${method} ${path}`);
      if (f.held.has(path)) {
        return new Promise<Response>((res) => f.held.set(path, res));
      }
      if (!f.authEnabled) {
        if (method === "DELETE") return new Response(null, { status: 204 });
        return json({ auth_enabled: false });
      }
      if (path === "/api/auth/session" && method === "POST") {
        const body = JSON.parse(String(init?.body));
        if (body.password !== "pw") {
          return new Response("invalid credentials", { status: 401 });
        }
        const live = { created: serverNow(f), refreshed: serverNow(f) };
        if (f.keepCookie) f.live = live;
        return json(statusOf(f, live));
      }
      if (path === "/api/auth/session" && method === "DELETE") {
        if (f.deleteStatus !== 204) {
          return new Response("not recorded", { status: f.deleteStatus });
        }
        f.live = null;
        return new Response(null, { status: 204 });
      }
      if (!alive(f)) return refusal();
      if (path === "/api/auth/session/refresh") {
        f.live!.refreshed = serverNow(f);
        return json(statusOf(f, f.live!));
      }
      if (path === "/api/auth/session") return json(statusOf(f, f.live!));
      return json({ ok: true });
    }),
  );
}

const keepAlives = (f: Fake) =>
  f.calls.filter((c) => c === "POST /api/auth/session/refresh").length;

let stops: (() => void)[] = [];

function boot(f: Fake): { s: Session; input: EventTarget } {
  install(f);
  const s = createSession();
  const input = new EventTarget();
  stops.push(s.start(input));
  return { s, input };
}

const settle = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(T0);
});

afterEach(() => {
  for (const stop of stops) stop();
  stops = [];
  setRefusalListener(null);
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

/* ----------------------------- state machine ----------------------------- */

describe("session state", () => {
  it("is anonymous with auth off and makes no session call beyond the first", async () => {
    const f = fake({ authEnabled: false });
    const { s, input } = boot(f);
    await settle();
    expect(s.getState()).toEqual({ kind: "anonymous", generation: 1 });
    for (let i = 0; i < 6; i++) {
      input.dispatchEvent(new Event("keydown"));
      input.dispatchEvent(new Event("pointermove"));
      await vi.advanceTimersByTimeAsync(30 * MIN);
    }
    expect(f.calls).toEqual(["GET /api/auth/session"]);
  });

  it("restores a live session at boot with its deadlines and the server's clock", async () => {
    const f = fake({ offsetMs: 90 * MIN });
    f.live = {
      created: serverNow(f) - 5 * MIN,
      refreshed: serverNow(f) - 5 * MIN,
    };
    const { s } = boot(f);
    await settle();
    const st = s.getState();
    expect(st).toMatchObject({
      kind: "signedIn",
      user: "ops",
      idleDeadline: serverNow(f) + 25 * MIN,
      absoluteDeadline: serverNow(f) + 595 * MIN,
      skewMs: 90 * MIN,
    });
  });

  it("is signed out with nothing to keep when there is no session at boot", async () => {
    const { s } = boot(fake());
    await settle();
    expect(s.getState()).toEqual({
      kind: "signedOut",
      reason: "none",
      user: null,
    });
    expect(signsInInPlace("none")).toBe(false);
  });

  it("signs in, reads the session back and starts a new generation", async () => {
    const f = fake();
    const { s } = boot(f);
    await settle();
    expect(await s.signIn("ops", "pw")).toBeNull();
    const st = s.getState();
    expect(st).toMatchObject({
      kind: "signedIn",
      user: "ops",
      idleDeadline: T0 + 30 * MIN,
    });
    // Boot made generation 1 (signed out); the sign-in starts the next.
    expect(st.kind === "signedIn" && st.generation).toBe(2);
    expect(f.calls).toEqual([
      "GET /api/auth/session",
      "POST /api/auth/session",
      "GET /api/auth/session",
    ]);
  });

  it("refuses a wrong password with a message and changes nothing", async () => {
    const f = fake();
    const { s } = boot(f);
    await settle();
    const before = s.getState();
    expect(await s.signIn("ops", "nope")).toBe("Wrong user name or password.");
    expect(s.getState()).toBe(before);
    expect(f.calls).toEqual([
      "GET /api/auth/session",
      "POST /api/auth/session",
    ]);
  });

  it("explains a cookie the browser did not keep instead of failing silently", async () => {
    const f = fake({ keepCookie: false });
    const { s } = boot(f);
    await settle();
    const msg = await s.signIn("ops", "pw");
    expect(msg).toContain("did not keep the session cookie");
    expect(msg).toContain("cookie_secure");
    expect(s.getState()).toMatchObject({ kind: "signedOut", reason: "none" });
  });

  it("ends at the idle deadline with the reason idle, page kept for an in-place sign-in", async () => {
    const f = fake();
    const { s } = boot(f);
    await settle();
    await s.signIn("ops", "pw");
    await vi.advanceTimersByTimeAsync(30 * MIN - 1_000);
    expect(s.getState().kind).toBe("signedIn");
    await vi.advanceTimersByTimeAsync(2_000);
    expect(s.getState()).toEqual({
      kind: "signedOut",
      reason: "idle",
      user: "ops",
    });
    expect(signsInInPlace("idle")).toBe(true);
  });

  it("names the absolute limit when a kept-alive session reaches it", async () => {
    const f = fake({ absMs: 60 * MIN });
    const { s, input } = boot(f);
    await settle();
    await s.signIn("ops", "pw");
    for (let t = 0; t < 59; t++) {
      await vi.advanceTimersByTimeAsync(MIN);
      input.dispatchEvent(new Event("pointerdown"));
    }
    expect(s.getState().kind).toBe("signedIn");
    await vi.advanceTimersByTimeAsync(2 * MIN);
    expect(s.getState()).toMatchObject({
      kind: "signedOut",
      reason: "absolute",
    });
  });

  it("names nothing when the server ends a session before either deadline", async () => {
    const f = fake();
    const { s } = boot(f);
    await settle();
    await s.signIn("ops", "pw");
    f.live = null; // signed out elsewhere
    const resp = await apiFetch("/api/targets");
    expect(resp.status).toBe(401);
    expect(s.getState()).toMatchObject({ kind: "signedOut", reason: "ended" });
  });

  it("ignores a refusal for a request sent before the current sign-in", async () => {
    const f = fake();
    const { s } = boot(f);
    await settle();
    await s.signIn("ops", "pw");
    f.held.set("/api/slow", () => {});
    const slow = apiFetch("/api/slow"); // in flight under the first session
    await settle();
    const release = f.held.get("/api/slow")!;
    f.held.delete("/api/slow");
    f.live = null;
    await apiFetch("/api/targets"); // the first session ends
    expect(s.getState().kind).toBe("signedOut");
    expect(await s.signIn("ops", "pw")).toBeNull();
    const second = s.getState();
    release(refusal()); // the old request comes back refused only now
    expect((await slow).status).toBe(401);
    expect(s.getState()).toBe(second);
  });

  it("signs out to the login page, and stays signed in if the server cannot record it", async () => {
    const f = fake({ deleteStatus: 500 });
    const { s } = boot(f);
    await settle();
    await s.signIn("ops", "pw");
    expect(await s.signOut()).toContain("Sign-out failed (500)");
    expect(s.getState().kind).toBe("signedIn");
    f.deleteStatus = 204;
    expect(await s.signOut()).toBeNull();
    expect(s.getState()).toEqual({
      kind: "signedOut",
      reason: "signed-out",
      user: "ops",
    });
    expect(signsInInPlace("signed-out")).toBe(false);
  });
});

/* ----------------------------- idle tracking ----------------------------- */

describe("idle tracking", () => {
  it("sends the keep-alive on input at most once a minute", async () => {
    const f = fake();
    const { s, input } = boot(f);
    await settle();
    await s.signIn("ops", "pw");
    for (let i = 0; i < 50; i++) {
      input.dispatchEvent(new Event(i % 2 ? "pointermove" : "keydown"));
      await vi.advanceTimersByTimeAsync(2_500); // 50 inputs over 125 s
    }
    expect(keepAlives(f)).toBe(2);
    const st = s.getState() as Extract<SessionState, { kind: "signedIn" }>;
    expect(st.idleDeadline).toBe(f.live!.refreshed + 30 * MIN);
    expect(st.idleDeadline).toBeGreaterThan(T0 + 31 * MIN);
  });

  it("sends nothing when a keep-alive cannot extend the session or nobody is signed in", async () => {
    const off = fake({ idleMs: 0 });
    const a = boot(off);
    await settle();
    await a.s.signIn("ops", "pw");
    for (let i = 0; i < 10; i++) {
      a.input.dispatchEvent(new Event("pointerdown"));
      await vi.advanceTimersByTimeAsync(MIN);
    }
    expect(keepAlives(off)).toBe(0);
    stops.pop()!();

    const clamped = fake({ idleMs: 30 * MIN, absMs: 31 * MIN });
    const b = boot(clamped);
    await settle();
    await b.s.signIn("ops", "pw");
    for (let i = 0; i < 10; i++) {
      await vi.advanceTimersByTimeAsync(MIN);
      b.input.dispatchEvent(new Event("pointerdown"));
    }
    expect(keepAlives(clamped)).toBe(1); // after it the idle deadline is the absolute one
    stops.pop()!();

    const out = fake();
    const c = boot(out);
    await settle();
    for (let i = 0; i < 5; i++) {
      c.input.dispatchEvent(new Event("keydown"));
      await vi.advanceTimersByTimeAsync(2 * MIN);
    }
    expect(keepAlives(out)).toBe(0);
  });

  it("lets polling run into the idle deadline: reads never extend the session", async () => {
    const f = fake();
    const { s } = boot(f);
    await settle();
    await s.signIn("ops", "pw");
    let refusedAt: number | null = null;
    for (let t = 0; t < 31 * 12 && refusedAt === null; t++) {
      await vi.advanceTimersByTimeAsync(5_000);
      const r = await apiFetch("/api/targets");
      if (r.status === 401) refusedAt = Date.now() - T0;
    }
    expect(keepAlives(f)).toBe(0);
    expect(refusedAt).toBe(30 * MIN);
    expect(s.getState()).toMatchObject({ kind: "signedOut", reason: "idle" });
  });

  it("re-reads the session at the prompt time and sees another tab's keep-alive", async () => {
    const f = fake();
    const { s } = boot(f);
    await settle();
    await s.signIn("ops", "pw");
    await vi.advanceTimersByTimeAsync(20 * MIN);
    f.live!.refreshed = serverNow(f); // a keep-alive from another tab
    await vi.advanceTimersByTimeAsync(8 * MIN + 1_000);
    const st = s.getState() as Extract<SessionState, { kind: "signedIn" }>;
    expect(st.idleDeadline).toBe(T0 + 50 * MIN);
    expect(sessionNotice(st, Date.now())).toBeNull();
  });
});

/* ----------------------------- notices ----------------------------- */

function signedIn(
  over: Partial<Extract<SessionState, { kind: "signedIn" }>> = {},
): Extract<SessionState, { kind: "signedIn" }> {
  return {
    kind: "signedIn",
    user: "ops",
    idleDeadline: T0 + 30 * MIN,
    absoluteDeadline: T0 + 600 * MIN,
    skewMs: 0,
    generation: 1,
    ...over,
  };
}

describe("sessionNotice", () => {
  it("prompts in the last two minutes before an idle deadline a keep-alive can move", () => {
    const st = signedIn();
    expect(sessionNotice(st, T0 + 28 * MIN - 1)).toBeNull();
    expect(sessionNotice(st, T0 + 28 * MIN)).toEqual({
      kind: "idle",
      endsAtLocal: T0 + 30 * MIN,
      remainingMs: 2 * MIN,
    });
  });

  it("gives the end notice in the last ten minutes before the absolute deadline", () => {
    const st = signedIn({ idleDeadline: T0 + 600 * MIN });
    expect(sessionNotice(st, T0 + 590 * MIN - 1)).toBeNull();
    expect(sessionNotice(st, T0 + 595 * MIN)).toEqual({
      kind: "absolute",
      endsAtLocal: T0 + 600 * MIN,
      remainingMs: 5 * MIN,
    });
    const noIdle = signedIn({ idleDeadline: null });
    expect(sessionNotice(noIdle, T0 + 595 * MIN)?.kind).toBe("absolute");
  });

  it("times both against the server's clock, not the browser's", () => {
    const st = signedIn({ skewMs: 3_600_000 }); // server an hour ahead
    expect(sessionNotice(st, T0 + 28 * MIN - 3_600_000 - 1)).toBeNull();
    expect(sessionNotice(st, T0 + 28 * MIN - 3_600_000)).toEqual({
      kind: "idle",
      endsAtLocal: T0 + 30 * MIN - 3_600_000,
      remainingMs: 2 * MIN,
    });
  });

  it("shows nothing when nobody is signed in", () => {
    expect(sessionNotice({ kind: "anonymous", generation: 1 }, T0)).toBeNull();
    expect(
      sessionNotice({ kind: "signedOut", reason: "idle", user: "ops" }, T0),
    ).toBeNull();
  });
});

describe("isSessionStatus", () => {
  it("accepts both shapes the server sends", () => {
    expect(isSessionStatus({ auth_enabled: false })).toBeNull();
    expect(
      isSessionStatus({
        auth_enabled: true,
        user: "ops",
        idle_deadline_ms: null,
        absolute_deadline_ms: 1,
        server_time_ms: 0,
      }),
    ).toBeNull();
  });

  it("names the first field that does not fit", () => {
    expect(isSessionStatus("x")).toBe("expected object");
    expect(isSessionStatus({})).toContain("auth_enabled");
    expect(
      isSessionStatus({
        auth_enabled: true,
        absolute_deadline_ms: 1,
        server_time_ms: 0,
      }),
    ).toContain("user");
    expect(
      isSessionStatus({
        auth_enabled: true,
        user: "ops",
        idle_deadline_ms: "soon",
        absolute_deadline_ms: 1,
        server_time_ms: 0,
      }),
    ).toContain("idle_deadline_ms");
  });
});
