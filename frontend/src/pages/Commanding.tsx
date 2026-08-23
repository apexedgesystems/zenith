import { useState, useEffect, useCallback } from "react";
import {
  bytesToHex,
  decodeField,
  formatValue,
  hexToBytes,
  type FieldDef,
} from "../api/decode";

/* ----------------------------- Types ----------------------------- */

export interface CommandFieldDef {
  name: string;
  type: string;
  desc?: string;
  default?: number | string;
}

interface CommandDef {
  name: string;
  opcode: string;
  desc?: string;
  fields: CommandFieldDef[];
}

interface ComponentCommands {
  fullUid: string;
  commands: CommandDef[];
}

interface QuickCommand {
  label: string;
  fullUid: string;
  opcode: string;
  desc?: string;
  payload?: string;
}

interface CommandResult {
  status: number;
  status_name: string;
  extra_hex?: string;
  extra_length?: number;
}

interface HistoryEntry {
  id: number;
  timestamp: string;
  target: string;
  component: string;
  command: string;
  fullUid: string;
  opcode: string;
  payload: string;
  result: CommandResult | null;
  error: string | null;
}

/* ----------------------------- Helpers ----------------------------- */

/** Decode a hex response using struct dict field definitions */
function decodeResponseHex(
  hex: string,
  fields: FieldDef[],
): Record<string, string> {
  // Thin wrapper over THE shared decoder: same bytes render the same
  // way here as on every other page. Skips pad/reserved names; strings
  // and arrays now decode instead of being dropped.
  const result: Record<string, string> = {};
  const bytes = hexToBytes(hex);
  if (!bytes || bytes.length === 0) return result;
  const view = new DataView(bytes.buffer);
  for (const f of fields) {
    if (f.name.startsWith("pad") || f.name.startsWith("reserved")) continue;
    if (f.size === 0) continue;
    const v = decodeField(view, f);
    if (v === null) {
      // Unknown shape: show raw hex rather than guessing.
      if (f.offset + f.size <= bytes.length) {
        result[f.name] = `0x${bytesToHex(
          bytes.slice(f.offset, f.offset + f.size),
        )}`;
      }
      continue;
    }
    result[f.name] = formatValue(v, f);
  }
  return result;
}

export function encodeField(field: CommandFieldDef, value: string): string {
  // Parse as hex if starts with 0x, otherwise decimal
  const num =
    value.startsWith("0x") || value.startsWith("0X")
      ? parseInt(value, 16)
      : parseInt(value);
  if (isNaN(num)) return "00"; // Safe fallback: zero
  // Clamp unsigned types to non-negative
  const n = field.type.startsWith("uint") ? Math.max(0, num) : num;
  switch (field.type) {
    case "uint8":
      return (n & 0xff).toString(16).padStart(2, "0");
    case "uint16": {
      const buf = new ArrayBuffer(2);
      new DataView(buf).setUint16(0, n & 0xffff, true);
      return [...new Uint8Array(buf)]
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
    }
    case "uint32": {
      const buf = new ArrayBuffer(4);
      new DataView(buf).setUint32(0, n >>> 0, true);
      return [...new Uint8Array(buf)]
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
    }
    default:
      return (n & 0xff).toString(16).padStart(2, "0");
  }
}

/* ----------------------------- Page ----------------------------- */

