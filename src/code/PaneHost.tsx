import { useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { CodeView } from "./CodeView";
import { loadWidth, ResizeHandle, saveWidth } from "../components/ResizeHandle";
import { useStore } from "../lib/store";
import { usePaneDrag, startPaneDrag } from "../lib/paneDrag";
import {
  columnsFor,
  defaultOrientation,
  MIN_PANE_FLOOR,
  MIN_PANE_FLOOR_HEIGHT,
  MIN_PANE_HEIGHT,
  MIN_PANE_WIDTH,
  paneKeys,
  rowsFor,
  type PaneKeys,
} from "../lib/panes";
import { isRemoteApp } from "../lib/platform";
import { PaneContext, type PaneInfo } from "../lib/shortcuts";

import { getCurrentWindow } from "@tauri-apps/api/window";

const REMOTE = isRemoteApp();
const ORIENTATION = defaultOrientation(REMOTE);
const HORIZONTAL = ORIENTATION === "horizontal";

const WINDOW_LABEL: string = (() => {
  try {
    return getCurrentWindow().label ?? "main";
  } catch {
    return "main";
  }
})();
const IS_MAIN_WINDOW = WINDOW_LABEL === "main";
const PANE_SIZE_KEY = REMOTE
  ? "patchbay.remote.paneHeight"
  : IS_MAIN_WINDOW
    ? "patchbay.paneWidth"
    : `patchbay.paneWidth.${WINDOW_LABEL}`;
const PANE_SIZES_KEY = REMOTE
  ? "patchbay.remote.paneHeights"
  : IS_MAIN_WINDOW
    ? "patchbay.paneWidths"
    : `patchbay.paneWidths.${WINDOW_LABEL}`;

// If this is a non-main window, clean up any leftover localStorage keys from previous launches
if (!IS_MAIN_WINDOW && !REMOTE) {
  try {
    localStorage.removeItem(PANE_SIZE_KEY);
    localStorage.removeItem(PANE_SIZES_KEY);
  } catch {
    /* ignore */
  }
}

function loadSavedSizes(): Record<number, number[]> {
  if (!IS_MAIN_WINDOW && !REMOTE) return {};
  try {
    const raw = localStorage.getItem(PANE_SIZES_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)) {
        const out: Record<number, number[]> = {};
        for (const [k, v] of Object.entries(parsed)) {
          const numK = Number(k);
          if (Array.isArray(v) && v.every((n) => typeof n === "number" && Number.isFinite(n) && n > 0)) {
            out[numK] = v;
          }
        }
        return out;
      }
    }
  } catch {
    /* ignore */
  }
  return {};
}

function saveSavedSizes(record: Record<number, number[]>) {
  try {
    localStorage.setItem(PANE_SIZES_KEY, JSON.stringify(record));
  } catch {
    /* ignore */
  }
}

/** The session title a pane shows, for the label that follows a dragged pane. */
const titleOf = (el: Element) => el.closest("[data-pane]")?.querySelector("h1")?.textContent ?? "Session";

/** What dropping a dragged session would do, and a label that follows the pointer. */
function DragFeedback() {
  const drag = usePaneDrag((s) => s.drag);
  if (!drag) return null;
  const target = drag.target;

  return (
    <>
      {target && (
        <div
          aria-hidden="true"
          className={`pointer-events-none absolute z-30 border-2 border-accent bg-accent/10 transition-[left,width,top,height] duration-75 ${
            HORIZONTAL ? "inset-x-0" : "inset-y-0"
          }`}
          style={
            HORIZONTAL
              ? { top: target.box.left, height: target.box.width }
              : { left: target.box.left, width: target.box.width }
          }
        >
          <span
            className={`absolute left-1/2 -translate-x-1/2 rounded-full bg-accent px-3 py-1 text-[12px] font-medium text-accent-fg shadow-md whitespace-nowrap ${
              HORIZONTAL ? "top-1/2 -translate-y-1/2" : "top-4"
            }`}
          >
            {target.badge}
          </span>
        </div>
      )}
      {createPortal(
        <div
          aria-hidden="true"
          className="pointer-events-none fixed z-50 max-w-60 truncate rounded-lg border border-border bg-raised px-3 py-1.5 text-[13px] shadow-float"
          style={{ left: drag.x + 12, top: drag.y + 12 }}
        >
          {drag.label}
        </div>,
        document.body,
      )}
    </>
  );
}

