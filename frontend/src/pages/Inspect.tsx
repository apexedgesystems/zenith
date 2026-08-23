import { useCallback, useEffect, useMemo, useState } from "react";
import { useTargets } from "../api/queries";
import {
  decodeField,
  formatValue,
  hexToBytes as rawHexToBytes,
  type FieldDef,
} from "../api/decode";

/** Whitespace-tolerant hex (the INSPECT browser displays spaced hex
 *  and re-parses it); empty result on malformed input. */
function sharedHexToBytes(hex: string): Uint8Array {
  return rawHexToBytes(hex.replace(/\s+/g, "")) ?? new Uint8Array(0);
}

/**
 * INSPECT Browser (Phase 4 of the MVP roadmap).
 *
 * View any registered data block (STATIC_PARAM, TUNABLE_PARAM, STATE,
 * INPUT, OUTPUT) for any component, decoded via struct dictionary.
 * Auto-refresh option for live state debugging.
 *
 * Reuses the existing /api/targets/{id}/inspect/{uid}?category=&offset=&length=
 * endpoint that was already wired up.
 */

/* ----------------------------- Types ----------------------------- */

interface RegistryComponent {
  fullUid: string;
  name: string;
  type: string;
  reachable: boolean;
}

interface StructDef {
  category: string;
  size: number;
  fields: FieldDef[];
}

interface ComponentDict {
  component: string;
  structs: Record<string, StructDef>;
}

const CATEGORIES = [
  { id: 0, label: "STATIC_PARAM", desc: "Read-only constants" },
  { id: 1, label: "TUNABLE_PARAM", desc: "Runtime parameters" },
  { id: 2, label: "STATE", desc: "Internal component state" },
  { id: 3, label: "INPUT", desc: "External data fed to component" },
  { id: 4, label: "OUTPUT", desc: "Data produced by component" },
] as const;

/* ----------------------------- Decoding ----------------------------- */

function decodeFieldValue(view: DataView, field: FieldDef): string {
  // Shared decoder; unknown shapes fall back to spaced raw hex, which
  // is what the INSPECT browser historically showed for them.
  const v = decodeField(view, field);
  if (v === null) {
    const off = field.offset;
    if (off >= view.byteLength) return "--";
    const bytes = new Uint8Array(
      view.buffer,
      view.byteOffset + off,
      Math.min(field.size, view.byteLength - off),
    );
    return Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join(" ");
  }
  if (field.type === "bool") return v ? "true" : "false";
  return formatValue(v, field);
}

/* ----------------------------- Page ----------------------------- */

