import { memo, useCallback, useEffect, useRef, useState } from "react";

/* ----------------------------- Types ----------------------------- */

interface Target {
  id: string;
  name: string;
  host: string;
  port: number;
  connected: boolean;
}

interface RegistryComponent {
  fullUid: string;
  name: string;
  type: string;
  reachable: boolean;
}

interface FieldDef {
  name: string;
  type: string;
  offset: number;
  size: number;
}

interface TelemetryStruct {
  component: string;
  structName: string;
  opcode: number;
  size: number;
  fields: FieldDef[];
}

interface HealthCard {
  title: string;
  metrics: { label: string; value: string; bad?: boolean }[];
}

/* ----------------------------- Generic Binary Decoder ----------------------------- */

function decodeField(view: DataView, field: FieldDef): number | null {
  const off = field.offset;
  if (off + field.size > view.byteLength) return null;

  switch (`${field.type}:${field.size}`) {
    case "uint:1":
      return view.getUint8(off);
    case "uint:2":
      return view.getUint16(off, true);
    case "uint:4":
      return view.getUint32(off, true);
    case "uint:8":
      return Number(view.getBigUint64(off, true));
    case "int:1":
      return view.getInt8(off);
    case "int:2":
      return view.getInt16(off, true);
    case "int:4":
      return view.getInt32(off, true);
    case "float:4":
      return view.getFloat32(off, true);
    case "float:8":
      return view.getFloat64(off, true);
    case "bool:1":
      return view.getUint8(off);
    default:
      return null;
  }
}

function decodeHex(hex: string): DataView | null {
  if (!hex || hex.length < 2) return null;
  const bytes = new Uint8Array(hex.match(/.{2}/g)!.map((b) => parseInt(b, 16)));
  return new DataView(bytes.buffer);
}

function formatValue(value: number, field: FieldDef): string {
  if (field.type === "float") return value.toFixed(2);
  if (value > 100000) return value.toLocaleString();
  return String(value);
}

/** Heuristic: fields that indicate problems when nonzero */
const BAD_WHEN_NONZERO = new Set([
  "overruns",
  "frameoverruns",
  "watchdogwarnings",
  "watchdogwarns",
  "totalperiodviolations",
  "violationsthistick",
  "totalskipcount",
  "packetsinvalid",
  "framingerrors",
  "cmdqueueoverflows",
  "tlmqueueoverflows",
  "internalcommandsfailed",
  "warncount",
  "critcount",
]);

function isBad(field: FieldDef, value: number): boolean {
  const key = field.name.toLowerCase().replace(/_/g, "");
  return BAD_WHEN_NONZERO.has(key) && value > 0;
}

/* ----------------------------- Struct Dict Loader ----------------------------- */

/** Load TELEMETRY structs for the components on a specific target.
 *  Returns a map: component_name_lowercase -> TelemetryStruct.
 *
 *  Struct dicts are loaded PER TARGET on the backend. The previous
 *  implementation hit `/api/structs` (the global fallback dict, usually
 *  empty), so the dashboard widgets silently never decoded anything
 *  -- explaining why the dashboard rendered the registry table fast
 *  and the health cards never. Use the per-target endpoints added in
 *  auth middleware integration. */
