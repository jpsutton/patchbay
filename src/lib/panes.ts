/**
 * Split view for Code sessions: which sessions sit in panes, and which one has focus. Pure and
 * import-free, so it runs under node (see panes.selftest.ts); the store holds the state.
 *
 * `panes` is empty when there is no split. Otherwise it lists the sessions along the split axis
 * (left→right for vertical columns, top→bottom for horizontal rows) and `active` is one of them:
 * the focused pane, which the rest of the app treats as "the" open session.
 */

export interface PaneState {
  panes: string[];
  active: string | null;
}

/** Axis of the split: columns (desktop) or stacked rows (phone). */
export type PaneOrientation = "vertical" | "horizontal";

export type Side = "left" | "right";

/** Maximum columns of split panes side by side on desktop. */
export const MAX_PANES = 4;
/** Maximum panes on the phone / remote app. */
export const MAX_PANES_REMOTE = 2;
export const MIN_PANE_WIDTH = 240;
/** Minimum density per pane (pixels) before a vertical split collapses to the focused pane. */
export const MIN_PANE_FLOOR = 180;
/** Minimum row height (pixels) before a horizontal split collapses to the focused pane. */
export const MIN_PANE_FLOOR_HEIGHT = 140;
export const MIN_PANE_HEIGHT = 160;

/** How many panes this platform may hold. */
export const maxPanesFor = (remote: boolean) => (remote ? MAX_PANES_REMOTE : MAX_PANES);

/** Default split axis: phone stacks rows; desktop uses columns. */
export const defaultOrientation = (remote: boolean): PaneOrientation => (remote ? "horizontal" : "vertical");

/** How many panes fit side by side in `width` pixels. */
export const columnsFor = (width: number) => Math.max(1, Math.min(MAX_PANES, Math.floor(width / MIN_PANE_WIDTH)));

/** How many stacked panes fit in `height` pixels (phone horizontal split). */
export const rowsFor = (height: number) =>
  Math.max(1, Math.min(MAX_PANES_REMOTE, Math.floor(height / MIN_PANE_HEIGHT)));

/** Open `id` as the app's single session, or in the focused pane when split. `null` closes that pane. */
export function selectPane(s: PaneState, id: string | null): PaneState {
  if (s.panes.length === 0) return { panes: [], active: id };
  if (id === null) return closePane(s, s.active);
  if (s.panes.includes(id)) return { panes: s.panes, active: id };
  const panes = s.panes.slice();
  panes[Math.max(0, panes.indexOf(s.active ?? ""))] = id;
  return { panes, active: id };
}

/** The side for "open beside": the pane you're not working in, so the focused one stays put. */
export const otherSide = (s: PaneState): Side =>
  s.panes.length > 1 && s.panes.indexOf(s.active ?? "") === s.panes.length - 1 ? "left" : "right";

/** Open `id` beside the open session. Adds a pane if under `maxPanes`; otherwise replaces on `side`. */
export function openInSplit(s: PaneState, id: string, side: Side, maxPanes: number = MAX_PANES): PaneState {
  if (s.panes.includes(id)) return { panes: s.panes, active: id };
  if (s.panes.length === 0) {
    // Nothing to put it beside.
    if (s.active === null || s.active === id) return { panes: [], active: id };
    return { panes: side === "left" ? [id, s.active] : [s.active, id], active: id };
  }
  const panes = s.panes.slice();
  if (panes.length < maxPanes) {
    const activeIdx = Math.max(0, panes.indexOf(s.active ?? ""));
    const insertAt = side === "left" ? activeIdx : activeIdx + 1;
    panes.splice(insertAt, 0, id);
  } else {
    // When full, replace the neighbour adjacent to the active pane on the requested side
    const activeIdx = Math.max(0, panes.indexOf(s.active ?? ""));
    const targetIdx =
      side === "left"
        ? (activeIdx > 0 ? activeIdx - 1 : 1)
        : (activeIdx < panes.length - 1 ? activeIdx + 1 : activeIdx - 1);
    panes[targetIdx] = id;
  }
  return { panes, active: id };
}

/** Close `id`'s pane. The last pane left becomes the single open session. */
export function closePane(s: PaneState, id: string | null): PaneState {
  if (id === null || !s.panes.includes(id)) return s;
  const idx = s.panes.indexOf(id);
  const rest = s.panes.filter((p) => p !== id);
  if (rest.length <= 1) return { panes: [], active: rest[0] ?? null };
  const nextActive = s.active === id ? (rest[Math.min(idx, rest.length - 1)] ?? rest[0]) : s.active;
  return { panes: rest, active: nextActive };
}