export default function InspectPage({
  selectedTarget,
}: {
  selectedTarget: string;
}) {
  const [registry, setRegistry] = useState<RegistryComponent[]>([]);
  const [connected, setConnected] = useState(false);
  const [selectedUid, setSelectedUid] = useState("");
  const [selectedCategory, setSelectedCategory] = useState(2); // STATE default
  const [dict, setDict] = useState<ComponentDict | null>(null);
  // ALL component dicts for this target. Loaded once per target. We need
  // the full set because the registry's display names ("Executive",
  // "Interface", "Action") don't match the dict component names
  // ("ApexExecutive", "ApexInterface", "ActionComponent"). Fuzzy match
  // is the only reliable join.
  const [allDictNames, setAllDictNames] = useState<string[]>([]);
  const [decoded, setDecoded] = useState<{ field: FieldDef; value: string }[]>(
    [],
  );
  const [rawHex, setRawHex] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [autoRefresh, setAutoRefresh] = useState(false);
  const [loading, setLoading] = useState(false);
  const [lastFetchTs, setLastFetchTs] = useState<number | null>(null);

  /* ---- Connection state ---- */

  // Connection state from the app-wide targets cache: one poller for
  // the whole app instead of a private copy with its own divergent
  // notion of "connected".
  const targetsData = useTargets().data;
  useEffect(() => {
    const t = (targetsData ?? []).find((x) => x.id === selectedTarget);
    setConnected(!!t?.connected);
  }, [targetsData, selectedTarget]);

  /* ---- Registry ---- */

  useEffect(() => {
    if (!connected) {
      setRegistry([]);
      return;
    }
    fetch(`/api/targets/${selectedTarget}/registry`)
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        if (data?.components) {
          const reachable = (data.components as RegistryComponent[]).filter(
            (c) => c.reachable,
          );
          setRegistry(reachable);
          if (reachable.length > 0 && !selectedUid) {
            setSelectedUid(reachable[0].fullUid.replace("0x", ""));
          }
        }
      })
      .catch(() => {});
    // selectedUid intentionally not in deps -- only set on
    // ONCE on initial load (when it's empty). Including it would re-fetch
    // the registry every time the user picks a different component.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedTarget, connected]);

  /* ---- Load full per-target dict listing on target change ---- */

  useEffect(() => {
    if (!selectedTarget) {
      setAllDictNames([]);
      return;
    }
    fetch(`/api/targets/${selectedTarget}/structs`)
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        if (!data?.components) {
          setAllDictNames([]);
          return;
        }
        setAllDictNames(
          data.components.map((c: { component: string }) => c.component),
        );
      })
      .catch(() => setAllDictNames([]));
  }, [selectedTarget]);

  /* ---- Load dict for selected component ---- */

  const selectedComponent = useMemo(
    () => registry.find((c) => c.fullUid.replace("0x", "") === selectedUid),
    [registry, selectedUid],
  );

  useEffect(() => {
    if (!selectedComponent || allDictNames.length === 0) {
      setDict(null);
      return;
    }
    // The registry uses display names ("Executive", "Interface", "Action")
    // while the struct dict uses concrete class names ("ApexExecutive",
    // "ApexInterface", "ActionComponent"). Fuzzy match against the
    // pre-loaded dict listing: try exact match first, then "contains"
    // either direction, then strip common prefixes.
    const display = selectedComponent.name.split("#")[0].split(" ")[0].trim();
    const lower = display.toLowerCase();
    const dictName =
      // 1. exact case-insensitive match
      allDictNames.find((d) => d.toLowerCase() === lower) ||
      // 2. dict name CONTAINS display ("ApexExecutive" contains "Executive")
      allDictNames.find((d) => d.toLowerCase().includes(lower)) ||
      // 3. display CONTAINS dict ("WaveGenerator#0" base "WaveGenerator" matches "WaveGenerator")
      allDictNames.find((d) => lower.includes(d.toLowerCase()));

    if (!dictName) {
      setDict(null);
      return;
    }
    fetch(
      `/api/targets/${selectedTarget}/structs/${encodeURIComponent(dictName)}`,
    )
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        if (data?.component) setDict(data);
        else setDict(null);
      })
      .catch(() => setDict(null));
  }, [selectedTarget, selectedComponent, allDictNames]);

  /* ---- Find ALL matching structs for category ---- */

  // A component can have multiple structs in the same category
  // (e.g. ApexExecutive has ExecutiveTunableParams (48B) and
  // ExecutiveThreadConfigTprm (66B), both TUNABLE_PARAM). We need
  // to expose them all so the user can pick which one to inspect
  // -- silently grabbing the first one was a real bug.
  const candidateStructs = useMemo(() => {
    if (!dict) return [];
    const wantCategory =
      CATEGORIES.find((c) => c.id === selectedCategory)?.label || "";
    const out: { name: string; size: number; fields: FieldDef[] }[] = [];
    for (const [name, sdef] of Object.entries(dict.structs || {})) {
      if (
        sdef.category === wantCategory &&
        sdef.fields?.length > 0 &&
        sdef.size > 0
      ) {
        out.push({ name, size: sdef.size, fields: sdef.fields });
      }
    }
    // Sort by size for stable order
    out.sort((a, b) => a.size - b.size);
    return out;
  }, [dict, selectedCategory]);

  // The currently picked struct. Defaults to the first candidate.
  // User can override via the new struct picker.
  const [structPick, setStructPick] = useState<string>("");
  useEffect(() => {
    // Auto-pick first candidate when the candidate list changes
    if (
      candidateStructs.length > 0 &&
      !candidateStructs.find((s) => s.name === structPick)
    ) {
      setStructPick(candidateStructs[0].name);
    } else if (candidateStructs.length === 0 && structPick !== "") {
      setStructPick("");
    }
  }, [candidateStructs, structPick]);

  const matchingStruct = useMemo(
    () =>
      candidateStructs.find((s) => s.name === structPick) ||
      candidateStructs[0] ||
      null,
    [candidateStructs, structPick],
  );

  /* ---- Run INSPECT ---- */

  const runInspect = useCallback(async () => {
    if (!selectedUid || !matchingStruct) return;
    setLoading(true);
    setError(null);
    try {
      const url = `/api/targets/${selectedTarget}/inspect/${selectedUid}?category=${selectedCategory}&offset=0&length=${matchingStruct.size}`;
      const r = await fetch(url);
      if (!r.ok) {
        const text = await r.text();
        setError(text || `HTTP ${r.status}`);
        setDecoded([]);
        setRawHex("");
        return;
      }
      const data = await r.json();
      if (data.status !== 0) {
        setError(`${data.status_name} (status ${data.status})`);
        setDecoded([]);
        setRawHex("");
        return;
      }
      const hex = data.extra_hex || "";
      setRawHex(hex);
      const bytes = sharedHexToBytes(hex);
      const view = new DataView(bytes.buffer);
      const rows: { field: FieldDef; value: string }[] = [];
      for (const field of matchingStruct.fields) {
        if (field.size === 0) continue;
        if (field.name.startsWith("pad") || field.name.startsWith("reserved"))
          continue;
        rows.push({ field, value: decodeFieldValue(view, field) });
      }
      setDecoded(rows);
      setLastFetchTs(Date.now());
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
      setDecoded([]);
    } finally {
      setLoading(false);
    }
  }, [selectedTarget, selectedUid, selectedCategory, matchingStruct]);

  /* ---- Reset on component/category change ---- */

  useEffect(() => {
    setDecoded([]);
    setRawHex("");
    setError(null);
    setLastFetchTs(null);
    if (matchingStruct) runInspect();
    // runInspect is intentionally not in deps -- it depends on
    // matchingStruct which IS in deps (via .name). Including the
    // function would cause double-fires whenever its deps change
    // because the closure identity flips on every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedUid, selectedCategory, matchingStruct?.name]);

  /* ---- Auto-refresh ---- */

  useEffect(() => {
    if (!autoRefresh) return;
    const interval = setInterval(runInspect, 1000);
    return () => clearInterval(interval);
  }, [autoRefresh, runInspect]);

  /* ---- Render ---- */

  if (!connected) {
    return (
      <div className="max-w-3xl">
        <h1 className="text-xl font-bold mb-4">INSPECT Browser</h1>
        <div
          className="rounded-lg p-8 text-center text-sm"
          style={{
            border: "1px dashed var(--color-border)",
            color: "var(--color-text-muted)",
          }}
        >
          Target not connected.
        </div>
      </div>
    );
  }

  return (
    <div className="max-w-5xl">
      <div className="flex items-center justify-between mb-4">
        <h1 className="text-xl font-bold">INSPECT Browser</h1>
        <div className="text-xs" style={{ color: "var(--color-text-muted)" }}>
          {registry.length} reachable components
        </div>
      </div>

      {/* Controls */}
      <div
        className="rounded-lg p-3 mb-4 flex items-center gap-3 flex-wrap"
        style={{
          backgroundColor: "var(--color-surface)",
          border: "1px solid var(--color-border)",
        }}
      >
        <div className="flex flex-col">
          <label
            className="text-[10px] uppercase tracking-wider mb-1"
            style={{ color: "var(--color-text-muted)" }}
          >
            Component
          </label>
          <select
            value={selectedUid}
            onChange={(e) => setSelectedUid(e.target.value)}
            className="text-xs px-2 py-1"
            style={{ minWidth: "200px" }}
          >
            {registry.map((c) => (
              <option key={c.fullUid} value={c.fullUid.replace("0x", "")}>
                {c.name} ({c.fullUid})
              </option>
            ))}
          </select>
        </div>

        <div className="flex flex-col">
          <label
            className="text-[10px] uppercase tracking-wider mb-1"
            style={{ color: "var(--color-text-muted)" }}
          >
            Category
          </label>
          <div className="flex gap-1">
            {CATEGORIES.map((cat) => (
              <button
                key={cat.id}
                onClick={() => setSelectedCategory(cat.id)}
                title={cat.desc}
                className="text-[10px] px-2 py-1"
                style={{
                  backgroundColor:
                    selectedCategory === cat.id
                      ? "var(--color-accent)"
                      : "var(--color-elevated)",
                  color:
                    selectedCategory === cat.id
                      ? "#fff"
                      : "var(--color-text-muted)",
                  border: "1px solid var(--color-border)",
                  borderRadius: "3px",
                  cursor: "pointer",
                }}
              >
                {cat.label}
              </button>
            ))}
          </div>
        </div>

        {/* Struct picker -- only shown when there are multiple structs
            for the selected component+category. Common case (1 struct)
            stays a no-op visually. */}
        {candidateStructs.length > 1 && (
          <div className="flex flex-col">
            <label
              className="text-[10px] uppercase tracking-wider mb-1"
              style={{ color: "var(--color-text-muted)" }}
            >
              Struct ({candidateStructs.length} options)
            </label>
            <select
              value={structPick}
              onChange={(e) => setStructPick(e.target.value)}
              className="mono text-xs px-2 py-1"
            >
              {candidateStructs.map((s) => (
                <option key={s.name} value={s.name}>
                  {s.name} ({s.size}B)
                </option>
              ))}
            </select>
          </div>
        )}

        <div className="flex flex-col">
          <label
            className="text-[10px] uppercase tracking-wider mb-1"
            style={{ color: "var(--color-text-muted)" }}
          >
            Actions
          </label>
          <div className="flex gap-1">
            <button
              onClick={runInspect}
              disabled={!matchingStruct || loading}
              className="text-xs px-3 py-1"
              style={{
                backgroundColor: "var(--color-accent)",
                color: "#fff",
                border: "none",
                borderRadius: "4px",
                cursor: matchingStruct && !loading ? "pointer" : "not-allowed",
                opacity: matchingStruct && !loading ? 1 : 0.5,
              }}
            >
              {loading ? "..." : "Refresh"}
            </button>
            <label
              className="text-xs flex items-center gap-1 px-2"
              style={{ color: "var(--color-text-secondary)" }}
            >
              <input
                type="checkbox"
                checked={autoRefresh}
                onChange={(e) => setAutoRefresh(e.target.checked)}
              />
              Auto (1s)
            </label>
          </div>
        </div>

        {lastFetchTs && (
          <div
            className="ml-auto text-[10px] mono"
            style={{ color: "var(--color-text-muted)" }}
          >
            Last:{" "}
            {new Date(lastFetchTs).toISOString().split("T")[1].split(".")[0]}
          </div>
        )}
      </div>

      {/* Error */}
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

      {/* No struct case */}
      {!matchingStruct && selectedComponent && (
        <div
          className="rounded-lg p-6 text-center text-sm"
          style={{
            border: "1px dashed var(--color-border)",
            color: "var(--color-text-muted)",
          }}
        >
          No{" "}
          <span className="mono">
            {CATEGORIES.find((c) => c.id === selectedCategory)?.label}
          </span>{" "}
          struct registered for{" "}
          <span className="mono">{selectedComponent.name}</span>.
        </div>
      )}

      {/* Decoded fields */}
      {decoded.length > 0 && matchingStruct && (
        <div
          className="rounded-lg overflow-hidden mb-4"
          style={{ border: "1px solid var(--color-border)" }}
        >
          <div
            className="px-3 py-2 flex items-center justify-between"
            style={{
              backgroundColor: "var(--color-elevated)",
              borderBottom: "1px solid var(--color-border)",
            }}
          >
            <div>
              <span className="text-xs font-bold">{matchingStruct.name}</span>
              <span
                className="text-[10px] mono ml-2"
                style={{ color: "var(--color-text-muted)" }}
              >
                {matchingStruct.size} bytes, {decoded.length} fields
              </span>
            </div>
          </div>
          <table
            style={{
              width: "100%",
              borderCollapse: "collapse",
              fontSize: "0.78rem",
            }}
          >
            <thead>
              <tr
                style={{ borderBottom: "1px solid var(--color-border-muted)" }}
              >
                <th
                  style={{
                    textAlign: "left",
                    padding: "0.4rem 0.8rem",
                    width: "60px",
                  }}
                >
                  Offset
                </th>
                <th style={{ textAlign: "left", padding: "0.4rem 0.8rem" }}>
                  Field
                </th>
                <th
                  style={{
                    textAlign: "left",
                    padding: "0.4rem 0.8rem",
                    width: "120px",
                  }}
                >
                  Type
                </th>
                <th style={{ textAlign: "right", padding: "0.4rem 0.8rem" }}>
                  Value
                </th>
              </tr>
            </thead>
            <tbody>
              {decoded.map(({ field, value }) => (
                <tr
                  key={`${field.offset}-${field.name}`}
                  style={{
                    borderBottom: "1px solid var(--color-border-muted)",
                  }}
                >
                  <td
                    className="mono"
                    style={{
                      padding: "0.3rem 0.8rem",
                      color: "var(--color-text-muted)",
                      fontSize: "0.7rem",
                    }}
                  >
                    +{field.offset}
                  </td>
                  <td
                    className="font-bold"
                    style={{ padding: "0.3rem 0.8rem" }}
                  >
                    {field.name}
                  </td>
                  <td
                    className="mono"
                    style={{
                      padding: "0.3rem 0.8rem",
                      color: "var(--color-text-muted)",
                      fontSize: "0.7rem",
                    }}
                  >
                    {field.type}:{field.size}
                  </td>
                  <td
                    className="mono"
                    style={{
                      padding: "0.3rem 0.8rem",
                      textAlign: "right",
                      color: "var(--color-text-primary)",
                    }}
                  >
                    {value}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* Raw hex (collapsible) */}
      {rawHex && (
        <details
          className="rounded-lg overflow-hidden"
          style={{ border: "1px solid var(--color-border)" }}
        >
          <summary
            className="px-3 py-2 text-xs cursor-pointer"
            style={{
              backgroundColor: "var(--color-elevated)",
              color: "var(--color-text-secondary)",
            }}
          >
            Raw hex ({rawHex.length / 2} bytes)
          </summary>
          <div
            className="p-3 mono text-[10px]"
            style={{
              color: "var(--color-text-muted)",
              wordBreak: "break-all",
              backgroundColor: "var(--color-surface)",
            }}
          >
            {rawHex}
          </div>
        </details>
      )}
    </div>
  );
}