async function loadTelemetryStructs(
  targetId: string,
): Promise<Map<string, TelemetryStruct>> {
  const map = new Map<string, TelemetryStruct>();
  if (!targetId) return map;
  try {
    const r = await fetch(`/api/targets/${targetId}/structs`);
    if (!r.ok) return map;
    const data = await r.json();
    for (const comp of data.components || []) {
      for (const s of comp.structs || []) {
        if (s.category !== "TELEMETRY" || !s.opcode || s.fieldCount === 0)
          continue;
        // Skip generic system opcodes (0x0080, 0x0081) -- only component-specific health
        const opcode = parseInt(s.opcode.replace("0x", ""), 16);
        if (opcode < 0x0100) continue;

        // Fetch full struct def for field details from the per-target dict
        const dr = await fetch(
          `/api/targets/${targetId}/structs/${encodeURIComponent(
            comp.component,
          )}`,
        );
        if (!dr.ok) continue;
        const dict = await dr.json();
        const sdef = dict.structs?.[s.name];
        if (!sdef?.fields) continue;

        map.set(comp.component.toLowerCase(), {
          component: comp.component,
          structName: s.name,
          opcode,
          size: s.size,
          fields: sdef.fields.filter(
            (f: FieldDef) =>
              f.size > 0 &&
              f.type !== "array" &&
              f.type !== "string" &&
              !f.name.startsWith("pad") &&
              !f.name.startsWith("reserved"),
          ),
        });
        break; // one TELEMETRY struct per component is enough
      }
    }
  } catch {
    /* ignore */
  }
  return map;
}

/* ----------------------------- Metric Card ----------------------------- */

const MetricCard = memo(function MetricCard({ card }: { card: HealthCard }) {
  // Count bad metrics for card border color
  const hasBad = card.metrics.some((m) => m.bad);
  return (
    <div
      className="rounded-lg p-3"
      style={{
        backgroundColor: "var(--color-surface)",
        border: `1px solid ${
          hasBad ? "var(--color-crit)" : "var(--color-border)"
        }`,
        borderLeft: hasBad
          ? "3px solid var(--color-crit)"
          : "3px solid var(--color-border)",
      }}
    >
      <div
        className="text-xs uppercase tracking-wider mb-2 font-bold"
        style={{ color: "var(--color-text-secondary)" }}
      >
        {card.title}
      </div>
      <div className="grid grid-cols-2 gap-x-4 gap-y-1.5">
        {card.metrics
          .filter((m) => !m.label.startsWith("reserved"))
          .map((m) => (
            <div key={m.label}>
              <div
                className="text-[10px]"
                style={{ color: "var(--color-text-muted)" }}
              >
                {m.label}
              </div>
              <div
                className="mono text-xs font-bold"
                style={{
                  color: m.bad
                    ? "var(--color-crit)"
                    : "var(--color-text-primary)",
                }}
              >
                {m.value}
              </div>
            </div>
          ))}
      </div>
    </div>
  );
});

/* ----------------------------- Dashboard Page ----------------------------- */

/** Executive summary -- always-present banner for the guaranteed component. */
const ExecutiveSummary = memo(function ExecutiveSummary({
  metrics,
}: {
  metrics: HealthCard["metrics"];
}) {
  // Format large cycle counts with compact notation
  const formatMetric = (m: { label: string; value: string }) => {
    if (
      m.label.toLowerCase().includes("cycle") ||
      m.label.toLowerCase().includes("count")
    ) {
      const n = parseInt(m.value.replace(/,/g, ""));
      if (!isNaN(n) && n >= 1000000) return (n / 1000000).toFixed(1) + "M";
      if (!isNaN(n) && n >= 1000) return (n / 1000).toFixed(1) + "K";
    }
    return m.value;
  };

  return (
    <div
      className="rounded-lg p-4 mb-4"
      style={{
        backgroundColor: "var(--color-surface)",
        border: "1px solid var(--color-border)",
      }}
    >
      <div className="flex items-center gap-2 mb-3">
        <div
          className="w-2 h-2 rounded-full"
          style={{ backgroundColor: "var(--color-accent)" }}
        />
        <span
          className="text-xs uppercase tracking-widest font-bold"
          style={{ color: "var(--color-accent)" }}
        >
          Executive
        </span>
      </div>
      <div className="grid grid-cols-4 gap-3">
        {metrics
          .filter((m) => !m.label.startsWith("reserved"))
          .slice(0, 8)
          .map((m) => (
            <div
              key={m.label}
              className="rounded-md p-2"
              style={{ backgroundColor: "var(--color-elevated)" }}
            >
              <div
                className="text-[10px] uppercase tracking-wider mb-0.5"
                style={{ color: "var(--color-text-muted)" }}
              >
                {m.label}
              </div>
              <div
                className="mono text-sm font-bold"
                style={{
                  color: m.bad
                    ? "var(--color-crit)"
                    : "var(--color-text-primary)",
                }}
              >
                {formatMetric(m)}
              </div>
            </div>
          ))}
      </div>
    </div>
  );
});

