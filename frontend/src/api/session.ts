/** THE owner of the browser session state.
 *
 *  The server is the authority on whether a session is alive; this
 *  module mirrors what it says, and nothing else in the UI holds a
 *  session fact. States: unknown (boot, before the first answer),
 *  anonymous (auth off), signed in, signed out with a reason. Pages
 *  read the state through the hooks below and never see a cookie (the
 *  browser holds it, HttpOnly) or a deadline.
 *
 *  Deadlines are server instants. Each answer carries the server's
 *  clock, and every comparison here runs on the server's clock (local
 *  clock plus the measured skew), so a browser clock that is off by
 *  minutes cannot move a prompt early or late.
 *
 *  Only operator input renews a session: input sends the keep-alive at
 *  most once a minute, and only when it can extend the session. Reads
 *  of the session (boot, checkpoints) never extend it.
 */

import { useSyncExternalStore } from "react";
import { apiFetch, setRefusalListener } from "./apiFetch";
import { field, isObject, type Validator } from "./client";

/* ----------------------------- Types ----------------------------- */

/** Mirrors the backend's SessionStatus (core/session.rs): the body of
 *  every /api/auth/session answer. */
export type SessionStatus =
  | { auth_enabled: false }
  | {
      auth_enabled: true;
      user: string;
      /** null when the server has no idle limit. */
      idle_deadline_ms: number | null;
      absolute_deadline_ms: number;
      server_time_ms: number;
    };

/** Why the UI is signed out. */
export type SignedOutReason =
  /** No session when the console opened. */
  | "none"
  /** The operator signed out. */
  | "signed-out"
  /** The idle limit passed without a keep-alive. */
  | "idle"
  /** The absolute limit after sign-in passed. */
  | "absolute"
  /** The server refused the session for another reason (signed out
   *  elsewhere, ended on the server). */
  | "ended";

export type SessionState =
  | { kind: "unknown" }
  | { kind: "anonymous"; generation: number }
  | {
      kind: "signedIn";
      user: string;
      /** Server instant; null without an idle limit. */
      idleDeadline: number | null;
      /** Server instant. */
      absoluteDeadline: number;
      /** Server clock minus local clock, measured at the last answer. */
      skewMs: number;
      generation: number;
    }
  | { kind: "signedOut"; reason: SignedOutReason; user: string | null };

/** What the shell should tell the operator about the session now. */
export type SessionNotice =
  | { kind: "idle"; endsAtLocal: number; remainingMs: number }
  | { kind: "absolute"; endsAtLocal: number; remainingMs: number };

export interface Session {
  getState: () => SessionState;
  subscribe: (listener: () => void) => () => void;
  /** Attach to apiFetch and to operator input, then read the session.
   *  Returns the matching stop. */
  start: (input?: EventTarget) => () => void;
  /** Re-read the session from the server; never extends it. */
  check: () => Promise<void>;
  /** Sign in; resolves to null on success or a message to show. */
  signIn: (user: string, password: string) => Promise<string | null>;
  /** Sign out; resolves to null on success or a message to show. */
  signOut: () => Promise<string | null>;
  /** Operator input happened (the idle tracker calls this). */
  noteInput: () => void;
  /** Send the keep-alive now (the "stay signed in" action). */
  stayActive: () => Promise<void>;
}

/* ----------------------------- Constants ----------------------------- */

/** Input sends the keep-alive at most this often. */
export const KEEPALIVE_EVERY_MS = 60_000;
/** The stay-signed-in prompt shows this long before the idle deadline. */
export const IDLE_PROMPT_MS = 120_000;
/** The end-of-session notice shows this long before the absolute deadline. */
export const ABSOLUTE_NOTICE_MS = 600_000;
/** The close code the server ends a session's telemetry socket with. */
export const SESSION_END_CLOSE = 1008;
/** Real input: pointer, key and touch (wheel is pointer input too). */
export const INPUT_EVENTS = [
  "pointerdown",
  "pointermove",
  "keydown",
  "wheel",
  "touchstart",
] as const;

/** The server clock is known to within a round trip; a refusal within
 *  this margin of a deadline is attributed to that deadline. */
const DEADLINE_MARGIN_MS = 5_000;
/** Checkpoints re-read the session at least this often and never
 *  sooner than this after the previous one. */
const CHECK_MAX_MS = 600_000;
const CHECK_MIN_MS = 1_000;
const SESSION_PATH = "/api/auth/session";

/* ----------------------------- Helpers ----------------------------- */

export const isSessionStatus: Validator<SessionStatus> = (v) => {
  if (!isObject(v)) return "expected object";
  const enabled = field(v, "auth_enabled", "boolean");
  if (enabled !== null) return enabled;
  if (v.auth_enabled === false) return null;
  const problem =
    field(v, "user", "string") ??
    field(v, "absolute_deadline_ms", "number") ??
    field(v, "server_time_ms", "number");
  if (problem !== null) return problem;
  const idle = v.idle_deadline_ms;
  return idle === null || typeof idle === "number"
    ? null
    : "idle_deadline_ms: expected number or null";
};

