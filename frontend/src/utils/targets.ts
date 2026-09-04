/* Pure helpers for working with the Target list. Extracted from App.tsx
   so they can be unit-tested in isolation. */

export interface Target {
  id: string;
  name: string;
  host: string;
  port: number;
  connected: boolean;
  capabilities?: string[];
  /** Dashboard display policy served from this target's config. */
  health_nonzero_bad?: string[];
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

/**
 * Shallow-equal two target arrays by the fields the UI actually uses.
 * Used by the targets-polling effect to skip state updates when the
 * server response hasn't changed -- avoids re-rendering every page on
 * the 3-second poll interval.
 */
export function targetsEqual(a: Target[], b: Target[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    const x = a[i];
    const y = b[i];
    if (
      x.id !== y.id ||
      x.name !== y.name ||
      x.host !== y.host ||
      x.port !== y.port ||
      x.connected !== y.connected
    ) {
      return false;
    }
  }
  return true;
}