export default function CommandingPage({
  selectedTarget,
  targets,
}: {
  selectedTarget: string;
  targets: { id: string; name: string }[];
}) {
  const [quickCommands, setQuickCommands] = useState<QuickCommand[]>([]);
  const [components, setComponents] = useState<
    Record<string, ComponentCommands>
  >({});
  const [selectedComponent, setSelectedComponent] = useState("");
  const [selectedCommand, setSelectedCommand] = useState("");
  const [fieldValues, setFieldValues] = useState<Record<string, string>>({});
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [sending, setSending] = useState(false);
  const [showRaw, setShowRaw] = useState(false);
  const [structDicts, setStructDicts] = useState<
    Record<
      string,
      {
        name: string;
        category: string;
        size: number;
        fields: { name: string; type: string; offset: number; size: number }[];
      }[]
    >
  >({});
  // Per-history-entry: which struct the user picked to interpret the response.
  // Key is entry id; value is "{component}/{struct}" or "" for raw hex.
  const [interpretChoice, setInterpretChoice] = useState<
    Record<number, string>
  >({});
  const [rawUid, setRawUid] = useState("0x000000");
  const [rawOpcode, setRawOpcode] = useState("0x0000");
  const [rawPayload, setRawPayload] = useState("");
  let nextId = history.length;

  // Load ALL non-empty structs (any category) from the per-target dict
  // for the Interpret feature. The previous version only loaded TELEMETRY
  // structs, which meant Operations responses (STATE/OUTPUT/etc.) couldn't
  // be decoded at all. Per-target endpoints -- the /api/structs global
  // is the optional fallback dict and is usually empty.
  useEffect(() => {
    if (!selectedTarget) return;
    setStructDicts({});
    setInterpretChoice({});
    fetch(`/api/targets/${selectedTarget}/structs`)
      .then((r) => (r.ok ? r.json() : { components: [] }))
      .then((data) => {
        for (const comp of data.components || []) {
          // Pull every struct that has fields and a non-zero size
          const useful = (comp.structs || []).filter(
            (s: { fieldCount: number; size: number }) =>
              s.fieldCount > 0 && s.size > 0,
          );
          if (useful.length === 0) continue;
          fetch(
            `/api/targets/${selectedTarget}/structs/${encodeURIComponent(
              comp.component,
            )}`,
          )
            .then((r) => (r.ok ? r.json() : null))
            .then((detail) => {
              if (!detail?.structs) return;
              const list: {
                name: string;
                category: string;
                size: number;
                fields: {
                  name: string;
                  type: string;
                  offset: number;
                  size: number;
                }[];
              }[] = [];
              for (const s of useful) {
                const sdef = detail.structs[s.name];
                if (!sdef?.fields) continue;
                list.push({
                  name: s.name,
                  category: sdef.category || "",
                  size: sdef.size,
                  fields: sdef.fields,
                });
              }
              if (list.length > 0) {
                setStructDicts((prev) => ({ ...prev, [comp.component]: list }));
              }
            })
            .catch(() => {});
        }
      })
      .catch(() => {});
  }, [selectedTarget]);

  // Load command config from backend
  useEffect(() => {
    fetch(`/api/targets/${selectedTarget}/commands`)
      .then((r) => (r.ok ? r.json() : { quickCommands: [], components: {} }))
      .then((data) => {
        setQuickCommands(data.quickCommands || []);
        setComponents(data.components || {});
        const compNames = Object.keys(data.components || {});
        if (compNames.length > 0 && !selectedComponent) {
          setSelectedComponent(compNames[0]);
        }
      })
      .catch(() => {});
  }, [selectedTarget]);

  // Reset command selection when component changes
  useEffect(() => {
    if (!selectedComponent || !components[selectedComponent]) return;
    const cmds = components[selectedComponent].commands;
    if (cmds.length > 0) {
      setSelectedCommand(cmds[0].name);
      // Set default field values
      const defaults: Record<string, string> = {};
      for (const f of cmds[0].fields) {
        defaults[f.name] = f.default !== undefined ? String(f.default) : "";
      }
      setFieldValues(defaults);
    }
  }, [selectedComponent, components]);

  // Update field defaults when command changes
  useEffect(() => {
    if (!selectedComponent || !components[selectedComponent]) return;
    const cmd = components[selectedComponent].commands.find(
      (c) => c.name === selectedCommand,
    );
    if (!cmd) return;
    const defaults: Record<string, string> = {};
    for (const f of cmd.fields) {
      defaults[f.name] = f.default !== undefined ? String(f.default) : "";
    }
    setFieldValues(defaults);
  }, [selectedCommand, selectedComponent, components]);

  const sendCommand = useCallback(
    async (
      uid: string,
      opc: string,
      payload: string,
      compName: string,
      cmdName: string,
    ) => {
      setSending(true);
      const entry: HistoryEntry = {
        id: nextId++,
        timestamp: new Date().toISOString().split("T")[1].split(".")[0],
        target:
          targets.find((t) => t.id === selectedTarget)?.name || selectedTarget,
        component: compName,
        command: cmdName,
        fullUid: uid,
        opcode: opc,
        payload,
        result: null,
        error: null,
      };

      try {
        const r = await fetch(`/api/targets/${selectedTarget}/command`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            full_uid: uid,
            opcode: opc,
            payload_hex: payload,
          }),
        });
        if (r.ok) {
          entry.result = await r.json();
        } else {
          entry.error = await r.text();
        }
      } catch (e: unknown) {
        entry.error = e instanceof Error ? e.message : String(e);
      }

      setHistory((h) => [entry, ...h].slice(0, 200));
      setSending(false);
    },
    [selectedTarget],
  );

  const sendFormCommand = () => {
    if (!selectedComponent || !components[selectedComponent]) return;
    const comp = components[selectedComponent];
    const cmd = comp.commands.find((c) => c.name === selectedCommand);
    if (!cmd) return;

    // Encode fields to hex payload
    let payload = "";
    for (const field of cmd.fields) {
      const val = fieldValues[field.name] || "0";
      payload += encodeField(field, val);
    }

    sendCommand(comp.fullUid, cmd.opcode, payload, selectedComponent, cmd.name);
  };

  const currentComp = components[selectedComponent];
  const currentCmd = currentComp?.commands.find(
    (c) => c.name === selectedCommand,
  );

  return (
    <div className="max-w-4xl">
      <h1 className="text-xl font-bold mb-4">Commanding</h1>

      {/* Quick Commands */}
      {quickCommands.length > 0 && (
        <div className="mb-5">
          <div
            className="text-xs uppercase tracking-wider mb-2"
            style={{ color: "var(--color-text-muted)" }}
          >
            Quick Commands
          </div>
          <div className="flex flex-wrap gap-1.5">
            {quickCommands.map((cmd) => (
              <button
                key={cmd.label}
                onClick={() =>
                  sendCommand(
                    cmd.fullUid,
                    cmd.opcode,
                    cmd.payload || "",
                    "Quick",
                    cmd.label,
                  )
                }
                disabled={sending}
                title={cmd.desc}
                className="text-xs px-3 py-1.5 rounded-md"
                style={{
                  backgroundColor: "var(--color-elevated)",
                  color: "var(--color-text-secondary)",
                  border: "1px solid var(--color-border)",
                  cursor: "pointer",
                  opacity: sending ? 0.6 : 1,
                }}
              >
                {cmd.label}
              </button>
            ))}
          </div>
        </div>
      )}

      {/* Component Command Form */}
      <div
        className="rounded-lg p-4 mb-5"
        style={{
          backgroundColor: "var(--color-surface)",
          border: "1px solid var(--color-border)",
        }}
      >
        <div
          className="text-xs uppercase tracking-wider mb-3"
          style={{ color: "var(--color-text-muted)" }}
        >
          Send Command
        </div>

        <div className="flex gap-3 mb-3">
          {/* Component selector */}
          <div className="flex-1">
            <label
              className="text-[10px] uppercase tracking-wider mb-1 block"
              style={{ color: "var(--color-text-muted)" }}
            >
              Component
            </label>
            <select
              value={selectedComponent}
              onChange={(e) => setSelectedComponent(e.target.value)}
              className="w-full text-sm"
              style={{ padding: "6px 8px" }}
            >
              {Object.keys(components).map((name) => (
                <option key={name} value={name}>
                  {name} ({components[name].fullUid})
                </option>
              ))}
            </select>
          </div>

          {/* Command selector */}
          <div className="flex-1">
            <label
              className="text-[10px] uppercase tracking-wider mb-1 block"
              style={{ color: "var(--color-text-muted)" }}
            >
              Command
            </label>
            <select
              value={selectedCommand}
              onChange={(e) => setSelectedCommand(e.target.value)}
              className="w-full text-sm"
              style={{ padding: "6px 8px" }}
            >
              {currentComp?.commands.map((cmd) => (
                <option key={cmd.name} value={cmd.name}>
                  {cmd.name} ({cmd.opcode})
                </option>
              ))}
            </select>
          </div>
        </div>

        {/* Command description */}
        {currentCmd?.desc && (
          <div
            className="text-xs mb-3"
            style={{ color: "var(--color-text-muted)" }}
          >
            {currentCmd.desc}
          </div>
        )}

        {/* Field inputs */}
        {currentCmd && currentCmd.fields.length > 0 && (
          <div className="grid grid-cols-2 gap-3 mb-3">
            {currentCmd.fields.map((field) => (
              <div key={field.name}>
                <label
                  className="text-[10px] uppercase tracking-wider mb-1 block"
                  style={{ color: "var(--color-text-muted)" }}
                >
                  {field.name}
                  <span
                    className="mono"
                    style={{
                      color: "var(--color-text-muted)",
                      marginLeft: "4px",
                    }}
                  >
                    ({field.type})
                  </span>
                </label>
                <input
                  value={fieldValues[field.name] || ""}
                  onChange={(e) =>
                    setFieldValues((prev) => ({
                      ...prev,
                      [field.name]: e.target.value,
                    }))
                  }
                  placeholder={field.desc || field.name}
                  className="w-full mono text-sm"
                  style={{ padding: "6px 8px" }}
                />
              </div>
            ))}
          </div>
        )}

        <div className="flex items-center gap-3">
          <button
            onClick={sendFormCommand}
            disabled={sending}
            className="text-sm px-4 py-1.5 font-bold text-white rounded-md"
            style={{
              backgroundColor: "var(--color-accent)",
              border: "none",
              cursor: "pointer",
              opacity: sending ? 0.6 : 1,
            }}
          >
            {sending ? "Sending..." : "Send"}
          </button>
          <button
            onClick={() => setShowRaw(!showRaw)}
            className="text-xs px-2 py-1"
            style={{
              backgroundColor: "var(--color-elevated)",
              color: "var(--color-text-muted)",
              border: "1px solid var(--color-border)",
              borderRadius: "4px",
              cursor: "pointer",
            }}
          >
            {showRaw ? "Hide Raw" : "Raw Mode"}
          </button>
        </div>
      </div>

      {/* Raw command form (collapsible) */}
      {showRaw && (
        <div
          className="rounded-lg p-4 mb-5"
          style={{
            backgroundColor: "var(--color-elevated)",
            border: "1px solid var(--color-border)",
          }}
        >
          <div
            className="text-xs uppercase tracking-wider mb-2"
            style={{ color: "var(--color-text-muted)" }}
          >
            Raw Command
          </div>
          <div className="grid grid-cols-4 gap-2 items-end">
            <div>
              <label
                className="text-[10px] block mb-1"
                style={{ color: "var(--color-text-muted)" }}
              >
                fullUid
              </label>
              <input
                value={rawUid}
                onChange={(e) => setRawUid(e.target.value)}
                className="w-full mono text-sm"
                style={{ padding: "4px 6px" }}
              />
            </div>
            <div>
              <label
                className="text-[10px] block mb-1"
                style={{ color: "var(--color-text-muted)" }}
              >
                Opcode
              </label>
              <input
                value={rawOpcode}
                onChange={(e) => setRawOpcode(e.target.value)}
                className="w-full mono text-sm"
                style={{ padding: "4px 6px" }}
              />
            </div>
            <div>
              <label
                className="text-[10px] block mb-1"
                style={{ color: "var(--color-text-muted)" }}
              >
                Payload (hex)
              </label>
              <input
                value={rawPayload}
                onChange={(e) => setRawPayload(e.target.value)}
                className="w-full mono text-sm"
                style={{ padding: "4px 6px" }}
              />
            </div>
            <button
              onClick={() =>
                sendCommand(
                  rawUid,
                  rawOpcode,
                  rawPayload,
                  "Raw",
                  `${rawOpcode}`,
                )
              }
              disabled={sending}
              className="text-sm px-3 py-1 text-white rounded-md"
              style={{
                backgroundColor: "var(--color-accent)",
                border: "none",
                cursor: "pointer",
              }}
            >
              Send
            </button>
          </div>
        </div>
      )}

      {/* Command History */}
      <div
        className="text-xs uppercase tracking-wider mb-2"
        style={{ color: "var(--color-text-muted)" }}
      >
        History ({history.length})
      </div>
      {history.length === 0 ? (
        <div className="text-sm" style={{ color: "var(--color-text-muted)" }}>
          No commands sent yet.
        </div>
      ) : (
        <div
          className="flex flex-col gap-1.5"
          style={{ maxHeight: "400px", overflow: "auto" }}
        >
          {history.map((entry) => {
            const isOk = entry.result?.status === 0;
            const isErr = !!entry.error;
            return (
              <div
                key={entry.id}
                className="rounded-md px-3 py-2 mono text-xs"
                style={{
                  border: `1px solid ${
                    isErr
                      ? "var(--color-crit)"
                      : isOk
                        ? "var(--color-ok)"
                        : "var(--color-warn)"
                  }`,
                  backgroundColor: isErr
                    ? "rgba(248,81,73,0.08)"
                    : isOk
                      ? "rgba(63,185,80,0.05)"
                      : "rgba(210,153,34,0.05)",
                }}
              >
                <div className="flex justify-between">
                  <span>
                    <span style={{ color: "var(--color-text-muted)" }}>
                      {entry.timestamp}
                    </span>{" "}
                    <span
                      style={{
                        color: "var(--color-warn)",
                        fontSize: "0.65rem",
                      }}
                    >
                      [{entry.target}]
                    </span>{" "}
                    <span className="font-bold">{entry.component}</span>{" "}
                    <span style={{ color: "var(--color-accent)" }}>
                      {entry.command}
                    </span>{" "}
                    <span style={{ color: "var(--color-text-muted)" }}>
                      uid={entry.fullUid} opc={entry.opcode}
                    </span>
                    {entry.payload && (
                      <span style={{ color: "var(--color-text-muted)" }}>
                        {" "}
                        payload={entry.payload}
                      </span>
                    )}
                  </span>
                  <span
                    className="font-bold"
                    style={{
                      color: isErr
                        ? "var(--color-crit)"
                        : isOk
                          ? "var(--color-ok)"
                          : "var(--color-warn)",
                    }}
                  >
                    {isErr ? "ERROR" : entry.result?.status_name}
                  </span>
                </div>
                {entry.result?.extra_hex &&
                  (() => {
                    // Build a list of every struct in the per-target dict
                    // whose byte size matches the response payload length.
                    // The user can pick which one to interpret as -- a single
                    // command opcode can sometimes return any of several
                    // related structs depending on the apex side state.
                    const extraLen = entry.result?.extra_length || 0;
                    const compName = entry.component;
                    type Cand = {
                      component: string;
                      struct: (typeof structDicts)[string][number];
                      key: string;
                    };
                    const candidates: Cand[] = [];
                    for (const [dictComp, structs] of Object.entries(
                      structDicts,
                    )) {
                      for (const s of structs) {
                        if (s.size !== extraLen) continue;
                        candidates.push({
                          component: dictComp,
                          struct: s,
                          key: `${dictComp}/${s.name}`,
                        });
                      }
                    }

                    // Auto-pick: prefer a struct from the matching component +
                    // category=TELEMETRY; otherwise the first matching component;
                    // otherwise any struct of matching size.
                    const sameComp = (a: string, b: string) =>
                      a.toLowerCase().includes(b.toLowerCase()) ||
                      b.toLowerCase().includes(a.toLowerCase());
                    const auto =
                      candidates.find(
                        (c) =>
                          sameComp(c.component, compName) &&
                          c.struct.category === "TELEMETRY",
                      ) ||
                      candidates.find((c) => sameComp(c.component, compName)) ||
                      candidates[0];

                    const userPick = interpretChoice[entry.id];
                    const picked =
                      userPick === undefined
                        ? auto
                        : userPick === ""
                          ? undefined
                          : candidates.find((c) => c.key === userPick) || auto;

                    let decoded: Record<string, string> | null = null;
                    if (picked) {
                      decoded = decodeResponseHex(
                        entry.result!.extra_hex!,
                        picked.struct.fields,
                      );
                    }

                    return (
                      <div className="mt-1">
                        {/* Picker (only show if there are multiple candidates or current pick differs from auto) */}
                        {candidates.length > 0 && (
                          <div
                            className="flex items-center gap-2 mb-1"
                            style={{
                              color: "var(--color-text-muted)",
                              fontSize: "0.65rem",
                            }}
                          >
                            <span>Interpret as:</span>
                            <select
                              value={picked ? picked.key : ""}
                              onChange={(e) =>
                                setInterpretChoice((prev) => ({
                                  ...prev,
                                  [entry.id]: e.target.value,
                                }))
                              }
                              className="mono text-[10px]"
                              style={{ padding: "1px 4px" }}
                            >
                              <option value="">-- raw hex --</option>
                              {candidates.map((c) => (
                                <option key={c.key} value={c.key}>
                                  {c.component}.{c.struct.name} (
                                  {c.struct.category || "?"}, {c.struct.size}B)
                                </option>
                              ))}
                            </select>
                            {candidates.length > 1 && (
                              <span
                                style={{ color: "var(--color-text-muted)" }}
                              >
                                {candidates.length} candidates
                              </span>
                            )}
                          </div>
                        )}
                        {decoded && Object.keys(decoded).length > 0 ? (
                          <div className="grid grid-cols-3 gap-x-3 gap-y-0.5">
                            {Object.entries(decoded).map(([name, val]) => (
                              <div key={name} className="flex justify-between">
                                <span
                                  style={{ color: "var(--color-text-muted)" }}
                                >
                                  {name}
                                </span>
                                <span
                                  className="font-bold"
                                  style={{ color: "var(--color-text-primary)" }}
                                >
                                  {val}
                                </span>
                              </div>
                            ))}
                          </div>
                        ) : (
                          <div
                            style={{
                              color: "var(--color-text-secondary)",
                              wordBreak: "break-all",
                            }}
                          >
                            response ({extraLen}B): {entry.result!.extra_hex}
                          </div>
                        )}
                      </div>
                    );
                  })()}
                {entry.error && (
                  <div className="mt-1" style={{ color: "var(--color-crit)" }}>
                    {entry.error}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
