/** Pure helpers for the Storage page's pressure readouts. */

/** 0..100 fill percentage, clamped (the file can briefly exceed the
 *  cap between maintenance ticks). */
export function fillPercent(sizeBytes: number, capBytes: number): number {
  if (capBytes <= 0) return 0;
  return Math.min(100, (sizeBytes / capBytes) * 100);
}

/** Signed human fill rate, e.g. "+1.2 MB/min", "-340 KB/min",
 *  "steady". Values under 1 KB/min read as steady: WAL jitter, not
 *  trend. */
export function fillRateLabel(bytesPerMin: number): string {
  const abs = Math.abs(bytesPerMin);
  if (abs < 1024) return "steady";
  const sign = bytesPerMin > 0 ? "+" : "-";
  if (abs >= 1024 * 1024) return `${sign}${(abs / 1048576).toFixed(1)} MB/min`;
  return `${sign}${(abs / 1024).toFixed(0)} KB/min`;
}

/** Compact duration for projections and retention spans:
 *  "42s", "12m", "3.5h", "2.1d". */
export function formatDuration(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  if (seconds < 86400) return `${(seconds / 3600).toFixed(1)}h`;
  return `${(seconds / 86400).toFixed(1)}d`;
}

/** What the cap projection means for an operator. null projection is
 *  a healthy signal (shrinking, holding, or too young to measure),
 *  not missing data. */
export function timeToCapLabel(
  projectedSecs: number | null,
  fillBytesPerMin: number,
  sizeBytes: number,
  capBytes: number,
): string {
  if (sizeBytes >= capBytes) return "at cap (FIFO active)";
  if (projectedSecs !== null) return `~${formatDuration(projectedSecs)} to cap`;
  if (fillBytesPerMin < -1024) return "shrinking";
  return "holding steady";
}
