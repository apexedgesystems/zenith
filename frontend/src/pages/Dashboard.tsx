import { memo, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { deletePref, savePref, usePref } from "../api/queries";
import {
  arrangeCards,
  moveTitle,
  toggleHidden,
  type CardArrangement,
} from "../utils/cardPrefs";
import {
  decodeField,
  formatValue,
  hexToBytes,
  type FieldDef,
} from "../api/decode";
import {
  useRegistry,
  useTargetMetrics,
  type PipelineMetrics,
} from "../api/queries";

/* ----------------------------- Types ----------------------------- */

interface Target {
  id: string;
  name: string;
  host: string;
  port: number;
  connected: boolean;
  health_nonzero_bad?: string[];
}

interface RegistryComponent {
  fullUid: string;
  name: string;
  type: string;
  reachable: boolean;
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

/** Display policy from the target's config (health_nonzero_bad):
 *  fields flagged bad when nonzero. The key normalization (lowercase,
 *  underscores stripped) matches the backend's documented convention.
 *  Policy lives in per-target config, not in this file. */
function isBad(field: FieldDef, value: number, rules: Set<string>): boolean {
  const key = field.name.toLowerCase().replace(/_/g, "");
  return rules.has(key) && value > 0;
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

/** Build the executive summary and per-component health cards: one
 *  executive INSPECT, parallel per-component telemetry commands, and
 *  push-telemetry grouping for components that publish their own
 *  health. Runs as a 2 s query so it pauses when the tab is hidden --
 *  it drives real device commands and must not poll unattended. */
async function buildHealthCards(
  selectedTarget: string,
  registry: RegistryComponent[],
  tlmStructs: Map<string, TelemetryStruct>,
  badRules: Set<string>,
): Promise<{ exec: HealthCard["metrics"] | null; cards: HealthCard[] }> {
  const cards: HealthCard[] = [];
  let exec: HealthCard["metrics"] | null = null;

  // Executive summary (always first, always present -- UID 0x000000)
  try {
    const r = await fetch(`/api/targets/${selectedTarget}/health`);
    if (r.ok) {
      const data = await r.json();
      if (data.status === 0 && data.extra_hex) {
        const bytes = hexToBytes(data.extra_hex);
        const view = bytes ? new DataView(bytes.buffer) : null;
        // Find executive struct from dicts
        let execStruct: TelemetryStruct | undefined;
        for (const [key, val] of tlmStructs) {
          if (key.includes("executive")) {
            execStruct = val;
            break;
          }
        }
        if (view && execStruct) {
          const metrics: HealthCard["metrics"] = [];
          for (const field of execStruct.fields) {
            const value = decodeField(view, field);
            if (value === null) continue;
            if (typeof value === "number" && !isFinite(value)) continue;
            metrics.push({
              label: field.name,
              value: formatValue(value, field),
              bad: typeof value === "number" && isBad(field, value, badRules),
            });
          }
          exec = metrics.length > 0 ? metrics : null;
        }
      }
    }
  } catch {
    /* ignore */
  }

  // All other components with TELEMETRY structs (skip Executive, already handled).
  // Fire all health requests in parallel; the per-component results are
  // independent and the backend serializes per-target writes anyway.
  // The card order is preserved via the index from registry.
  const componentTasks = registry
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
      // Use the *local* tlmStructs (was buggy state read here).
      let tlmStruct: TelemetryStruct | undefined;
      for (const [key, val] of tlmStructs) {
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
      const bytes = hexToBytes(data.extra_hex);
      const view = bytes ? new DataView(bytes.buffer) : null;
      if (!view) return null;
      const metrics: HealthCard["metrics"] = [];
      for (const field of tlmStruct.fields) {
        const value = decodeField(view, field);
        if (value === null) continue;
        if (typeof value === "number" && !isFinite(value)) continue;
        metrics.push({
          label: field.name,
          value: formatValue(value, field),
          bad: typeof value === "number" && isBad(field, value, badRules),
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
          cards.some((c) => c.title.toLowerCase().includes(comp.toLowerCase()))
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
            bad: isBad(
              { name, type: "float", offset: 0, size: 4 },
              value,
              badRules,
            ),
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

  return { exec, cards };
}

/** Executive summary -- always-present banner for the guaranteed component. */
const ExecutiveSummary = memo(function ExecutiveSummary({
  metrics,
  pref,
  customizing,
  onPersist,
}: {
  metrics: HealthCard["metrics"];
  pref: CardArrangement | null;
  customizing: boolean;
  onPersist: (next: CardArrangement) => void;
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
        {(() => {
          const eligible = metrics
            .filter((m) => !m.label.startsWith("reserved"))
            .map((m) => ({ title: m.label, m }));
          const arranged = arrangeCards(eligible, pref, customizing);
          const shown = customizing ? arranged : arranged.slice(0, 8);
          const allTitles = eligible.map((e) => e.title);
          const hiddenSet = new Set(pref?.hidden ?? []);
          return shown.map(({ title, m }) => (
            <div
              key={m.label}
              className="rounded-md p-2"
              style={{
                backgroundColor: "var(--color-elevated)",
                opacity: customizing && hiddenSet.has(title) ? 0.35 : 1,
              }}
            >
              {customizing && (
                <div className="flex gap-1 mb-1">
                  {([-1, 1] as const).map((d) => (
                    <button
                      key={d}
                      onClick={() =>
                        onPersist({
                          hidden: pref?.hidden ?? [],
                          order: moveTitle(
                            pref?.order ?? [],
                            allTitles,
                            title,
                            d,
                          ),
                        })
                      }
                      className="text-[9px] px-1"
                      style={{
                        backgroundColor: "var(--color-surface)",
                        border: "1px solid var(--color-border)",
                        color: "var(--color-text-secondary)",
                      }}
                    >
                      {d === -1 ? "<" : ">"}
                    </button>
                  ))}
                  <button
                    onClick={() =>
                      onPersist({
                        hidden: toggleHidden(pref?.hidden ?? [], title),
                        order: pref?.order ?? [],
                      })
                    }
                    className="text-[9px] px-1"
                    style={{
                      backgroundColor: "var(--color-surface)",
                      border: "1px solid var(--color-border)",
                      color: hiddenSet.has(title)
                        ? "var(--color-ok)"
                        : "var(--color-warn)",
                    }}
                  >
                    {hiddenSet.has(title) ? "show" : "hide"}
                  </button>
                </div>
              )}
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
          ));
        })()}
      </div>
    </div>
  );
});

/** Pipeline summary -- the "are we losing data" banner. */
const PipelineSummary = memo(function PipelineSummary({
  m,
}: {
  m: PipelineMetrics;
}) {
  const compact = (n: number) => {
    if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
    if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
    return String(n);
  };
  const lagDrops = m.router_lag_drops + m.db_writer_lag_drops + m.ws_lag_drops;
  const tiles: { label: string; value: string; bad?: boolean }[] = [
    { label: "samples stored", value: compact(m.db_written_samples) },
    { label: "dedup drops", value: compact(m.dedup_drops) },
    { label: "lag drops", value: compact(lagDrops), bad: lagDrops > 0 },
    {
      label: "write failures",
      value: compact(m.db_write_failures),
      bad: m.db_write_failures > 0,
    },
    {
      label: "cmd latency",
      value:
        m.command_latency_avg_us >= 1000
          ? (m.command_latency_avg_us / 1000).toFixed(1) + "ms"
          : m.command_latency_avg_us + "us",
    },
    {
      label: "cmd timeouts",
      value: compact(m.command_timeouts),
      bad: m.command_timeouts > 0,
    },
    { label: "ws clients", value: String(m.ws_clients) },
    {
      label: "last sample",
      value:
        m.last_sample_age_ms === null
          ? "never"
          : (m.last_sample_age_ms / 1000).toFixed(1) + "s ago",
      bad: m.connected && (m.last_sample_age_ms ?? 0) > 10_000,
    },
  ];

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
          Pipeline
        </span>
      </div>
      <div className="grid grid-cols-4 gap-3">
        {tiles.map((t) => (
          <div
            key={t.label}
            className="rounded-md p-2"
            style={{ backgroundColor: "var(--color-elevated)" }}
          >
            <div
              className="text-[10px] uppercase tracking-wider mb-0.5"
              style={{ color: "var(--color-text-muted)" }}
            >
              {t.label}
            </div>
            <div
              className="mono text-sm font-bold"
              style={{
                color: t.bad
                  ? "var(--color-crit)"
                  : "var(--color-text-primary)",
              }}
            >
              {t.value}
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
  const queryClient = useQueryClient();
  const target = targets.find((t) => t.id === selectedTarget);
  const isConnected = target?.connected ?? false;

  // Pipeline counters via the shared cache (atomic-snapshot endpoint,
  // no device I/O; 2 s).
  const pipeline: PipelineMetrics | null =
    useTargetMetrics(selectedTarget).data ?? null;

  // Struct dicts are immutable for a running target process: fetch
  // once per target, in parallel, and cache until the target changes.
  const structsQuery = useQuery({
    queryKey: ["tlm-structs", selectedTarget],
    enabled: !!selectedTarget,
    staleTime: Infinity,
    queryFn: () => loadTelemetryStructs(selectedTarget),
  });
  const tlmStructs = structsQuery.data ?? new Map<string, TelemetryStruct>();

  // Component registry via the shared hook (disconnect evicts it, so
  // reconnect re-fetches fresh).
  const registryQuery = useRegistry(selectedTarget, isConnected);
  const registry = registryQuery.data ?? [];

  // Health cards: composes the executive INSPECT, parallel
  // per-component commands, and push-telemetry grouping. As a query
  // it deduplicates, cancels on target switch, and pauses when the
  // tab is hidden -- this poll drives real device commands.
  // Card arrangement preference (per target). Customize mode shows
  // hidden cards greyed with controls; normal mode applies the pref.
  const [customizing, setCustomizing] = useState(false);
  const prefScope = `target:${selectedTarget}`;
  const cardPrefQuery = usePref<CardArrangement>(
    prefScope,
    "dashboard",
    "cards",
  );
  const execPrefQuery = usePref<CardArrangement>(
    prefScope,
    "dashboard",
    "exec-tiles",
  );
  const execPref = execPrefQuery.data ?? null;
  const persistExecTiles = (next: CardArrangement) => {
    queryClient.setQueryData(
      ["pref", prefScope, "dashboard", "exec-tiles"],
      next,
    );
    savePref(prefScope, "dashboard", "exec-tiles", next).catch(() => {
      execPrefQuery.refetch();
    });
  };
  const cardPref = cardPrefQuery.data ?? null;
  const persistCards = (next: CardArrangement) => {
    queryClient.setQueryData(["pref", prefScope, "dashboard", "cards"], next);
    savePref(prefScope, "dashboard", "cards", next).catch(() => {
      cardPrefQuery.refetch();
    });
  };
  const resetCards = () => {
    queryClient.setQueryData(["pref", prefScope, "dashboard", "cards"], null);
    queryClient.setQueryData(
      ["pref", prefScope, "dashboard", "exec-tiles"],
      null,
    );
    deletePref(prefScope, "dashboard", "cards").catch(() => {
      cardPrefQuery.refetch();
    });
    deletePref(prefScope, "dashboard", "exec-tiles").catch(() => {
      execPrefQuery.refetch();
    });
  };

  const badRules = useMemo(
    () =>
      new Set<string>(
        targets.find((t) => t.id === selectedTarget)?.health_nonzero_bad ?? [],
      ),
    [targets, selectedTarget],
  );
  const healthQuery = useQuery({
    queryKey: ["health-cards", selectedTarget],
    enabled: isConnected && registry.length > 0 && tlmStructs.size > 0,
    refetchInterval: 2000,
    queryFn: () =>
      buildHealthCards(selectedTarget, registry, tlmStructs, badRules),
  });
  const execSummary = healthQuery.data?.exec ?? null;
  const healthCards = healthQuery.data?.cards ?? [];
  const [error, setError] = useState<string | null>(null);

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
    // Drop cached health/registry immediately rather than waiting for
    // the next targets poll to disable the queries.
    queryClient.removeQueries({ queryKey: ["health-cards", selectedTarget] });
    queryClient.removeQueries({ queryKey: ["registry", selectedTarget] });
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
      {isConnected && execSummary && (
        <ExecutiveSummary
          metrics={execSummary}
          pref={execPref}
          customizing={customizing}
          onPersist={persistExecTiles}
        />
      )}

      {/* Pipeline counters (present whenever the backend knows the target) */}
      {pipeline && <PipelineSummary m={pipeline} />}

      {/* Component Health Cards */}
      {isConnected && healthCards.length > 0 ? (
        <>
          <div className="flex items-center justify-end gap-2 mb-2">
            {customizing && (cardPref || execPref) && (
              <button
                onClick={resetCards}
                className="text-xs px-2 py-1"
                style={{
                  color: "var(--color-text-muted)",
                  backgroundColor: "transparent",
                  border: "1px solid var(--color-border)",
                }}
              >
                Reset to defaults
              </button>
            )}
            <button
              onClick={() => setCustomizing((c) => !c)}
              className="text-xs px-2 py-1"
              style={{
                color: customizing
                  ? "var(--color-accent)"
                  : "var(--color-text-muted)",
                backgroundColor: "transparent",
                border: "1px solid var(--color-border)",
              }}
            >
              {customizing ? "Done" : "Customize"}
            </button>
          </div>
          <div className="grid grid-cols-2 gap-3 mb-5">
            {arrangeCards(healthCards, cardPref, customizing).map((card) => {
              const isHidden = (cardPref?.hidden ?? []).includes(card.title);
              const allTitles = healthCards.map((c) => c.title);
              return (
                <div
                  key={card.title}
                  style={
                    customizing && isHidden ? { opacity: 0.35 } : undefined
                  }
                >
                  {customizing && (
                    <div className="flex items-center gap-1 mb-1">
                      <span
                        className="text-[10px] flex-1 truncate"
                        style={{ color: "var(--color-text-muted)" }}
                      >
                        {card.title}
                      </span>
                      {([-1, 1] as const).map((d) => (
                        <button
                          key={d}
                          onClick={() =>
                            persistCards({
                              hidden: cardPref?.hidden ?? [],
                              order: moveTitle(
                                cardPref?.order ?? [],
                                allTitles,
                                card.title,
                                d,
                              ),
                            })
                          }
                          className="text-[10px] px-1.5"
                          style={{
                            backgroundColor: "var(--color-elevated)",
                            border: "1px solid var(--color-border)",
                            color: "var(--color-text-secondary)",
                          }}
                        >
                          {d === -1 ? "<" : ">"}
                        </button>
                      ))}
                      <button
                        onClick={() =>
                          persistCards({
                            hidden: toggleHidden(
                              cardPref?.hidden ?? [],
                              card.title,
                            ),
                            order: cardPref?.order ?? [],
                          })
                        }
                        className="text-[10px] px-1.5"
                        style={{
                          backgroundColor: "var(--color-elevated)",
                          border: "1px solid var(--color-border)",
                          color: isHidden
                            ? "var(--color-ok)"
                            : "var(--color-warn)",
                        }}
                      >
                        {isHidden ? "Show" : "Hide"}
                      </button>
                    </div>
                  )}
                  <MetricCard card={card} />
                </div>
              );
            })}
          </div>
        </>
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
