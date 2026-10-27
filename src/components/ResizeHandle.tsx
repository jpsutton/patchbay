import { useState } from "react";

/** A panel width kept in localStorage, within `min` and `max`; `fallback` when none is saved. */
export function loadWidth(key: string, fallback: number, min: number, max: number): number {
  try {
    const w = Number(localStorage.getItem(key));
    return w ? clampWidth(w, min, max) : fallback;
  } catch {
    return fallback;
  }
}

export function saveWidth(key: string, w: number) {
  try {
    localStorage.setItem(key, String(w));
  } catch {
    /* storage unavailable */
  }
}

export const clampWidth = (w: number, min: number, max: number) => Math.round(Math.min(max, Math.max(min, w)));

/**
 * Drag a panel's edge to resize it; double-click resets, arrow keys nudge. `edge` is the panel
 * side the handle sits on: a panel at the left of the window grows when its right edge moves
 * right, one at the right grows when its left edge moves left. The panel must be `relative`.
 * `onChange(width, done)`: `done` once the drag (or key press) ended, the time to save.
 */
export function ResizeHandle({
  width,
  min,
  max,
  initial,
  edge,
  label,
  onChange,
}: {
  width: number;
  min: number;
  max: number;
  initial: number;
  edge: "left" | "right";
  label: string;
  onChange: (w: number, done: boolean) => void;
}) {
  const [dragging, setDragging] = useState(false);
  // Pointer movement to the right makes the panel wider on its right edge, narrower on its left.
  const dir = edge === "right" ? 1 : -1;
  return (
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuenow={width}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      title="Drag to resize · double-click to reset"
      className={`group absolute ${edge === "right" ? "-right-1" : "-left-1"} top-0 bottom-0 z-20 w-2 cursor-col-resize outline-none`}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        const startX = e.clientX;
        const startW = width;
        const el = e.currentTarget;
        el.setPointerCapture(e.pointerId);
        setDragging(true);
        document.body.style.cursor = "col-resize";
        document.body.style.userSelect = "none";
        let last = startW;
        const move = (ev: PointerEvent) => {
          last = clampWidth(startW + dir * (ev.clientX - startX), min, max);
          onChange(last, false);
        };
        const up = () => {
          el.removeEventListener("pointermove", move);
          el.removeEventListener("pointerup", up);
          el.removeEventListener("pointercancel", up);
          document.body.style.cursor = "";
          document.body.style.userSelect = "";
          setDragging(false);
          onChange(last, true);
        };
        el.addEventListener("pointermove", move);
        el.addEventListener("pointerup", up);
        el.addEventListener("pointercancel", up);
      }}
      onDoubleClick={() => onChange(initial, true)}
      onKeyDown={(e) => {
        const step = e.shiftKey ? 48 : 16;
        if (e.key === "ArrowLeft") onChange(clampWidth(width - dir * step, min, max), true);
        else if (e.key === "ArrowRight") onChange(clampWidth(width + dir * step, min, max), true);
        else return;
        e.preventDefault();
      }}
    >
      <div
        className={`mx-auto h-full w-0.5 transition-colors ${
          dragging ? "bg-accent" : "bg-transparent group-hover:bg-accent/50 group-focus-visible:bg-accent"
        }`}
      />
    </div>
  );
}
