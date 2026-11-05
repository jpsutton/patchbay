import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { RowKeyContext } from "../lib/rowMemory";

/** One row of a transcript: an entry, a group of tool calls, or a note between parts. */
export type TranscriptRow = {
  /** Stable across renders, so a row keeps its measured height. */
  key: string;
  /** Earlier history, drawn faded. */
  dim?: boolean;
  render: () => ReactNode;
};

/** Gap between rows, as the transcript's `gap-4`. */
const GAP_PX = 16;
/** A guess for rows not measured yet; most are a tool row or a short paragraph. */
const ESTIMATE_PX = 72;

/**
 * The rows of a transcript, with only those near the viewport mounted. A session with thousands
 * of tool calls otherwise keeps tens of thousands of nodes in the page, and every update lays
 * them all out. `scroller` is the scrolling element this list sits in, below whatever comes
 * before it there.
 */
export function TranscriptRows({ rows, scroller }: { rows: TranscriptRow[]; scroller: HTMLElement | null }) {
  const list = useRef<HTMLDivElement>(null);
  // Where the list starts in the scroller's content: the banner and notes above it move it down.
  const [margin, setMargin] = useState(0);
  useLayoutEffect(() => {
    const top = list.current?.offsetTop ?? 0;
    if (top !== margin) setMargin(top);
  });
  const v = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller,
    estimateSize: () => ESTIMATE_PX,
    getItemKey: (i) => rows[i].key,
    gap: GAP_PX,
    overscan: 6,
    scrollMargin: margin,
  });
  // The library's default skips a row that was measured before while the user scrolls up. Rows
  // grow and shrink after they mount (highlighting, images, a row coming back from unmount), so
  // every such change above the viewport then shoved what the reader was looking at. Keep the
  // viewport still for any row that lies wholly above it, whichever way the user scrolls.
  v.shouldAdjustScrollPositionOnItemSizeChange = (item, _delta, instance) => {
    const top = instance.scrollOffset ?? 0;
    // First measure, in place of the estimate: the row's top being above the viewport is enough.
    if (!instance.itemSizeCache.has(item.key)) return item.start < top;
    return item.end <= top;
  };
  return (
    <div ref={list} className="relative w-full" style={{ height: v.getTotalSize() }}>
      {v.getVirtualItems().map((item) => {
        const row = rows[item.index];
        return (
          <div
            key={item.key}
            data-index={item.index}
            ref={v.measureElement}
            className={`absolute left-0 top-0 w-full ${row.dim ? "opacity-70" : ""}`}
            style={{ transform: `translateY(${item.start - margin}px)` }}
          >
            <RowKeyContext.Provider value={row.key}>{row.render()}</RowKeyContext.Provider>
          </div>
        );
      })}
    </div>
  );
}
