/** Gap between the chip and its menu, and the inset kept from the viewport edge. */
const GAP = 6;
const MARGIN = 8;

export interface AnchorBox {
  top: number;
  bottom: number;
  left: number;
  right: number;
}

export interface MenuPlace {
  left: number;
  top: number;
  maxHeight: number;
  minWidth: number;
  side: "up" | "down";
}

/**
 * Place a chip menu inside the viewport.
 *
 * Opens downward when the menu fits there. A composer chip sits on the bottom edge of a
 * maximized window, where the space below is a few pixels: a downward menu runs off the screen.
 * Open upward in that case. Height is capped to the chosen side so a long list scrolls
 * instead of leaving the window.
 */
export function placeChipMenu(
  anchor: AnchorBox,
  menu: { width: number; height: number },
  viewport: { width: number; height: number },
): MenuPlace {
  const spaceBelow = Math.max(0, viewport.height - anchor.bottom - GAP - MARGIN);
  const spaceAbove = Math.max(0, anchor.top - GAP - MARGIN);
  const wants = menu.height > 0 ? menu.height : Math.max(spaceAbove, spaceBelow);
  const side = wants > spaceBelow && spaceAbove > spaceBelow ? "up" : "down";
  const maxHeight = side === "up" ? spaceAbove : spaceBelow;
  const height = Math.min(Math.max(0, menu.height), maxHeight);
  const top = side === "up" ? anchor.top - GAP - height : anchor.bottom + GAP;
  const room = Math.max(0, viewport.width - 2 * MARGIN);
  const width = Math.min(Math.max(0, menu.width), room);
  let left = anchor.left;
  const maxLeft = viewport.width - MARGIN - width;
  if (left > maxLeft) left = maxLeft;
  if (left < MARGIN) left = MARGIN;
  const minWidth = Math.min(Math.max(0, anchor.right - anchor.left), room);
  return { left, top, maxHeight, minWidth, side };
}

/**
 * Next label that starts with `prefix`.
 * `advance` skips `from`: a repeated first letter moves to the next match.
 * Returns `from` when nothing matches.
 */
export function matchByPrefix(labels: readonly string[], from: number, prefix: string, advance: boolean): number {
  const p = prefix.toLowerCase();
  const n = labels.length;
  if (!p || n === 0) return from;
  const start = advance ? from + 1 : Math.max(0, from);
  for (let i = 0; i < n; i++) {
    const idx = (((start + i) % n) + n) % n;
    if (labels[idx]!.toLowerCase().startsWith(p)) return idx;
  }
  return from;
}