export default function DashboardPage({
  selectedTarget,
  targets,
}: {
  selectedTarget: string;
  targets: Target[];
}) {
  const [execSummary, setExecSummary] = useState<HealthCard["metrics"] | null>(
    null,
  );
  const [healthCards, setHealthCards] = useState<HealthCard[]>([]);
  const [registry, setRegistry] = useState<RegistryComponent[]>([]);
  const [tlmStructs, setTlmStructs] = useState<Map<string, TelemetryStruct>>(
    new Map(),
  );
  const [error, setError] = useState<string | null>(null);

  const target = targets.find((t) => t.id === selectedTarget);
  const isConnected = target?.connected ?? false;

  // Stable refs for polling callback (avoids interval reset on every state change)
  const registryRef = useRef(registry);
  registryRef.current = registry;
  const tlmStructsRef = useRef(tlmStructs);
  tlmStructsRef.current = tlmStructs;

  // Load struct dicts for the currently-selected target. Reloads when
  // the target changes -- different targets can have different dicts.
  useEffect(() => {
    if (!selectedTarget) return;
    loadTelemetryStructs(selectedTarget).then(setTlmStructs);
  }, [selectedTarget]);

  // Fetch component registry
  useEffect(() => {
    if (!selectedTarget || !isConnected) {
      setRegistry([]);
      return;
    }
    fetch(`/api/targets/${selectedTarget}/registry`)
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        if (data?.components) setRegistry(data.components);
      })
      .catch(() => {});
  }, [selectedTarget, isConnected]);

  // Poll health data every 2s -- generic, driven by struct dicts
  const fetchHealth = useCallback(async () => {
    const currentRegistry = registryRef.current;
    const currentTlmStructs = tlmStructsRef.current;
    if (!selectedTarget || !isConnected || currentRegistry.length === 0) return;
    const cards: HealthCard[] = [];

    // Executive summary (always first, always present -- UID 0x000000)
    try {
      const r = await fetch(`/api/targets/${selectedTarget}/health`);
      if (r.ok) {
        const data = await r.json();
        if (data.status === 0 && data.extra_hex) {
          const view = decodeHex(data.extra_hex);
          // Find executive struct from dicts
          let execStruct: TelemetryStruct | undefined;
          for (const [key, val] of currentTlmStructs) {
            if (key.includes("executive")) {
              execStruct = val;
              break;
            }
          }
          if (view && execStruct) {
            const metrics: HealthCard["metrics"] = [];
            for (const field of execStruct.fields) {
              const value = decodeField(view, field);
              if (value === null || !isFinite(value)) continue;
              metrics.push({
                label: field.name,
                value: formatValue(value, field),
                bad: isBad(field, value),
              });
            }
            setExecSummary(metrics.length > 0 ? metrics : null);
          }
        }
      }
    } catch {
      /* ignore */
    }

    // All other components with TELEMETRY structs (skip Executive, already handled).
    // Fire all health requests in parallel; the per-component results are
    // independent and the backend serializes per-target writes anyway.
    // The card order is preserved via the index from currentRegistry.
    const componentTasks = currentRegistry
      .map((comp, idx) => ({ comp, idx }))
      .filter(({ comp }) => {
        if (!comp.reachable) return false;
        const uid = parseInt(comp.fullUid.replace("0x", ""), 16);
        return uid !== 0x000000;
      })
      .map(({ comp, idx }) => {
        const uid = parseInt(comp.fullUid.replace("0x", ""), 16);
        const baseName = comp.name
          .split(" #")[0]
          .split("#")[0]
          .trim()
          .toLowerCase();
        // Use the *local* currentTlmStructs (was buggy state read here).
        let tlmStruct: TelemetryStruct | undefined;
        for (const [key, val] of currentTlmStructs) {
          if (
            key === baseName ||
            key.includes(baseName) ||
            baseName.includes(key)
          ) {
            tlmStruct = val;
            break;
          }
        }
        return { comp, idx, uid, tlmStruct };
      })
      .filter((t) => t.tlmStruct !== undefined) as {
      comp: RegistryComponent;
      idx: number;
      uid: number;
      tlmStruct: TelemetryStruct;
    }[];

    const results = await Promise.allSettled(
      componentTasks.map(async ({ comp, uid, tlmStruct }) => {
        const r = await fetch(`/api/targets/${selectedTarget}/command`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ full_uid: uid, opcode: tlmStruct.opcode }),
        });
        if (!r.ok) return null;
        const data = await r.json();
        if (data.status !== 0 || !data.extra_hex) return null;
        const view = decodeHex(data.extra_hex);
        if (!view) return null;
        const metrics: HealthCard["metrics"] = [];
        for (const field of tlmStruct.fields) {
          const value = decodeField(view, field);
          if (value === null || !isFinite(value)) continue;
          metrics.push({
            label: field.name,
            value: formatValue(value, field),
            bad: isBad(field, value),
          });
        }
        return metrics.length > 0 ? { title: comp.name, metrics } : null;
      }),
    );

    for (const r of results) {
      if (r.status === "fulfilled" && r.value) cards.push(r.value);
    }

    // Push-telemetry components (e.g. SystemMonitor pushes its own health)
    try {
      const r = await fetch(`/api/targets/${selectedTarget}/telemetry/latest`);
      if (r.ok) {
        const data = await r.json();
        const channels = (data.channels || []) as {
          channel: string;
          value: number;
        }[];

        // Group by component prefix, build cards for any that aren't already covered
        const groups = new Map<string, Map<string, number>>();
        for (const ch of channels) {
          const dot = ch.channel.indexOf(".");
          if (dot < 0) continue;
          const comp = ch.channel.slice(0, dot);
          const field = ch.channel.slice(dot + 1);
          if (!groups.has(comp)) groups.set(comp, new Map());
          groups.get(comp)!.set(field, ch.value);
        }

        for (const [comp, fields] of groups) {
          // Skip if a card already exists from command-based health
          if (
            cards.some((c) =>
              c.title.toLowerCase().includes(comp.toLowerCase()),
            )
          )
            continue;

          const metrics: HealthCard["metrics"] = [];
          for (const [name, value] of fields) {
            if (
              name.startsWith("field") ||
              name.startsWith("pad") ||
              name.startsWith("reserved")
            )
              continue;
            metrics.push({
              label: name,
              value:
                typeof value === "number" && !Number.isInteger(value)
                  ? value.toFixed(1)
                  : String(Math.round(value)),
              bad: isBad({ name, type: "float", offset: 0, size: 4 }, value),
            });
          }
          if (metrics.length > 0) {
            cards.push({ title: comp, metrics });
          }
        }
      }
    } catch {
      /* ignore */
    }

    setHealthCards(cards);
  }, [selectedTarget, isConnected]); // registry + tlmStructs accessed via refs

  useEffect(() => {
    fetchHealth();
    const interval = setInterval(fetchHealth, 2000);
    return () => clearInterval(interval);
  }, [fetchHealth]);

  // Connect / disconnect
  const connect = async () => {
    try {
      const r = await fetch(`/api/targets/${selectedTarget}/connect`, {
        method: "POST",
      });
      if (!r.ok) setError(await r.text());
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const disconnect = async () => {
    await fetch(`/api/targets/${selectedTarget}/disconnect`, {
      method: "POST",
    });
    setExecSummary(null);
    setHealthCards([]);
    setRegistry([]);
  };

  if (!target) {
    return (
      <div
        className="flex items-center justify-center h-full"
        style={{ color: "var(--color-text-muted)" }}
      >
        Select a target from the sidebar
      </div>
    );
  }

  return (
    <div className="max-w-4xl">
      {/* Header */}
      <div className="flex items-center justify-between mb-5">
        <div>
          <h1 className="text-xl font-bold">{target.name}</h1>
          <div
            className="mono text-xs"
            style={{ color: "var(--color-text-muted)" }}
          >
            {target.host}:{target.port}
          </div>
        </div>
        <div className="flex items-center gap-3">
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
          {isConnected ? (
            <button
              onClick={disconnect}
              className="text-xs px-3 py-1"
              style={{
                backgroundColor: "var(--color-elevated)",
                color: "var(--color-text-secondary)",
                border: "1px solid var(--color-border)",
              }}
            >
              Disconnect
            </button>
          ) : (
            <button
              onClick={connect}
              className="text-xs px-3 py-1 text-white"
              style={{ backgroundColor: "var(--color-ok)" }}
            >
              Connect
            </button>
          )}
        </div>
      </div>

      {error && (
        <div
          className="rounded-lg p-3 mb-4 text-sm cursor-pointer"
          style={{
            backgroundColor: "rgba(248,81,73,0.1)",
            color: "var(--color-crit)",
            border: "1px solid rgba(248,81,73,0.3)",
          }}
          onClick={() => setError(null)}
        >
          {error}
        </div>
      )}

      {/* Executive Summary (always present when connected) */}
      {isConnected && execSummary && <ExecutiveSummary metrics={execSummary} />}

      {/* Component Health Cards */}
      {isConnected && healthCards.length > 0 ? (
        <div className="grid grid-cols-2 gap-3 mb-5">
          {healthCards.map((card) => (
            <MetricCard key={card.title} card={card} />
          ))}
        </div>
      ) : !isConnected ? (
        <div
          className="rounded-lg p-8 mb-5 text-center text-sm"
          style={{
            border: "1px dashed var(--color-border)",
            color: "var(--color-text-muted)",
          }}
        >
          Connect to view health data
        </div>
      ) : null}

      {/* Component Registry */}
      {registry.length > 0 && (
        <div>
          <div
            className="text-xs uppercase tracking-wider mb-2"
            style={{ color: "var(--color-text-muted)" }}
          >
            Components ({registry.length})
          </div>
          <div
            className="rounded-lg overflow-hidden"
            style={{ border: "1px solid var(--color-border)" }}
          >
            <table
              style={{
                width: "100%",
                borderCollapse: "collapse",
                fontSize: "0.8rem",
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
                  <th style={{ textAlign: "left", padding: "0.5rem 0.8rem" }}>
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
                  return (
                    <tr
                      key={c.fullUid}
                      style={{
                        borderBottom: "1px solid var(--color-border-muted)",
                      }}
                    >
                      <td
                        className="font-bold"
                        style={{ padding: "0.4rem 0.8rem" }}
                      >
                        {c.name}
                      </td>
                      <td
                        className="mono"
                        style={{
                          padding: "0.4rem 0.8rem",
                          color: "var(--color-text-muted)",
                          fontSize: "0.75rem",
                        }}
                      >
                        {c.fullUid}
                      </td>
                      <td style={{ padding: "0.4rem 0.8rem" }}>
                        <span
                          style={{
                            padding: "0.15rem 0.5rem",
                            borderRadius: "999px",
                            fontSize: "0.7rem",
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
                          className="w-2.5 h-2.5 rounded-full mx-auto"
                          style={{
                            backgroundColor: c.reachable
                              ? "var(--color-ok)"
                              : "var(--color-crit)",
                          }}
                        />
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}
