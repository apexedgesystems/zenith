import { useCallback, useEffect, useState } from "react";
import { useTargets } from "../api/queries";

/**
 * Audit Log viewer.
 *
 * Displays the append-only audit log of operator actions captured by
 * the backend whenever a write-side endpoint is called. Used for
 * compliance review and post-incident triage.
 */

interface AuditEntry {
  id: number;
  ts_ms: number;
  actor: string;
  action: string;
  target_id: string | null;
  detail: string | null;
  status: string;
  source_ip: string | null;
}

const PAGE_SIZE = 100;

export default function AuditPage() {
  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [autoRefresh, setAutoRefresh] = useState(false);
  const [offset, setOffset] = useState(0);
  // Map target_id -> friendly display name, from the app-wide targets
  // cache (audit rows carry raw ids).
  const targetsData = useTargets().data;
  const targetNames: Record<string, string> = Object.fromEntries(
    (targetsData ?? []).map((t) => [t.id, t.name]),
  );

  const fetchAudit = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const r = await fetch(`/api/audit?limit=${PAGE_SIZE}&offset=${offset}`);
      if (!r.ok) {
        setError(await r.text());
        return;
      }
      const data = await r.json();
      setEntries(data.entries || []);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [offset]);

  useEffect(() => {
    fetchAudit();
  }, [fetchAudit]);

  useEffect(() => {
    if (!autoRefresh) return;
    const interval = setInterval(fetchAudit, 5000);
    return () => clearInterval(interval);
  }, [autoRefresh, fetchAudit]);

  const filtered = filter
    ? entries.filter((e) =>
        [e.actor, e.action, e.target_id, e.detail, e.status, e.source_ip]
          .filter(Boolean)
          .some((s) => s!.toLowerCase().includes(filter.toLowerCase())),
      )
    : entries;

  return (
    <div className="max-w-6xl">
      <div className="flex items-center justify-between mb-4">
        <h1 className="text-xl font-bold">Audit Log</h1>
        <div className="text-xs" style={{ color: "var(--color-text-muted)" }}>
          Showing {filtered.length} of {entries.length} entries
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
        <input
          type="text"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Filter by actor, action, target, IP..."
          className="text-xs px-2 py-1 flex-1"
          style={{ minWidth: "200px" }}
        />
        <button
          onClick={fetchAudit}
          disabled={loading}
          className="text-xs px-3 py-1"
          style={{
            backgroundColor: "var(--color-accent)",
            color: "#fff",
            border: "none",
            borderRadius: "4px",
            cursor: loading ? "not-allowed" : "pointer",
            opacity: loading ? 0.5 : 1,
          }}
        >
          {loading ? "..." : "Refresh"}
        </button>
        <label
          className="text-xs flex items-center gap-1"
          style={{ color: "var(--color-text-secondary)" }}
        >
          <input
            type="checkbox"
            checked={autoRefresh}
            onChange={(e) => setAutoRefresh(e.target.checked)}
          />
          Auto (5s)
        </label>
        <div className="flex items-center gap-1 ml-auto">
          <button
            onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
            disabled={offset === 0}
            className="text-xs px-2 py-1"
            style={{
              backgroundColor: "var(--color-elevated)",
              color:
                offset === 0
                  ? "var(--color-text-muted)"
                  : "var(--color-text-primary)",
              border: "1px solid var(--color-border)",
              borderRadius: "3px",
              cursor: offset === 0 ? "not-allowed" : "pointer",
            }}
          >
            &lt; Newer
          </button>
          <span
            className="text-[10px] mono px-1"
            style={{ color: "var(--color-text-muted)" }}
          >
            {offset}-{offset + PAGE_SIZE}
          </span>
          <button
            onClick={() => setOffset(offset + PAGE_SIZE)}
            disabled={entries.length < PAGE_SIZE}
            className="text-xs px-2 py-1"
            style={{
              backgroundColor: "var(--color-elevated)",
              color:
                entries.length < PAGE_SIZE
                  ? "var(--color-text-muted)"
                  : "var(--color-text-primary)",
              border: "1px solid var(--color-border)",
              borderRadius: "3px",
              cursor: entries.length < PAGE_SIZE ? "not-allowed" : "pointer",
            }}
          >
            Older &gt;
          </button>
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

      {filtered.length === 0 && !loading && (
        <div
          className="rounded-lg p-8 text-center text-sm"
          style={{
            border: "1px dashed var(--color-border)",
            color: "var(--color-text-muted)",
          }}
        >
          {entries.length === 0
            ? "No audit entries yet"
            : "No entries match the filter"}
        </div>
      )}

      {filtered.length > 0 && (
        <div
          className="rounded-lg overflow-hidden"
          style={{ border: "1px solid var(--color-border)" }}
        >
          <table
            style={{
              width: "100%",
              borderCollapse: "collapse",
              fontSize: "0.72rem",
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
                    padding: "0.4rem 0.6rem",
                    width: "150px",
                  }}
                >
                  Time (UTC)
                </th>
                <th
                  style={{
                    textAlign: "left",
                    padding: "0.4rem 0.6rem",
                    width: "100px",
                  }}
                >
                  Actor
                </th>
                <th
                  style={{
                    textAlign: "left",
                    padding: "0.4rem 0.6rem",
                    width: "150px",
                  }}
                >
                  Action
                </th>
                <th
                  style={{
                    textAlign: "left",
                    padding: "0.4rem 0.6rem",
                    width: "100px",
                  }}
                >
                  Target
                </th>
                <th style={{ textAlign: "left", padding: "0.4rem 0.6rem" }}>
                  Detail
                </th>
                <th
                  style={{
                    textAlign: "left",
                    padding: "0.4rem 0.6rem",
                    width: "100px",
                  }}
                >
                  IP
                </th>
                <th
                  style={{
                    textAlign: "right",
                    padding: "0.4rem 0.6rem",
                    width: "120px",
                  }}
                >
                  Status
                </th>
              </tr>
            </thead>
            <tbody>
              {filtered.map((e) => {
                const isOk = e.status === "ok";
                const isErr =
                  e.status.startsWith("err") || e.status.startsWith("nak");
                return (
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
                      }}
                    >
                      {new Date(e.ts_ms)
                        .toISOString()
                        .replace("T", " ")
                        .slice(0, 19)}
                    </td>
                    <td style={{ padding: "0.3rem 0.6rem" }}>{e.actor}</td>
                    <td
                      className="mono"
                      style={{
                        padding: "0.3rem 0.6rem",
                        color: "var(--color-text-primary)",
                      }}
                    >
                      {e.action}
                    </td>
                    <td
                      style={{
                        padding: "0.3rem 0.6rem",
                        color: "var(--color-text-secondary)",
                      }}
                      title={e.target_id || ""}
                    >
                      {e.target_id
                        ? targetNames[e.target_id] || e.target_id
                        : "-"}
                    </td>
                    <td
                      className="mono"
                      style={{
                        padding: "0.3rem 0.6rem",
                        color: "var(--color-text-muted)",
                        wordBreak: "break-all",
                      }}
                    >
                      {e.detail || "-"}
                    </td>
                    <td
                      className="mono"
                      style={{
                        padding: "0.3rem 0.6rem",
                        color: "var(--color-text-muted)",
                      }}
                    >
                      {e.source_ip || "-"}
                    </td>
                    <td
                      style={{ padding: "0.3rem 0.6rem", textAlign: "right" }}
                    >
                      <span
                        style={{
                          padding: "0.1rem 0.4rem",
                          borderRadius: "3px",
                          backgroundColor: isOk
                            ? "rgba(63,185,80,0.15)"
                            : isErr
                              ? "rgba(248,81,73,0.15)"
                              : "rgba(255,255,255,0.05)",
                          color: isOk
                            ? "var(--color-ok)"
                            : isErr
                              ? "var(--color-crit)"
                              : "var(--color-text-muted)",
                          fontSize: "0.65rem",
                          fontWeight: "bold",
                        }}
                      >
                        {e.status}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
