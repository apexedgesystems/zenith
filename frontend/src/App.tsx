import { useEffect, useRef, useState } from "react";
import DashboardPage from "./pages/Dashboard";
import TelemetryPage from "./pages/Telemetry";
import CommandingPage from "./pages/Commanding";
import OperationsPage from "./pages/Operations";
import InspectPage from "./pages/Inspect";
import AuditPage from "./pages/Audit";
import TunablesPage from "./pages/Tunables";
import FileTransferPage from "./pages/Files";
import Clock from "./components/Clock";
import ErrorBoundary from "./components/ErrorBoundary";
import { useDialogs } from "./components/dialogs";
import { type Target, formatBytes, formatCount } from "./utils/targets";
import { useAllTargetStorage, useTargets } from "./api/queries";

/* ----------------------------- Nav ----------------------------- */

const NAV_ITEMS = [
  { path: "/", label: "Dashboard" },
  { path: "/telemetry", label: "Telemetry" },
  { path: "/operations", label: "Operations" },
  { path: "/commanding", label: "Command" },
  { path: "/tunables", label: "Tunables" },
  { path: "/inspect", label: "INSPECT" },
  { path: "/files", label: "File Transfer" },
  { path: "/audit", label: "Audit Log" },
];

/* ----------------------------- Add Target Form ----------------------------- */

