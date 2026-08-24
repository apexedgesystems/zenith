/** Chart geometry shared by the renderer and the pointer handlers.
 *
 *  The left padding depends on Y-label width, so any code mapping
 *  pixels to time MUST use the same computation the renderer used --
 *  a diverging copy shifts every crosshair readout onto the wrong
 *  timestamp.
 */

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