/** Whether a keep-alive could move the session's end: an idle limit
 *  exists and its deadline has not reached the absolute one. */
function canExtend(s: Extract<SessionState, { kind: "signedIn" }>): boolean {
  return s.idleDeadline !== null && s.idleDeadline < s.absoluteDeadline;
}

/** Signing in again after these reasons happens over the current page,
 *  so its unsent state survives; the others show the login page. */
export function signsInInPlace(reason: SignedOutReason): boolean {
  return reason === "idle" || reason === "absolute" || reason === "ended";
}

/** The generation requests run under while a session (or auth-off
 *  access) is live; null while signed out or unknown. */
export function liveGeneration(s: SessionState): number | null {
  return s.kind === "signedIn" || s.kind === "anonymous" ? s.generation : null;
}

/** The notice the shell shows at local instant `now`: the
 *  stay-signed-in prompt in the last two minutes before an idle
 *  deadline that a keep-alive can move, else the end-of-session notice
 *  in the last ten minutes before the absolute deadline. */
export function sessionNotice(
  s: SessionState,
  now: number,
): SessionNotice | null {
  if (s.kind !== "signedIn") return null;
  const serverNow = now + s.skewMs;
  if (
    canExtend(s) &&
    (s.idleDeadline as number) - serverNow <= IDLE_PROMPT_MS
  ) {
    const idle = s.idleDeadline as number;
    return {
      kind: "idle",
      endsAtLocal: idle - s.skewMs,
      remainingMs: Math.max(0, idle - serverNow),
    };
  }
  if (s.absoluteDeadline - serverNow <= ABSOLUTE_NOTICE_MS) {
    return {
      kind: "absolute",
      endsAtLocal: s.absoluteDeadline - s.skewMs,
      remainingMs: Math.max(0, s.absoluteDeadline - serverNow),
    };
  }
  return null;
}

/** Answer to a session request: the validated body, or null when the
 *  request failed for any reason other than a refusal. */
async function readStatus(resp: Response): Promise<SessionStatus | null> {
  if (!resp.ok) return null;
  const body: unknown = await resp.json().catch(() => null);
  return isSessionStatus(body) === null ? (body as SessionStatus) : null;
}

/* ----------------------------- API ----------------------------- */

