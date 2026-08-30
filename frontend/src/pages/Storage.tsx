import { useQueryClient } from "@tanstack/react-query";
import { useDialogs } from "../components/dialogs";
import {
  useAllMetrics,
  useAllTargetStorage,
  useStorageVitals,
  useTargets,
} from "../api/queries";
import { ApiError, field, isObject, request } from "../api/client";
import { formatBytes, formatCount } from "../utils/targets";
import {
  fillPercent,
  fillRateLabel,
  formatDuration,
  timeToCapLabel,
} from "../utils/storage";

/* ----------------------------- Sub-blocks ----------------------------- */

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div
        className="text-[10px] uppercase tracking-wider"
        style={{ color: "var(--color-text-muted)" }}
      >
        {label}
      </div>
      <div className="mono text-sm mt-0.5">{value}</div>
    </div>
  );
}

function Card({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
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
        {title}
      </div>
      {children}
    </div>
  );
}

/* ----------------------------- Page ----------------------------- */

export default function StoragePage() {
  const { notify, confirmDialog } = useDialogs();
  const queryClient = useQueryClient();
  const vitalsQuery = useStorageVitals();
  const targetsQuery = useTargets();
  const targets = targetsQuery.data ?? [];
  const storageQuery = useAllTargetStorage(targets.map((t) => t.id));
  const storage = storageQuery.data ?? {};
  const metricsQuery = useAllMetrics();
  const metrics = metricsQuery.data ?? {};

  const v = vitalsQuery.data;
  const pct = v ? fillPercent(v.db_size_bytes, v.cap_bytes) : 0;
  const atCap = v ? v.db_size_bytes >= v.cap_bytes : false;

  const refreshStorage = () => {
    queryClient.invalidateQueries({ queryKey: ["storage-vitals"] });
    queryClient.invalidateQueries({ queryKey: ["storage-all"] });
  };

  const trimTarget = async (id: string, name: string) => {
    const s = storage[id];
    const count = s ? Math.max(1, Math.floor(s.sample_count / 4)) : 0;
    if (!count) {
      await notify("No samples to trim");
      return;
    }
    if (
      !(await confirmDialog(
        `Delete the oldest ~${formatCount(count)} samples for ${name}?`,
        "Trim stored telemetry",
      ))
    )
      return;
    try {
      await request(`/targets/${id}/storage/trim`, () => null, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ count }),
      });
      refreshStorage();
    } catch (e) {
      await notify(
        e instanceof ApiError ? e.message : String(e),
        "Trim failed",
      );
    }
  };

  const deleteTarget = async (id: string, name: string) => {
    if (
      !(await confirmDialog(
        `Delete ALL stored telemetry for ${name}? This cannot be undone.`,
        "Delete target history",
      ))
    )
      return;
    try {
      await request(`/targets/${id}/storage/delete`, () => null, {
        method: "POST",
      });
      refreshStorage();
    } catch (e) {
      await notify(
        e instanceof ApiError ? e.message : String(e),
        "Delete failed",
      );
    }
  };

  const downsample = async () => {
    if (
      !(await confirmDialog(
        "Average telemetry older than 1 hour into 1-minute buckets? " +
          "Full-resolution history for that span is replaced.",
        "Downsample old data",
      ))
    )
      return;
    try {
      const res = await request<{ samples_before: number; removed: number }>(
        "/telemetry/downsample",
        (d) => {
          if (!isObject(d)) return "expected object";
          return (
            field(d, "samples_before", "number") ??
            field(d, "removed", "number")
          );
        },
        { method: "POST" },
      );
      await notify(
        `Downsampled ${formatCount(
          res.samples_before,
        )} samples, removed ${formatCount(res.removed)}.`,
        "Downsample complete",
      );
      refreshStorage();
    } catch (e) {
      await notify(
        e instanceof ApiError ? e.message : String(e),
        "Downsample failed",
      );
    }
  };

  const totalEstimate = Object.values(storage).reduce(
    (acc, s) => acc + s.byte_estimate,
    0,
  );

  return (
    <div className="max-w-5xl">
      <h1 className="text-xl font-bold mb-4">Storage</h1>

      {vitalsQuery.isError && (
        <div className="text-sm mb-4" style={{ color: "var(--color-crit)" }}>
          Storage stats unavailable:{" "}
          {vitalsQuery.error instanceof Error
            ? vitalsQuery.error.message
            : "unknown error"}
        </div>
      )}

      {/* Global capacity gauge */}
      <Card title="Database capacity">
        {v ? (
          <>
            <div className="flex items-baseline justify-between mb-2">
              <span className="mono text-lg font-bold">
                {formatBytes(v.db_size_bytes)}
                <span
                  className="text-sm font-normal"
                  style={{ color: "var(--color-text-muted)" }}
                >
                  {" "}
                  / {formatBytes(v.cap_bytes)} cap
                </span>
              </span>
              <span
                className="mono text-sm"
                style={{
                  color: atCap
                    ? "var(--color-warn)"
                    : "var(--color-text-secondary)",
                }}
              >
                {timeToCapLabel(
                  v.projected_secs_to_cap,
                  v.fill_bytes_per_min,
                  v.db_size_bytes,
                  v.cap_bytes,
                )}
              </span>
            </div>
            <div
              className="rounded h-3 mb-3 overflow-hidden"
              style={{ backgroundColor: "var(--color-elevated)" }}
              title={`${pct.toFixed(1)}% of cap`}
            >
              <div
                className="h-full rounded transition-all"
                style={{
                  width: `${pct}%`,
                  backgroundColor:
                    pct > 99
                      ? "var(--color-crit)"
                      : pct > 90
                        ? "var(--color-warn)"
                        : "var(--color-accent)",
                }}
              />
            </div>
            <div className="grid grid-cols-3 gap-4 sm:grid-cols-6">
              <Stat label="Samples" value={formatCount(v.total_samples)} />
              <Stat
                label="Fill rate"
                value={fillRateLabel(v.fill_bytes_per_min)}
              />
              <Stat label="WAL" value={formatBytes(v.wal_bytes)} />
              <Stat label="Audit rows" value={formatCount(v.audit_rows)} />
              <Stat
                label="FIFO evicted"
                value={formatCount(v.fifo_evicted_samples)}
              />
              <Stat
                label="Retention pruned"
                value={formatCount(v.retention_pruned_samples)}
              />
            </div>
          </>
        ) : (
          <div className="text-sm" style={{ color: "var(--color-text-muted)" }}>
            Loading...
          </div>
        )}
      </Card>

      {/* Per-target usage */}
      <Card title="Per-target usage">
        {targets.length === 0 ? (
          <div className="text-sm" style={{ color: "var(--color-text-muted)" }}>
            No targets configured.
          </div>
        ) : (
          <div className="flex flex-col gap-3">
            {targets.map((t) => {
              const s = storage[t.id];
              const share =
                s && totalEstimate > 0
                  ? (s.byte_estimate / totalEstimate) * 100
                  : 0;
              return (
                <div key={t.id}>
                  <div className="flex items-baseline justify-between mb-1">
                    <span className="text-sm font-semibold">{t.name}</span>
                    <span
                      className="mono text-xs"
                      style={{ color: "var(--color-text-muted)" }}
                    >
                      {s
                        ? `${formatCount(s.sample_count)} samples | ${
                            s.channel_count
                          } channels | ${formatBytes(s.byte_estimate)}` +
                          (s.span_seconds !== null
                            ? ` | spans ${formatDuration(s.span_seconds)}`
                            : "")
                        : "no data"}
                    </span>
                  </div>
                  <div className="flex items-center gap-3">
                    <div
                      className="rounded h-2 flex-1 overflow-hidden"
                      style={{ backgroundColor: "var(--color-elevated)" }}
                      title={`${share.toFixed(1)}% of stored telemetry`}
                    >
                      <div
                        className="h-full rounded"
                        style={{
                          width: `${share}%`,
                          backgroundColor: "var(--color-accent)",
                        }}
                      />
                    </div>
                    <button
                      onClick={() => trimTarget(t.id, t.name)}
                      className="text-xs px-2 py-1 rounded-md"
                      style={{
                        color: "var(--color-warn)",
                        backgroundColor: "transparent",
                        border: "1px solid var(--color-border)",
                        cursor: "pointer",
                      }}
                    >
                      Trim 25%
                    </button>
                    <button
                      onClick={() => deleteTarget(t.id, t.name)}
                      className="text-xs px-2 py-1 rounded-md"
                      style={{
                        color: "var(--color-crit)",
                        backgroundColor: "transparent",
                        border: "1px solid var(--color-border)",
                        cursor: "pointer",
                      }}
                    >
                      Delete all
                    </button>
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </Card>

      {/* Retention ladder */}
      <Card title="Retention ladder">
        {v ? (
          v.tiers.enabled ? (
            <>
              <div className="flex flex-col gap-2 mb-3">
                {[
                  {
                    label: `Full resolution (newest ${formatDuration(
                      v.tiers.full_resolution_minutes * 60,
                    )})`,
                    rows: v.tiers.band_rows[0] ?? 0,
                  },
                  {
                    label: `${
                      v.tiers.mid_bucket_seconds
                    }s envelope buckets (to ${formatDuration(
                      v.tiers.mid_horizon_hours * 3600,
                    )})`,
                    rows: v.tiers.band_rows[1] ?? 0,
                  },
                  {
                    label: `${v.tiers.coarse_bucket_seconds}s envelope buckets (older)`,
                    rows: v.tiers.band_rows[2] ?? 0,
                  },
                ].map((band) => {
                  const total = v.tiers.band_rows.reduce((a, b) => a + b, 0);
                  const share = total > 0 ? (band.rows / total) * 100 : 0;
                  return (
                    <div key={band.label}>
                      <div className="flex items-baseline justify-between mb-0.5">
                        <span
                          className="text-xs"
                          style={{ color: "var(--color-text-secondary)" }}
                        >
                          {band.label}
                        </span>
                        <span
                          className="mono text-xs"
                          style={{ color: "var(--color-text-muted)" }}
                        >
                          {formatCount(band.rows)} rows
                        </span>
                      </div>
                      <div
                        className="rounded h-1.5 overflow-hidden"
                        style={{ backgroundColor: "var(--color-elevated)" }}
                      >
                        <div
                          className="h-full rounded"
                          style={{
                            width: `${share}%`,
                            backgroundColor: "var(--color-accent)",
                          }}
                        />
                      </div>
                    </div>
                  );
                })}
              </div>
              <div
                className="text-xs"
                style={{ color: "var(--color-text-muted)" }}
              >
                {formatCount(v.tiers.converted_rows)} rows converted to envelope
                buckets since start. Buckets keep min/max/count, so spikes stay
                visible as excursions on the charts.
              </div>
            </>
          ) : (
            <div
              className="text-sm"
              style={{ color: "var(--color-text-muted)" }}
            >
              Disabled. Enable [storage.tiers] in config.toml to keep the newest
              window at full resolution and age older data into envelope buckets
              (mean + min/max), multiplying how much history fits under the cap.
            </div>
          )
        ) : (
          <div className="text-sm" style={{ color: "var(--color-text-muted)" }}>
            Loading...
          </div>
        )}
      </Card>

      {/* Pipeline accounting */}
      <Card title="Pipeline accounting (since connect)">
        <div className="overflow-x-auto">
          <table className="w-full text-xs mono">
            <thead>
              <tr
                className="text-left"
                style={{ color: "var(--color-text-muted)" }}
              >
                <th className="py-1 pr-4 font-normal">Target</th>
                <th className="py-1 pr-4 font-normal">Decoded</th>
                <th className="py-1 pr-4 font-normal">Written</th>
                <th className="py-1 pr-4 font-normal">Counted drops</th>
                <th className="py-1 pr-4 font-normal">Residue</th>
              </tr>
            </thead>
            <tbody>
              {targets.map((t) => {
                const m = metrics[t.id];
                if (!m) return null;
                const drops =
                  m.dedup_drops +
                  m.router_lag_drops +
                  m.db_writer_lag_drops +
                  m.db_failed_samples;
                const residue =
                  m.decoded_samples - m.db_written_samples - drops;
                return (
                  <tr key={t.id}>
                    <td className="py-1 pr-4">{t.name}</td>
                    <td className="py-1 pr-4">
                      {formatCount(m.decoded_samples)}
                    </td>
                    <td className="py-1 pr-4">
                      {formatCount(m.db_written_samples)}
                    </td>
                    <td className="py-1 pr-4">{formatCount(drops)}</td>
                    <td
                      className="py-1 pr-4 font-bold"
                      style={{
                        color:
                          residue === 0
                            ? "var(--color-ok)"
                            : "var(--color-crit)",
                      }}
                      title="decoded - written - counted drops; nonzero means samples are unaccounted for"
                    >
                      {residue}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </Card>

      {/* Maintenance */}
      <Card title="Maintenance">
        <div className="flex items-center gap-3">
          <button
            onClick={downsample}
            className="text-sm px-4 py-1.5 rounded-md"
            style={{
              color: "var(--color-text-primary)",
              backgroundColor: "var(--color-elevated)",
              border: "1px solid var(--color-border)",
              cursor: "pointer",
            }}
          >
            Downsample old data
          </button>
          <span
            className="text-xs"
            style={{ color: "var(--color-text-muted)" }}
          >
            Averages telemetry older than 1 hour into 1-minute buckets.
            Retention pruning and cap-based FIFO run automatically every minute.
          </span>
        </div>
      </Card>
    </div>
  );
}
