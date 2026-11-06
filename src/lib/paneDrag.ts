import type { PointerEvent as ReactPointerEvent } from "react";
import { create } from "zustand";
import { columnsFor, maxPanesFor, MIN_PANE_FLOOR } from "./panes";
import { isRemoteApp } from "./platform";
import { isMod } from "./shortcuts";
import { useStore } from "./store";

const MAX = () => maxPanesFor(isRemoteApp());

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

  if (panes.length < MAX() && hostRect.width >= (panes.length + 1) * MIN_PANE_FLOOR) {
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

/** The press captured at pointerdown. A touch drag is armed by a still hold, so `track` starts
 * later than the React event is dispatched and `e.currentTarget` would already be null. */
interface Press {
  currentTarget: HTMLElement;
  pointerId: number;
  clientX: number;
  clientY: number;
}

function pressOf(e: ReactPointerEvent<HTMLElement>): Press {
  return { currentTarget: e.currentTarget, pointerId: e.pointerId, clientX: e.clientX, clientY: e.clientY };
}

/**
 * Follow the pointer from a press until it's released. Pointer events, not HTML5 drag: the window
 * intercepts native drops (tauri.conf.json, dragDropEnabled). Esc cancels. On touch the drag is
 * already armed by a still hold; from here we own the gesture, so keep the list from scrolling
 * under the pill and Android's long-press `contextmenu` from opening a second menu over it.
 */
function track(press: Press, spec: Spec, touch = false) {
  const startX = press.clientX;
  const startY = press.clientY;
  let moved = false;
  let done = false;
  try {
    // Keeps the drag ours when the pointer leaves the window.
    press.currentTarget.setPointerCapture(press.pointerId);
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

  // An armed touch drag owns the gesture: a non-passive touchmove keeps the browser from starting
  // a scroll, and a capture-phase contextmenu guard stops Android's long-press menu. Both are
  // added only once the hold has armed, so an ordinary swipe before then still scrolls the list.
  const stopTouchScroll = (ev: TouchEvent) => {
    if (ev.cancelable) ev.preventDefault();
  };
  const stopMenu = (ev: Event) => {
    ev.preventDefault();
    ev.stopPropagation();
  };
  if (touch) {
    document.addEventListener("touchmove", stopTouchScroll, { passive: false });
    document.addEventListener("contextmenu", stopMenu, true);
  }

  const finish = (commit: boolean, released = true) => {
    if (done) return;
    done = true;
    document.removeEventListener("selectstart", stopNative);
    document.removeEventListener("dragstart", stopNative);
    document.removeEventListener("pointermove", move);
    document.removeEventListener("pointerup", up);
    document.removeEventListener("pointercancel", cancel);
    document.removeEventListener("keydown", key, true);
    document.removeEventListener("touchmove", stopTouchScroll);
    document.removeEventListener("contextmenu", stopMenu, true);
    const target = usePaneDrag.getState().drag?.target ?? null;
    usePaneDrag.setState({ drag: null });
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    document.body.style.removeProperty("-webkit-user-select");
    if (!moved) {
      // An armed hold is not a tap: don't let its release open the row underneath.
      if (touch && released) swallowNextClick();
      return;
    }
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

/** A still touch press of this long arms a drag (the iOS home-screen gesture). */
const HOLD_MS = 400;
/** Finger drift during the hold that still counts as holding, not scrolling. */
const TOUCH_SLOP = 8;

/**
 * Press on a session row, move past a few pixels, and the session follows the pointer; let go over
 * a pane to open or insert it beside the target. On touch, a still press-and-hold arms the drag
 * first; an ordinary swipe is left to the browser so the session list keeps scrolling.
 */
export function startSessionDrag(e: ReactPointerEvent<HTMLElement>, id: string, label: string) {
  // Plain primary-button presses only: modifiers select rows, and a button inside the row is a click.
  if (e.button !== 0 || e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
  if ((e.target as Element).closest("button, input")) return;
  const press = pressOf(e);
  const spec: Spec = {
    kind: "session",
    id,
    label,
    targetAt: (x, y) => findDragTarget("session", id, x, y),
    drop: (target) => {
      const st = useStore.getState();
      if (st.panes.length < 2) {
        st.openInSplit(id, target.side === "left" ? "left" : "right");
      } else if (target.side === "whole" || st.panes.length >= MAX()) {
        st.replacePaneAt(id, target.paneIndex);
      } else {
        const insertAt = target.side === "left" ? target.paneIndex : target.paneIndex + 1;
        st.insertPaneAt(id, insertAt);
      }
    },
  };
  if (e.pointerType !== "touch") {
    track(press, spec);
    return;
  }
  // Touch: arm only after a still hold. A move before then is a scroll/swipe, so cancel the hold
  // and never start `track` — the row keeps its normal touch-action and no drag is left behind.
  let timer = 0;
  const stopHolding = () => {
    window.clearTimeout(timer);
    document.removeEventListener("pointermove", holdMove);
    document.removeEventListener("pointerup", stopHolding);
    document.removeEventListener("pointercancel", stopHolding);
  };
  const holdMove = (ev: PointerEvent) => {
    if (Math.hypot(ev.clientX - press.clientX, ev.clientY - press.clientY) > TOUCH_SLOP) stopHolding();
  };
  timer = window.setTimeout(() => {
    stopHolding();
    track(press, spec, true);
  }, HOLD_MS);
  document.addEventListener("pointermove", holdMove);
  document.addEventListener("pointerup", stopHolding);
  document.addEventListener("pointercancel", stopHolding);
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
  track(pressOf(e), {
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
