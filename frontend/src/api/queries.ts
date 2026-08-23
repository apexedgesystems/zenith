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

/** The one /api/targets poller. Three independent copies of this loop
 *  used to run simultaneously (App shell, reconnect probe, whichever
 *  page was mounted), each with its own divergent notion of
 *  "connected". */
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
