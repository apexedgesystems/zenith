/** Server state, declared once.
 *
 *  Every polling loop in the app collapses into these hooks: one
 *  cache, one in-flight request per key regardless of how many
 *  components subscribe, automatic cancellation on unmount or target
 *  switch, and error states that surface instead of vanishing into
 *  empty catch blocks.
 */

import { useQuery } from "@tanstack/react-query";
import { arrayOf, field, isObject, request, Validator } from "./client";

/* ------------------------------ targets ------------------------------ */

export interface Target {
  id: string;
  name: string;
  host: string;
  port: number;
  connected: boolean;
  /** Command-surface capabilities the target's dictionaries declare
   *  (e.g. "readback"). Absent on older backends. */
  capabilities?: string[];
  /** Dashboard display policy served from this target's config. */
  health_nonzero_bad?: string[];
}

const isTarget: Validator<Target> = (v) => {
  if (!isObject(v)) return "expected object";
  return (
    field(v, "id", "string") ??
    field(v, "name", "string") ??
    field(v, "host", "string") ??
    field(v, "port", "number") ??
    field(v, "connected", "boolean")
  );
};

const isTargetsResponse: Validator<{ targets: Target[] }> = (v) => {
  if (!isObject(v)) return "expected object";
  return arrayOf(v.targets, isTarget, "targets");
};

/** The one /api/targets poller for the whole app. Keep it singular:
 *  parallel per-page pollers each develop their own divergent notion
 *  of "connected" that can disagree for seconds. */
export function useTargets() {
  return useQuery({
    queryKey: ["targets"],
    queryFn: () => request("/targets", isTargetsResponse),
    refetchInterval: 3000,
    select: (d) => d.targets,
  });
}

/* ------------------------------ metrics ------------------------------ */

export interface PipelineMetrics {
  decoded_samples: number;
  dedup_drops: number;
  router_lag_drops: number;
  db_writer_lag_drops: number;
  db_written_samples: number;
  db_write_failures: number;
  db_failed_samples: number;
  ws_lag_drops: number;
  ws_clients: number;
  commands_sent: number;
  command_errors: number;
  command_timeouts: number;
  command_latency_avg_us: number;
  last_sample_age_ms: number | null;
  connected: boolean;
}

const isMetrics: Validator<PipelineMetrics> = (v) => {
  if (!isObject(v)) return "expected object";
  for (const k of [
    "decoded_samples",
    "db_written_samples",
    "db_write_failures",
    "commands_sent",
  ]) {
    const p = field(v, k, "number");
    if (p !== null) return p;
  }
  return field(v, "connected", "boolean");
};

export function useTargetMetrics(targetId: string | null) {
  return useQuery({
    queryKey: ["metrics", targetId],
    enabled: targetId !== null,
    refetchInterval: 2000,
    queryFn: () =>
      request(`/metrics`, (v) => {
        if (!isObject(v) || !isObject(v.targets)) return "expected targets map";
        const t = (v.targets as Record<string, unknown>)[targetId as string];
        if (t === undefined) return null; // unknown target: absent, not invalid
        return isMetrics(t);
      }),
    select: (d) =>
      (d as { targets: Record<string, PipelineMetrics> }).targets[
        targetId as string
      ] ?? null,
  });
}

/* ------------------------------ storage ------------------------------ */

export interface TargetStorage {
  sample_count: number;
  channel_count: number;
  byte_estimate: number;
  oldest_ms: number | null;
  newest_ms: number | null;
  span_seconds: number | null;
}

const isStorage: Validator<TargetStorage> = (v) => {
  if (!isObject(v)) return "expected object";
  return (
    field(v, "sample_count", "number") ??
    field(v, "channel_count", "number") ??
    field(v, "byte_estimate", "number")
  );
};

/** Storage for every listed target in one keyed query (the sidebar's
 *  10 s usage readout). Structural sharing suppresses re-renders when
 *  nothing moved. */
export function useAllTargetStorage(targetIds: string[]) {
  return useQuery({
    queryKey: ["storage-all", targetIds],
    enabled: targetIds.length > 0,
    refetchInterval: 10_000,
    queryFn: async () => {
      const out: Record<string, TargetStorage> = {};
      await Promise.allSettled(
        targetIds.map(async (id) => {
          out[id] = await request(`/targets/${id}/storage`, isStorage);
        }),
      );
      return out;
    },
  });
}

export function useTargetStorage(targetId: string | null) {
  return useQuery({
    queryKey: ["storage", targetId],
    enabled: targetId !== null,
    refetchInterval: 10_000,
    queryFn: () => request(`/targets/${targetId}/storage`, isStorage),
  });
}

/* ------------------------------ storage vitals ------------------------------ */

