import { useCallback, useEffect, useState } from "react";
import { useDialogs } from "../components/dialogs";

/**
 * Operations Page (Phase 2 of the MVP roadmap).
 *
 * Operator command panel covering all the system-level executive
 * commands an operator needs to actually run a system: sleep/wake,
 * pause/resume, restart, set verbosity, and per-component
 * lock/unlock.
 *
 * Talks to the existing `/api/targets/{id}/command` endpoint with
 * client-side payload hex assembly. No new backend endpoints needed.
 */

/* ----------------------------- Types ----------------------------- */

interface RegistryComponent {
  fullUid: string;
  name: string;
  type: string;
  reachable: boolean;
}

interface AuditEntry {
  id: number;
  ts: string;
  label: string;
  detail: string;
  status: "ok" | "err" | "pending";
  message?: string;
}

/* ----------------------------- Opcodes ----------------------------- */

const EXEC_UID = "0x000000";

const OP = {
  SLEEP: { opcode: "0x0116", label: "Sleep" },
  WAKE: { opcode: "0x0117", label: "Wake" },
  PAUSE: { opcode: "0x0110", label: "Pause" },
  RESUME: { opcode: "0x0111", label: "Resume" },
  RESTART: { opcode: "0x0127", label: "Restart Executive" },
  SET_VERBOSITY: { opcode: "0x0121", label: "Set Verbosity" },
  LOCK: { opcode: "0x0114", label: "Lock" },
  UNLOCK: { opcode: "0x0115", label: "Unlock" },
} as const;

