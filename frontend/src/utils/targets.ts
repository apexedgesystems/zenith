/* A target as the target list serves it, and the byte and count
   formatters the sidebar and the Storage page share. */
import type { HealthRule } from "./health";

export interface Target {
  id: string;
  name: string;
  host: string;
  port: number;
  connected: boolean;
  /** A listening carrier (tcp-listen or udp) is bound; with `connected`
   *  false it is waiting for its target to dial in or be heard. */
  listening?: boolean;
  capabilities?: string[];
  /** Dashboard display policy served from this target's config. */
  health?: HealthRule[];
}

/** Format a byte count as a short human-readable string (e.g. "1.2 MB"). */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

/** Format a sample count compactly (1.2K, 3.4M). */
export function formatCount(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}K`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}