export interface StorageVitals {
  total_samples: number;
  db_size_bytes: number;
  wal_bytes: number;
  audit_rows: number;
  cap_bytes: number;
  fill_bytes_per_min: number;
  projected_secs_to_cap: number | null;
  fifo_evicted_samples: number;
  retention_pruned_samples: number;
  tiers: TierVitals;
}

/** Retention-ladder state: config echo plus live band populations
 *  ([full-res window, mid horizon, older]) and the cumulative rows
 *  converted to envelope buckets. */
export interface TierVitals {
  enabled: boolean;
  full_resolution_minutes: number;
  mid_bucket_seconds: number;
  mid_horizon_hours: number;
  coarse_bucket_seconds: number;
  converted_rows: number;
  band_rows: number[];
}

const isStorageVitals: Validator<StorageVitals> = (v) => {
  if (!isObject(v)) return "expected object";
  for (const k of [
    "total_samples",
    "db_size_bytes",
    "wal_bytes",
    "audit_rows",
    "cap_bytes",
    "fill_bytes_per_min",
    "fifo_evicted_samples",
    "retention_pruned_samples",
  ]) {
    const p = field(v, k, "number");
    if (p !== null) return p;
  }
  // projected_secs_to_cap is number | null by design.
  const p = v.projected_secs_to_cap;
  if (p !== null && typeof p !== "number") {
    return "projected_secs_to_cap: expected number or null";
  }
  if (!isObject(v.tiers)) return "tiers: expected object";
  const t = v.tiers;
  for (const k of ["full_resolution_minutes", "converted_rows"]) {
    const problem = field(t, k, "number");
    if (problem !== null) return `tiers.${problem}`;
  }
  if (!Array.isArray(t.band_rows)) return "tiers.band_rows: expected array";
  return field(t, "enabled", "boolean") === null
    ? null
    : "tiers.enabled: expected boolean";
};

/** Global storage pressure for the Storage page: cap, fill rate,
 *  projection, and cumulative eviction counters. */
export function useStorageVitals() {
  return useQuery({
    queryKey: ["storage-vitals"],
    refetchInterval: 5000,
    queryFn: () => request("/telemetry/stats", isStorageVitals),
  });
}

/** The whole per-target metrics map in one request -- the Storage
 *  page's accounting strip needs every target at once, and /metrics
 *  already returns them all. */
export function useAllMetrics() {
  return useQuery({
    queryKey: ["metrics-all"],
    refetchInterval: 5000,
    queryFn: () =>
      request(`/metrics`, (v) =>
        isObject(v) && isObject(v.targets) ? null : "expected targets map",
      ),
    select: (d) =>
      (d as { targets: Record<string, PipelineMetrics> }).targets ?? {},
  });
}

/* ------------------------------ registry ------------------------------ */

export interface RegistryComponent {
  fullUid: string;
  name: string;
  type: string;
  reachable: boolean;
}

const isRegistryComponent: Validator<RegistryComponent> = (v) => {
  if (!isObject(v)) return "expected object";
  return (
    field(v, "fullUid", "string") ??
    field(v, "name", "string") ??
    field(v, "reachable", "boolean")
  );
};

/** Component registry for a target. One cache entry shared by every
 *  page that shows components (Dashboard, Tunables, ...). Callers that
 *  need reconnect-freshness invalidate on connect; disconnect eviction
 *  removes it by key prefix. */
export function useRegistry(targetId: string | null, enabled = true) {
  return useQuery({
    queryKey: ["registry", targetId],
    enabled: !!targetId && enabled,
    queryFn: () =>
      request(`/targets/${targetId}/registry`, (v) => {
        if (!isObject(v)) return "expected object";
        return arrayOf(v.components, isRegistryComponent, "components");
      }),
    select: (d) => (d as { components: RegistryComponent[] }).components,
  });
}

/** TUNABLE_PARAM field definitions for one component, from the
 *  per-target struct-dict endpoint. */
export function useTunableFields(
  targetId: string | null,
  component: string | null,
) {
  return useQuery({
    queryKey: ["tunable-fields", targetId, component],
    enabled: !!targetId && !!component,
    staleTime: Infinity,
    queryFn: async () => {
      const data = await request(
        `/targets/${targetId}/structs/${encodeURIComponent(
          component as string,
        )}`,
        (v) => (isObject(v) ? null : "expected object"),
      );
      const structs =
        (data as { structs?: Record<string, unknown> }).structs ?? {};
      for (const sdef of Object.values(structs)) {
        const def = sdef as { category?: string; fields?: unknown[] };
        if (def.category === "TUNABLE_PARAM" && (def.fields?.length ?? 0) > 0) {
          return def.fields as unknown[];
        }
      }
      return [] as unknown[];
    },
  });
}
