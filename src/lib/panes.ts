/**
 * Split view for Code sessions: which sessions sit in panes, and which one has focus. Pure and
 * import-free, so it runs under node (see panes.selftest.ts); the store holds the state.
 *
 * `panes` is empty when there is no split. Otherwise it lists the sessions left to right and
 * `active` is one of them: the focused pane, which the rest of the app treats as "the" open session.
 */

export interface PaneState {
  panes: string[];
  active: string | null;
}

export type Side = "left" | "right";

/** shortcut: two columns, no rows. The state is a list so more columns can follow. */
export const MAX_PANES = 2;
export const MIN_PANE_WIDTH = 360;

/** How many panes fit side by side in `width` pixels. */
export const columnsFor = (width: number) => Math.max(1, Math.min(MAX_PANES, Math.floor(width / MIN_PANE_WIDTH)));

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
export const otherSide = (s: PaneState): Side => (s.panes.length > 1 && s.panes.indexOf(s.active ?? "") === 1 ? "left" : "right");

/** Open `id` beside the open session. When the split is full it replaces the pane on `side`. */
export function openInSplit(s: PaneState, id: string, side: Side): PaneState {
  if (s.panes.includes(id)) return { panes: s.panes, active: id };
  if (s.panes.length === 0) {
    // Nothing to put it beside.
    if (s.active === null || s.active === id) return { panes: [], active: id };
    return { panes: side === "left" ? [id, s.active] : [s.active, id], active: id };
  }
  const panes = s.panes.slice();
  panes[side === "left" ? 0 : panes.length - 1] = id;
  return { panes, active: id };
}

/** Close `id`'s pane. The last pane left becomes the single open session. */
export function closePane(s: PaneState, id: string | null): PaneState {
  if (id === null || !s.panes.includes(id)) return s;
  const rest = s.panes.filter((p) => p !== id);
  if (rest.length <= 1) return { panes: [], active: rest[0] ?? null };
  return { panes: rest, active: s.active === id ? rest[0] : s.active };
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

/** Swap the two panes' places; focus stays with its session. */
export function swapPanes(s: PaneState): PaneState {
  return s.panes.length < 2 ? s : { panes: [...s.panes].reverse(), active: s.active };
}

/** The panes to restore from storage: only a full split of sessions that still exist. */
export function restorePanes(saved: string[], live: ReadonlySet<string>): string[] {
  const panes = [...new Set(saved)].filter((p) => live.has(p));
  return panes.length === MAX_PANES ? panes : [];
}

/** Which side of a host at `left`, `width` wide the pointer at `x` is over. */
export const dropSide = (x: number, left: number, width: number): Side => (x < left + width / 2 ? "left" : "right");

/** Whether dropping session `id` on a host `hostWidth` wide would change anything: there's room for two, and it isn't already a pane or the only open session. */
export function dropAllowed(s: PaneState, id: string, hostWidth: number): boolean {
  if (columnsFor(hostWidth) < 2 || s.panes.includes(id)) return false;
  return !(s.panes.length === 0 && s.active === id);
}

/** Whether dragging pane `id` onto `side` of a host `hostWidth` wide would swap the two: both are on screen, and it's the other side's. */
export function swapAllowed(s: PaneState, id: string, side: Side, hostWidth: number): boolean {
  return s.panes.length === 2 && columnsFor(hostWidth) >= 2 && s.panes.indexOf(id) === (side === "left" ? 1 : 0);
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

/** The left pane's width, kept so neither pane drops below the minimum. */
export function clampLeftWidth(w: number, hostWidth: number): number {
  if (hostWidth < 2 * MIN_PANE_WIDTH) return Math.round(hostWidth / 2);
  return Math.round(Math.min(hostWidth - MIN_PANE_WIDTH, Math.max(MIN_PANE_WIDTH, w)));
}