/**
 * The Code tab: one `CodeView` per pane. With no split it is the single open session, as before.
 * A vertical split puts multiple side by side; a horizontal split stacks them. A viewport too small
 * for all shows only the focused one and keeps the others mounted (hidden), so their terminal and
 * scroll positions survive. `leading` is phone chrome (back / sessions) shown on the first visible pane.
 */
export function PaneHost({ leading }: { leading?: ReactNode } = {}) {
  const panes = useStore((s) => s.panes);
  const open = useStore((s) => s.activeCodeSessionId);
  const select = useStore((s) => s.selectCodeSession);
  const host = useRef<HTMLDivElement>(null);
  const [extent, setExtent] = useState(0);

  const [savedSizes, setSavedSizes] = useState<Record<number, number[]>>(() => {
    const loaded = loadSavedSizes();
    if (!loaded[2]) {
      const oldTwo = loadWidth(PANE_SIZE_KEY, 0, 0, 100_000);
      if (oldTwo > 0) loaded[2] = [oldTwo];
    }
    return loaded;
  });

  useLayoutEffect(() => {
    const el = host.current;
    if (!el) return;
    const measure = () => {
      const r = el.getBoundingClientRect();
      setExtent(HORIZONTAL ? r.height : r.width);
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const inSplit = panes.length >= 2;
  const ids: (string | null)[] = inSplit ? panes : [open];
  const n = ids.length;
  const equalShare = Math.round(extent / Math.max(1, n));
  const minPane = HORIZONTAL
    ? Math.min(MIN_PANE_HEIGHT, Math.max(120, Math.floor(extent / Math.max(1, n))))
    : Math.min(MIN_PANE_WIDTH, Math.max(160, Math.floor(extent / Math.max(1, n))));
  // Too-small viewport: allow panes to flex down to a usable density before collapsing to the
  // single focused pane, keeping the others mounted so state survives.
  const sideBySide =
    inSplit &&
    (HORIZONTAL
      ? rowsFor(extent) >= 2 && extent >= n * MIN_PANE_FLOOR_HEIGHT
      : columnsFor(extent) >= 2 && extent >= n * MIN_PANE_FLOOR);

  const currentSizes = useMemo(() => {
    if (!sideBySide || n < 2) return [];
    const saved = savedSizes[n];
    if (!saved || saved.length !== n - 1) {
      return Array.from({ length: n - 1 }, () => equalShare);
    }
    const result: number[] = [];
    let used = 0;
    for (let i = 0; i < n - 1; i++) {
      const remainingPanes = n - i;
      const maxForThis = extent - used - (remainingPanes - 1) * minPane;
      if (maxForThis < minPane) {
        return Array.from({ length: n - 1 }, () => equalShare);
      }
      const clamped = Math.max(minPane, Math.min(maxForThis, saved[i]));
      result.push(clamped);
      used += clamped;
    }
    return result;
  }, [sideBySide, n, extent, savedSizes, equalShare, minPane]);

  const maxFor = (i: number) => {
    const cur = currentSizes[i] ?? equalShare;
    const next =
      i < n - 2
        ? (currentSizes[i + 1] ?? equalShare)
        : extent - currentSizes.reduce((a, b) => a + b, 0);
    return Math.max(minPane, cur + next - minPane);
  };

  const handleResize = (i: number, newSize: number, done: boolean) => {
    const nextSizes = [...currentSizes];
    const d = newSize - nextSizes[i];
    nextSizes[i] = newSize;
    if (i < n - 2) {
      nextSizes[i + 1] = Math.max(minPane, nextSizes[i + 1] - d);
    }
    const next = { ...savedSizes, [n]: nextSizes };
    setSavedSizes(next);
    if (done) {
      saveSavedSizes(next);
      if (n === 2) saveWidth(PANE_SIZE_KEY, nextSizes[0]);
    }
  };

  const handleReset = () => {
    const next = { ...savedSizes };
    delete next[n];
    setSavedSizes(next);
    saveSavedSizes(next);
    if (n === 2) saveWidth(PANE_SIZE_KEY, 0);
  };

  // Keyed per session in a split, so a swap or a closed pane doesn't hand one pane's state (open
  // terminal, panels) to another session (see paneKeys).
  const keyState = useRef<PaneKeys>({ anchor: null, single: "single", last: new Map() });
  const { keys, next } = paneKeys(keyState.current, ids, inSplit);
  keyState.current = next;

  const infos = useMemo<PaneInfo[]>(
    () =>
      ids.map((id, i) => ({
        id,
        focused: !inSplit || id === open,
        split: sideBySide,
        last: !sideBySide || i === ids.length - 1,
        closable: inSplit,
      })),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [inSplit, sideBySide, open, ids.join("\n")],
  );

  // Phone chrome (back / sessions) on the first visible pane so there is one of each.
  const leadingIndex = sideBySide ? 0 : infos.findIndex((p) => p.focused);

  return (
    <div
      ref={host}
      data-pane-host
      data-pane-orientation={ORIENTATION}
      className={`relative flex min-w-0 flex-1 ${HORIZONTAL ? "min-h-0 flex-col" : ""}`}
    >
      {ids.map((id, i) => {
        const info = infos[i];
        // Too small for all: the unfocused panes stay mounted but out of sight.
        const hidden = inSplit && !sideBySide && !info.focused;
        const isNotLast = sideBySide && i < ids.length - 1;
        const paneSize = isNotLast ? (currentSizes[i] ?? equalShare) : undefined;
        const showLeading = !!leading && i === leadingIndex && !hidden;
        return (
          <PaneContext.Provider key={keys[i]} value={info}>
            <div
              role="region"
              aria-label={inSplit ? `Pane ${i + 1}` : "Session"}
              aria-current={inSplit && info.focused ? "true" : undefined}
              data-pane={i}
              data-pane-focused={info.focused ? "true" : "false"}
              inert={hidden}
              // Capture, so a click anywhere in the pane (a menu, a message) focuses it first.
              onPointerDownCapture={() => id && !info.focused && select(id)}
              onFocusCapture={() => id && !info.focused && select(id)}
              // Cmd (Ctrl elsewhere) held: drag the pane. Plain presses are left alone. Phone skips drag.
              onPointerDown={(e) => !REMOTE && sideBySide && id && startPaneDrag(e, id, titleOf(e.currentTarget))}
              className={`relative min-w-0 ${HORIZONTAL ? "min-h-0" : ""} ${
                hidden
                  ? "hidden"
                  : `flex ${
                      isNotLast
                        ? `shrink-0 ${HORIZONTAL ? "border-b border-line" : "border-r border-line"}`
                        : "flex-1"
                    } ${
                      sideBySide && !info.focused
                        ? "opacity-75 saturate-[.85] transition-[opacity,filter] hover:opacity-100"
                        : ""
                    }`
              }`}
              style={isNotLast ? (HORIZONTAL ? { height: paneSize } : { width: paneSize }) : undefined}
            >
              <CodeView leading={showLeading ? leading : undefined} />
              {sideBySide && id && !REMOTE && (
                <button
                  type="button"
                  aria-hidden="true"
                  tabIndex={-1}
                  title={ids.length === 2 ? "Drag to swap the panes" : "Drag to move pane"}
                  className={`group absolute z-20 cursor-grab ${
                    HORIZONTAL
                      ? "left-0 top-1/2 h-11 w-2 -translate-y-1/2"
                      : "left-1/2 top-0 h-2 w-11 -translate-x-1/2"
                  }`}
                  onPointerDown={(e) => {
                    e.stopPropagation();
                    startPaneDrag(e, id, titleOf(e.currentTarget), true);
                  }}
                >
                  <span
                    className={`block rounded-full bg-fg/30 opacity-0 transition-opacity group-hover:opacity-100 ${
                      HORIZONTAL ? "ml-[2px] h-8 w-[3px]" : "mx-auto mt-[2px] h-[3px] w-8"
                    }`}
                  />
                </button>
              )}
              {isNotLast && (
                <ResizeHandle
                  width={paneSize ?? equalShare}
                  min={minPane}
                  max={maxFor(i)}
                  initial={equalShare}
                  edge={HORIZONTAL ? "bottom" : "right"}
                  label={`Resize pane ${i + 1}`}
                  onChange={(w, done) => {
                    handleResize(i, w, done);
                  }}
                  onReset={handleReset}
                />
              )}
            </div>
          </PaneContext.Provider>
        );
      })}
      {!REMOTE && <DragFeedback />}
    </div>
  );
}
