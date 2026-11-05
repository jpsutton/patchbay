import { useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { CodeView } from "./CodeView";
import { loadWidth, ResizeHandle, saveWidth } from "../components/ResizeHandle";
import { useStore } from "../lib/store";
import { usePaneDrag, startPaneDrag } from "../lib/paneDrag";
import { columnsFor, MIN_PANE_FLOOR, MIN_PANE_WIDTH, paneKeys, type PaneKeys } from "../lib/panes";
import { PaneContext, type PaneInfo } from "../lib/shortcuts";

import { getCurrentWindow } from "@tauri-apps/api/window";

const WINDOW_LABEL: string = (() => {
  try {
    return getCurrentWindow().label ?? "main";
  } catch {
    return "main";
  }
})();
const IS_MAIN_WINDOW = WINDOW_LABEL === "main";
const PANE_WIDTH_KEY = IS_MAIN_WINDOW ? "patchbay.paneWidth" : `patchbay.paneWidth.${WINDOW_LABEL}`;
const PANE_WIDTHS_KEY = IS_MAIN_WINDOW ? "patchbay.paneWidths" : `patchbay.paneWidths.${WINDOW_LABEL}`;

// If this is a non-main window, clean up any leftover localStorage keys from previous launches
if (!IS_MAIN_WINDOW) {
  try {
    localStorage.removeItem(PANE_WIDTH_KEY);
    localStorage.removeItem(PANE_WIDTHS_KEY);
  } catch {
    /* ignore */
  }
}

function loadSavedWidths(): Record<number, number[]> {
  if (!IS_MAIN_WINDOW) return {};
  try {
    const raw = localStorage.getItem(PANE_WIDTHS_KEY);
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

function saveSavedWidths(record: Record<number, number[]>) {
  try {
    localStorage.setItem(PANE_WIDTHS_KEY, JSON.stringify(record));
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
          className="pointer-events-none absolute inset-y-0 z-30 border-2 border-accent bg-accent/10 transition-[left,width] duration-75"
          style={{ left: target.box.left, width: target.box.width }}
        >
          <span className="absolute left-1/2 top-4 -translate-x-1/2 rounded-full bg-accent px-3 py-1 text-[12px] font-medium text-accent-fg shadow-md whitespace-nowrap">
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
 * A split puts multiple side by side with dividers; a window too narrow for all shows only the focused
 * one and keeps the others mounted (hidden), so their terminal and scroll positions survive.
 */
export function PaneHost() {
  const panes = useStore((s) => s.panes);
  const open = useStore((s) => s.activeCodeSessionId);
  const select = useStore((s) => s.selectCodeSession);
  const host = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);

  const [savedWidths, setSavedWidths] = useState<Record<number, number[]>>(() => {
    const loaded = loadSavedWidths();
    if (!loaded[2]) {
      const oldTwo = loadWidth(PANE_WIDTH_KEY, 0, 0, 100_000);
      if (oldTwo > 0) loaded[2] = [oldTwo];
    }
    return loaded;
  });

  useLayoutEffect(() => {
    const el = host.current;
    if (!el) return;
    setWidth(el.getBoundingClientRect().width);
    const ro = new ResizeObserver(([e]) => setWidth(e.contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const inSplit = panes.length >= 2;
  const ids: (string | null)[] = inSplit ? panes : [open];
  const n = ids.length;
  const equalShare = Math.round(width / Math.max(1, n));
  const minPaneW = Math.min(MIN_PANE_WIDTH, Math.max(160, Math.floor(width / Math.max(1, n))));
  // Narrow window fallback: allow panes to flex down to a usable density (MIN_PANE_FLOOR,
  // deliberately below MIN_PANE_WIDTH of 240px) before collapsing to the single focused pane,
  // so 3 or 4 panes remain side-by-side on typical desktop/laptop splits without overflowing.
  const sideBySide = inSplit && columnsFor(width) >= 2 && width >= n * MIN_PANE_FLOOR;

  const currentWidths = useMemo(() => {
    if (!sideBySide || n < 2) return [];
    const saved = savedWidths[n];
    if (!saved || saved.length !== n - 1) {
      return Array.from({ length: n - 1 }, () => equalShare);
    }
    const result: number[] = [];
    let used = 0;
    for (let i = 0; i < n - 1; i++) {
      const remainingPanes = n - i;
      const maxForThis = width - used - (remainingPanes - 1) * minPaneW;
      if (maxForThis < minPaneW) {
        return Array.from({ length: n - 1 }, () => equalShare);
      }
      const clamped = Math.max(minPaneW, Math.min(maxForThis, saved[i]));
      result.push(clamped);
      used += clamped;
    }
    return result;
  }, [sideBySide, n, width, savedWidths, equalShare, minPaneW]);

  const maxFor = (i: number) => {
    const curW = currentWidths[i] ?? equalShare;
    const nextW =
      i < n - 2
        ? (currentWidths[i + 1] ?? equalShare)
        : width - currentWidths.reduce((a, b) => a + b, 0);
    return Math.max(minPaneW, curW + nextW - minPaneW);
  };

  const handleResize = (i: number, newW: number, done: boolean) => {
    const nextWidths = [...currentWidths];
    const dw = newW - nextWidths[i];
    nextWidths[i] = newW;
    if (i < n - 2) {
      nextWidths[i + 1] = Math.max(minPaneW, nextWidths[i + 1] - dw);
    }
    const next = { ...savedWidths, [n]: nextWidths };
    setSavedWidths(next);
    if (done) {
      saveSavedWidths(next);
      if (n === 2) saveWidth(PANE_WIDTH_KEY, nextWidths[0]);
    }
  };

  const handleReset = () => {
    const next = { ...savedWidths };
    delete next[n];
    setSavedWidths(next);
    saveSavedWidths(next);
    if (n === 2) saveWidth(PANE_WIDTH_KEY, 0);
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

  return (
    <div ref={host} data-pane-host className="relative flex min-w-0 flex-1">
      {ids.map((id, i) => {
        const info = infos[i];
        // Too narrow for all: the unfocused panes stay mounted but out of sight.
        const hidden = inSplit && !sideBySide && !info.focused;
        const isNotLast = sideBySide && i < ids.length - 1;
        const paneW = isNotLast ? (currentWidths[i] ?? equalShare) : undefined;
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
              // Cmd (Ctrl elsewhere) held: drag the pane. Plain presses are left alone.
              onPointerDown={(e) => sideBySide && id && startPaneDrag(e, id, titleOf(e.currentTarget))}
              className={`relative min-w-0 ${
                hidden
                  ? "hidden"
                  : `flex ${isNotLast ? "shrink-0 border-r border-line" : "flex-1"} ${
                      sideBySide && !info.focused
                        ? "opacity-75 saturate-[.85] transition-[opacity,filter] hover:opacity-100"
                        : ""
                    }`
              }`}
              style={isNotLast ? { width: paneW } : undefined}
            >
              <CodeView />
              {sideBySide && id && (
                <button
                  type="button"
                  aria-hidden="true"
                  tabIndex={-1}
                  title={ids.length === 2 ? "Drag to swap the panes" : "Drag to move pane"}
                  className="group absolute left-1/2 top-0 z-20 h-2 w-11 -translate-x-1/2 cursor-grab"
                  onPointerDown={(e) => {
                    e.stopPropagation();
                    startPaneDrag(e, id, titleOf(e.currentTarget), true);
                  }}
                >
                  <span className="mx-auto mt-[2px] block h-[3px] w-8 rounded-full bg-fg/30 opacity-0 transition-opacity group-hover:opacity-100" />
                </button>
              )}
              {isNotLast && (
                <ResizeHandle
                  width={paneW ?? equalShare}
                  min={minPaneW}
                  max={maxFor(i)}
                  initial={equalShare}
                  edge="right"
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
      <DragFeedback />
    </div>
  );
}
