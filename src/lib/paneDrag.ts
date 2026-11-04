import type { PointerEvent as ReactPointerEvent } from "react";
import { create } from "zustand";
import { dropAllowed, dropSide, swapAllowed, type Side } from "./panes";
import { isMod } from "./shortcuts";
import { useStore } from "./store";

/** How far the pointer must move before a press becomes a drag. */
const THRESHOLD = 4;

export interface PaneDrag {
  /** A session dragged from the sidebar, or a pane dragged by its grip. */
  kind: "session" | "pane";
  id: string;
  label: string;
  x: number;
  y: number;
  /** The side of the Code area the pointer is over, when dropping there would do something. */
  side: Side | null;
}

/** The drag in progress, for the overlay and the label that follows the pointer. */
export const usePaneDrag = create<{ drag: PaneDrag | null }>(() => ({ drag: null }));

/** The Code area's box and the split state, if `x`, `y` is over it. */
function overHost(x: number, y: number) {
  const host = document.querySelector<HTMLElement>("[data-pane-host]");
  if (!host) return null;
  const r = host.getBoundingClientRect();
  if (x < r.left || x >= r.right || y < r.top || y >= r.bottom) return null;
  const { panes, activeCodeSessionId } = useStore.getState();
  return { r, state: { panes, active: activeCodeSessionId } };
}

/** The click that ends a drag would open the row it ends on: swallow it. */
function swallowNextClick() {
  const stop = (e: Event) => {
    e.preventDefault();
    e.stopPropagation();
  };
  document.addEventListener("click", stop, { capture: true, once: true });
  setTimeout(() => document.removeEventListener("click", stop, true), 0);
}

/** A drag cancelled with Esc ends before the button is released: swallow the click that release makes. */
function swallowClickAfterRelease() {
  const arm = () => {
    off();
    swallowNextClick();
  };
  const off = () => {
    document.removeEventListener("pointerup", arm, true);
    document.removeEventListener("pointercancel", off, true);
  };
  document.addEventListener("pointerup", arm, true);
  document.addEventListener("pointercancel", off, true);
}

interface Spec {
  kind: PaneDrag["kind"];
  id: string;
  label: string;
  /** The side a drop at `x`, `y` would act on, or null when it would do nothing. */
  sideAt: (x: number, y: number) => Side | null;
  drop: (side: Side) => void;
}

/**
 * Follow the pointer from a press until it's released. Pointer events, not HTML5 drag: the window
 * intercepts native drops (tauri.conf.json, dragDropEnabled). Esc cancels.
 */
function track(e: ReactPointerEvent<HTMLElement>, spec: Spec) {
  const startX = e.clientX;
  const startY = e.clientY;
  let moved = false;
  let done = false;
  try {
    // Keeps the drag ours when the pointer leaves the window.
    e.currentTarget.setPointerCapture(e.pointerId);
  } catch {
    /* the pointer is already gone */
  }
  // From the press, not from the first few pixels of movement: by then the browser has started
  // selecting the row's text (WebKit, which Tauri uses on macOS, does so at once) and would go on
  // selecting, or start dragging the text, instead of leaving the drag to us.
  const stopNative = (ev: Event) => ev.preventDefault();
  document.addEventListener("selectstart", stopNative);
  document.addEventListener("dragstart", stopNative);
  document.getSelection()?.removeAllRanges();
  // The unprefixed property is newer than WebKit's own.
  document.body.style.setProperty("-webkit-user-select", "none");
  document.body.style.userSelect = "none";

  const finish = (commit: boolean, released = true) => {
    if (done) return;
    done = true;
    document.removeEventListener("selectstart", stopNative);
    document.removeEventListener("dragstart", stopNative);
    document.removeEventListener("pointermove", move);
    document.removeEventListener("pointerup", up);
    document.removeEventListener("pointercancel", cancel);
    document.removeEventListener("keydown", key, true);
    const side = usePaneDrag.getState().drag?.side ?? null;
    usePaneDrag.setState({ drag: null });
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    document.body.style.removeProperty("-webkit-user-select");
    if (!moved) return;
    if (released) swallowNextClick();
    else swallowClickAfterRelease();
    if (commit && side) spec.drop(side);
  };
  const move = (ev: PointerEvent) => {
    if (!moved) {
      if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < THRESHOLD) return;
      moved = true;
      document.body.style.cursor = "grabbing";
      document.getSelection()?.removeAllRanges();
    }
    usePaneDrag.setState({
      drag: { kind: spec.kind, id: spec.id, label: spec.label, x: ev.clientX, y: ev.clientY, side: spec.sideAt(ev.clientX, ev.clientY) },
    });
  };
  const up = () => finish(true);
  const cancel = () => finish(false);
  const key = (ev: KeyboardEvent) => {
    if (ev.key !== "Escape") return;
    ev.preventDefault();
    ev.stopPropagation();
    // The button is still down: the click comes later.
    finish(false, false);
  };
  document.addEventListener("pointermove", move);
  document.addEventListener("pointerup", up);
  document.addEventListener("pointercancel", cancel);
  document.addEventListener("keydown", key, true);
}

/**
 * Press on a session row, move past a few pixels, and the session follows the pointer; let go over
 * the Code area to open it beside the open one (left or right half).
 */
export function startSessionDrag(e: ReactPointerEvent<HTMLElement>, id: string, label: string) {
  // Plain primary-button presses only: modifiers select rows, and a button inside the row is a click.
  if (e.button !== 0 || e.pointerType === "touch" || e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
  if ((e.target as Element).closest("button, input")) return;
  track(e, {
    kind: "session",
    id,
    label,
    sideAt: (x, y) => {
      const o = overHost(x, y);
      return o && dropAllowed(o.state, id, o.r.width) ? dropSide(x, o.r.left, o.r.width) : null;
    },
    drop: (side) => useStore.getState().openInSplit(id, side),
  });
}

/** What a Cmd/Ctrl-drag anywhere in a pane must leave alone: things that take a press themselves. */
const INTERACTIVE =
  "a, input, textarea, select, button, summary, [role='button'], [role='separator'], [contenteditable]:not([contenteditable='false']), canvas, .xterm";

/**
 * Drag a pane to the other side to swap them. From its grip (`grip`), or with Cmd (Ctrl elsewhere)
 * held anywhere in the pane except on controls, the transcript's links and the terminal.
 */
export function startPaneDrag(e: ReactPointerEvent<HTMLElement>, id: string, label: string, grip = false) {
  if (e.button !== 0 || e.pointerType === "touch") return;
  if (!grip && (!isMod(e) || (e.target as Element).closest(INTERACTIVE))) return;
  // No text selection or focus change from a press that is a drag.
  e.preventDefault();
  track(e, {
    kind: "pane",
    id,
    label,
    sideAt: (x, y) => {
      const o = overHost(x, y);
      if (!o) return null;
      const side = dropSide(x, o.r.left, o.r.width);
      return swapAllowed(o.state, id, side, o.r.width) ? side : null;
    },
    drop: () => {
      const st = useStore.getState();
      st.swapPanes();
      st.selectCodeSession(id);
    },
  });
}
