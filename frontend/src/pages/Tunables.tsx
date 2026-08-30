import { useEffect, useState } from "react";
import { useRegistry, useTunableFields } from "../api/queries";

/* ----------------------------- Types ----------------------------- */

interface FieldInfo {
  name: string;
  type: string;
  offset: number;
  size: number;
}

interface FlatParamsData {
  fullUid: string;
  component: string;
  struct_name: string;
  fields: Record<string, number | string>;
  raw_hex: string;
  raw_length: number;
  variable_length?: false;
}

interface VarLenParamsData {
  fullUid: string;
  component: string;
  struct_name: string;
  fields: {
    header: Record<string, number | string>;
    entries: Record<string, number | string>[];
  };
  raw_hex: string;
  raw_length: number;
  variable_length: true;
}

type ParamsData = FlatParamsData | VarLenParamsData;

function isVarLen(p: ParamsData): p is VarLenParamsData {
  return p.variable_length === true;
}

/* ----------------------------- Page ----------------------------- */

export default function TunablesPage({
  selectedTarget,
}: {
  selectedTarget: string;
}) {
  const [registryComponents, setRegistryComponents] = useState<
    { fullUid: string; name: string; reachable: boolean }[]
  >([]);
  const [selectedUid, setSelectedUid] = useState("");
  const [params, setParams] = useState<ParamsData | null>(null);
  const [fieldInfo, setFieldInfo] = useState<FieldInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  // Component registry via the shared cache; reachable subset drives
  // the picker, and the first reachable component auto-selects once.
  const registryQuery = useRegistry(selectedTarget);
  useEffect(() => {
    const reachable = (registryQuery.data ?? []).filter((c) => c.reachable);
    setRegistryComponents(reachable);
    if (!selectedUid && reachable.length > 0) {
      setSelectedUid(reachable[0].fullUid.replace("0x", ""));
    }
    // selectedUid intentionally not in deps -- only auto-pick the
    // initial UID when it's empty.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [registryQuery.data]);

  // Struct-dict field info for the selected component, cached
  // per (target, component) -- dictionaries are immutable per process.
  const tunableFieldsQuery = useTunableFields(
    selectedTarget,
    params?.component ?? null,
  );
  useEffect(() => {
    setFieldInfo((tunableFieldsQuery.data ?? []) as FieldInfo[]);
  }, [tunableFieldsQuery.data]);

  const loadParams = async () => {
    if (!selectedUid) return;
    setLoading(true);
    setError(null);
    setMessage(null);
    try {
      const uid = selectedUid.replace("0x", "").replace("0X", "");
      if (!uid) {
        setLoading(false);
        return;
      }
      const r = await fetch(`/api/targets/${selectedTarget}/params/${uid}`);
      if (r.ok) {
        const data = await r.json();
        if (data.fields) {
          setParams(data);
        } else {
          setParams(null);
          setMessage("No tunable parameters registered for this component.");
        }
      } else {
        const text = await r.text();
        setParams(null);
        if (text.includes("COMPONENT_NOT_FOUND")) {
          setMessage(
            "This component has no tunable parameters in the data registry.",
          );
        } else {
          setError(text);
        }
      }
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setLoading(false);
  };

  useEffect(() => {
    loadParams();
    // loadParams is intentionally not in deps -- it's defined every render
    // (no useCallback) and including it would loop. The function only reads
    // selectedTarget + selectedUid which are both in deps.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedTarget, selectedUid]);

  const updateField = (name: string, value: string) => {
    if (!params || isVarLen(params)) return;
    const numVal = parseFloat(value);
    setParams({
      ...params,
      fields: { ...params.fields, [name]: isNaN(numVal) ? value : numVal },
    });
  };

  const applyParams = async () => {
    if (!params) return;
    setLoading(true);
    setError(null);
    setMessage(null);
    try {
      const uid = selectedUid.replace("0x", "").replace("0X", "");
      // Include raw_hex so backend can use it as base (no hidden INSPECT)
      const body = isVarLen(params)
        ? {
            fields: params.fields.header,
            entries: params.fields.entries,
            variable_length: true,
            raw_hex: params.raw_hex,
          }
        : { fields: params.fields, raw_hex: params.raw_hex };
      const r = await fetch(
        `/api/targets/${selectedTarget}/params/${uid}/update`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        },
      );
      if (r.ok) {
        const result = await r.json();
        const staged = result.staged as
          | {
              step: string;
              outcome?: string;
              verify?: string;
              verdict?: { verdict_name?: string } | null;
            }
          | undefined;
        if (result.status === 0) {
          // A readback-capable target ran the full staged pipeline;
          // say so -- "verified on vehicle" is a stronger claim than
          // "uploaded" and the operator should know which they got.
          setMessage(
            staged?.step === "applied"
              ? `Verified on vehicle (readback match, VERIFY OK) and applied` +
                  ` (${result.uploaded_bytes} bytes${
                    result.queued ? ", completed deferred" : ""
                  })`
              : `Parameters updated and reloaded (${result.uploaded_bytes} bytes uploaded)`,
          );
          setTimeout(loadParams, 500);
        } else if (staged && staged.step !== "applied") {
          // The pipeline refused before apply: active bytes untouched.
          const verdict =
            staged.verdict?.verdict_name ?? result.status_name ?? "refused";
          setError(
            `Not applied -- ${staged.step} step refused (${verdict}). ` +
              `Active configuration untouched.`,
          );
        } else {
          const verdict = result.reload_verdict
            ? ` (verdict: ${result.reload_verdict})`
            : "";
          setError(`Upload failed: ${result.status_name}${verdict}`);
        }
      } else {
        setError(await r.text());
      }
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setLoading(false);
  };

  const getFieldType = (name: string): string => {
    const fi = fieldInfo.find((f) => f.name === name);
    return fi ? `${fi.type}${fi.size > 0 ? fi.size * 8 : ""}` : "";
  };

  return (
    <div className="max-w-4xl">
      <div className="flex items-center gap-3 mb-5">
        <h1 className="text-xl font-bold">Tunables</h1>
        <select
          value={selectedUid}
          onChange={(e) => setSelectedUid(e.target.value)}
          className="text-sm"
          style={{ padding: "4px 8px" }}
        >
          {registryComponents.map((c) => (
            <option key={c.fullUid} value={c.fullUid.replace("0x", "")}>
              {c.name} ({c.fullUid})
            </option>
          ))}
        </select>
        <button
          onClick={loadParams}
          disabled={loading}
          className="text-xs px-3 py-1"
          style={{
            backgroundColor: "var(--color-elevated)",
            color: "var(--color-text-primary)",
            border: "1px solid var(--color-border)",
          }}
        >
          {loading ? "..." : "Reload"}
        </button>
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

      {message && (
        <div
          className="rounded-lg p-3 mb-4 text-sm"
          style={{
            backgroundColor: "rgba(63,185,80,0.1)",
            color: "var(--color-ok)",
            border: "1px solid rgba(63,185,80,0.3)",
          }}
        >
          {message}
        </div>
      )}

      {/* Flat tunable params (editable) */}
      {params && !isVarLen(params) && (
        <div>
          <div
            className="mb-3 text-sm"
            style={{ color: "var(--color-text-secondary)" }}
          >
            <strong>{params.component}</strong> / {params.struct_name} (
            {params.raw_length} bytes)
          </div>

          <div
            className="rounded-lg overflow-hidden"
            style={{ border: "1px solid var(--color-border)" }}
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
                    backgroundColor: "var(--color-elevated)",
                    borderBottom: "1px solid var(--color-border)",
                  }}
                >
                  <th
                    style={{
                      textAlign: "left",
                      padding: "0.5rem 0.8rem",
                      width: "35%",
                    }}
                  >
                    Field
                  </th>
                  <th
                    style={{
                      textAlign: "left",
                      padding: "0.5rem 0.8rem",
                      width: "15%",
                    }}
                  >
                    Type
                  </th>
                  <th style={{ textAlign: "left", padding: "0.5rem 0.8rem" }}>
                    Value
                  </th>
                </tr>
              </thead>
              <tbody>
                {Object.entries(params.fields)
                  .filter(
                    ([name]) =>
                      !name.startsWith("reserved") && !name.startsWith("pad"),
                  )
                  .map(([name, value]) => (
                    <tr
                      key={name}
                      style={{
                        borderBottom: "1px solid var(--color-border-muted)",
                      }}
                    >
                      <td
                        className="mono font-bold"
                        style={{ padding: "0.4rem 0.8rem" }}
                      >
                        {name}
                      </td>
                      <td
                        className="mono text-xs"
                        style={{
                          padding: "0.4rem 0.8rem",
                          color: "var(--color-text-muted)",
                        }}
                      >
                        {getFieldType(name)}
                      </td>
                      <td style={{ padding: "0.4rem 0.8rem" }}>
                        <input
                          type="number"
                          step="any"
                          value={value}
                          onChange={(e) => updateField(name, e.target.value)}
                          className="mono text-sm w-full"
                        />
                      </td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>

          <div className="mt-3 flex gap-2">
            <button
              onClick={applyParams}
              disabled={loading}
              className="text-sm px-4 py-1.5 font-bold text-white rounded-md"
              style={{
                backgroundColor: "var(--color-accent)",
                border: "none",
                cursor: "pointer",
                opacity: loading ? 0.6 : 1,
              }}
            >
              Apply
            </button>
            <button
              onClick={loadParams}
              disabled={loading}
              className="text-sm px-3 py-1.5 rounded-md"
              style={{
                backgroundColor: "var(--color-elevated)",
                color: "var(--color-text-primary)",
                border: "1px solid var(--color-border)",
                cursor: "pointer",
              }}
            >
              Reset
            </button>
          </div>

          <details className="mt-4">
            <summary
              className="cursor-pointer text-xs"
              style={{ color: "var(--color-text-muted)" }}
            >
              Raw hex ({params.raw_length} bytes)
            </summary>
            <pre
              className="mono text-xs mt-2 p-3 rounded-lg break-all"
              style={{ backgroundColor: "var(--color-elevated)" }}
            >
              {params.raw_hex}
            </pre>
          </details>
        </div>
      )}

      {/* Variable-length params (header + entries, editable) */}
      {params && isVarLen(params) && (
        <div>
          <div
            className="mb-3 text-sm"
            style={{ color: "var(--color-text-secondary)" }}
          >
            <strong>{params.component}</strong> / {params.struct_name} (
            {params.raw_length} bytes)
          </div>

          {/* Editable header */}
          <div
            className="mb-3 text-xs uppercase tracking-wider"
            style={{ color: "var(--color-text-muted)" }}
          >
            Configuration
          </div>
          <div
            className="rounded-lg overflow-hidden mb-4"
            style={{ border: "1px solid var(--color-border)" }}
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
                    backgroundColor: "var(--color-elevated)",
                    borderBottom: "1px solid var(--color-border)",
                  }}
                >
                  <th style={{ textAlign: "left", padding: "0.5rem 0.8rem" }}>
                    Field
                  </th>
                  <th style={{ textAlign: "left", padding: "0.5rem 0.8rem" }}>
                    Value
                  </th>
                </tr>
              </thead>
              <tbody>
                {Object.entries(params.fields.header).map(([name, value]) => (
                  <tr
                    key={name}
                    style={{
                      borderBottom: "1px solid var(--color-border-muted)",
                    }}
                  >
                    <td
                      className="mono font-bold"
                      style={{ padding: "0.4rem 0.8rem" }}
                    >
                      {name}
                    </td>
                    <td style={{ padding: "0.4rem 0.8rem" }}>
                      <input
                        type="number"
                        step="any"
                        value={value}
                        onChange={(e) => {
                          const numVal = parseFloat(e.target.value);
                          setParams({
                            ...params,
                            fields: {
                              ...params.fields,
                              header: {
                                ...params.fields.header,
                                [name]: isNaN(numVal) ? e.target.value : numVal,
                              },
                            },
                          });
                        }}
                        className="mono text-sm w-full"
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {/* Editable entries table */}
          <div className="flex items-center justify-between mb-3">
            <div
              className="text-xs uppercase tracking-wider"
              style={{ color: "var(--color-text-muted)" }}
            >
              Entries ({params.fields.entries.length})
            </div>
          </div>
          <div
            className="rounded-lg overflow-hidden"
            style={{ border: "1px solid var(--color-border)" }}
          >
            <div className="overflow-x-auto">
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
                    <th
                      style={{
                        textAlign: "left",
                        padding: "0.4rem 0.5rem",
                        width: "30px",
                      }}
                    >
                      #
                    </th>
                    {params.fields.entries.length > 0 &&
                      Object.keys(params.fields.entries[0]).map((col) => (
                        <th
                          key={col}
                          className="mono"
                          style={{
                            textAlign: "left",
                            padding: "0.4rem 0.5rem",
                          }}
                        >
                          {col}
                        </th>
                      ))}
                  </tr>
                </thead>
                <tbody>
                  {params.fields.entries.map((entry, i) => (
                    <tr
                      key={i}
                      style={{
                        borderBottom: "1px solid var(--color-border-muted)",
                      }}
                    >
                      <td
                        className="mono"
                        style={{
                          padding: "0.3rem 0.5rem",
                          color: "var(--color-text-muted)",
                        }}
                      >
                        {i}
                      </td>
                      {Object.entries(entry).map(([col, val]) => (
                        <td key={col} style={{ padding: "0.2rem 0.3rem" }}>
                          <input
                            type="number"
                            step="any"
                            value={col === "fullUid" ? Number(val) : val}
                            onChange={(e) => {
                              const numVal = parseFloat(e.target.value);
                              const newVal = isNaN(numVal)
                                ? e.target.value
                                : numVal;
                              setParams({
                                ...params,
                                fields: {
                                  ...params.fields,
                                  entries: params.fields.entries.map(
                                    (ent, ei) =>
                                      ei === i
                                        ? { ...ent, [col]: newVal }
                                        : ent,
                                  ),
                                },
                              });
                            }}
                            className="mono text-xs w-full"
                            style={{ padding: "2px 4px" }}
                          />
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>

          <div className="mt-3 flex gap-2">
            <button
              onClick={applyParams}
              disabled={loading}
              className="text-sm px-4 py-1.5 font-bold text-white rounded-md"
              style={{
                backgroundColor: "var(--color-accent)",
                border: "none",
                cursor: "pointer",
                opacity: loading ? 0.6 : 1,
              }}
            >
              Apply
            </button>
            <button
              onClick={loadParams}
              disabled={loading}
              className="text-sm px-3 py-1.5 rounded-md"
              style={{
                backgroundColor: "var(--color-elevated)",
                color: "var(--color-text-primary)",
                border: "1px solid var(--color-border)",
                cursor: "pointer",
              }}
            >
              Reset
            </button>
          </div>

          <details className="mt-4">
            <summary
              className="cursor-pointer text-xs"
              style={{ color: "var(--color-text-muted)" }}
            >
              Raw hex ({params.raw_length} bytes)
            </summary>
            <pre
              className="mono text-xs mt-2 p-3 rounded-lg break-all"
              style={{ backgroundColor: "var(--color-elevated)" }}
            >
              {params.raw_hex}
            </pre>
          </details>
        </div>
      )}
    </div>
  );
}
