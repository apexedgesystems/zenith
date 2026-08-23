/** In-app dialogs replacing window.alert/confirm/prompt.
 *
 *  The natives block the event loop, can't be styled or validated,
 *  are suppressible by the browser, and the worst flow (plot
 *  threshold setup) chained four of them. One provider exposes
 *  promise-based notify / confirmDialog / promptForm; a multi-field
 *  form with validation replaces prompt chains. Enter confirms,
 *  Escape cancels, focus lands on the first input.
 */

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";

export interface PromptField {
  name: string;
  label: string;
  initial?: string;
  placeholder?: string;
  /** Return an error message to block submission, null to accept. */
  validate?: (value: string) => string | null;
}

type Pending =
  | { kind: "notify"; title: string; message: string; resolve: () => void }
  | {
      kind: "confirm";
      title: string;
      message: string;
      resolve: (ok: boolean) => void;
    }
  | {
      kind: "prompt";
      title: string;
      fields: PromptField[];
      resolve: (values: Record<string, string> | null) => void;
    };

interface DialogApi {
  notify: (message: string, title?: string) => Promise<void>;
  confirmDialog: (message: string, title?: string) => Promise<boolean>;
  promptForm: (
    title: string,
    fields: PromptField[],
  ) => Promise<Record<string, string> | null>;
}

const DialogContext = createContext<DialogApi | null>(null);

export function useDialogs(): DialogApi {
  const ctx = useContext(DialogContext);
  if (!ctx) throw new Error("useDialogs outside DialogProvider");
  return ctx;
}

export function DialogProvider({ children }: { children: ReactNode }) {
  const [pending, setPending] = useState<Pending | null>(null);

  const notify = useCallback(
    (message: string, title = "Notice") =>
      new Promise<void>((resolve) =>
        setPending({ kind: "notify", title, message, resolve }),
      ),
    [],
  );
  const confirmDialog = useCallback(
    (message: string, title = "Confirm") =>
      new Promise<boolean>((resolve) =>
        setPending({ kind: "confirm", title, message, resolve }),
      ),
    [],
  );
  const promptForm = useCallback(
    (title: string, fields: PromptField[]) =>
      new Promise<Record<string, string> | null>((resolve) =>
        setPending({ kind: "prompt", title, fields, resolve }),
      ),
    [],
  );

  return (
    <DialogContext.Provider value={{ notify, confirmDialog, promptForm }}>
      {children}
      {pending && (
        <DialogHost pending={pending} close={() => setPending(null)} />
      )}
    </DialogContext.Provider>
  );
}

function DialogHost({
  pending,
  close,
}: {
  pending: Pending;
  close: () => void;
}) {
  const [values, setValues] = useState<Record<string, string>>(() => {
    if (pending.kind !== "prompt") return {};
    return Object.fromEntries(
      pending.fields.map((f) => [f.name, f.initial ?? ""]),
    );
  });
  const [problem, setProblem] = useState<string | null>(null);
  const firstInputRef = useRef<HTMLInputElement>(null);
  const okRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    (firstInputRef.current ?? okRef.current)?.focus();
  }, []);

  const cancel = () => {
    if (pending.kind === "notify") pending.resolve();
    else if (pending.kind === "confirm") pending.resolve(false);
    else pending.resolve(null);
    close();
  };

  const accept = () => {
    if (pending.kind === "prompt") {
      for (const f of pending.fields) {
        const err = f.validate?.(values[f.name] ?? "");
        if (err) {
          setProblem(`${f.label}: ${err}`);
          return;
        }
      }
      pending.resolve(values);
    } else if (pending.kind === "confirm") {
      pending.resolve(true);
    } else {
      pending.resolve();
    }
    close();
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") cancel();
    if (e.key === "Enter") accept();
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={pending.title}
      onKeyDown={onKeyDown}
      className="fixed inset-0 z-50 flex items-center justify-center"
      style={{ backgroundColor: "rgba(0,0,0,0.55)" }}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) cancel();
      }}
    >
      <div
        className="rounded-lg p-4 min-w-[320px] max-w-[440px]"
        style={{
          backgroundColor: "var(--color-surface)",
          border: "1px solid var(--color-border)",
        }}
      >
        <div
          className="text-xs uppercase tracking-widest font-bold mb-2"
          style={{ color: "var(--color-accent)" }}
        >
          {pending.title}
        </div>
        {pending.kind !== "prompt" && (
          <div
            className="text-sm mb-4"
            style={{ color: "var(--color-text-primary)" }}
          >
            {pending.message}
          </div>
        )}
        {pending.kind === "prompt" && (
          <div className="mb-4 flex flex-col gap-2">
            {pending.fields.map((f, i) => (
              <label key={f.name} className="text-xs flex flex-col gap-1">
                <span style={{ color: "var(--color-text-muted)" }}>
                  {f.label}
                </span>
                <input
                  ref={i === 0 ? firstInputRef : undefined}
                  value={values[f.name] ?? ""}
                  placeholder={f.placeholder}
                  onChange={(e) =>
                    setValues((v) => ({ ...v, [f.name]: e.target.value }))
                  }
                  className="mono text-xs px-2 py-1.5 rounded"
                  style={{
                    backgroundColor: "var(--color-elevated)",
                    color: "var(--color-text-primary)",
                    border: "1px solid var(--color-border)",
                  }}
                />
              </label>
            ))}
            {problem && (
              <div className="text-xs" style={{ color: "var(--color-crit)" }}>
                {problem}
              </div>
            )}
          </div>
        )}
        <div className="flex justify-end gap-2">
          {pending.kind !== "notify" && (
            <button
              className="text-xs px-3 py-1.5 rounded"
              style={{
                backgroundColor: "var(--color-elevated)",
                color: "var(--color-text-muted)",
              }}
              onClick={cancel}
            >
              Cancel
            </button>
          )}
          <button
            ref={okRef}
            className="text-xs px-3 py-1.5 rounded font-bold"
            style={{
              backgroundColor: "var(--color-accent)",
              color: "var(--color-bg, #0d1117)",
            }}
            onClick={accept}
          >
            OK
          </button>
        </div>
      </div>
    </div>
  );
}
