import { useEffect, useState } from "react";

interface Component {
  fullUid: string;
  name: string;
  type: string;
  reachable: boolean;
}

interface SchedulerInfo {
  tickCount?: number;
  taskCount?: number;
  periodViolations?: number;
  totalSkipCount?: number;
  fundamentalFreqHz?: number;
  poolCount?: number;
  sleeping?: boolean;
  error?: string;
}

interface ExecutiveInfo {
  clockCycles?: number;
  clockFreqHz?: number;
  uptimeSeconds?: number;
  frameOverruns?: number;
  watchdogWarnings?: number;
  error?: string;
}

export default function ConfigurationPage() {
  const [components, setComponents] = useState<Component[]>([]);
  const [scheduler, setScheduler] = useState<SchedulerInfo | null>(null);
  const [executive, setExecutive] = useState<ExecutiveInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [targets, setTargets] = useState<{ id: string; name: string }[]>([]);
  const [selectedTarget, setSelectedTarget] = useState("target-0");

  useEffect(() => {
    fetch("/api/targets")
      .then((r) => r.json())
      .then((data) =>
        setTargets(
          data.targets.map((t: { id: string; name: string }) => ({
            id: t.id,
            name: t.name,
          })),
        ),
      )
      .catch(() => {});
  }, []);

  const fetchAll = async () => {
    setLoading(true);
    try {
      const [regRes, schedRes] = await Promise.all([
        fetch(`/api/targets/${selectedTarget}/registry`),
        fetch(`/api/targets/${selectedTarget}/schedule`),
      ]);

      if (regRes.ok) {
        const data = await regRes.json();
        setComponents(data.components);
      } else {
        setError(await regRes.text());
      }

      if (schedRes.ok) {
        const data = await schedRes.json();
        setScheduler(data.scheduler);
        setExecutive(data.executive);
      }
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setLoading(false);
  };

  useEffect(() => {
    fetchAll();
    const interval = setInterval(fetchAll, 5000);
    return () => clearInterval(interval);
    // fetchAll is intentionally not in deps -- it's defined every render
    // and including it would re-create the interval on every render. The
    // function only reads selectedTarget which IS in deps.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedTarget]);

  const formatUptime = (sec: number) => {
    const h = Math.floor(sec / 3600);
    const m = Math.floor((sec % 3600) / 60);
    const s = sec % 60;
    if (h > 0) return `${h}h ${m}m ${s}s`;
    if (m > 0) return `${m}m ${s}s`;
    return `${s}s`;
  };

  const typeColor: Record<string, string> = {
    CORE: "#6366f1",
    SW_MODEL: "#22c55e",
    SUPPORT: "#f59e0b",
    HW_MODEL: "#3b82f6",
    DRIVER: "#ef4444",
  };

  return (
    <div style={{ padding: "2rem", maxWidth: "900px", margin: "0 auto" }}>
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          marginBottom: "1.5rem",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "1rem" }}>
          <h1 style={{ fontSize: "1.8rem", fontWeight: "bold", margin: 0 }}>
            Configuration
          </h1>
          <select
            value={selectedTarget}
            onChange={(e) => setSelectedTarget(e.target.value)}
            style={{
              padding: "0.3rem 0.5rem",
              borderRadius: "4px",
              border: "1px solid var(--color-border)",
              background: "var(--color-elevated)",
              color: "var(--color-text-secondary)",
              fontSize: "0.85rem",
            }}
          >
            {targets.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </select>
        </div>
        <button
          onClick={fetchAll}
          disabled={loading}
          style={{
            padding: "0.4rem 0.8rem",
            borderRadius: "4px",
            border: "1px solid var(--color-border)",
            background: "var(--color-elevated)",
            cursor: "pointer",
            fontSize: "0.85rem",
          }}
        >
          {loading ? "Loading..." : "Refresh"}
        </button>
      </div>

      {error && (
        <div
          style={{
            background: "rgba(248,81,73,0.15)",
            border: "1px solid rgba(248,81,73,0.3)",
            padding: "1rem",
            borderRadius: "4px",
            marginBottom: "1rem",
            cursor: "pointer",
          }}
          onClick={() => setError(null)}
        >
          {error}
        </div>
      )}

      {/* Executive Overview */}
      {executive && !executive.error && (
        <div style={{ marginBottom: "1.5rem" }}>
          <h3
            style={{
              fontSize: "0.9rem",
              color: "var(--color-text-muted)",
              marginBottom: "0.5rem",
              textTransform: "uppercase",
              letterSpacing: "0.05em",
            }}
          >
            Executive
          </h3>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(5, 1fr)",
              gap: "0.5rem",
            }}
          >
            {[
              {
                label: "Uptime",
                value: formatUptime(executive.uptimeSeconds || 0),
              },
              { label: "Clock", value: `${executive.clockFreqHz} Hz` },
              {
                label: "Cycles",
                value: (executive.clockCycles || 0).toLocaleString(),
              },
              {
                label: "Overruns",
                value: String(executive.frameOverruns || 0),
                bad: (executive.frameOverruns || 0) > 0,
              },
              {
                label: "Watchdog",
                value: String(executive.watchdogWarnings || 0),
                bad: (executive.watchdogWarnings || 0) > 0,
              },
            ].map((m) => (
              <div
                key={m.label}
                style={{
                  background: "var(--color-elevated)",
                  padding: "0.5rem",
                  borderRadius: "4px",
                  fontSize: "0.85rem",
                }}
              >
                <div
                  style={{
                    color: "var(--color-text-muted)",
                    fontSize: "0.7rem",
                  }}
                >
                  {m.label}
                </div>
                <div
                  style={{
                    fontWeight: "bold",
                    fontFamily: "monospace",
                    color: "bad" in m && m.bad ? "#c00" : undefined,
                  }}
                >
                  {m.value}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Scheduler */}
      {scheduler && !scheduler.error && (
        <div style={{ marginBottom: "1.5rem" }}>
          <h3
            style={{
              fontSize: "0.9rem",
              color: "var(--color-text-muted)",
              marginBottom: "0.5rem",
              textTransform: "uppercase",
              letterSpacing: "0.05em",
            }}
          >
            Scheduler
          </h3>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(5, 1fr)",
              gap: "0.5rem",
            }}
          >
            {[
              { label: "Tasks", value: String(scheduler.taskCount || 0) },
              {
                label: "Ticks",
                value: (scheduler.tickCount || 0).toLocaleString(),
              },
              {
                label: "Frequency",
                value: `${scheduler.fundamentalFreqHz} Hz`,
              },
              { label: "Pools", value: String(scheduler.poolCount || 0) },
              {
                label: "Violations",
                value: String(scheduler.periodViolations || 0),
                bad: (scheduler.periodViolations || 0) > 0,
              },
            ].map((m) => (
              <div
                key={m.label}
                style={{
                  background: "var(--color-elevated)",
                  padding: "0.5rem",
                  borderRadius: "4px",
                  fontSize: "0.85rem",
                }}
              >
                <div
                  style={{
                    color: "var(--color-text-muted)",
                    fontSize: "0.7rem",
                  }}
                >
                  {m.label}
                </div>
                <div
                  style={{
                    fontWeight: "bold",
                    fontFamily: "monospace",
                    color: "bad" in m && m.bad ? "#c00" : undefined,
                  }}
                >
                  {m.value}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Component Registry */}
      <h3
        style={{
          fontSize: "0.9rem",
          color: "var(--color-text-muted)",
          marginBottom: "0.5rem",
          textTransform: "uppercase",
          letterSpacing: "0.05em",
        }}
      >
        Component Registry ({components.length})
      </h3>
      <div
        style={{
          border: "1px solid var(--color-border)",
          borderRadius: "8px",
          overflow: "hidden",
        }}
      >
        <table
          style={{
            width: "100%",
            borderCollapse: "collapse",
            fontSize: "0.85rem",
          }}
        >
          <thead>
            <tr
              style={{
                background: "var(--color-elevated)",
                borderBottom: "1px solid var(--color-border)",
              }}
            >
              <th style={{ textAlign: "left", padding: "0.6rem 0.8rem" }}>
                Component
              </th>
              <th style={{ textAlign: "left", padding: "0.6rem 0.8rem" }}>
                fullUid
              </th>
              <th style={{ textAlign: "left", padding: "0.6rem 0.8rem" }}>
                Type
              </th>
              <th style={{ textAlign: "center", padding: "0.6rem 0.8rem" }}>
                Status
              </th>
            </tr>
          </thead>
          <tbody>
            {components.map((c) => (
              <tr
                key={c.fullUid}
                style={{ borderBottom: "1px solid var(--color-border-muted)" }}
              >
                <td style={{ padding: "0.5rem 0.8rem", fontWeight: "bold" }}>
                  {c.name}
                </td>
                <td
                  style={{
                    padding: "0.5rem 0.8rem",
                    fontFamily: "monospace",
                    color: "var(--color-text-secondary)",
                  }}
                >
                  {c.fullUid}
                </td>
                <td style={{ padding: "0.5rem 0.8rem" }}>
                  <span
                    style={{
                      padding: "0.15rem 0.5rem",
                      borderRadius: "999px",
                      fontSize: "0.75rem",
                      fontWeight: "bold",
                      background: `${typeColor[c.type] || "#888"}20`,
                      color: typeColor[c.type] || "#888",
                    }}
                  >
                    {c.type}
                  </span>
                </td>
                <td style={{ padding: "0.5rem 0.8rem", textAlign: "center" }}>
                  <span
                    style={{
                      display: "inline-block",
                      width: "10px",
                      height: "10px",
                      borderRadius: "50%",
                      background: c.reachable ? "#22c55e" : "#ef4444",
                    }}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
