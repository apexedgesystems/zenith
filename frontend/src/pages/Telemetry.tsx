import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useDialogs } from "../components/dialogs";
import MultiChart from "../components/MultiChart";
import PlotContextMenu from "../components/PlotContextMenu";
import {
  type PlotDef,
  type TelemetryLayout,
  type Sample,
  type ChannelData,
  TIME_WINDOWS,
  fieldName,
  groupChannels,
} from "../types/telemetry";

const MAX_LIVE_POINTS = 6000; // WS buffer cap for sidebar live values
const WS_BATCH_MS = 100; // Flush WS batch every 100ms

export default function TelemetryPage({
  selectedTarget,
}: {
  selectedTarget: string;
}) {
  const { promptForm } = useDialogs();
  const [allChannels, setAllChannels] = useState<ChannelData>({});
  const [plots, setPlots] = useState<PlotDef[]>([]);
  const [layouts, setLayouts] = useState<TelemetryLayout[]>([]);
  const [selectedLayout, setSelectedLayout] = useState("");
  const [connected, setConnected] = useState(false);
  const [sampleCount, setSampleCount] = useState(0);
  const [pickerFilter, setPickerFilter] = useState("");
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set());
  const [focusedPlot, setFocusedPlot] = useState(0);
  const [timeWindowMs, setTimeWindowMs] = useState(30000);
  const [gridCols, setGridCols] = useState(1);
  const [hiddenChannels, setHiddenChannels] = useState<Set<string>>(new Set());
  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
    plotIdx: number;
  } | null>(null);
  const [channelMenu, setChannelMenu] = useState<{
    x: number;
    y: number;
    channel: string;
  } | null>(null);
  const channelMenuRef = useRef<HTMLDivElement>(null);
  const [sidebarWidth, setSidebarWidth] = useState(240);
  const [saveMsg, setSaveMsg] = useState<string | null>(null);
  const [dragIdx, setDragIdx] = useState<number | null>(null);

  const [paused, setPaused] = useState(false);
  const pausedRef = useRef(false);
  pausedRef.current = paused;
  // Refs for values the WS flush timer needs (avoids stale closures)
  const timeWindowRef = useRef(timeWindowMs);
  timeWindowRef.current = timeWindowMs;
  const plotsRef = useRef(plots);
  plotsRef.current = plots;
  const draggingRef = useRef(false);

  // Load layouts
  useEffect(() => {
    fetch(`/api/targets/${selectedTarget}/telemetry/layouts`)
      .then((r) => (r.ok ? r.json() : { layouts: [] }))
      .then((data) => {
        setLayouts(data.layouts || []);
        if (data.layouts?.length > 0 && !selectedLayout) {
          setSelectedLayout(data.layouts[0].name);
          setPlots(data.layouts[0].plots);
        }
      })
      .catch(() => {});
  }, [selectedTarget]);

  // Reset plots from a layout when the selected NAME changes. We use a
  // ref-tracked previous name so that an `setLayouts(...)` re-fetch (e.g.
  // after Save) does NOT re-trigger setPlots and clobber any in-flight
  // edits the user made between hitting Save and the round-trip
  // completing. The layout list updating is a no-op for plot state.
  const prevSelectedLayoutRef = useRef<string>("");
  useEffect(() => {
    if (!selectedLayout) return;
    if (prevSelectedLayoutRef.current === selectedLayout) return;
    prevSelectedLayoutRef.current = selectedLayout;
    const layout = layouts.find((l) => l.name === selectedLayout);
    if (layout) {
      setPlots(layout.plots.map((p) => ({ ...p })));
      setFocusedPlot(0);
      setDragIdx(null);
      setHiddenChannels(new Set());
      setContextMenu(null);
      setChannelMenu(null);
    }
  }, [selectedLayout, layouts]);

  // ---- DATA LAYER: Hybrid WS buffer + DB poll ----
  //
  // Small windows (<= 2min): WS buffer provides smooth real-time data
  // Large windows (> 2min): DB poll every 2s for full historical coverage
  // No merge between the two -- chart reads from one source based on window size.

  const batchRef = useRef<Sample[]>([]);
  const liveDataRef = useRef<ChannelData>({});

  // WebSocket: accumulate live data + detect connection
  useEffect(() => {
    setAllChannels({});
    setSampleCount(0);
    setConnected(false);
    setContextMenu(null);
    setChannelMenu(null);
    liveDataRef.current = {};
    batchRef.current = [];

    let wsInstance: WebSocket | null = null;
    let reconnectTimer: number | undefined;
    let reconnectDelay = 1000;
    let stopped = false;

    function connect() {
      if (stopped) return;
      const ws = new WebSocket(
        `${window.location.protocol === "https:" ? "wss" : "ws"}://${
          window.location.host
        }/api/targets/${selectedTarget}/telemetry/live`,
      );
      wsInstance = ws;
      let receivedData = false;

      ws.onopen = () => {
        reconnectDelay = 1000;
        setTimeout(() => {
          if (!receivedData && !stopped) setConnected(false);
        }, 3000);
      };
      ws.onmessage = (event) => {
        if (!receivedData) {
          receivedData = true;
          setConnected(true);
        }
        if (pausedRef.current) return;
        try {
          batchRef.current.push(JSON.parse(event.data));
        } catch {
          /* ignore */
        }
      };
      ws.onclose = () => {
        if (stopped) return;
        setConnected(false);
        reconnectTimer = window.setTimeout(() => {
          reconnectDelay = Math.min(30000, reconnectDelay * 2);
          connect();
        }, reconnectDelay);
      };
    }

    // Flush WS batch into liveDataRef and update state (for small windows)
    const flushTimer = window.setInterval(() => {
      const batch = batchRef.current;
      if (batch.length === 0) return;
      batchRef.current = [];

      const live = liveDataRef.current;
      for (const s of batch) {
        const ch = live[s.channel] || [];
        ch.push({ t: s.timestamp_ms, v: s.value });
        if (ch.length > MAX_LIVE_POINTS) {
          live[s.channel] = ch.slice(-MAX_LIVE_POINTS);
        } else {
          live[s.channel] = ch;
        }
      }
      liveDataRef.current = live;
      setSampleCount((c) => c + batch.length);

      // For non-plotted channels: push full live buffer (sidebar values).
      // For plotted channels: append only the NEWEST live samples to the
      // DB-polled data. This gives smooth real-time at the right edge
      // while the DB provides historical coverage.
      const currentPlots = plotsRef.current;
      const plottedChs = new Set(currentPlots.flatMap((p) => p.channels));
      setAllChannels((prev) => {
        const next = { ...prev };
        for (const [ch, pts] of Object.entries(live)) {
          if (!plottedChs.has(ch)) {
            next[ch] = pts; // sidebar: full live buffer
          } else {
            // Plotted: append live samples newer than the DB data's latest
            const existing = prev[ch] || [];
            const dbLatest =
              existing.length > 0 ? existing[existing.length - 1].t : 0;
            const newPts = pts.filter((p) => p.t > dbLatest);
            if (newPts.length > 0) {
              next[ch] = [...existing, ...newPts];
            }
          }
        }
        return next;
      });
    }, WS_BATCH_MS);

    connect();
    return () => {
      stopped = true;
      if (wsInstance) wsInstance.close();
      if (reconnectTimer) window.clearTimeout(reconnectTimer);
      window.clearInterval(flushTimer);
    };
  }, [selectedTarget]);

  // Stable key for plotted channels (order-independent, memoized)
  const plottedChannelKey = useMemo(
    () => [...new Set(plots.flatMap((p) => p.channels))].sort().join(","),
    [plots],
  );

  // Widest window any plot needs, derived outside the poll effect so a
  // per-plot window change re-drives the fetch -- keying the effect on
  // plot identity would reset it on every rename or threshold edit.
  const maxWindow = Math.max(
    timeWindowMs,
    ...plots.map((p) => p.time_window_ms || 0),
  );

  // DB polling: all plotted channels, all time windows.
  // Poll interval adjusts: 500ms for short windows (smooth), 2s for large.
  useEffect(() => {
    if (paused) return;
    const pollInterval = maxWindow <= 60000 ? 500 : 2000;

    const plottedChannels = [...new Set(plots.flatMap((p) => p.channels))];
    if (plottedChannels.length === 0) return;

    let cancelled = false;
    const controller = new AbortController();

    async function poll() {
      if (cancelled) return;
      const now = Date.now();
      const startMs = now - maxWindow;
      // Fetch all raw data -- pixel decimation happens in the chart renderer.
      // Limit high enough for 1hr at 10Hz = 36000 rows.
      const limit = 50000;

      // Parallelize all channel fetches for speed
      const newData: ChannelData = {};
      await Promise.allSettled(
        plottedChannels.map(async (ch) => {
          if (cancelled) return;
          try {
            const r = await fetch(
              `/api/targets/${selectedTarget}/telemetry/history?channel=${encodeURIComponent(
                ch,
              )}&start_ms=${startMs}&end_ms=${now}&limit=${limit}`,
              { signal: controller.signal },
            );
            if (!r.ok || cancelled) return;
            const resp = await r.json();
            const samples = (resp.samples || []) as {
              timestamp_ms: number;
              value: number;
            }[];
            if (samples.length > 0) {
              // Only update if data arrived
              newData[ch] = samples.map((s) => ({
                t: s.timestamp_ms,
                v: s.value,
              }));
            }
          } catch {
            /* aborted */
          }
        }),
      );

      if (!cancelled && Object.keys(newData).length > 0) {
        setAllChannels((prev) => {
          const merged = { ...prev };
          for (const [ch, pts] of Object.entries(newData)) merged[ch] = pts;
          return merged;
        });
      }
    }

    poll();
    const timer = window.setInterval(poll, pollInterval);
    return () => {
      cancelled = true;
      controller.abort();
      window.clearInterval(timer);
    };
    // `plots` intentionally not in deps -- depends on plottedChannelKey
    // (the SORTED, JOINED string of plotted channels) which is reference-stable
    // when the channel set hasn't actually changed. Including `plots` would
    // re-create the interval on every plot edit (rename, threshold change,
    // etc.) and lose in-flight poll responses.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedTarget, timeWindowMs, maxWindow, paused, plottedChannelKey]);

  // Memoized channel lists
  const availableChannels = useMemo(
    () => Object.keys(allChannels).sort(),
    [allChannels],
  );
  const filteredChannels = useMemo(
    () =>
      pickerFilter
        ? availableChannels.filter((c) =>
            c.toLowerCase().includes(pickerFilter.toLowerCase()),
          )
        : availableChannels,
    [availableChannels, pickerFilter],
  );
  const grouped = useMemo(
    () => groupChannels(filteredChannels),
    [filteredChannels],
  );
  const plottedSet = useMemo(
    () => new Set(plots.flatMap((p) => p.channels)),
    [plots],
  );

  // Callbacks
  const toggleGroup = useCallback((group: string) => {
    setExpandedGroups((prev) => {
      const n = new Set(prev);
      if (n.has(group)) n.delete(group);
      else n.add(group);
      return n;
    });
  }, []);

  const addChannelToPlot = useCallback(
    (ch: string) => {
      setPlots((prev) => {
        if (prev.length === 0) return [{ title: ch, channels: [ch] }];
        return prev
          .map((p, i) => {
            if (i !== focusedPlot) return p;
            if (p.channels.includes(ch))
              return { ...p, channels: p.channels.filter((c) => c !== ch) };
            return { ...p, channels: [...p.channels, ch] };
          })
          .filter((p) => p.channels.length > 0);
      });
    },
    [focusedPlot],
  );

  const removeChannelFromPlot = useCallback((plotIdx: number, ch: string) => {
    setPlots((prev) =>
      prev
        .map((p, i) =>
          i !== plotIdx
            ? p
            : { ...p, channels: p.channels.filter((c) => c !== ch) },
        )
        .filter((p) => p.channels.length > 0),
    );
  }, []);

  // One stable handler per plot index: an inline closure here defeats
  // MultiChart's React.memo on every render, which is exactly what the
  // reference-reuse data slicing exists to enable.
  const removeHandlers = useMemo(
    () => plots.map((_, i) => (ch: string) => removeChannelFromPlot(i, ch)),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [plots.length, removeChannelFromPlot],
  );

  const updatePlot = useCallback((idx: number, changes: Partial<PlotDef>) => {
    setPlots((prev) =>
      prev.map((p, i) => (i === idx ? { ...p, ...changes } : p)),
    );
  }, []);

  const deletePlot = useCallback((idx: number) => {
    setPlots((prev) => {
      const next = prev.filter((_, i) => i !== idx);
      setFocusedPlot((f) => Math.min(f, Math.max(0, next.length - 1)));
      return next;
    });
  }, []);

  const duplicatePlot = useCallback((idx: number) => {
    setPlots((prev) => [
      ...prev,
      { ...prev[idx], title: prev[idx].title + " (copy)" },
    ]);
  }, []);

  const addChannelToSpecificPlot = useCallback(
    (ch: string, plotIdx: number) => {
      setPlots((prev) =>
        prev.map((p, i) => {
          if (i !== plotIdx) return p;
          if (p.channels.includes(ch)) return p; // already there
          return { ...p, channels: [...p.channels, ch] };
        }),
      );
    },
    [],
  );

  const toggleChannelVisibility = useCallback((ch: string) => {
    setHiddenChannels((prev) => {
      const n = new Set(prev);
      if (n.has(ch)) n.delete(ch);
      else n.add(ch);
      return n;
    });
  }, []);

  const MAX_PLOTS = 20;
  const addNewPlot = useCallback(() => {
    setPlots((prev) => {
      if (prev.length >= MAX_PLOTS) return prev;
      return [...prev, { title: "New Plot", channels: [] }];
    });
    setFocusedPlot((prev) => Math.min(prev + 1, MAX_PLOTS - 1));
  }, []);

  // Drag-to-reorder plots
  const handleDragOver = useCallback(
    (e: React.DragEvent, targetIdx: number) => {
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      if (dragIdx === null || dragIdx === targetIdx) return;
      // Only swap once per target (dragIdx updates to targetIdx, preventing re-fire)
      setDragIdx((prevDrag) => {
        if (prevDrag === null || prevDrag === targetIdx) return prevDrag;
        setPlots((prev) => {
          const next = [...prev];
          const [moved] = next.splice(prevDrag, 1);
          next.splice(targetIdx, 0, moved);
          return next;
        });
        return targetIdx;
      });
    },
    [dragIdx],
  );

  const saveLayout = useCallback(async () => {
    const form = await promptForm("Save layout", [
      {
        name: "name",
        label: "Layout name",
        initial: selectedLayout || "My Layout",
        validate: (v) => (v.trim() ? null : "required"),
      },
    ]);
    const name = form?.name?.trim();
    if (!name) return;
    try {
      const r = await fetch(
        `/api/targets/${selectedTarget}/telemetry/layouts/save`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            name,
            grid: `1x${gridCols}`,
            time_window_s: Math.round(timeWindowMs / 1000),
            plots: plots.map((p, i) => ({
              title: p.title,
              channels: p.channels,
              height: p.height || 180,
              position: [i, 0],
              y_min: p.y_min ?? null,
              y_max: p.y_max ?? null,
              y_label: p.y_label ?? null,
              thresholds: p.thresholds ?? null,
            })),
          }),
        },
      );
      if (r.ok) {
        setSaveMsg(`Saved "${name}"`);
        setTimeout(() => setSaveMsg(null), 2000);
        const lr = await fetch(
          `/api/targets/${selectedTarget}/telemetry/layouts`,
        );
        if (lr.ok) {
          const data = await lr.json();
          setLayouts(data.layouts || []);
          setSelectedLayout(name);
        }
      } else {
        // Surface the backend error (e.g. 409 from config-name collision)
        // instead of silently ignoring it. The previous catch-all hid
        // problems users had no way to debug.
        const text = await r.text();
        setSaveMsg(`Save failed: ${text || `HTTP ${r.status}`}`);
        setTimeout(() => setSaveMsg(null), 5000);
      }
    } catch (e) {
      setSaveMsg(`Save failed: ${e instanceof Error ? e.message : String(e)}`);
      setTimeout(() => setSaveMsg(null), 5000);
    }
  }, [selectedTarget, selectedLayout, gridCols, timeWindowMs, plots]);

  // Resize handle
  useEffect(() => {
    const onMouseMove = (e: MouseEvent) => {
      if (!draggingRef.current) return;
      document.body.classList.add("dragging");
      setSidebarWidth(Math.max(180, Math.min(400, e.clientX - 192)));
    };
    const onMouseUp = () => {
      draggingRef.current = false;
      document.body.classList.remove("dragging");
    };
    window.addEventListener("mousemove", onMouseMove);
    window.addEventListener("mouseup", onMouseUp);
    return () => {
      window.removeEventListener("mousemove", onMouseMove);
      window.removeEventListener("mouseup", onMouseUp);
    };
  }, []);

  // Close channel menu on outside click. useEffect cleanup ensures we
  // don't stack listeners on rapid open/close (the previous ref-callback
  // approach could leak listeners that only got removed on outside click).
  useEffect(() => {
    if (!channelMenu) return;
    const handler = (e: MouseEvent) => {
      const el = channelMenuRef.current;
      if (el && !el.contains(e.target as Node)) {
        setChannelMenu(null);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [channelMenu]);

  // Per-plot data slice: each MultiChart only needs its own channels.
  // We build a fresh slice per plot, then compare it to the previous
  // render's slice and reuse the previous *reference* if shallow-equal
  // (i.e. all the inner channel-data arrays are still the same refs).
  // This is what gives MultiChart's React.memo a stable `data` prop:
  // a plot whose channels weren't in this WS batch keeps its previous
  // data reference and skips re-render entirely.
  const lastSlicesRef = useRef<ChannelData[]>([]);
  const plotDataSlices = useMemo(() => {
    const next: ChannelData[] = [];
    for (let i = 0; i < plots.length; i++) {
      const plot = plots[i];
      const slice: ChannelData = {};
      for (const ch of plot.channels) {
        const arr = allChannels[ch];
        if (arr) slice[ch] = arr;
      }
      const prev = lastSlicesRef.current[i];
      // Reuse previous slice reference if every channel array is identical
      let reuse = false;
      if (prev) {
        const prevKeys = Object.keys(prev);
        const nextKeys = Object.keys(slice);
        if (prevKeys.length === nextKeys.length) {
          reuse = true;
          for (const k of nextKeys) {
            if (prev[k] !== slice[k]) {
              reuse = false;
              break;
            }
          }
        }
      }
      next.push(reuse ? prev : slice);
    }
    lastSlicesRef.current = next;
    return next;
  }, [plots, allChannels]);

  return (
    <div className="flex gap-0" style={{ height: "calc(100vh - 64px)" }}>
      {/* Sidebar */}
      <div
        className="shrink-0 overflow-auto p-3"
        style={{
          width: sidebarWidth,
          backgroundColor: "var(--color-surface)",
          border: "1px solid var(--color-border)",
          borderRadius: "8px",
        }}
      >
        {/* Layout selector */}
        {layouts.length > 0 && (
          <select
            value={selectedLayout}
            onChange={(e) => setSelectedLayout(e.target.value)}
            className="w-full text-xs mb-3"
            style={{ padding: "4px 6px" }}
          >
            <option value="">-- Custom --</option>
            {layouts.map((l) => (
              <option key={l.name} value={l.name}>
                {l.name}
                {l.source === "user" ? " *" : ""}
                {(l.unknown_channels?.length ?? 0) > 0 ? " (!)" : ""}
              </option>
            ))}
          </select>
        )}
        {(() => {
          const cur = layouts.find((l) => l.name === selectedLayout);
          const unknown = cur?.unknown_channels ?? [];
          return unknown.length > 0 ? (
            <div
              className="text-[10px] mb-2 px-1.5 py-1 rounded"
              style={{
                color: "var(--color-warn, #d29922)",
                backgroundColor: "var(--color-elevated)",
              }}
              title={unknown.join(", ")}
            >
              {unknown.length} channel{unknown.length > 1 ? "s" : ""} in this
              layout no longer exist{unknown.length > 1 ? "" : "s"} in the
              target dictionaries: {unknown.slice(0, 3).join(", ")}
              {unknown.length > 3 ? ", ..." : ""}
            </div>
          ) : null;
        })()}

        {/* Controls */}
        <div className="flex items-center justify-between mb-2">
          <div
            className="flex items-center gap-1.5 text-xs"
            style={{
              color: connected
                ? paused
                  ? "var(--color-warn)"
                  : "var(--color-ok)"
                : "var(--color-crit)",
            }}
          >
            <div
              className="w-1.5 h-1.5 rounded-full"
              style={{
                backgroundColor: connected
                  ? paused
                    ? "var(--color-warn)"
                    : "var(--color-ok)"
                  : "var(--color-crit)",
              }}
            />
            {!connected ? "Off" : paused ? "Paused" : "Live"}
          </div>
          <div className="flex gap-1">
            <button
              onClick={() => setPaused(!paused)}
              className="text-xs px-2 py-0.5"
              style={{
                backgroundColor: paused
                  ? "var(--color-warn)"
                  : "var(--color-elevated)",
                color: paused ? "#fff" : "var(--color-text-secondary)",
                border: paused ? "none" : "1px solid var(--color-border)",
                borderRadius: "4px",
                cursor: "pointer",
              }}
            >
              {paused ? "Resume" : "Pause"}
            </button>
            <button
              onClick={addNewPlot}
              className="text-xs px-2 py-0.5"
              style={{
                backgroundColor: "var(--color-accent)",
                color: "#fff",
                border: "none",
                borderRadius: "4px",
                cursor: "pointer",
              }}
            >
              + Plot
            </button>
          </div>
        </div>

        {/* Time + grid */}
        <div className="flex items-center gap-1 mb-2">
          {TIME_WINDOWS.map((tw) => (
            <button
              key={tw.ms}
              onClick={() => {
                setTimeWindowMs(tw.ms);
                // Clear per-plot overrides so all plots use the new global window
                setPlots((prev) =>
                  prev.map((p) => ({ ...p, time_window_ms: null })),
                );
              }}
              className="text-[10px] px-1.5 py-0.5"
              style={{
                backgroundColor:
                  timeWindowMs === tw.ms
                    ? "var(--color-accent)"
                    : "var(--color-elevated)",
                color:
                  timeWindowMs === tw.ms ? "#fff" : "var(--color-text-muted)",
                border: "none",
                borderRadius: "3px",
                cursor: "pointer",
              }}
            >
              {tw.label}
            </button>
          ))}
          <div style={{ flex: 1 }} />
          <button
            onClick={() => setGridCols(gridCols === 1 ? 2 : 1)}
            className="text-[10px] px-1.5 py-0.5"
            style={{
              backgroundColor: "var(--color-elevated)",
              color: "var(--color-text-muted)",
              border: "none",
              borderRadius: "3px",
              cursor: "pointer",
            }}
          >
            {gridCols === 1 ? "2col" : "1col"}
          </button>
        </div>

        {/* Save */}
        <div className="flex items-center justify-between mb-2">
          <span
            className="mono text-[10px]"
            style={{ color: "var(--color-text-muted)" }}
          >
            {sampleCount.toLocaleString()} samples
          </span>
          <button
            onClick={saveLayout}
            className="text-xs px-2 py-0.5"
            style={{
              backgroundColor: "var(--color-elevated)",
              color: "var(--color-text-secondary)",
              border: "1px solid var(--color-border)",
              borderRadius: "4px",
              cursor: "pointer",
            }}
          >
            Save
          </button>
        </div>
        {saveMsg && (
          <div
            className="text-xs mb-2 px-2 py-1 rounded"
            style={{
              backgroundColor: "rgba(63,185,80,0.15)",
              color: "var(--color-ok)",
            }}
          >
            {saveMsg}
          </div>
        )}

        <input
          value={pickerFilter}
          onChange={(e) => setPickerFilter(e.target.value)}
          placeholder="Filter channels..."
          className="w-full mb-2 text-xs"
        />

        {/* Channel groups */}
        <div className="flex flex-col gap-1">
          {Object.entries(grouped)
            .sort(([a], [b]) => a.localeCompare(b))
            .map(([group, chans]) => (
              <div key={group}>
                <div
                  className="flex items-center justify-between px-1 py-1 rounded cursor-pointer"
                  style={{ backgroundColor: "var(--color-elevated)" }}
                  onClick={() => toggleGroup(group)}
                >
                  <div className="flex items-center gap-1">
                    <span
                      style={{
                        color: "var(--color-text-muted)",
                        fontSize: "0.6rem",
                        width: "12px",
                        textAlign: "center",
                      }}
                    >
                      {expandedGroups.has(group) ? "\u25BC" : "\u25B6"}
                    </span>
                    <span
                      className="font-bold"
                      style={{
                        fontSize: "0.75rem",
                        color: "var(--color-text-primary)",
                      }}
                    >
                      {group}
                    </span>
                  </div>
                </div>
                {expandedGroups.has(group) && (
                  <div
                    className="flex flex-col"
                    style={{ paddingLeft: "16px" }}
                  >
                    {[...chans].sort().map((ch) => {
                      const isPlotted = plottedSet.has(ch);
                      const lastVal =
                        allChannels[ch]?.[allChannels[ch].length - 1]?.v;
                      return (
                        <div
                          key={ch}
                          onClick={() => addChannelToPlot(ch)}
                          onContextMenu={(e) => {
                            e.preventDefault();
                            setChannelMenu({
                              x: e.clientX,
                              y: e.clientY,
                              channel: ch,
                            });
                          }}
                          className="flex items-center justify-between px-2 py-0.5 rounded cursor-pointer"
                          style={{
                            backgroundColor: isPlotted
                              ? "rgba(88,166,255,0.1)"
                              : "transparent",
                            color: isPlotted
                              ? "var(--color-text-primary)"
                              : "var(--color-text-muted)",
                            fontSize: "0.72rem",
                          }}
                        >
                          <span className="mono" title={ch}>
                            {isPlotted && (
                              <span style={{ color: "var(--color-accent)" }}>
                                *{" "}
                              </span>
                            )}
                            {fieldName(ch)}
                          </span>
                          {lastVal !== undefined && (
                            <span
                              className="mono"
                              style={{
                                color: "var(--color-text-muted)",
                                fontSize: "0.65rem",
                              }}
                            >
                              {lastVal.toFixed(2)}
                            </span>
                          )}
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            ))}
          {Object.keys(grouped).length === 0 && (
            <div
              className="text-xs py-4 text-center"
              style={{ color: "var(--color-text-muted)" }}
            >
              {connected ? "Waiting for channels..." : "Not connected"}
            </div>
          )}
        </div>

        {/* Channel context menu (right-click on channel in sidebar) */}
        {channelMenu && (
          <div
            ref={channelMenuRef}
            style={{
              position: "fixed",
              left: channelMenu.x,
              top: channelMenu.y,
              zIndex: 100,
              backgroundColor: "var(--color-surface)",
              border: "1px solid var(--color-border)",
              borderRadius: "6px",
              padding: "4px 0",
              minWidth: "160px",
              boxShadow: "0 4px 12px rgba(0,0,0,0.4)",
            }}
          >
            <div
              className="px-3 py-1 text-[10px]"
              style={{ color: "var(--color-text-muted)" }}
            >
              Add to plot
            </div>
            {plots.map((p, i) => (
              <div
                key={i}
                onClick={() => {
                  addChannelToSpecificPlot(channelMenu.channel, i);
                  setChannelMenu(null);
                }}
                className="px-3 py-1.5 text-xs cursor-pointer"
                style={{
                  color: p.channels.includes(channelMenu.channel)
                    ? "var(--color-accent)"
                    : "var(--color-text-primary)",
                }}
                onMouseEnter={(e) =>
                  (e.currentTarget.style.backgroundColor =
                    "var(--color-elevated)")
                }
                onMouseLeave={(e) =>
                  (e.currentTarget.style.backgroundColor = "transparent")
                }
              >
                {p.channels.includes(channelMenu.channel) ? "\u2713 " : ""}
                {p.title}
              </div>
            ))}
            <div
              style={{
                borderTop: "1px solid var(--color-border-muted)",
                margin: "4px 0",
              }}
            />
            <div
              onClick={() => {
                setPlots((prev) =>
                  prev.length >= 20
                    ? prev
                    : [
                        ...prev,
                        {
                          title: channelMenu.channel,
                          channels: [channelMenu.channel],
                        },
                      ],
                );
                setChannelMenu(null);
              }}
              className="px-3 py-1.5 text-xs cursor-pointer"
              style={{ color: "var(--color-accent)" }}
              onMouseEnter={(e) =>
                (e.currentTarget.style.backgroundColor =
                  "var(--color-elevated)")
              }
              onMouseLeave={(e) =>
                (e.currentTarget.style.backgroundColor = "transparent")
              }
            >
              + New plot with this channel
            </div>
          </div>
        )}

        {/* Plot context menu */}
        {contextMenu && plots[contextMenu.plotIdx] && (
          <PlotContextMenu
            x={contextMenu.x}
            y={contextMenu.y}
            plot={plots[contextMenu.plotIdx]}
            onUpdate={(changes) => updatePlot(contextMenu.plotIdx, changes)}
            onDuplicate={() => duplicatePlot(contextMenu.plotIdx)}
            onDelete={() => deletePlot(contextMenu.plotIdx)}
            onClose={() => setContextMenu(null)}
          />
        )}
      </div>

      {/* Resize handle */}
      <div
        onMouseDown={() => {
          draggingRef.current = true;
        }}
        style={{
          width: "4px",
          cursor: "col-resize",
          flexShrink: 0,
          backgroundColor: "transparent",
        }}
        onMouseEnter={(e) =>
          (e.currentTarget.style.backgroundColor = "var(--color-accent)")
        }
        onMouseLeave={(e) =>
          (e.currentTarget.style.backgroundColor = "transparent")
        }
      />

      {/* Plot area */}
      <div className="flex-1 overflow-auto px-3">
        <div className="flex items-center justify-between mb-3">
          <h1 className="text-xl font-bold">Telemetry</h1>
        </div>

        {plots.length === 0 ? (
          <div
            className="rounded-lg p-8 text-center"
            style={{
              border: "1px dashed var(--color-border)",
              color: "var(--color-text-muted)",
            }}
          >
            {layouts.length > 0
              ? "Select a layout or click + Plot"
              : "Click + Plot to create a plot"}
          </div>
        ) : (
          <div
            className={
              gridCols === 2 ? "grid grid-cols-2 gap-3" : "flex flex-col gap-3"
            }
          >
            {plots.map((plot, i) => (
              <div
                key={`${plot.title}-${i}`}
                onDragOver={(e) => handleDragOver(e, i)}
                onClick={() => setFocusedPlot(i)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  if (dragIdx !== null) return;
                  setFocusedPlot(i);
                  setContextMenu({ x: e.clientX, y: e.clientY, plotIdx: i });
                }}
                className="flex"
                style={{}}
              >
                {/* Drag handle */}
                <div
                  draggable
                  onDragStart={(e) => {
                    setDragIdx(i);
                    e.dataTransfer.effectAllowed = "move";
                    document.body.classList.add("dragging");
                  }}
                  onDragEnd={() => {
                    setDragIdx(null);
                    document.body.classList.remove("dragging");
                  }}
                  style={{
                    width: "18px",
                    cursor: "grab",
                    flexShrink: 0,
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    borderLeft:
                      i === focusedPlot
                        ? "2px solid var(--color-accent)"
                        : "2px solid transparent",
                    borderRadius: "4px 0 0 4px",
                    color: "var(--color-text-muted)",
                    fontSize: "12px",
                    userSelect: "none",
                    WebkitUserSelect: "none",
                  }}
                  title="Drag to reorder"
                >
                  &#8942;&#8942;
                </div>
                <div style={{ flex: 1, minWidth: 0 }}>
                  {plot.channels.length > 0 ? (
                    <MultiChart
                      channels={plot.channels}
                      data={plotDataSlices[i]}
                      title={plot.title}
                      height={plot.height || 180}
                      timeWindowMs={plot.time_window_ms || timeWindowMs}
                      hiddenChannels={hiddenChannels}
                      onRemoveChannel={removeHandlers[i]}
                      onToggleChannel={toggleChannelVisibility}
                      yMin={plot.y_min}
                      yMax={plot.y_max}
                      yLabel={plot.y_label}
                      thresholds={plot.thresholds}
                    />
                  ) : (
                    <div
                      className="rounded-lg p-4 text-center text-xs"
                      style={{
                        border: "1px dashed var(--color-border)",
                        color: "var(--color-text-muted)",
                      }}
                    >
                      {plot.title} -- click channels in sidebar to add
                    </div>
                  )}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
