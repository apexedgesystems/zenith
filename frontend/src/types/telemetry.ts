/* Shared types, constants, and helpers for telemetry visualization. */

export interface Sample {
  target_id: string;
  timestamp_ms: number;
  channel: string;
  value: number;
}

export interface ThresholdDef {
  value: number;
  color: string;
  label?: string;
}

export interface PlotDef {
  title: string;
  channels: string[];
  height?: number;
  position?: [number, number];
  y_min?: number | null;
  y_max?: number | null;
  y_label?: string | null;
  thresholds?: ThresholdDef[] | null;
  time_window_ms?: number | null;
}

export interface TelemetryLayout {
  id?: number;
  name: string;
  source?: string;
  grid?: string;
  time_window_s?: number;
  plots: PlotDef[];
  /** Channel references the target's current dictionaries cannot
   *  produce (stale after a target-config refresh). */
  unknown_channels?: string[];
}

export const COLORS = [
  "#58a6ff",
  "#3fb950",
  "#d29922",
  "#f778ba",
  "#bc8cff",
  "#f0883e",
  "#a5d6ff",
  "#7ee787",
  "#e3b341",
  "#ff9bce",
];

export const TIME_WINDOWS = [
  { label: "10s", ms: 10000 },
  { label: "30s", ms: 30000 },
  { label: "1m", ms: 60000 },
  { label: "5m", ms: 300000 },
  { label: "30m", ms: 1800000 },
  { label: "1h", ms: 3600000 },
];

export type ChannelData = Record<string, { t: number; v: number }[]>;

export function fieldName(ch: string): string {
  const dot = ch.indexOf(".");
  return dot > 0 ? ch.slice(dot + 1) : ch;
}

export function groupChannels(channels: string[]): Record<string, string[]> {
  const groups: Record<string, string[]> = {};
  for (const ch of channels) {
    const dot = ch.indexOf(".");
    const group = dot > 0 ? ch.slice(0, dot) : "Other";
    if (!groups[group]) groups[group] = [];
    groups[group].push(ch);
  }
  return groups;
}

export function exportPng(canvas: HTMLCanvasElement, title: string) {
  const link = document.createElement("a");
  link.download = `${title.replace(/\s+/g, "_")}_${new Date()
    .toISOString()
    .slice(0, 19)}.png`;
  link.href = canvas.toDataURL("image/png");
  link.click();
}

export function exportCsv(
  channels: string[],
  data: ChannelData,
  title: string,
) {
  const rows: string[] = ["timestamp_ms," + channels.map(fieldName).join(",")];
  const allTs = new Set<number>();
  for (const ch of channels) {
    for (const p of data[ch] || []) allTs.add(p.t);
  }
  const sorted = [...allTs].sort((a, b) => a - b);
  const chMaps = channels.map((ch) => {
    const m = new Map<number, number>();
    for (const p of data[ch] || []) m.set(p.t, p.v);
    return m;
  });
  for (const t of sorted) {
    rows.push(t + "," + chMaps.map((m) => m.get(t) ?? "").join(","));
  }
  const blob = new Blob([rows.join("\n")], { type: "text/csv" });
  const link = document.createElement("a");
  link.download = `${title.replace(/\s+/g, "_")}_${new Date()
    .toISOString()
    .slice(0, 19)}.csv`;
  link.href = URL.createObjectURL(blob);
  link.click();
  URL.revokeObjectURL(link.href);
}