/** Pack a u32 as 4-char little-endian hex. */
function u32Hex(n: number): string {
  const buf = new ArrayBuffer(4);
  new DataView(buf).setUint32(0, n >>> 0, true);
  return [...new Uint8Array(buf)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** Pack a u8 as 2-char hex. */
function u8Hex(n: number): string {
  return (n & 0xff).toString(16).padStart(2, "0");
}

/* ----------------------------- Page ----------------------------- */

export default function OperationsPage({
  selectedTarget,
}: {
  selectedTarget: string;
}) {
  const { notify } = useDialogs();
  const [registry, setRegistry] = useState<RegistryComponent[]>([]);
  const [connected, setConnected] = useState(false);
  const [audit, setAudit] = useState<AuditEntry[]>([]);
  const [verbosity, setVerbosity] = useState("3");
  const [confirmRestart, setConfirmRestart] = useState(false);
  const [busy, setBusy] = useState<Set<string>>(new Set());
  const [systemPaused, setSystemPaused] = useState<boolean | null>(null);
  const [systemAsleep, setSystemAsleep] = useState<boolean | null>(null);

  // Library swap state
  const [swapUid, setSwapUid] = useState("");
  const [swapInstance, setSwapInstance] = useState("0");
  const [swapBank, setSwapBank] = useState("bank_b");
  const [swapFile, setSwapFile] = useState<File | null>(null);
  const [swapInProgress, setSwapInProgress] = useState(false);
  const [confirmSwap, setConfirmSwap] = useState(false);

  // Per-component lock state. Tracked client-side because the apex side
  // doesn't currently expose a "is this component locked" query opcode
  // (apex todo: add GET_LOCK_STATE). Reset on target switch since lock
  // state lives in the apex process and doesn't survive a restart.
  const [lockedUids, setLockedUids] = useState<Set<string>>(new Set());
  useEffect(() => {
    setLockedUids(new Set());
  }, [selectedTarget]);

  /* ---- Load connection state + registry ---- */

  useEffect(() => {
    let cancelled = false;
    const fetchState = async () => {
      try {
        const r = await fetch("/api/targets");
        if (!r.ok || cancelled) return;
        const data = await r.json();
        const t = (data.targets || []).find(
          (x: { id: string }) => x.id === selectedTarget,
        );
        setConnected(!!t?.connected);
      } catch {
        /* ignore */
      }
    };
    fetchState();
    const interval = setInterval(fetchState, 3000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [selectedTarget]);

  useEffect(() => {
    if (!connected) {
      setRegistry([]);
      return;
    }
    let cancelled = false;
    fetch(`/api/targets/${selectedTarget}/registry`)
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        if (cancelled) return;
        if (data?.components) setRegistry(data.components);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [selectedTarget, connected]);

  /* ---- Send command via existing generic endpoint ---- */

  const send = useCallback(
    async (
      label: string,
      fullUid: string,
      opcode: string,
      payloadHex: string,
      detail: string,
    ) => {
      const id = Date.now() + Math.random();
      const ts = new Date().toISOString().split("T")[1].split(".")[0];
      const key = `${fullUid}-${opcode}-${payloadHex}`;
      setBusy((prev) => new Set(prev).add(key));
      const pending: AuditEntry = { id, ts, label, detail, status: "pending" };
      setAudit((prev) => [pending, ...prev].slice(0, 50));

      try {
        const r = await fetch(`/api/targets/${selectedTarget}/command`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            full_uid: fullUid,
            opcode,
            payload_hex: payloadHex,
          }),
        });
        if (!r.ok) {
          const text = await r.text();
          setAudit((prev) =>
            prev.map((e) =>
              e.id === id
                ? { ...e, status: "err", message: text || `HTTP ${r.status}` }
                : e,
            ),
          );
          return false;
        }
        const data = await r.json();
        const ok = data.status === 0;
        setAudit((prev) =>
          prev.map((e) =>
            e.id === id
              ? {
                  ...e,
                  status: ok ? "ok" : "err",
                  message: ok
                    ? data.status_name
                    : `${data.status_name} (status ${data.status})`,
                }
              : e,
          ),
        );
        return ok;
      } catch (err) {
        setAudit((prev) =>
          prev.map((e) =>
            e.id === id ? { ...e, status: "err", message: String(err) } : e,
          ),
        );
        return false;
      } finally {
        setBusy((prev) => {
          const n = new Set(prev);
          n.delete(key);
          return n;
        });
      }
    },
    [selectedTarget],
  );

  /* ---- Action handlers ---- */

  const sleepWake = useCallback(async () => {
    if (systemAsleep) {
      const ok = await send(
        OP.WAKE.label,
        EXEC_UID,
        OP.WAKE.opcode,
        "",
        "Wake from sleep",
      );
      if (ok) setSystemAsleep(false);
    } else {
      const ok = await send(
        OP.SLEEP.label,
        EXEC_UID,
        OP.SLEEP.opcode,
        "",
        "Sleep system",
      );
      if (ok) setSystemAsleep(true);
    }
  }, [send, systemAsleep]);

  const pauseResume = useCallback(async () => {
    if (systemPaused) {
      const ok = await send(
        OP.RESUME.label,
        EXEC_UID,
        OP.RESUME.opcode,
        "",
        "Resume scheduler",
      );
      if (ok) setSystemPaused(false);
    } else {
      const ok = await send(
        OP.PAUSE.label,
        EXEC_UID,
        OP.PAUSE.opcode,
        "",
        "Pause scheduler",
      );
      if (ok) setSystemPaused(true);
    }
  }, [send, systemPaused]);

  const restart = useCallback(async () => {
    setConfirmRestart(false);
    // Use the dedicated /restart endpoint instead of the generic /command.
    // The dedicated path knows that "connection closed by remote" is
    // the EXPECTED outcome of a successful restart (apex calls execv()
    // before the ACK can be sent), and reports a clean SUCCESS to the
    // audit log instead of the misleading "err: connection closed".
    const id = Date.now() + Math.random();
    const ts = new Date().toISOString().split("T")[1].split(".")[0];
    const pending: AuditEntry = {
      id,
      ts,
      label: "Restart Executive",
      detail: "execve",
      status: "pending",
    };
    setAudit((prev) => [pending, ...prev].slice(0, 50));
    try {
      const r = await fetch(`/api/targets/${selectedTarget}/restart`, {
        method: "POST",
      });
      if (r.ok) {
        const data = await r.json();
        setAudit((prev) =>
          prev.map((e) =>
            e.id === id ? { ...e, status: "ok", message: data.status_name } : e,
          ),
        );
        // Lock state is gone after a restart -- clear local tracking
        setLockedUids(new Set());
      } else {
        const text = await r.text();
        setAudit((prev) =>
          prev.map((e) =>
            e.id === id
              ? { ...e, status: "err", message: text || `HTTP ${r.status}` }
              : e,
          ),
        );
        return;
      }
    } catch (e) {
      setAudit((prev) =>
        prev.map((e2) =>
          e2.id === id ? { ...e2, status: "err", message: String(e) } : e2,
        ),
      );
      return;
    }

    // Schedule auto-reconnect attempts. We try a few times with backoff
    // because exact restart time varies with target hardware.
    const tryReconnect = async (
      delayMs: number,
      attempt: number,
    ): Promise<boolean> => {
      await new Promise((res) => setTimeout(res, delayMs));
      try {
        const r = await fetch(`/api/targets/${selectedTarget}/connect`, {
          method: "POST",
        });
        if (r.ok) {
          const okEntry: AuditEntry = {
            id: Date.now() + Math.random(),
            ts: new Date().toISOString().split("T")[1].split(".")[0],
            label: "Auto-reconnect",
            detail: `attempt ${attempt}`,
            status: "ok",
            message: "reconnected",
          };
          setAudit((prev) => [okEntry, ...prev].slice(0, 50));
          return true;
        }
      } catch {
        /* keep trying */
      }
      return false;
    };
    // 3 attempts: 3s, 6s, 12s
    if (await tryReconnect(3000, 1)) return;
    if (await tryReconnect(3000, 2)) return;
    if (await tryReconnect(6000, 3)) return;
    const failEntry: AuditEntry = {
      id: Date.now() + Math.random(),
      ts: new Date().toISOString().split("T")[1].split(".")[0],
      label: "Auto-reconnect",
      detail: "gave up after 3 attempts",
      status: "err",
      message: "click Connect manually",
    };
    setAudit((prev) => [failEntry, ...prev].slice(0, 50));
  }, [send, selectedTarget]);

  const setVerb = useCallback(async () => {
    const v = parseInt(verbosity);
    if (isNaN(v) || v < 0 || v > 7) {
      void notify("Verbosity must be 0-7", "Invalid verbosity");
      return;
    }
    await send(
      OP.SET_VERBOSITY.label,
      EXEC_UID,
      OP.SET_VERBOSITY.opcode,
      u8Hex(v),
      `level ${v}`,
    );
  }, [send, verbosity]);

  const lockComp = useCallback(
    async (comp: RegistryComponent) => {
      const uid = parseInt(comp.fullUid.replace("0x", ""), 16);
      const ok = await send(
        OP.LOCK.label,
        EXEC_UID,
        OP.LOCK.opcode,
        u32Hex(uid),
        comp.name,
      );
      if (ok) {
        setLockedUids((prev) => {
          const n = new Set(prev);
          n.add(comp.fullUid);
          return n;
        });
      }
    },
    [send],
  );

  const unlockComp = useCallback(
    async (comp: RegistryComponent) => {
      const uid = parseInt(comp.fullUid.replace("0x", ""), 16);
      const ok = await send(
        OP.UNLOCK.label,
        EXEC_UID,
        OP.UNLOCK.opcode,
        u32Hex(uid),
        comp.name,
      );
      if (ok) {
        setLockedUids((prev) => {
          const n = new Set(prev);
          n.delete(comp.fullUid);
          return n;
        });
      }
    },
    [send],
  );

  /* ---- Library swap ---- */

  const swapLibrary = useCallback(async () => {
    setConfirmSwap(false);
    if (!swapUid || !swapFile) {
      void notify("Pick a component and a .so file", "Library swap");
      return;
    }
    const comp = registry.find((c) => c.fullUid.replace("0x", "") === swapUid);
    if (!comp) {
      void notify("Component not in registry", "Library swap");
      return;
    }
    const baseName = comp.name.split("#")[0].split(" ")[0].trim();
    const idx = parseInt(swapInstance);
    if (isNaN(idx) || idx < 0) {
      void notify("Instance index must be >= 0", "Library swap");
      return;
    }

    setSwapInProgress(true);
    const id = Date.now() + Math.random();
    const ts = new Date().toISOString().split("T")[1].split(".")[0];
    const detail = `${baseName}#${idx} <- ${swapFile.name} (${(
      swapFile.size / 1024
    ).toFixed(1)} KB) -> ${swapBank}`;
    const pending: AuditEntry = {
      id,
      ts,
      label: "Swap Library",
      detail,
      status: "pending",
    };
    setAudit((prev) => [pending, ...prev].slice(0, 50));

    try {
      const buf = await swapFile.arrayBuffer();
      const bytes = new Uint8Array(buf);
      let binary = "";
      for (const b of bytes) binary += String.fromCharCode(b);
      const base64 = btoa(binary);

      const r = await fetch(
        `/api/targets/${selectedTarget}/components/${swapUid}/library`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            component_name: baseName,
            instance_index: idx,
            inactive_bank: swapBank,
            content_base64: base64,
          }),
        },
      );

      if (!r.ok) {
        const text = await r.text();
        setAudit((prev) =>
          prev.map((e) =>
            e.id === id
              ? { ...e, status: "err", message: text || `HTTP ${r.status}` }
              : e,
          ),
        );
        return;
      }
      const data = await r.json();
      const ok = data.status === 0;
      setAudit((prev) =>
        prev.map((e) =>
          e.id === id
            ? {
                ...e,
                status: ok ? "ok" : "err",
                message: ok
                  ? `${data.status_name} (${(
                      data.uploaded_bytes / 1024
                    ).toFixed(1)} KB)`
                  : `${data.status_name} (status ${data.status})`,
              }
            : e,
        ),
      );
      if (ok) setSwapFile(null);
    } catch (e) {
      setAudit((prev) =>
        prev.map((e2) =>
          e2.id === id ? { ...e2, status: "err", message: String(e) } : e2,
        ),
      );
    } finally {
      setSwapInProgress(false);
    }
  }, [selectedTarget, swapUid, swapInstance, swapBank, swapFile, registry]);

  /* ---- Render ---- */

  if (!connected) {
    return (
      <div className="max-w-3xl">
        <h1 className="text-xl font-bold mb-4">Operations</h1>
        <div
          className="rounded-lg p-8 text-center text-sm"
          style={{
            border: "1px dashed var(--color-border)",
            color: "var(--color-text-muted)",
          }}
        >
          Target not connected. Connect from the Dashboard or sidebar to issue
          operations.
        </div>
      </div>
    );
  }

  return (
    <div className="max-w-5xl">
      <div className="flex items-center justify-between mb-4">
        <h1 className="text-xl font-bold">Operations</h1>
        <div className="text-xs" style={{ color: "var(--color-text-muted)" }}>
          {registry.length} components
        </div>
      </div>

      {/* System Controls */}
      <Section title="System Controls">
        <div className="grid grid-cols-2 gap-3">
          <ToggleControl
            label={systemAsleep ? "Wake" : "Sleep"}
            description={
              systemAsleep ? "Resume from sleep" : "Put system to sleep"
            }
            active={systemAsleep === true}
            danger={false}
            onClick={sleepWake}
            disabled={busy.size > 0}
          />
          <ToggleControl
            label={systemPaused ? "Resume" : "Pause"}
            description={systemPaused ? "Resume scheduler" : "Pause scheduler"}
            active={systemPaused === true}
            danger={false}
            onClick={pauseResume}
            disabled={busy.size > 0}
          />
          <NumericControl
            label="Set Verbosity"
            value={verbosity}
            onChange={setVerbosity}
            unit="level"
            placeholder="0-7"
            onApply={setVerb}
            disabled={busy.size > 0}
          />
        </div>

        {/* Restart -- separate, dangerous */}
        <div
          className="mt-3 rounded-md p-3"
          style={{
            border: "1px solid var(--color-crit)",
            backgroundColor: "rgba(248,81,73,0.05)",
          }}
        >
          <div className="flex items-center justify-between">
            <div>
              <div
                className="text-xs font-bold"
                style={{ color: "var(--color-crit)" }}
              >
                Restart Executive
              </div>
              <div
                className="text-[10px]"
                style={{ color: "var(--color-text-muted)" }}
              >
                Hard restart via execve. The connection will drop; reconnect
                from the Dashboard.
              </div>
            </div>
            {confirmRestart ? (
              <div className="flex gap-1">
                <button
                  onClick={restart}
                  className="text-xs px-3 py-1 text-white"
                  style={{
                    backgroundColor: "var(--color-crit)",
                    border: "none",
                    borderRadius: "4px",
                  }}
                >
                  Confirm Restart
                </button>
                <button
                  onClick={() => setConfirmRestart(false)}
                  className="text-xs px-3 py-1"
                  style={{
                    backgroundColor: "transparent",
                    color: "var(--color-text-muted)",
                    border: "1px solid var(--color-border)",
                    borderRadius: "4px",
                  }}
                >
                  Cancel
                </button>
              </div>
            ) : (
              <button
                onClick={() => setConfirmRestart(true)}
                className="text-xs px-3 py-1"
                style={{
                  backgroundColor: "var(--color-elevated)",
                  color: "var(--color-crit)",
                  border: "1px solid var(--color-crit)",
                  borderRadius: "4px",
                }}
              >
                Restart
              </button>
            )}
          </div>
        </div>
      </Section>

      {/* Per-Component Controls */}
      <Section title="Component Controls">
        {registry.length === 0 ? (
          <div
            className="text-xs py-4 text-center"
            style={{ color: "var(--color-text-muted)" }}
          >
            Loading registry...
          </div>
        ) : (
          <div
            className="rounded-lg overflow-hidden"
            style={{ border: "1px solid var(--color-border)" }}
          >
            <table
              style={{
                width: "100%",
                borderCollapse: "collapse",
                fontSize: "0.78rem",
              }}
            >
              <thead>
                <tr
                  style={{
                    backgroundColor: "var(--color-elevated)",
                    borderBottom: "1px solid var(--color-border)",
                  }}
                >
                  <th style={{ textAlign: "left", padding: "0.5rem 0.8rem" }}>
                    Component
                  </th>
                  <th style={{ textAlign: "left", padding: "0.5rem 0.8rem" }}>
                    UID
                  </th>
                  <th
                    style={{
                      textAlign: "left",
                      padding: "0.5rem 0.8rem",
                      width: "80px",
                    }}
                  >
                    Type
                  </th>
                  <th
                    style={{
                      textAlign: "center",
                      padding: "0.5rem 0.8rem",
                      width: "60px",
                    }}
                  >
                    Status
                  </th>
                  <th
                    style={{
                      textAlign: "right",
                      padding: "0.5rem 0.8rem",
                      width: "180px",
                    }}
                  >
                    Actions
                  </th>
                </tr>
              </thead>
              <tbody>
                {registry.map((c) => {
                  const typeColors: Record<string, string> = {
                    CORE: "#6366f1",
                    SW_MODEL: "#22c55e",
                    SUPPORT: "#f59e0b",
                    HW_MODEL: "#3b82f6",
                    DRIVER: "#ef4444",
                  };
                  const color = typeColors[c.type] || "#888";
                  const isBusy = Array.from(busy).some((k) =>
                    k.includes(c.fullUid.toLowerCase().replace("0x", "")),
                  );
                  const isLocked = lockedUids.has(c.fullUid);
                  return (
                    <tr
                      key={c.fullUid}
                      style={{
                        borderBottom: "1px solid var(--color-border-muted)",
                        backgroundColor: isLocked
                          ? "rgba(210,153,34,0.06)"
                          : "transparent",
                      }}
                    >
                      <td
                        className="font-bold"
                        style={{ padding: "0.4rem 0.8rem" }}
                      >
                        {isLocked && (
                          <span
                            title="Locked"
                            style={{
                              marginRight: "0.4rem",
                              color: "var(--color-warn)",
                              fontSize: "0.7rem",
                              fontWeight: "bold",
                              padding: "0.1rem 0.3rem",
                              border: "1px solid var(--color-warn)",
                              borderRadius: "3px",
                            }}
                          >
                            LOCK
                          </span>
                        )}
                        {c.name}
                      </td>
                      <td
                        className="mono"
                        style={{
                          padding: "0.4rem 0.8rem",
                          color: "var(--color-text-muted)",
                          fontSize: "0.72rem",
                        }}
                      >
                        {c.fullUid}
                      </td>
                      <td style={{ padding: "0.4rem 0.8rem" }}>
                        <span
                          style={{
                            padding: "0.1rem 0.4rem",
                            borderRadius: "999px",
                            fontSize: "0.65rem",
                            fontWeight: "bold",
                            backgroundColor: `${color}20`,
                            color,
                          }}
                        >
                          {c.type}
                        </span>
                      </td>
                      <td
                        style={{
                          textAlign: "center",
                          padding: "0.4rem 0.8rem",
                        }}
                      >
                        <div
                          className="w-2 h-2 rounded-full mx-auto"
                          style={{
                            backgroundColor: c.reachable
                              ? "var(--color-ok)"
                              : "var(--color-crit)",
                          }}
                        />
                      </td>
                      <td
                        style={{ padding: "0.4rem 0.8rem", textAlign: "right" }}
                      >
                        <button
                          onClick={() => lockComp(c)}
                          disabled={isBusy || !c.reachable || isLocked}
                          className="text-[10px] px-2 py-0.5 mr-1"
                          style={{
                            backgroundColor: isLocked
                              ? "var(--color-warn)"
                              : "var(--color-elevated)",
                            color: isLocked
                              ? "#000"
                              : c.reachable
                                ? "var(--color-warn)"
                                : "var(--color-text-muted)",
                            border: isLocked
                              ? "none"
                              : "1px solid var(--color-border)",
                            borderRadius: "3px",
                            cursor:
                              c.reachable && !isLocked
                                ? "pointer"
                                : "not-allowed",
                            opacity:
                              isBusy || isLocked ? (isLocked ? 1 : 0.5) : 1,
                            fontWeight: isLocked ? "bold" : "normal",
                          }}
                        >
                          {isLocked ? "LOCKED" : "Lock"}
                        </button>
                        <button
                          onClick={() => unlockComp(c)}
                          disabled={isBusy || !c.reachable || !isLocked}
                          className="text-[10px] px-2 py-0.5"
                          style={{
                            backgroundColor: "var(--color-elevated)",
                            color:
                              c.reachable && isLocked
                                ? "var(--color-ok)"
                                : "var(--color-text-muted)",
                            border: "1px solid var(--color-border)",
                            borderRadius: "3px",
                            cursor:
                              c.reachable && isLocked
                                ? "pointer"
                                : "not-allowed",
                            opacity: isBusy || !isLocked ? 0.5 : 1,
                          }}
                        >
                          Unlock
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Section>

      {/* Library Swap */}
      <Section title="Library Hot-Swap">
        <div
          className="rounded-md p-3"
          style={{
            backgroundColor: "var(--color-surface)",
            border: "1px solid var(--color-warn)",
          }}
        >
          <div
            className="text-[10px] mb-3"
            style={{ color: "var(--color-text-muted)" }}
          >
            Lock the component, upload the new <span className="mono">.so</span>{" "}
            to the inactive bank, and reload. The executive auto-unlocks on
            success and auto-attempts unlock on failure.
            <strong
              style={{ color: "var(--color-warn)", marginLeft: "0.5rem" }}
            >
              Library swap is destructive -- verify the binary first.
            </strong>
          </div>
          <div className="grid grid-cols-3 gap-2 mb-2">
            <div>
              <label
                className="text-[10px] uppercase tracking-wider"
                style={{ color: "var(--color-text-muted)" }}
              >
                Component
              </label>
              <select
                value={swapUid}
                onChange={(e) => setSwapUid(e.target.value)}
                className="text-xs px-2 py-1 w-full"
              >
                <option value="">-- select --</option>
                {registry
                  .filter((c) => c.reachable)
                  .map((c) => (
                    <option key={c.fullUid} value={c.fullUid.replace("0x", "")}>
                      {c.name} ({c.fullUid})
                    </option>
                  ))}
              </select>
            </div>
            <div>
              <label
                className="text-[10px] uppercase tracking-wider"
                style={{ color: "var(--color-text-muted)" }}
              >
                Instance
              </label>
              <input
                type="number"
                min="0"
                value={swapInstance}
                onChange={(e) => setSwapInstance(e.target.value)}
                className="mono text-xs px-2 py-1 w-full"
              />
            </div>
            <div>
              <label
                className="text-[10px] uppercase tracking-wider"
                style={{ color: "var(--color-text-muted)" }}
              >
                Target Bank
              </label>
              <select
                value={swapBank}
                onChange={(e) => setSwapBank(e.target.value)}
                className="text-xs px-2 py-1 w-full"
              >
                <option value="bank_a">bank_a</option>
                <option value="bank_b">bank_b</option>
              </select>
            </div>
          </div>
          <div className="flex items-center gap-2 mt-2">
            <input
              type="file"
              accept=".so"
              onChange={(e) => {
                const f = e.target.files?.[0] || null;
                if (f && f.size > 50 * 1024 * 1024) {
                  void notify("File exceeds 50MB cap", "Library swap");
                  return;
                }
                setSwapFile(f);
                setConfirmSwap(false);
              }}
              className="text-xs flex-1"
            />
            {confirmSwap ? (
              <div className="flex gap-1">
                <button
                  onClick={swapLibrary}
                  disabled={swapInProgress}
                  className="text-xs px-3 py-1 text-white"
                  style={{
                    backgroundColor: "var(--color-warn)",
                    border: "none",
                    borderRadius: "4px",
                    cursor: swapInProgress ? "not-allowed" : "pointer",
                  }}
                >
                  {swapInProgress ? "Swapping..." : "Confirm Swap"}
                </button>
                <button
                  onClick={() => setConfirmSwap(false)}
                  disabled={swapInProgress}
                  className="text-xs px-3 py-1"
                  style={{
                    backgroundColor: "transparent",
                    color: "var(--color-text-muted)",
                    border: "1px solid var(--color-border)",
                    borderRadius: "4px",
                  }}
                >
                  Cancel
                </button>
              </div>
            ) : (
              <button
                onClick={() => setConfirmSwap(true)}
                disabled={!swapUid || !swapFile || swapInProgress}
                className="text-xs px-3 py-1"
                style={{
                  backgroundColor: "var(--color-elevated)",
                  color:
                    !swapUid || !swapFile
                      ? "var(--color-text-muted)"
                      : "var(--color-warn)",
                  border: "1px solid var(--color-warn)",
                  borderRadius: "4px",
                  cursor:
                    !swapUid || !swapFile || swapInProgress
                      ? "not-allowed"
                      : "pointer",
                  opacity: !swapUid || !swapFile ? 0.5 : 1,
                }}
              >
                Hot-Swap Library
              </button>
            )}
          </div>
          {swapFile && (
            <div
              className="mono text-[10px] mt-2"
              style={{ color: "var(--color-text-muted)" }}
            >
              Selected: {swapFile.name} ({(swapFile.size / 1024).toFixed(1)} KB)
            </div>
          )}
        </div>
      </Section>

      {/* Audit Log */}
      <Section title={`Audit Log (${audit.length})`}>
        {audit.length === 0 ? (
          <div
            className="text-xs py-4 text-center"
            style={{ color: "var(--color-text-muted)" }}
          >
            No commands sent yet
          </div>
        ) : (
          <div
            className="rounded-lg overflow-hidden"
            style={{
              border: "1px solid var(--color-border)",
              maxHeight: "300px",
              overflowY: "auto",
            }}
          >
            <table
              style={{
                width: "100%",
                borderCollapse: "collapse",
                fontSize: "0.72rem",
              }}
            >
              <tbody>
                {audit.map((e) => (
                  <tr
                    key={e.id}
                    style={{
                      borderBottom: "1px solid var(--color-border-muted)",
                    }}
                  >
                    <td
                      className="mono"
                      style={{
                        padding: "0.3rem 0.6rem",
                        color: "var(--color-text-muted)",
                        width: "80px",
                      }}
                    >
                      {e.ts}
                    </td>
                    <td
                      style={{
                        padding: "0.3rem 0.6rem",
                        width: "150px",
                        fontWeight: "bold",
                      }}
                    >
                      {e.label}
                    </td>
                    <td
                      style={{
                        padding: "0.3rem 0.6rem",
                        color: "var(--color-text-muted)",
                      }}
                    >
                      {e.detail}
                    </td>
                    <td
                      style={{ padding: "0.3rem 0.6rem", textAlign: "right" }}
                    >
                      <span
                        style={{
                          padding: "0.1rem 0.4rem",
                          borderRadius: "3px",
                          backgroundColor:
                            e.status === "ok"
                              ? "rgba(63,185,80,0.15)"
                              : e.status === "err"
                                ? "rgba(248,81,73,0.15)"
                                : "rgba(255,255,255,0.05)",
                          color:
                            e.status === "ok"
                              ? "var(--color-ok)"
                              : e.status === "err"
                                ? "var(--color-crit)"
                                : "var(--color-text-muted)",
                          fontSize: "0.65rem",
                          fontWeight: "bold",
                        }}
                      >
                        {e.status === "ok"
                          ? e.message || "OK"
                          : e.status === "err"
                            ? e.message || "FAIL"
                            : "..."}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Section>
    </div>
  );
}

/* ----------------------------- UI Helpers ----------------------------- */

function Section({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="mb-5">
      <div
        className="text-xs uppercase tracking-wider mb-2 font-bold"
        style={{ color: "var(--color-text-secondary)" }}
      >
        {title}
      </div>
      {children}
    </div>
  );
}

function ToggleControl({
  label,
  description,
  active,
  danger,
  onClick,
  disabled,
}: {
  label: string;
  description: string;
  active: boolean;
  danger: boolean;
  onClick: () => void;
  disabled: boolean;
}) {
  return (
    <div
      className="rounded-md p-3"
      style={{
        backgroundColor: "var(--color-surface)",
        border: `1px solid ${
          active ? "var(--color-warn)" : "var(--color-border)"
        }`,
      }}
    >
      <div className="flex items-center justify-between">
        <div>
          <div className="text-xs font-bold">{label}</div>
          <div
            className="text-[10px]"
            style={{ color: "var(--color-text-muted)" }}
          >
            {description}
          </div>
        </div>
        <button
          onClick={onClick}
          disabled={disabled}
          className="text-xs px-3 py-1"
          style={{
            backgroundColor: active
              ? "var(--color-warn)"
              : "var(--color-elevated)",
            color: active
              ? "#000"
              : danger
                ? "var(--color-crit)"
                : "var(--color-text-primary)",
            border: active ? "none" : "1px solid var(--color-border)",
            borderRadius: "4px",
            cursor: disabled ? "not-allowed" : "pointer",
            opacity: disabled ? 0.5 : 1,
          }}
        >
          {label}
        </button>
      </div>
    </div>
  );
}

function NumericControl({
  label,
  value,
  onChange,
  unit,
  placeholder,
  onApply,
  disabled,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  unit: string;
  placeholder: string;
  onApply: () => void;
  disabled: boolean;
}) {
  return (
    <div
      className="rounded-md p-3"
      style={{
        backgroundColor: "var(--color-surface)",
        border: "1px solid var(--color-border)",
      }}
    >
      <div className="text-xs font-bold mb-1">{label}</div>
      <div className="flex items-center gap-1">
        <input
          type="number"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={placeholder}
          className="mono text-xs px-2 py-1 flex-1"
          style={{ minWidth: 0 }}
        />
        <span
          className="text-[10px]"
          style={{ color: "var(--color-text-muted)" }}
        >
          {unit}
        </span>
        <button
          onClick={onApply}
          disabled={disabled}
          className="text-xs px-2 py-1"
          style={{
            backgroundColor: "var(--color-accent)",
            color: "#fff",
            border: "none",
            borderRadius: "4px",
            cursor: disabled ? "not-allowed" : "pointer",
            opacity: disabled ? 0.5 : 1,
          }}
        >
          Apply
        </button>
      </div>
    </div>
  );
}
