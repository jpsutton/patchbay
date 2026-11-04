import type { PointerEvent as ReactPointerEvent } from "react";
import { create } from "zustand";
import { columnsFor, MAX_PANES, MIN_PANE_FLOOR } from "./panes";
import { isMod } from "./shortcuts";
import { useStore } from "./store";

/** How far the pointer must move before a press becomes a drag. */
const THRESHOLD = 4;

export interface PaneDragTarget {
  /** The target pane index in the host. */
  paneIndex: number;
  /** Whether inserting on the left or right of this pane, or targeting the whole pane. */
  side: "left" | "right" | "whole";
  /** Pixel coordinates relative to data-pane-host. */
  box: { left: number; width: number };
  /** Text to show in the pill badge. */
  badge: string;
}

export interface PaneDrag {
  /** A session dragged from the sidebar, or a pane dragged by its grip. */
  kind: "session" | "pane";
  id: string;
  label: string;
  x: number;
  y: number;
  target: PaneDragTarget | null;
}

/** The drag in progress, for the overlay and the label that follows the pointer. */
export const usePaneDrag = create<{ drag: PaneDrag | null }>(() => ({ drag: null }));

/** Find which pane and drop zone the pointer at (x, y) is targeting. */
export function findDragTarget(kind: "session" | "pane", id: string, x: number, y: number): PaneDragTarget | null {
  const host = document.querySelector<HTMLElement>("[data-pane-host]");
  if (!host) return null;
  const hostRect = host.getBoundingClientRect();
  if (x < hostRect.left || x >= hostRect.right || y < hostRect.top || y >= hostRect.bottom) return null;

  const { panes, activeCodeSessionId } = useStore.getState();
  const inSplit = panes.length >= 2;
  const paneElements = Array.from(host.querySelectorAll<HTMLElement>(":scope > [data-pane]"));
  if (paneElements.length === 0) return null;

  // Single pane (no split)
  if (!inSplit) {
    if (columnsFor(hostRect.width) < 2) return null;
    if (kind === "session" && id === activeCodeSessionId) return null;
    const isLeft = x < hostRect.left + hostRect.width / 2;
    const half = Math.round(hostRect.width / 2);
    return {
      paneIndex: 0,
      side: isLeft ? "left" : "right",
      box: { left: isLeft ? 0 : half, width: half },
      badge: "Open in split view",
    };
  }

  // Find which pane the pointer is over
  let foundIndex = -1;
  let paneRect: DOMRect | null = null;
  for (let i = 0; i < paneElements.length; i++) {
    const r = paneElements[i].getBoundingClientRect();
    if (x >= r.left && x <= r.right) {
      foundIndex = i;
      paneRect = r;
      break;
    }
  }
  if (foundIndex === -1 || !paneRect) return null;

  const relLeft = Math.round(paneRect.left - hostRect.left);
  const paneWidth = Math.round(paneRect.width);

  if (kind === "pane") {
    // Dragging an existing pane to reorder
    const fromIdx = panes.indexOf(id);
    if (fromIdx === -1) return null;
    if (panes.length === 2) {
      if (fromIdx === foundIndex) return null;
      return {
        paneIndex: foundIndex,
        side: "whole",
        box: { left: relLeft, width: paneWidth },
        badge: "Swap places",
      };
    }
    // For >2 panes, edge-based insertion so the user sees whether the pane lands before or after
    const isLeft = x < paneRect.left + paneRect.width / 2;
    const targetIdx = isLeft ? foundIndex : foundIndex + 1;
    if (targetIdx === fromIdx || targetIdx === fromIdx + 1) return null;
    const finalIdx = targetIdx > fromIdx ? targetIdx - 1 : targetIdx;
    const halfWidth = Math.round(paneWidth / 2);
    return {
      paneIndex: finalIdx,
      side: isLeft ? "left" : "right",
      box: {
        left: isLeft ? relLeft : relLeft + halfWidth,
        width: halfWidth,
      },
      badge: isLeft ? "Move left" : "Move right",
    };
  }

  // Dragging a session from the sidebar
  if (panes.includes(id)) return null;

  if (panes.length < MAX_PANES && hostRect.width >= (panes.length + 1) * MIN_PANE_FLOOR) {
    const isLeft = x < paneRect.left + paneRect.width / 2;
    const halfWidth = Math.round(paneWidth / 2);
    return {
      paneIndex: foundIndex,
      side: isLeft ? "left" : "right",
      box: {
        left: isLeft ? relLeft : relLeft + halfWidth,
        width: halfWidth,
      },
      badge: isLeft ? "Insert left" : "Insert right",
    };
  } else {
    return {
      paneIndex: foundIndex,
      side: "whole",
      box: { left: relLeft, width: paneWidth },
      badge: "Replace this pane",
    };
  }
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
  targetAt: (x: number, y: number) => PaneDragTarget | null;
  drop: (target: PaneDragTarget) => void;
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
    const target = usePaneDrag.getState().drag?.target ?? null;
    usePaneDrag.setState({ drag: null });
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    document.body.style.removeProperty("-webkit-user-select");
    if (!moved) return;
    if (released) swallowNextClick();
    else swallowClickAfterRelease();
    if (commit && target) spec.drop(target);
  };
  const move = (ev: PointerEvent) => {
    if (!moved) {
      if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < THRESHOLD) return;
      moved = true;
      document.body.style.cursor = "grabbing";
      document.getSelection()?.removeAllRanges();
    }
    usePaneDrag.setState({
      drag: { kind: spec.kind, id: spec.id, label: spec.label, x: ev.clientX, y: ev.clientY, target: spec.targetAt(ev.clientX, ev.clientY) },
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
 * a pane to open or insert it beside the target.
 */
export function startSessionDrag(e: ReactPointerEvent<HTMLElement>, id: string, label: string) {
  // Plain primary-button presses only: modifiers select rows, and a button inside the row is a click.
  if (e.button !== 0 || e.pointerType === "touch" || e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
  if ((e.target as Element).closest("button, input")) return;
  track(e, {
    kind: "session",
    id,
    label,
    targetAt: (x, y) => findDragTarget("session", id, x, y),
    drop: (target) => {
      const st = useStore.getState();
      if (st.panes.length < 2) {
        st.openInSplit(id, target.side === "left" ? "left" : "right");
      } else if (target.side === "whole" || st.panes.length >= MAX_PANES) {
        st.replacePaneAt(id, target.paneIndex);
      } else {
        const insertAt = target.side === "left" ? target.paneIndex : target.paneIndex + 1;
        st.insertPaneAt(id, insertAt);
      }
    },
  });
}

/** What a Cmd/Ctrl-drag anywhere in a pane must leave alone: things that take a press themselves. */
const INTERACTIVE =
  "a, input, textarea, select, button, summary, [role='button'], [role='separator'], [contenteditable]:not([contenteditable='false']), canvas, .xterm";

/**
 * Drag a pane to another pane to move or swap them.
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
    targetAt: (x, y) => findDragTarget("pane", id, x, y),
    drop: (target) => {
      const st = useStore.getState();
      const fromIdx = st.panes.indexOf(id);
      if (fromIdx !== -1 && fromIdx !== target.paneIndex) {
        st.movePane(fromIdx, target.paneIndex);
        st.selectCodeSession(id);
      }
    },
  });
}
