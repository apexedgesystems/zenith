import { useEffect, useState } from "react";

/**
 * Owns its own state so 1Hz updates don't re-render the entire App tree.
 * Without this, every chart, card, and table redraws once per second.
 *
 * Standalone clock component (avoids 1Hz App re-render).
 */
export default function Clock() {
  const [text, setText] = useState("");
  useEffect(() => {
    const tick = () =>
      setText(new Date().toISOString().split("T")[1].split(".")[0] + " UTC");
    tick();
    const interval = setInterval(tick, 1000);
    return () => clearInterval(interval);
  }, []);
  return (
    <div className="mono text-xs" style={{ color: "var(--color-text-muted)" }}>
      {text}
    </div>
  );
}
