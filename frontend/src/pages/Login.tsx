import { useState, type FormEvent } from "react";
import type { SignedOutReason } from "../api/session";

/* ----------------------------- Types ----------------------------- */

interface Props {
  /** Why the form is showing. */
  reason: SignedOutReason;
  /** The user name to prefill (the previous session's user). */
  defaultUser: string;
  /** Over the current page (an ended session) instead of a full page. */
  overlay: boolean;
  /** Resolves to null on success or the message to show. */
  onSignIn: (user: string, password: string) => Promise<string | null>;
}

/* ----------------------------- Constants ----------------------------- */

const REASON_TEXT: Record<SignedOutReason, string | null> = {
  none: null,
  "signed-out": "You signed out.",
  idle: "Your session ended after a period without input.",
  absolute: "Your session reached its time limit.",
  ended: "Your session ended.",
};

/* ----------------------------- Component ----------------------------- */

/** The console's sign-in form. As a full page it stands in for the
 *  shell; as an overlay it covers the page a session ended on, which
 *  stays mounted underneath, so nothing typed there is lost and
 *  nothing is resent. */
export default function LoginPage({
  reason,
  defaultUser,
  overlay,
  onSignIn,
}: Props) {
  const [user, setUser] = useState(defaultUser);
  const [password, setPassword] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (busy) return;
    setBusy(true);
    setProblem(null);
    const result = await onSignIn(user.trim(), password);
    // On success the session state changes and this form unmounts.
    if (result !== null) {
      setProblem(result);
      setPassword("");
      setBusy(false);
    }
  };

  const reasonText = REASON_TEXT[reason];
  const form = (
    <form
      onSubmit={submit}
      aria-label="Sign in"
      className="rounded-lg p-5 w-[340px] flex flex-col gap-3"
      style={{
        backgroundColor: "var(--color-surface)",
        border: "1px solid var(--color-border)",
      }}
    >
      <div
        className="font-bold text-sm tracking-widest"
        style={{ color: "var(--color-accent)" }}
      >
        ZENITH
      </div>
      {reasonText && (
        <div className="text-xs" style={{ color: "var(--color-warn)" }}>
          {reasonText}
          {overlay && (
            <>
              {" "}
              Sign in to continue: nothing was resent, and this page is kept as
              you left it.
            </>
          )}
        </div>
      )}
      <label className="text-xs flex flex-col gap-1">
        <span style={{ color: "var(--color-text-muted)" }}>User name</span>
        <input
          value={user}
          onChange={(e) => setUser(e.target.value)}
          autoComplete="username"
          autoFocus={!defaultUser}
          className="text-sm px-2 py-1.5 rounded"
          style={{
            backgroundColor: "var(--color-elevated)",
            color: "var(--color-text-primary)",
            border: "1px solid var(--color-border)",
          }}
        />
      </label>
      <label className="text-xs flex flex-col gap-1">
        <span style={{ color: "var(--color-text-muted)" }}>Password</span>
        <input
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          autoComplete="current-password"
          autoFocus={!!defaultUser}
          className="text-sm px-2 py-1.5 rounded"
          style={{
            backgroundColor: "var(--color-elevated)",
            color: "var(--color-text-primary)",
            border: "1px solid var(--color-border)",
          }}
        />
      </label>
      {problem && (
        <div
          role="alert"
          className="text-xs"
          style={{ color: "var(--color-crit)" }}
        >
          {problem}
        </div>
      )}
      <button
        type="submit"
        disabled={busy || !user.trim() || !password}
        className="text-sm px-3 py-1.5 rounded font-bold"
        style={{
          backgroundColor: "var(--color-accent)",
          color: "var(--color-bg, #0d1117)",
          opacity: busy || !user.trim() || !password ? 0.6 : 1,
        }}
      >
        {busy ? "Signing in..." : "Sign in"}
      </button>
    </form>
  );

  if (overlay) {
    return (
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Sign in again"
        className="fixed inset-0 z-[60] flex items-center justify-center"
        style={{ backgroundColor: "rgba(0,0,0,0.6)" }}
      >
        {form}
      </div>
    );
  }
  return (
    <div
      className="h-screen flex items-center justify-center"
      style={{ backgroundColor: "var(--color-body)" }}
    >
      {form}
    </div>
  );
}
