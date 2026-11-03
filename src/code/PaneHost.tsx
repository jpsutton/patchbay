import { useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { CodeView } from "./CodeView";
import { loadWidth, ResizeHandle, saveWidth } from "../components/ResizeHandle";
import { useStore } from "../lib/store";
import { usePaneDrag, startPaneDrag } from "../lib/paneDrag";
import { clampLeftWidth, columnsFor, MIN_PANE_WIDTH, paneKeys, type PaneKeys } from "../lib/panes";
import { PaneContext, type PaneInfo } from "../lib/shortcuts";

const PANE_WIDTH_KEY = "patchbay.paneWidth";

/** The session title a pane shows, for the label that follows a dragged pane. */
const titleOf = (el: Element) => el.closest("[data-pane]")?.querySelector("h1")?.textContent ?? "Session";

/** What dropping a dragged session would do (the half it lands on), and a label that follows the pointer. */
function DragFeedback({ inSplit }: { inSplit: boolean }) {
  const drag = usePaneDrag((s) => s.drag);
  if (!drag) return null;
  return (
    <>
      {drag.side && (
        <div
          aria-hidden="true"
          className={`pointer-events-none absolute inset-y-0 z-30 w-1/2 border-2 border-accent bg-accent/10 ${
            drag.side === "left" ? "left-0" : "right-0"
          }`}
        >
          <span className="absolute left-1/2 top-4 -translate-x-1/2 rounded-full bg-accent px-3 py-1 text-[12px] font-medium text-accent-fg">
            {drag.kind === "pane" ? "Swap places" : inSplit ? "Replace this pane" : "Open here"}
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
 * A split puts two side by side with a divider; a window too narrow for both shows only the focused
 * one and keeps the other mounted (hidden), so its terminal and scroll position survive.
 */
export function PaneHost() {
  const panes = useStore((s) => s.panes);
  const open = useStore((s) => s.activeCodeSessionId);
  const select = useStore((s) => s.selectCodeSession);
  const host = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  // 0 = no width chosen: the panes share the room equally.
  const [chosen, setChosen] = useState(() => loadWidth(PANE_WIDTH_KEY, 0, 0, 100_000));

  useLayoutEffect(() => {
    const el = host.current;
    if (!el) return;
    setWidth(el.getBoundingClientRect().width);
    const ro = new ResizeObserver(([e]) => setWidth(e.contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const inSplit = panes.length >= 2;
  const sideBySide = inSplit && columnsFor(width) >= panes.length;
  const half = Math.round(width / 2);
  const left = sideBySide ? (chosen ? clampLeftWidth(chosen, width) : half) : 0;
  const ids: (string | null)[] = inSplit ? panes : [open];
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
        // Too narrow for both: the unfocused pane stays mounted but out of sight.
        const hidden = inSplit && !sideBySide && !info.focused;
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
              // Cmd (Ctrl elsewhere) held: drag the pane to the other side. Plain presses are left alone.
              onPointerDown={(e) => sideBySide && id && startPaneDrag(e, id, titleOf(e.currentTarget))}
              className={`relative min-w-0 ${
                hidden
                  ? "hidden"
                  : `flex ${sideBySide && i === 0 ? "shrink-0 border-r border-line" : "flex-1"} ${
                      sideBySide && !info.focused
                        ? "opacity-75 saturate-[.85] transition-[opacity,filter] hover:opacity-100"
                        : ""
                    }`
              }`}
              style={sideBySide && i === 0 ? { width: left } : undefined}
            >
              <CodeView />
              {sideBySide && id && (
                <button
                  type="button"
                  aria-hidden="true"
                  tabIndex={-1}
                  title="Drag to swap the panes"
                  className="group absolute left-1/2 top-0 z-20 h-2 w-11 -translate-x-1/2 cursor-grab"
                  onPointerDown={(e) => {
                    e.stopPropagation();
                    startPaneDrag(e, id, titleOf(e.currentTarget), true);
                  }}
                >
                  <span className="mx-auto mt-[2px] block h-[3px] w-8 rounded-full bg-fg/30 opacity-0 transition-opacity group-hover:opacity-100" />
                </button>
              )}
              {sideBySide && i === 0 && (
                <ResizeHandle
                  width={left}
                  min={MIN_PANE_WIDTH}
                  max={Math.max(MIN_PANE_WIDTH, width - MIN_PANE_WIDTH)}
                  initial={half}
                  edge="right"
                  label="Resize panes"
                  onChange={(w, done) => {
                    // Back at the middle means "share equally" again, so a window resize keeps it there.
                    const auto = done && w === half;
                    setChosen(auto ? 0 : w);
                    if (done) saveWidth(PANE_WIDTH_KEY, auto ? 0 : w);
                  }}
                />
              )}
            </div>
          </PaneContext.Provider>
        );
      })}
      <DragFeedback inSplit={inSplit} />
    </div>
  );
}