export function createSession(): Session {
  let state: SessionState = { kind: "unknown" };
  // Names who the UI is signed in as (or that nobody is): bumps on
  // every change of identity, never on a keep-alive.
  let generation = 0;
  let lastKeepAlive = 0;
  let keepAliveInFlight = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const listeners = new Set<() => void>();

  function set(next: SessionState) {
    state = next;
    schedule();
    for (const l of listeners) l();
  }

  function signedOut(reason: SignedOutReason) {
    const user = state.kind === "signedIn" ? state.user : null;
    generation += 1;
    set({ kind: "signedOut", reason, user });
  }

  /** Apply a live answer that arrived at local instant `at`. A new
   *  session (none before, or a different absolute deadline) gets a
   *  new generation; the same session only moves its idle deadline
   *  forward, so answers landing out of order cannot pull it back. */
  function applyLive(body: SessionStatus, at: number) {
    if (!body.auth_enabled) {
      if (state.kind !== "anonymous") {
        generation += 1;
        set({ kind: "anonymous", generation });
      }
      return;
    }
    const prev =
      state.kind === "signedIn" &&
      state.absoluteDeadline === body.absolute_deadline_ms
        ? state
        : null;
    if (prev === null) generation += 1;
    const idle =
      prev !== null &&
      prev.idleDeadline !== null &&
      body.idle_deadline_ms !== null
        ? Math.max(prev.idleDeadline, body.idle_deadline_ms)
        : body.idle_deadline_ms;
    set({
      kind: "signedIn",
      user: body.user,
      idleDeadline: idle,
      absoluteDeadline: body.absolute_deadline_ms,
      skewMs: body.server_time_ms - at,
      generation,
    });
  }

  /** Why a request refused for the current session was refused. */
  function endReason(): SignedOutReason {
    if (state.kind !== "signedIn") return "ended";
    const serverNow = Date.now() + state.skewMs + DEADLINE_MARGIN_MS;
    if (serverNow >= state.absoluteDeadline) return "absolute";
    if (state.idleDeadline !== null && serverNow >= state.idleDeadline) {
      return "idle";
    }
    return "ended";
  }

  function refused(sentUnder: number) {
    if (sentUnder !== generation) return; // answer to an older session
    if (state.kind === "signedIn" || state.kind === "anonymous") {
      signedOut(endReason());
    }
  }

  /** Re-read the session at the next instant that matters: the
   *  stay-signed-in prompt (another tab may have kept the session
   *  alive) and the deadline itself (the end shows without waiting for
   *  a poll). */
  function schedule() {
    clearTimeout(timer);
    timer = undefined;
    if (state.kind !== "signedIn") return;
    const serverNow = Date.now() + state.skewMs;
    const end = Math.min(
      state.idleDeadline ?? state.absoluteDeadline,
      state.absoluteDeadline,
    );
    const promptAt = canExtend(state)
      ? (state.idleDeadline as number) - IDLE_PROMPT_MS
      : end;
    const next = promptAt > serverNow ? promptAt : end;
    const delay = Math.min(
      CHECK_MAX_MS,
      Math.max(CHECK_MIN_MS, next - serverNow),
    );
    timer = setTimeout(() => void check(), delay);
  }

  async function check() {
    const sentUnder = generation;
    let resp: Response;
    try {
      resp = await apiFetch(SESSION_PATH);
    } catch {
      schedule(); // unreachable server: try again at the next checkpoint
      return;
    }
    const at = Date.now();
    if (sentUnder !== generation) return; // signed in or out meanwhile
    if (resp.status === 401) {
      // apiFetch has reported the refusal; only boot has no state yet.
      if (state.kind === "unknown") signedOut("none");
      return;
    }
    const body = await readStatus(resp);
    if (body === null || sentUnder !== generation) {
      if (state.kind === "unknown") {
        clearTimeout(timer);
        timer = setTimeout(() => void check(), CHECK_MIN_MS * 5);
      } else {
        schedule();
      }
      return;
    }
    applyLive(body, at);
  }

  async function signIn(
    user: string,
    password: string,
  ): Promise<string | null> {
    let resp: Response;
    try {
      resp = await apiFetch(SESSION_PATH, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username: user, password }),
      });
    } catch {
      return "Cannot reach the server.";
    }
    if (resp.status === 401) return "Wrong user name or password.";
    if (resp.status === 429) {
      return "Too many attempts from this address; wait a moment and try again.";
    }
    if (!resp.ok) {
      const text = await resp.text().catch(() => "");
      return `Sign-in failed (${resp.status})${text ? `: ${text}` : ""}`;
    }
    // Read the session back: a browser that refused the cookie (a
    // Secure cookie over plain HTTP) must not look like a wrong
    // password or a session that ends at once.
    let verify: Response;
    try {
      verify = await apiFetch(SESSION_PATH);
    } catch {
      return "Cannot reach the server.";
    }
    const at = Date.now();
    if (verify.status === 401) {
      return (
        "Signed in, but this browser did not keep the session cookie. " +
        "The server sends it for HTTPS only: open the console over HTTPS, " +
        "or set [auth] cookie_secure = false on a trusted network."
      );
    }
    const body = await readStatus(verify);
    if (body === null) return `Sign-in failed (${verify.status}).`;
    lastKeepAlive = Date.now();
    applyLive(body, at);
    return null;
  }

  async function signOut(): Promise<string | null> {
    let resp: Response;
    try {
      resp = await apiFetch(SESSION_PATH, { method: "DELETE" });
    } catch {
      return "Cannot reach the server; still signed in.";
    }
    if (!resp.ok && resp.status !== 401) {
      const text = await resp.text().catch(() => "");
      return `Sign-out failed (${resp.status})${text ? `: ${text}` : ""}`;
    }
    signedOut("signed-out");
    return null;
  }

  async function keepAlive() {
    if (state.kind !== "signedIn" || keepAliveInFlight) return;
    keepAliveInFlight = true;
    lastKeepAlive = Date.now();
    const sentUnder = generation;
    try {
      const resp = await apiFetch(`${SESSION_PATH}/refresh`, {
        method: "POST",
      });
      const at = Date.now();
      const body = await readStatus(resp);
      if (body !== null && sentUnder === generation) applyLive(body, at);
    } catch {
      // The next input after the throttle tries again.
    } finally {
      keepAliveInFlight = false;
    }
  }

  function noteInput() {
    if (state.kind !== "signedIn" || !canExtend(state)) return;
    if (Date.now() - lastKeepAlive < KEEPALIVE_EVERY_MS) return;
    void keepAlive();
  }

  function start(input: EventTarget = window): () => void {
    setRefusalListener({ generation: () => generation, refused });
    const onInput = () => noteInput();
    for (const ev of INPUT_EVENTS) {
      input.addEventListener(ev, onInput, { capture: true, passive: true });
    }
    void check();
    return () => {
      setRefusalListener(null);
      for (const ev of INPUT_EVENTS) {
        input.removeEventListener(ev, onInput, { capture: true });
      }
      clearTimeout(timer);
      timer = undefined;
    };
  }

  return {
    getState: () => state,
    subscribe: (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    start,
    check,
    signIn,
    signOut,
    noteInput,
    stayActive: keepAlive,
  };
}

/** The app's session. */
export const session = createSession();

export function useSession(): SessionState {
  return useSyncExternalStore(session.subscribe, session.getState);
}

/** The live generation (see liveGeneration): re-renders only when it
 *  changes, so a keep-alive does not re-render its users. */
export function useSessionGeneration(): number | null {
  return useSyncExternalStore(session.subscribe, () =>
    liveGeneration(session.getState()),
  );
}
