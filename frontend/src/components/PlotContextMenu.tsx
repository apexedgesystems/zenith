import { useEffect, useRef } from "react";
import { TIME_WINDOWS } from "../types/telemetry";
import type { PlotDef } from "../types/telemetry";

export default function PlotContextMenu({
  x,
  y,
  plot,
  onUpdate,
  onDuplicate,
  onDelete,
  onClose,
}: {
  x: number;
  y: number;
  plot: PlotDef;
  onUpdate: (p: Partial<PlotDef>) => void;
  onDuplicate: () => void;
  onDelete: () => void;
  onClose: () => void;
}) {
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node))
        onClose();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [onClose]);

  const item = (label: string, action: () => void, color?: string) => (
    <div
      onClick={() => {
        action();
        onClose();
      }}
      className="px-3 py-1.5 text-xs cursor-pointer"
      style={{ color: color || "var(--color-text-primary)" }}
      onMouseEnter={(e) =>
        (e.currentTarget.style.backgroundColor = "var(--color-elevated)")
      }
      onMouseLeave={(e) =>
        (e.currentTarget.style.backgroundColor = "transparent")
      }
    >
      {label}
    </div>
  );

  const sep = (
    <div
      style={{
        borderTop: "1px solid var(--color-border-muted)",
        margin: "4px 0",
      }}
    />
  );

  return (
    <div
      ref={menuRef}
      style={{
        position: "fixed",
        left: x,
        top: y,
        zIndex: 100,
        backgroundColor: "var(--color-surface)",
        border: "1px solid var(--color-border)",
        borderRadius: "6px",
        padding: "4px 0",
        minWidth: "160px",
        boxShadow: "0 4px 12px rgba(0,0,0,0.4)",
      }}
    >
      {item("Rename...", () => {
        const name = prompt("Plot title:", plot.title);
        if (name) onUpdate({ title: name });
      })}
      {item("Set Y range...", () => {
        const min = prompt(
          "Y min (blank for auto):",
          plot.y_min != null ? String(plot.y_min) : "",
        );
        const max = prompt(
          "Y max (blank for auto):",
          plot.y_max != null ? String(plot.y_max) : "",
        );
        onUpdate({
          y_min: min ? parseFloat(min) : null,
          y_max: max ? parseFloat(max) : null,
        });
      })}
      {item("Set Y label...", () => {
        const label = prompt("Y axis label:", plot.y_label || "");
        onUpdate({ y_label: label || null });
      })}
      {item("Add threshold...", () => {
        const val = prompt("Threshold value:");
        if (!val) return;
        const color = prompt("Color (hex):", "#d29922") || "#d29922";
        const label = prompt("Label (optional):", "") || "";
        onUpdate({
          thresholds: [
            ...(plot.thresholds || []),
            { value: parseFloat(val), color, label },
          ],
        });
      })}
      {item("Clear thresholds", () => onUpdate({ thresholds: null }))}
      {sep}
      {/* Time window per-plot */}
      <div
        className="px-3 py-1 text-[10px]"
        style={{ color: "var(--color-text-muted)" }}
      >
        Time window
      </div>
      <div className="flex gap-1 px-3 pb-1">
        {TIME_WINDOWS.map((tw) => (
          <button
            key={tw.ms}
            onClick={() => {
              onUpdate({ time_window_ms: tw.ms });
              onClose();
            }}
            className="text-[10px] px-1.5 py-0.5"
            style={{
              backgroundColor:
                (plot.time_window_ms || 30000) === tw.ms
                  ? "var(--color-accent)"
                  : "var(--color-elevated)",
              color:
                (plot.time_window_ms || 30000) === tw.ms
                  ? "#fff"
                  : "var(--color-text-muted)",
              border: "none",
              borderRadius: "3px",
              cursor: "pointer",
            }}
          >
            {tw.label}
          </button>
        ))}
      </div>
      {sep}
      {item(plot.height === 120 ? "Normal height" : "Compact height", () =>
        onUpdate({ height: plot.height === 120 ? 180 : 120 }),
      )}
      {item("Tall", () => onUpdate({ height: 260 }))}
      {sep}
      {item("Duplicate", onDuplicate)}
      {item("Delete plot", onDelete, "var(--color-crit)")}
    </div>
  );
}
