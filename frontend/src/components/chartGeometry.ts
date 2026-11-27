/** Chart geometry shared by the renderer and the pointer handlers.
 *
 *  The left padding depends on Y-label width, so any code mapping
 *  pixels to time MUST use the same computation the renderer used --
 *  a diverging copy shifts every crosshair readout onto the wrong
 *  timestamp.
 */

import type { ChannelData } from "../types/telemetry";

/** One plotted point: a sample, or a tiered bucket's mean and envelope. */
type Point = ChannelData[string][number];

export const PAD_R = 10;

/** Left padding sized to the widest Y-axis label. */
export function computePadL(minV: number, maxV: number): number {
  const maxLabel = Math.max(Math.abs(minV), Math.abs(maxV));
  const labelLen =
    maxLabel >= 1000000 ? 10 : maxLabel >= 1000 ? 8 : maxLabel >= 1 ? 6 : 7;
  return Math.max(50, labelLen * 7 + 10);
}

/** Map a pointer x (CSS pixels, canvas-relative) to a timestamp in the
 *  plotted window. Returns null outside the plot region. */
export function pixelToTime(
  mx: number,
  width: number,
  padL: number,
  tMin: number,
  tMax: number,
): number | null {
  const plotW = width - padL - PAD_R;
  if (plotW <= 0 || mx < padL || mx > width - PAD_R) return null;
  return tMin + ((mx - padL) / plotW) * (tMax - tMin);
}

/** A channel's reading at time `t` for the crosshair: linear between
 *  the samples either side of it, with the nearer of the two (a tiered
 *  bucket's readout shows that point's envelope, so the operator sees
 *  mean and spread rather than a false point sample). Null before the
 *  channel's first sample and after its last: the readout never shows
 *  a value the channel did not have. */
export function readoutAt(
  pts: readonly Point[],
  t: number,
): { v: number; near: Point } | null {
  if (pts.length === 0 || t < pts[0].t || t > pts[pts.length - 1].t) {
    return null;
  }
  let lo = 0;
  let hi = pts.length - 1;
  while (lo < hi - 1) {
    const mid = (lo + hi) >> 1;
    if (pts[mid].t < t) lo = mid;
    else hi = mid;
  }
  const p0 = pts[lo];
  const p1 = pts[hi];
  const frac = p1.t === p0.t ? 0 : (t - p0.t) / (p1.t - p0.t);
  return { v: p0.v + frac * (p1.v - p0.v), near: frac < 0.5 ? p0 : p1 };
}
