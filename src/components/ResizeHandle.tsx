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
 * right, one at the right grows when its left edge moves left. Top/bottom edges resize height
 * the same way. The panel must be `relative`. `onChange(size, done)`: `done` once the drag (or
 * key press) ended, the time to save.
 */
export function ResizeHandle({
  width,
  min,
  max,
  initial,
  edge,
  label,
  onChange,
  onReset,
}: {
  width: number;
  min: number;
  max: number;
  initial: number;
  edge: "left" | "right" | "top" | "bottom";
  label: string;
  onChange: (w: number, done: boolean) => void;
  onReset?: () => void;
}) {
  const [dragging, setDragging] = useState(false);
  const horizontal = edge === "top" || edge === "bottom";
  // Pointer movement along the growth axis makes the panel larger when positive.
  const dir = edge === "right" || edge === "bottom" ? 1 : -1;
  const axisKey = horizontal ? ("clientY" as const) : ("clientX" as const);
  const growKey = horizontal ? ("ArrowDown" as const) : ("ArrowRight" as const);
  const shrinkKey = horizontal ? ("ArrowUp" as const) : ("ArrowLeft" as const);
  return (
    <div
      role="separator"
      aria-orientation={horizontal ? "horizontal" : "vertical"}
      aria-label={label}
      aria-valuenow={width}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      title="Drag to resize · double-click to reset"
      className={`group absolute z-20 outline-none ${
        horizontal
          ? `${edge === "bottom" ? "-bottom-1" : "-top-1"} left-0 right-0 h-2 cursor-row-resize`
          : `${edge === "right" ? "-right-1" : "-left-1"} top-0 bottom-0 w-2 cursor-col-resize`
      }`}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        const start = e[axisKey];
        const startW = width;
        const el = e.currentTarget;
        el.setPointerCapture(e.pointerId);
        setDragging(true);
        document.body.style.cursor = horizontal ? "row-resize" : "col-resize";
        document.body.style.userSelect = "none";
        let last = startW;
        const move = (ev: PointerEvent) => {
          last = clampWidth(startW + dir * (ev[axisKey] - start), min, max);
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
      onDoubleClick={() => (onReset ? onReset() : onChange(initial, true))}
      onKeyDown={(e) => {
        const step = e.shiftKey ? 48 : 16;
        if (e.key === shrinkKey) onChange(clampWidth(width - dir * step, min, max), true);
        else if (e.key === growKey) onChange(clampWidth(width + dir * step, min, max), true);
        else return;
        e.preventDefault();
      }}
    >
      <div
        className={`${
          horizontal ? "my-auto h-0.5 w-full" : "mx-auto h-full w-0.5"
        } transition-colors ${
          dragging ? "bg-accent" : "bg-transparent group-hover:bg-accent/50 group-focus-visible:bg-accent"
        }`}
      />
    </div>
  );
}