function AddTargetForm({
  onAdd,
  onCancel,
}: {
  onAdd: (name: string, host: string, port: number) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [host, setHost] = useState("");
  const [port, setPort] = useState("9000");

  const submit = () => {
    if (!name.trim() || !host.trim()) return;
    const p = parseInt(port);
    if (isNaN(p) || p < 1 || p > 65535) return; // Invalid port
    onAdd(name.trim(), host.trim(), p);
  };

  return (
    <div
      className="flex flex-col gap-1.5 p-2 rounded-md"
      style={{ backgroundColor: "var(--color-elevated)" }}
    >
      <input
        value={name}
        onChange={(e) => setName(e.target.value)}
        placeholder="Name"
        className="text-xs px-2 py-1"
        autoFocus
      />
      <input
        value={host}
        onChange={(e) => setHost(e.target.value)}
        placeholder="Host"
        className="mono text-xs px-2 py-1"
      />
      <input
        value={port}
        onChange={(e) => setPort(e.target.value)}
        placeholder="Port"
        className="mono text-xs px-2 py-1"
        type="number"
      />
      <div className="flex gap-1">
        <button
          onClick={submit}
          className="text-xs px-2 py-1 flex-1 text-white"
          style={{ backgroundColor: "var(--color-accent)", border: "none" }}
        >
          Add
        </button>
        <button
          onClick={onCancel}
          className="text-xs px-2 py-1 flex-1"
          style={{
            backgroundColor: "transparent",
            color: "var(--color-text-muted)",
            border: "1px solid var(--color-border)",
          }}
        >
          Cancel
        </button>
      </div>
    </div>
  );
}

/* ----------------------------- App ----------------------------- */

function App() {
  const [page, setPage] = useState(window.location.pathname);
  const [selectedTarget, setSelectedTarget] = useState("target-0");
  // Server state via the shared query cache: one poller regardless of
  // how many components need the target list, structural sharing in
  // place of the old hand-rolled equality diffing, and errors surfaced
  // instead of leaving the sidebar silently stale.
  const { notify, confirmDialog } = useDialogs();
  const targetsQuery = useTargets();
  const targets: Target[] = targetsQuery.data ?? [];
  const [showAddForm, setShowAddForm] = useState(false);
  const [targetMenu, setTargetMenu] = useState<{
    x: number;
    y: number;
    target: Target;
  } | null>(null);
  const targetMenuRef = useRef<HTMLDivElement>(null);
  const storageQuery = useAllTargetStorage(targets.map((t) => t.id));
  const storage = storageQuery.data ?? {};
  // Per-target auto-reconnect preferences. Persisted in localStorage so
  // the user's choice survives refresh. The reconnect loop checks every
  // 3s whether any target with the flag is currently disconnected.
  const [autoReconnect, setAutoReconnect] = useState<Record<string, boolean>>(
    () => {
      try {
        const raw = localStorage.getItem("zenith.autoReconnect");
        return raw ? JSON.parse(raw) : {};
      } catch {
        return {};
      }
    },
  );
  const targetsRef = useRef(targets);
  targetsRef.current = targets;
  const autoReconnectRef = useRef(autoReconnect);
  autoReconnectRef.current = autoReconnect;

  // SPA routing
  useEffect(() => {
    const handlePop = () => setPage(window.location.pathname);
    window.addEventListener("popstate", handlePop);
    return () => window.removeEventListener("popstate", handlePop);
  }, []);

  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      // Let the browser handle anything that is not a plain left
      // click: modifier clicks and middle clicks open new tabs, and
      // explicit targets navigate normally.
      if (e.defaultPrevented || e.button !== 0) return;
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      const target = e.target as HTMLElement;
      const anchor = target.closest("a");
      if (!anchor || !anchor.href) return;
      if (anchor.origin !== window.location.origin) return;
      if (anchor.target && anchor.target !== "_self") return;
      e.preventDefault();
      window.history.pushState({}, "", anchor.pathname);
      setPage(anchor.pathname);
    };
    document.addEventListener("click", handleClick);
    return () => document.removeEventListener("click", handleClick);
  }, []);

  // Close target menu on outside click. useEffect cleanup ensures we
  // never stack listeners (the previous ref-callback approach could
  // accumulate listeners on rapid open/close).
  useEffect(() => {
    if (!targetMenu) return;
    const handler = (e: MouseEvent) => {
      const el = targetMenuRef.current;
      if (el && !el.contains(e.target as Node)) {
        setTargetMenu(null);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [targetMenu]);

  // Auto-reconnect loop. Every 5s, for each target with the flag set,
  // Re-query backend directly for authoritative
  // connection state, then fire connect only for genuinely-down targets.
  //
  // This used to read `targetsRef.current` which is updated by the 3s
  // targets-poll effect. When the poll lagged behind the loop, it would see
  // stale `connected: false` and fire a redundant connect -- and apex
  // only allows one TCP per target, so each new connect kicked the
  // previous one off, producing a self-sustaining flap. The fresh fetch
  // here makes the check authoritative.
  //
  // The 5s interval (vs the targets poll's 3s) is also intentional --
  // Auto-reconnect should trail polling, not race it. And
  // the per-target cooldownRef prevents another connect from firing
  // within 10 seconds of a successful one.
  const reconnectInflightRef = useRef<Set<string>>(new Set());
  const reconnectCooldownRef = useRef<Map<string, number>>(new Map());
  useEffect(() => {
    const tick = async () => {
      const ar = autoReconnectRef.current;
      const enabledTargets = Object.keys(ar).filter((id) => ar[id]);
      if (enabledTargets.length === 0) return;

      // Authoritative state via fresh fetch (NOT the polled ref)
      let liveTargets: Target[];
      try {
        const r = await fetch("/api/targets");
        if (!r.ok) return;
        const data = await r.json();
        liveTargets = data.targets || [];
      } catch {
        return;
      }

      const inflight = reconnectInflightRef.current;
      const cooldown = reconnectCooldownRef.current;
      const now = Date.now();
      for (const t of liveTargets) {
        if (!ar[t.id]) continue;
        if (t.connected) continue;
        if (inflight.has(t.id)) continue;
        const lastTry = cooldown.get(t.id) || 0;
        if (now - lastTry < 10_000) continue; // 10s cooldown after last attempt
        inflight.add(t.id);
        cooldown.set(t.id, now);
        fetch(`/api/targets/${t.id}/connect`, { method: "POST" })
          .catch(() => {})
          .finally(() => {
            inflight.delete(t.id);
          });
      }
    };
    const interval = setInterval(tick, 5000);
    return () => clearInterval(interval);
  }, []);

  // Persist auto-reconnect prefs
  useEffect(() => {
    try {
      localStorage.setItem(
        "zenith.autoReconnect",
        JSON.stringify(autoReconnect),
      );
    } catch {
      /* quota / private mode */
    }
  }, [autoReconnect]);

  const toggleAutoReconnect = (id: string) => {
    setAutoReconnect((prev) => ({ ...prev, [id]: !prev[id] }));
  };

  const connectTarget = async (id: string) => {
    await fetch(`/api/targets/${id}/connect`, { method: "POST" });
  };

  const disconnectTarget = async (id: string) => {
    await fetch(`/api/targets/${id}/disconnect`, { method: "POST" });
  };

  const removeTarget = async (id: string) => {
    if (!(await confirmDialog("Remove this target?", "Remove target"))) return;
    await fetch(`/api/targets/${id}/remove`, { method: "POST" });
    if (selectedTarget === id) {
      const remaining = targets.filter((t) => t.id !== id);
      setSelectedTarget(remaining.length > 0 ? remaining[0].id : "");
    }
  };

  const trimTarget = async (id: string) => {
    const s = storage[id];
    const count = s ? Math.max(1, Math.floor(s.sample_count / 4)) : 0;
    if (!count) {
      await notify("No samples to trim");
      return;
    }
    if (
      !(await confirmDialog(
        `Delete the oldest ~${formatCount(count)} samples for this target?`,
        "Trim stored telemetry",
      ))
    )
      return;
    try {
      const r = await fetch(`/api/targets/${id}/storage/trim`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ count }),
      });
      if (!r.ok) {
        await notify(`Trim failed: ${await r.text()}`, "Trim failed");
      }
    } catch (e) {
      await notify(`Trim failed: ${e}`, "Trim failed");
    }
  };

  const addTarget = async (name: string, host: string, port: number) => {
    try {
      const r = await fetch("/api/targets/add", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ name, host, port }),
      });
      if (r.ok) {
        const data = await r.json();
        setSelectedTarget(data.id);
        setShowAddForm(false);
      }
    } catch {
      /* ignore */
    }
  };

  // Page content
  let content;
  if (page === "/telemetry")
    content = <TelemetryPage selectedTarget={selectedTarget} />;
  else if (page === "/operations")
    content = <OperationsPage selectedTarget={selectedTarget} />;
  else if (page === "/commanding")
    content = (
      <CommandingPage selectedTarget={selectedTarget} targets={targets} />
    );
  else if (page === "/tunables")
    content = <TunablesPage selectedTarget={selectedTarget} />;
  else if (page === "/inspect")
    content = <InspectPage selectedTarget={selectedTarget} />;
  else if (page === "/audit") content = <AuditPage />;
  else if (page === "/files")
    content = (
      <FileTransferPage selectedTarget={selectedTarget} targets={targets} />
    );
  else if (page === "/")
    content = (
      <DashboardPage selectedTarget={selectedTarget} targets={targets} />
    );
  else
    content = (
      <div className="p-6 text-sm" style={{ color: "var(--color-text-muted)" }}>
        No page at <span className="mono">{page}</span>.{" "}
        <a href="/" style={{ color: "var(--color-accent)" }}>
          Back to the Dashboard
        </a>
      </div>
    );

  return (
    <div
      className="flex h-screen"
      style={{ backgroundColor: "var(--color-body)" }}
    >
      {/* Sidebar */}
      <nav
        className="w-48 flex flex-col py-3 px-2 shrink-0 overflow-auto"
        style={{ backgroundColor: "var(--color-sidebar)" }}
      >
        {/* Brand */}
        <div
          className="font-bold text-sm mb-3 px-3 tracking-widest"
          style={{ color: "var(--color-accent)" }}
        >
          ZENITH
        </div>

        {/* Nav items */}
        <div className="flex flex-col gap-0.5 mb-4">
          {NAV_ITEMS.map((item) => {
            const active = page === item.path;
            return (
              <a
                key={item.path}
                href={item.path}
                className="relative flex items-center px-3 py-2 rounded-md text-sm transition-colors"
                style={{
                  color: active ? "#e6edf3" : "#8b949e",
                  backgroundColor: active
                    ? "var(--color-elevated)"
                    : "transparent",
                }}
              >
                {active && (
                  <div
                    className="absolute left-0 top-1.5 bottom-1.5 w-0.5 rounded-r"
                    style={{ backgroundColor: "var(--color-accent)" }}
                  />
                )}
                {item.label}
              </a>
            );
          })}
        </div>

        {/* Divider */}
        <div
          style={{
            borderTop: "1px solid var(--color-border-muted)",
            margin: "0 8px 8px",
          }}
        />

        {/* Targets label */}
        <div
          className="text-[10px] uppercase tracking-widest px-3 mb-2"
          style={{ color: "var(--color-text-muted)" }}
        >
          Targets
        </div>

        {/* Target list */}
        <div className="flex flex-col gap-1 flex-1">
          {targets.map((t) => {
            const active = t.id === selectedTarget;
            const s = storage[t.id];
            return (
              <div
                key={t.id}
                onClick={() => setSelectedTarget(t.id)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  setTargetMenu({ x: e.clientX, y: e.clientY, target: t });
                }}
                className="relative px-3 py-2 rounded-md cursor-pointer transition-colors"
                style={{
                  backgroundColor: active
                    ? "var(--color-elevated)"
                    : "transparent",
                }}
              >
                {active && (
                  <div
                    className="absolute left-0 top-2 bottom-2 w-0.5 rounded-r"
                    style={{ backgroundColor: "var(--color-accent)" }}
                  />
                )}
                <div className="flex items-center gap-1.5">
                  <div
                    className="w-1.5 h-1.5 rounded-full shrink-0"
                    style={{
                      backgroundColor: t.connected
                        ? "var(--color-ok)"
                        : "var(--color-border)",
                    }}
                  />
                  <span
                    className="text-xs font-semibold truncate"
                    style={{
                      color: active
                        ? "var(--color-text-primary)"
                        : "var(--color-text-secondary)",
                    }}
                  >
                    {t.name}
                  </span>
                  {autoReconnect[t.id] && (
                    <span
                      title="Auto-reconnect on"
                      style={{
                        fontSize: "0.55rem",
                        fontWeight: "bold",
                        color: "var(--color-accent)",
                        marginLeft: "auto",
                        letterSpacing: "0.05em",
                      }}
                    >
                      AR
                    </span>
                  )}
                </div>
                <div
                  className="mono text-[10px] mt-0.5 pl-3"
                  style={{ color: "var(--color-text-muted)" }}
                >
                  {t.host}:{t.port}
                </div>
                {s && s.sample_count > 0 && (
                  <div
                    className="mono text-[9px] pl-3 mt-0.5"
                    style={{ color: "var(--color-text-muted)" }}
                    title={`${s.sample_count.toLocaleString()} samples across ${
                      s.channel_count
                    } channels`}
                  >
                    {formatCount(s.sample_count)} |{" "}
                    {formatBytes(s.byte_estimate)}
                  </div>
                )}
              </div>
            );
          })}

          {/* Add Target */}
          {showAddForm ? (
            <AddTargetForm
              onAdd={addTarget}
              onCancel={() => setShowAddForm(false)}
            />
          ) : (
            <button
              onClick={() => setShowAddForm(true)}
              className="text-xs px-3 py-1.5 mt-1 rounded-md text-left"
              style={{
                color: "var(--color-text-muted)",
                backgroundColor: "transparent",
                border: "1px dashed var(--color-border-muted)",
              }}
            >
              + Add Target
            </button>
          )}
        </div>

        {/* Target context menu */}
        {targetMenu && (
          <div
            ref={targetMenuRef}
            style={{
              position: "fixed",
              left: targetMenu.x,
              top: targetMenu.y,
              zIndex: 100,
              backgroundColor: "var(--color-surface)",
              border: "1px solid var(--color-border)",
              borderRadius: "6px",
              padding: "4px 0",
              minWidth: "150px",
              boxShadow: "0 4px 12px rgba(0,0,0,0.4)",
            }}
          >
            {targetMenu.target.connected ? (
              <div
                onClick={() => {
                  disconnectTarget(targetMenu.target.id);
                  setTargetMenu(null);
                }}
                className="px-3 py-1.5 text-xs cursor-pointer"
                style={{ color: "var(--color-text-primary)" }}
                onMouseEnter={(e) =>
                  (e.currentTarget.style.backgroundColor =
                    "var(--color-elevated)")
                }
                onMouseLeave={(e) =>
                  (e.currentTarget.style.backgroundColor = "transparent")
                }
              >
                Disconnect
              </div>
            ) : (
              <div
                onClick={() => {
                  connectTarget(targetMenu.target.id);
                  setTargetMenu(null);
                }}
                className="px-3 py-1.5 text-xs cursor-pointer"
                style={{ color: "var(--color-ok)" }}
                onMouseEnter={(e) =>
                  (e.currentTarget.style.backgroundColor =
                    "var(--color-elevated)")
                }
                onMouseLeave={(e) =>
                  (e.currentTarget.style.backgroundColor = "transparent")
                }
              >
                Connect
              </div>
            )}
            <div
              onClick={() => {
                navigator.clipboard.writeText(
                  `${targetMenu.target.host}:${targetMenu.target.port}`,
                );
                setTargetMenu(null);
              }}
              className="px-3 py-1.5 text-xs cursor-pointer"
              style={{ color: "var(--color-text-primary)" }}
              onMouseEnter={(e) =>
                (e.currentTarget.style.backgroundColor =
                  "var(--color-elevated)")
              }
              onMouseLeave={(e) =>
                (e.currentTarget.style.backgroundColor = "transparent")
              }
            >
              Copy address
            </div>
            <div
              onClick={() => {
                toggleAutoReconnect(targetMenu.target.id);
                setTargetMenu(null);
              }}
              className="px-3 py-1.5 text-xs cursor-pointer flex items-center justify-between gap-2"
              style={{ color: "var(--color-text-primary)" }}
              onMouseEnter={(e) =>
                (e.currentTarget.style.backgroundColor =
                  "var(--color-elevated)")
              }
              onMouseLeave={(e) =>
                (e.currentTarget.style.backgroundColor = "transparent")
              }
              title="When enabled, zenith will keep trying to reconnect to this target every 5 seconds while it is disconnected"
            >
              <span>Auto-reconnect</span>
              <span
                style={{
                  color: autoReconnect[targetMenu.target.id]
                    ? "var(--color-ok)"
                    : "var(--color-text-muted)",
                }}
              >
                {autoReconnect[targetMenu.target.id] ? "ON" : "OFF"}
              </span>
            </div>
            <div
              style={{
                borderTop: "1px solid var(--color-border-muted)",
                margin: "4px 0",
              }}
            />
            <div
              onClick={() => {
                window.open(
                  `/api/targets/${targetMenu.target.id}/telemetry/csv?limit=100000`,
                  "_blank",
                );
                setTargetMenu(null);
              }}
              className="px-3 py-1.5 text-xs cursor-pointer"
              style={{ color: "var(--color-text-primary)" }}
              onMouseEnter={(e) =>
                (e.currentTarget.style.backgroundColor =
                  "var(--color-elevated)")
              }
              onMouseLeave={(e) =>
                (e.currentTarget.style.backgroundColor = "transparent")
              }
            >
              Export telemetry (CSV)
            </div>
            <div
              onClick={() => {
                trimTarget(targetMenu.target.id);
                setTargetMenu(null);
              }}
              className="px-3 py-1.5 text-xs cursor-pointer"
              style={{ color: "var(--color-warn)" }}
              onMouseEnter={(e) =>
                (e.currentTarget.style.backgroundColor =
                  "var(--color-elevated)")
              }
              onMouseLeave={(e) =>
                (e.currentTarget.style.backgroundColor = "transparent")
              }
            >
              Trim oldest 25%
            </div>
            <div
              style={{
                borderTop: "1px solid var(--color-border-muted)",
                margin: "4px 0",
              }}
            />
            <div
              onClick={() => {
                removeTarget(targetMenu.target.id);
                setTargetMenu(null);
              }}
              className="px-3 py-1.5 text-xs cursor-pointer"
              style={{ color: "var(--color-crit)" }}
              onMouseEnter={(e) =>
                (e.currentTarget.style.backgroundColor =
                  "var(--color-elevated)")
              }
              onMouseLeave={(e) =>
                (e.currentTarget.style.backgroundColor = "transparent")
              }
            >
              Remove target
            </div>
          </div>
        )}
      </nav>

      {/* Main */}
      <div className="flex-1 flex flex-col min-w-0">
        {/* Top bar */}
        <header
          className="h-10 flex items-center justify-between px-4 shrink-0"
          style={{
            backgroundColor: "var(--color-surface)",
            borderBottom: "1px solid var(--color-border-muted)",
          }}
        >
          <div className="text-xs" style={{ color: "var(--color-text-muted)" }}>
            {targets.find((t) => t.id === selectedTarget)?.name ||
              "No target selected"}
          </div>
          <Clock />
        </header>

        {/* Page content */}
        <main className="flex-1 overflow-auto p-5">
          <ErrorBoundary key={page + selectedTarget}>{content}</ErrorBoundary>
        </main>
      </div>
    </div>
  );
}

export default App;
