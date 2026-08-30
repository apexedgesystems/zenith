import { memo, useCallback, useEffect, useRef, useState, useMemo } from "react";
import { computePadL, pixelToTime } from "./chartGeometry";
import { COLORS, fieldName, exportPng, exportCsv } from "../types/telemetry";
import type { ThresholdDef, ChannelData } from "../types/telemetry";

interface Props {
  channels: string[];
  data: ChannelData;
  title: string;
  height?: number;
  timeWindowMs: number;
  hiddenChannels: Set<string>;
  yMin?: number | null;
  yMax?: number | null;
  yLabel?: string | null;
  thresholds?: ThresholdDef[] | null;
  onRemoveChannel: (ch: string) => void;
  onToggleChannel: (ch: string) => void;
}

const MultiChart = memo(function MultiChart({
  channels,
  data,
  title,
  height = 180,
  timeWindowMs,
  hiddenChannels,
  yMin,
  yMax,
  yLabel,
  thresholds,
  onRemoveChannel,
  onToggleChannel,
}: Props) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const [crosshair, setCrosshair] = useState<{
    x: number;
    values: {
      ch: string;
      v: number;
      color: string;
      // Present when the reading comes from a tiered envelope bucket.
      range?: [number, number];
    }[];
  } | null>(null);
  const rafRef = useRef(0);
  // The renderer's actual left pad, for pixel->time mapping in the
  // pointer handler -- a diverged copy shifts every readout.
  const padLRef = useRef(60);

  // Visible channels (filter hidden)
  const visibleChannels = useMemo(
    () => channels.filter((ch) => !hiddenChannels.has(ch)),
    [channels, hiddenChannels],
  );

  const draw = useCallback(() => {
    const canvas = canvasRef.current;
    const container = containerRef.current;
    if (!canvas || !container) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;
    const rect = container.getBoundingClientRect();
    const w = rect.width;
    const h = height;
    const W = Math.round(w * dpr);
    const H = Math.round(h * dpr);
    if (canvas.width !== W || canvas.height !== H) {
      canvas.width = W;
      canvas.height = H;
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    const PAD_R = 10,
      PAD_T = 14,
      PAD_B = 22;
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = "#22262e";
    ctx.fillRect(0, 0, w, h);

    // Time window anchored to latest data
    let globalMaxT = -Infinity;
    for (const ch of visibleChannels) {
      const pts = data[ch];
      if (!pts || pts.length === 0) continue;
      if (pts[pts.length - 1].t > globalMaxT)
        globalMaxT = pts[pts.length - 1].t;
    }
    if (!isFinite(globalMaxT)) return;
    const rangeT = timeWindowMs;
    const globalMinT = globalMaxT - rangeT;

    // Y range from visible window
    let minV = Infinity,
      maxV = -Infinity;
    for (const ch of visibleChannels) {
      const pts = data[ch];
      if (!pts) continue;
      for (const p of pts) {
        if (p.t < globalMinT) continue;
        // Envelope points scale by their full spread so a spike
        // preserved by tiering is never clipped out of view.
        if ((p.lo ?? p.v) < minV) minV = p.lo ?? p.v;
        if ((p.hi ?? p.v) > maxV) maxV = p.hi ?? p.v;
      }
    }
    if (!isFinite(minV)) {
      minV = -1;
      maxV = 1;
    }
    // Dynamic left padding based on Y-axis label width
    const PAD_L = computePadL(minV, maxV);
    padLRef.current = PAD_L;
    if (yMin != null) minV = yMin;
    if (yMax != null) maxV = yMax;
    if (maxV - minV < 0.001) {
      minV -= 1;
      maxV += 1;
    }
    const rangeV = maxV - minV;
    const plotW = w - PAD_L - PAD_R;
    const plotH = h - PAD_T - PAD_B;

    // Grid
    ctx.strokeStyle = "#373e47";
    ctx.lineWidth = 0.5;
    for (let i = 0; i <= 4; i++) {
      const y = PAD_T + (plotH * i) / 4;
      ctx.beginPath();
      ctx.moveTo(PAD_L, y);
      ctx.lineTo(w - PAD_R, y);
      ctx.stroke();
    }
    const nVTicks = Math.max(2, Math.floor(plotW / 100));
    ctx.setLineDash([3, 3]);
    for (let i = 1; i < nVTicks; i++) {
      const x = PAD_L + (plotW * i) / nVTicks;
      ctx.beginPath();
      ctx.moveTo(x, PAD_T);
      ctx.lineTo(x, h - PAD_B);
      ctx.stroke();
    }
    ctx.setLineDash([]);

    // X-axis time labels
    ctx.fillStyle = "#6e7681";
    ctx.font = "9px monospace";
    ctx.textAlign = "center";
    for (let i = 0; i <= nVTicks; i++) {
      const x = PAD_L + (plotW * i) / nVTicks;
      const t = globalMinT + (rangeT * i) / nVTicks;
      const d = new Date(t);
      ctx.fillText(
        `${d.getUTCHours().toString().padStart(2, "0")}:${d
          .getUTCMinutes()
          .toString()
          .padStart(2, "0")}:${d.getUTCSeconds().toString().padStart(2, "0")}`,
        x,
        h - 3,
      );
    }

    // Y-axis labels
    ctx.fillStyle = "#6e7681";
    ctx.font = "10px monospace";
    ctx.textAlign = "right";
    for (let i = 0; i <= 4; i++) {
      const y = PAD_T + (plotH * i) / 4;
      const yVal = maxV - (rangeV * i) / 4;
      const yLabel =
        Math.abs(yVal) >= 1e6
          ? (yVal / 1e6).toFixed(1) + "M"
          : Math.abs(yVal) >= 1e3
            ? (yVal / 1e3).toFixed(1) + "K"
            : yVal.toFixed(2);
      ctx.fillText(yLabel, PAD_L - 5, y + 3);
    }

    // Clip + draw channels with pixel-level decimation.
    // For each pixel column, track min/max values and draw both to
    // preserve the signal envelope at any zoom level.
    ctx.save();
    ctx.beginPath();
    ctx.rect(PAD_L, PAD_T, plotW, plotH);
    ctx.clip();
    visibleChannels.forEach((ch, ci) => {
      const pts = data[ch];
      if (!pts || pts.length === 0) return;
      ctx.strokeStyle = COLORS[ci % COLORS.length];
      ctx.lineWidth = 1.5;
      ctx.beginPath();

      let first = true;
      let lastPixelX = -1;
      let pixelMinV = Infinity;
      let pixelMaxV = -Infinity;

      const emitPixel = (px: number) => {
        if (pixelMinV === Infinity || !ctx) return;
        const yMin = PAD_T + ((maxV - pixelMaxV) / rangeV) * plotH;
        const yMax = PAD_T + ((maxV - pixelMinV) / rangeV) * plotH;
        if (first) {
          ctx.moveTo(px, yMin);
          first = false;
        } else {
          ctx.lineTo(px, yMin);
        }
        if (yMin !== yMax) ctx.lineTo(px, yMax);
      };

      for (const p of pts) {
        if (p.t < globalMinT - 1000) continue;
        const x = Math.round(PAD_L + ((p.t - globalMinT) / rangeT) * plotW);
        // Envelope points contribute their bucket spread -- the same
        // min/max machinery that preserves dense raw signals renders
        // tiered history as its excursion band.
        const lo = p.lo ?? p.v;
        const hi = p.hi ?? p.v;
        if (x === lastPixelX) {
          // Same pixel column: accumulate min/max
          if (lo < pixelMinV) pixelMinV = lo;
          if (hi > pixelMaxV) pixelMaxV = hi;
        } else {
          // New pixel column: emit previous, start new
          emitPixel(lastPixelX);
          lastPixelX = x;
          pixelMinV = lo;
          pixelMaxV = hi;
        }
      }
      emitPixel(lastPixelX); // emit last pixel
      ctx.stroke();
    });
    ctx.restore();

    // Thresholds
    if (thresholds) {
      for (const th of thresholds) {
        const y = PAD_T + ((maxV - th.value) / rangeV) * plotH;
        if (y >= PAD_T && y <= h - PAD_B) {
          ctx.strokeStyle = th.color;
          ctx.lineWidth = 1;
          ctx.setLineDash([6, 3]);
          ctx.beginPath();
          ctx.moveTo(PAD_L, y);
          ctx.lineTo(w - PAD_R, y);
          ctx.stroke();
          ctx.setLineDash([]);
          if (th.label) {
            ctx.fillStyle = th.color;
            ctx.font = "9px monospace";
            ctx.textAlign = "right";
            ctx.fillText(th.label, w - PAD_R - 2, y - 3);
          }
        }
      }
    }

    // Y-axis unit label
    if (yLabel) {
      ctx.fillStyle = "#6e7681";
      ctx.font = "9px monospace";
      ctx.textAlign = "left";
      ctx.fillText(yLabel, PAD_L + 2, h - 2);
    }
  }, [
    visibleChannels,
    data,
    height,
    yMin,
    yMax,
    yLabel,
    thresholds,
    timeWindowMs,
  ]);

  // Stable handle to the latest draw function (ref so the ResizeObserver
  // effect below can stay set up once with [] deps). Without this, the
  // observer is destroyed and recreated on every WS flush because `draw`
  // is a useCallback whose identity changes whenever its deps change.
  const drawRef = useRef(draw);
  drawRef.current = draw;

  // Crosshair on its own overlay canvas: pointer motion repaints a
  // dashed line and a few labels instead of re-stroking every series
  // (formerly up to 6000 points per channel per mousemove).
  const overlayRef = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    const overlay = overlayRef.current;
    const ctx = overlay?.getContext("2d");
    if (!overlay || !ctx) return;
    const rect = overlay.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    const W = Math.round(rect.width * dpr);
    const H = Math.round(rect.height * dpr);
    if (overlay.width !== W || overlay.height !== H) {
      overlay.width = W;
      overlay.height = H;
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, rect.width, rect.height);
    if (!crosshair) return;
    const PAD_T = 14,
      PAD_B = 22;
    ctx.strokeStyle = "rgba(230,237,243,0.3)";
    ctx.lineWidth = 1;
    ctx.setLineDash([4, 4]);
    ctx.beginPath();
    ctx.moveTo(crosshair.x, PAD_T);
    ctx.lineTo(crosshair.x, rect.height - PAD_B);
    ctx.stroke();
    ctx.setLineDash([]);
    let yOff = PAD_T + 12;
    for (const cv of crosshair.values) {
      ctx.fillStyle = cv.color;
      ctx.font = "bold 10px monospace";
      ctx.textAlign = "left";
      const lbl = cv.range
        ? `${fieldName(cv.ch)}: ${cv.v.toFixed(4)} [${cv.range[0].toFixed(
            2,
          )}..${cv.range[1].toFixed(2)}]`
        : `${fieldName(cv.ch)}: ${cv.v.toFixed(4)}`;
      const textX =
        crosshair.x + 8 > rect.width - 120
          ? crosshair.x - 8 - ctx.measureText(lbl).width
          : crosshair.x + 8;
      ctx.fillText(lbl, textX, yOff);
      yOff += 13;
    }
  }, [crosshair]);

  // Batched draw via requestAnimationFrame
  useEffect(() => {
    cancelAnimationFrame(rafRef.current);
    rafRef.current = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(rafRef.current);
  }, [draw]);

  useEffect(() => {
    const obs = new ResizeObserver(() => {
      cancelAnimationFrame(rafRef.current);
      rafRef.current = requestAnimationFrame(() => drawRef.current());
    });
    if (containerRef.current) obs.observe(containerRef.current);
    return () => obs.disconnect();
  }, []);

  const handleMouseMove = useCallback(
    (e: React.MouseEvent<HTMLCanvasElement>) => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const rect = canvas.getBoundingClientRect();
      const mx = e.clientX - rect.left;

      let globalMaxT = -Infinity;
      for (const ch of visibleChannels) {
        const pts = data[ch];
        if (!pts || pts.length === 0) continue;
        if (pts[pts.length - 1].t > globalMaxT)
          globalMaxT = pts[pts.length - 1].t;
      }
      if (!isFinite(globalMaxT)) return;
      const globalMinT = globalMaxT - timeWindowMs;
      const tAtMouse = pixelToTime(
        mx,
        rect.width,
        padLRef.current,
        globalMinT,
        globalMaxT,
      );
      if (tAtMouse === null) {
        setCrosshair(null);
        return;
      }

      const values: {
        ch: string;
        v: number;
        color: string;
        range?: [number, number];
      }[] = [];
      visibleChannels.forEach((ch, ci) => {
        const pts = data[ch];
        if (!pts || pts.length === 0) return;
        let lo = 0,
          hi = pts.length - 1;
        while (lo < hi - 1) {
          const mid = (lo + hi) >> 1;
          if (pts[mid].t < tAtMouse) lo = mid;
          else hi = mid;
        }
        const p0 = pts[lo],
          p1 = pts[hi];
        const frac = p1.t === p0.t ? 0 : (tAtMouse - p0.t) / (p1.t - p0.t);
        // Envelope readout comes from the nearer point: it makes the
        // resolution explicit -- an operator reading a tiered bucket
        // sees mean plus spread, not a false point sample.
        const near = frac < 0.5 ? p0 : p1;
        values.push({
          ch,
          v: p0.v + frac * (p1.v - p0.v),
          color: COLORS[ci % COLORS.length],
          ...(near.lo !== undefined && near.hi !== undefined
            ? { range: [near.lo, near.hi] as [number, number] }
            : {}),
        });
      });
      setCrosshair({ x: mx, values });
    },
    [visibleChannels, data, timeWindowMs],
  );

  // Stats: single-pass min/max/sum. Keep it allocation-free -- a
  // filter/map/spread chain here allocates three arrays per channel
  // per render, noticeable on the WS hot path.
  const stats = useMemo(() => {
    // Window anchored to the newest DATA timestamp, not wall clock:
    // Date.now() inside a memo without it in deps freezes at whatever
    // moment the memo last ran, and wall-anchoring would silently
    // empty the stats when a target pauses. Stats describe what the
    // chart shows.
    let newest = -Infinity;
    for (const ch of visibleChannels) {
      const pts = data[ch];
      if (pts && pts.length > 0 && pts[pts.length - 1].t > newest)
        newest = pts[pts.length - 1].t;
    }
    const cutoff = newest - timeWindowMs;
    const result: { ch: string; min: number; max: number; mean: number }[] = [];
    for (const ch of visibleChannels) {
      const pts = data[ch];
      if (!pts || pts.length === 0) continue;
      let min = Infinity;
      let max = -Infinity;
      let sum = 0;
      let count = 0;
      for (let i = 0; i < pts.length; i++) {
        const p = pts[i];
        if (p.t < cutoff) continue;
        if (p.v < min) min = p.v;
        if (p.v > max) max = p.v;
        sum += p.v;
        count++;
      }
      if (count > 0) {
        result.push({ ch, min, max, mean: sum / count });
      }
    }
    return result;
  }, [visibleChannels, data, timeWindowMs]);

  return (
    <div>
      {/* Title + legend */}
      <div className="flex items-center justify-between mb-1">
        <div className="flex items-center gap-2">
          <span
            className="text-xs font-bold"
            style={{ color: "var(--color-text-secondary)" }}
          >
            {title}
          </span>
          <button
            onClick={() =>
              canvasRef.current && exportPng(canvasRef.current, title)
            }
            className="text-[9px] px-1"
            style={{
              color: "var(--color-text-muted)",
              background: "none",
              border: "none",
              cursor: "pointer",
            }}
          >
            PNG
          </button>
          <button
            onClick={() => exportCsv(channels, data, title)}
            className="text-[9px] px-1"
            style={{
              color: "var(--color-text-muted)",
              background: "none",
              border: "none",
              cursor: "pointer",
            }}
          >
            CSV
          </button>
        </div>
        <div className="flex items-center gap-3 flex-wrap">
          {channels.map((ch, ci) => {
            const isHidden = hiddenChannels.has(ch);
            const lastVal = data[ch]?.[data[ch].length - 1]?.v;
            return (
              <div
                key={ch}
                className="flex items-center gap-1"
                style={{ opacity: isHidden ? 0.35 : 1 }}
              >
                <div
                  className="w-2 h-2 rounded-full cursor-pointer"
                  style={{ backgroundColor: COLORS[ci % COLORS.length] }}
                  onClick={() => onToggleChannel(ch)}
                  title="Toggle visibility"
                />
                <span
                  className="mono text-[10px] cursor-pointer"
                  style={{ color: "var(--color-text-muted)" }}
                  onClick={() => onToggleChannel(ch)}
                >
                  {fieldName(ch)}
                </span>
                {lastVal !== undefined && !isHidden && (
                  <span
                    className="mono text-[10px] font-bold"
                    style={{ color: "var(--color-text-primary)" }}
                  >
                    {lastVal.toFixed(3)}
                  </span>
                )}
                <button
                  onClick={() => onRemoveChannel(ch)}
                  style={{
                    fontSize: "0.55rem",
                    color: "var(--color-text-muted)",
                    background: "none",
                    border: "none",
                    cursor: "pointer",
                    padding: "0 2px",
                  }}
                >
                  x
                </button>
              </div>
            );
          })}
        </div>
      </div>
      <div ref={containerRef} style={{ position: "relative" }}>
        <canvas
          ref={canvasRef}
          style={{
            width: "100%",
            height: `${height}px`,
            borderRadius: "6px",
            border: "1px solid var(--color-border)",
          }}
        />
        <canvas
          ref={overlayRef}
          onMouseMove={handleMouseMove}
          onMouseLeave={() => setCrosshair(null)}
          style={{
            position: "absolute",
            inset: 0,
            width: "100%",
            height: `${height}px`,
            cursor: "crosshair",
          }}
        />
      </div>
      {stats.length > 0 && (
        <div className="flex gap-4 mt-0.5 px-1">
          {stats.map((s, i) => (
            <div
              key={s.ch}
              className="mono text-[9px]"
              style={{ color: COLORS[i % COLORS.length] }}
            >
              min:{s.min.toFixed(2)} max:{s.max.toFixed(2)} avg:
              {s.mean.toFixed(2)}
            </div>
          ))}
        </div>
      )}
    </div>
  );
});

export default MultiChart;