/** Drop panes whose session is gone (`live` lists the ones that still exist and aren't archived). */
export function prunePanes(s: PaneState, live: ReadonlySet<string>): PaneState {
  if (s.panes.length === 0) return s;
  const rest = s.panes.filter((p) => live.has(p));
  if (rest.length === s.panes.length) return s;
  if (rest.length <= 1) return { panes: [], active: rest[0] ?? null };
  return { panes: rest, active: s.active !== null && rest.includes(s.active) ? s.active : rest[0] };
}

/** Move focus to the next (+1) or previous (-1) pane, wrapping around. */
export function cyclePane(s: PaneState, delta: 1 | -1): PaneState {
  const n = s.panes.length;
  if (n < 2) return s;
  const at = Math.max(0, s.panes.indexOf(s.active ?? ""));
  return { panes: s.panes, active: s.panes[(at + delta + n) % n] };
}

/** Swap/reverse panes places. */
export function swapPanes(s: PaneState): PaneState {
  if (s.panes.length < 2) return s;
  return { panes: [...s.panes].reverse(), active: s.active };
}

/** Insert a pane at a specific index up to `maxPanes`. */
export function insertPaneAt(s: PaneState, id: string, atIndex: number, maxPanes: number = MAX_PANES): PaneState {
  if (s.panes.includes(id) || s.panes.length >= maxPanes) return s;
  if (s.panes.length === 0) {
    if (s.active === null || s.active === id) return { panes: [], active: id };
    const panes = atIndex === 0 ? [id, s.active] : [s.active, id];
    return { panes, active: id };
  }
  const panes = s.panes.slice();
  panes.splice(Math.max(0, Math.min(panes.length, atIndex)), 0, id);
  return { panes, active: id };
}

/** Replace a pane at a specific index. */
export function replacePaneAt(s: PaneState, id: string, atIndex: number): PaneState {
  if (s.panes.includes(id)) return { panes: s.panes, active: id };
  if (s.panes.length === 0) return { panes: [], active: id };
  const panes = s.panes.slice();
  panes[Math.max(0, Math.min(panes.length - 1, atIndex))] = id;
  return { panes, active: id };
}

/** Move a pane from one index to another. */
export function movePane(s: PaneState, fromIdx: number, toIdx: number): PaneState {
  if (s.panes.length < 2 || fromIdx === toIdx) return s;
  if (fromIdx < 0 || fromIdx >= s.panes.length) return s;
  const targetIdx = Math.max(0, Math.min(s.panes.length - 1, toIdx));
  const panes = s.panes.slice();
  const [item] = panes.splice(fromIdx, 1);
  panes.splice(targetIdx, 0, item);
  return { panes, active: s.active };
}

/** The panes to restore from storage: at least 2 sessions, up to `maxPanes`. */
export function restorePanes(saved: string[], live: ReadonlySet<string>, maxPanes: number = MAX_PANES): string[] {
  const panes = [...new Set(saved)].filter((p) => live.has(p));
  return panes.length >= 2 ? panes.slice(0, maxPanes) : [];
}

/** What `paneKeys` remembers between renders. Start with `{ anchor: null, single: "single", last: new Map() }`. */
export interface PaneKeys {
  anchor: string | null;
  single: string;
  last: Map<string | null, string>;
}

/**
 * React keys for the panes' views. A single pane keeps one key across session switches. In a split
 * each pane is keyed by its session, except the one open before the split, which keeps the single
 * pane's key so opening a split doesn't remount it. When a split ends, the pane left standing keeps
 * the key it had: a new key would remount it and close its terminal.
 */
export function paneKeys(prev: PaneKeys, ids: (string | null)[], inSplit: boolean): { keys: string[]; next: PaneKeys } {
  let { anchor, single } = prev;
  if (!inSplit) {
    single = prev.last.get(ids[0]) ?? single;
    anchor = ids[0];
  }
  const keyFor = (id: string | null) => {
    if (!inSplit || id === anchor) return single;
    const k = id ?? "none";
    // The single key can be a session's id; another pane showing that session needs its own.
    return k === single ? `${k}:pane` : k;
  };
  const keys = ids.map(keyFor);
  return { keys, next: { anchor, single, last: new Map(ids.map((id, i) => [id, keys[i]])) } };
}
