import { useEffect, useState } from "react";

interface Target {
  id: string;
  name: string;
  connected: boolean;
}

const NAV_ITEMS = [
  { path: "/", label: "Dashboard", icon: "D" },
  { path: "/telemetry", label: "Telemetry", icon: "T" },
  { path: "/commanding", label: "Command", icon: "C" },
  { path: "/config", label: "Config", icon: "S" },
  { path: "/params", label: "Params", icon: "P" },
  { path: "/files", label: "Files", icon: "F" },
];

export default function NavBar({
  selectedTarget,
  onTargetChange,
}: {
  selectedTarget: string;
  onTargetChange: (id: string) => void;
}) {
  const currentPath = window.location.pathname;
  const [targets, setTargets] = useState<Target[]>([]);
  const [clock, setClock] = useState("");

  useEffect(() => {
    const fetchTargets = () => {
      fetch("/api/targets")
        .then((r) => r.json())
        .then((data) => setTargets(data.targets))
        .catch(() => {});
    };
    fetchTargets();
    const interval = setInterval(fetchTargets, 3000);
    return () => clearInterval(interval);
  }, []);

  useEffect(() => {
    const tick = () => {
      const now = new Date();
      setClock(now.toISOString().split("T")[1].split(".")[0] + " UTC");
    };
    tick();
    const interval = setInterval(tick, 1000);
    return () => clearInterval(interval);
  }, []);

  const connectedTarget = targets.find((t) => t.id === selectedTarget);
  const isConnected = connectedTarget?.connected ?? false;

  return (
    <div className="flex h-screen">
      {/* Sidebar */}
      <nav
        className="w-14 flex flex-col items-center py-3 gap-1 shrink-0"
        style={{
          backgroundColor: "var(--color-surface)",
          borderRight: "1px solid var(--color-border-muted)",
        }}
      >
        <div className="text-accent font-bold text-xs mb-4 tracking-widest">
          Z
        </div>
        {NAV_ITEMS.map((item) => {
          const active = currentPath === item.path;
          return (
            <a
              key={item.path}
              href={item.path}
              title={item.label}
              className="relative w-10 h-10 flex items-center justify-center rounded-lg text-sm font-medium transition-colors"
              style={{
                color: active
                  ? "var(--color-accent)"
                  : "var(--color-text-muted)",
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
              {item.icon}
            </a>
          );
        })}
      </nav>

      {/* Main area */}
      <div className="flex-1 flex flex-col min-w-0">
        {/* Top bar */}
        <header
          className="h-12 flex items-center justify-between px-4 shrink-0"
          style={{
            backgroundColor: "var(--color-surface)",
            borderBottom: "1px solid var(--color-border-muted)",
          }}
        >
          <div className="flex items-center gap-3">
            <select
              value={selectedTarget}
              onChange={(e) => onTargetChange(e.target.value)}
              className="mono text-sm px-2 py-1"
              style={{
                backgroundColor: "var(--color-elevated)",
                border: "1px solid var(--color-border)",
              }}
            >
              {targets.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.name}
                </option>
              ))}
            </select>

            <div
              className="flex items-center gap-1.5 text-xs px-2 py-1 rounded-full"
              style={{
                backgroundColor: isConnected
                  ? "rgba(63,185,80,0.1)"
                  : "rgba(248,81,73,0.1)",
                color: isConnected ? "var(--color-ok)" : "var(--color-crit)",
              }}
            >
              <div
                className="w-2 h-2 rounded-full"
                style={{
                  backgroundColor: isConnected
                    ? "var(--color-ok)"
                    : "var(--color-crit)",
                }}
              />
              {isConnected ? "Connected" : "Disconnected"}
            </div>
          </div>

          <div
            className="mono text-sm"
            style={{ color: "var(--color-text-muted)" }}
          >
            {clock}
          </div>
        </header>

        {/* Page content */}
        <main className="flex-1 overflow-auto p-4">
          {/* Children rendered by App.tsx */}
        </main>
      </div>
    </div>
  );
}

declare const __APP_VERSION__: string;
