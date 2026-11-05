/**
 * Run: node --experimental-strip-types src/lib/panes.selftest.ts
 */
import {
  closePane,
  columnsFor,
  cyclePane,
  defaultOrientation,
  insertPaneAt,
  maxPanesFor,
  MAX_PANES_REMOTE,
  movePane,
  openInSplit,
  otherSide,
  paneKeys,
  prunePanes,
  replacePaneAt,
  restorePanes,
  rowsFor,
  selectPane,
  swapPanes,
  type PaneState,
} from "./panes.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}
const eq = (a: unknown, b: unknown, msg: string) =>
  assert(JSON.stringify(a) === JSON.stringify(b), `${msg}: got ${JSON.stringify(a)}, wanted ${JSON.stringify(b)}`);
const st = (panes: string[], active: string | null): PaneState => ({ panes, active });

// No split: selecting just sets the open session, as before.
eq(selectPane(st([], "a"), "b"), st([], "b"), "select without split");
eq(selectPane(st([], "a"), null), st([], null), "deselect without split");

// Opening a split puts the new session on the chosen side and focuses it.
eq(openInSplit(st([], "a"), "b", "right"), st(["a", "b"], "b"), "split right");
eq(openInSplit(st([], "a"), "b", "left"), st(["b", "a"], "b"), "split left");
eq(openInSplit(st([], null), "b", "right"), st([], "b"), "nothing to split with");
eq(openInSplit(st([], "a"), "a", "right"), st([], "a"), "splitting a session with itself");

// Opening more panes expands columns up to MAX_PANES:
eq(openInSplit(st(["a", "b"], "a"), "b", "left"), st(["a", "b"], "b"), "already open: focus");
eq(openInSplit(st(["a", "b"], "a"), "c", "right"), st(["a", "c", "b"], "c"), "expand right of a");
eq(openInSplit(st(["a", "b"], "b"), "c", "right"), st(["a", "b", "c"], "c"), "expand right of b");
eq(openInSplit(st(["a", "b"], "b"), "c", "left"), st(["a", "c", "b"], "c"), "expand left of b");
eq(openInSplit(st(["a", "b", "c"], "b"), "d", "right"), st(["a", "b", "d", "c"], "d"), "expand 4th pane");
// At MAX_PANES (4), replaces the neighbour adjacent to the active pane on that side:
eq(openInSplit(st(["a", "b", "c", "d"], "b"), "e", "right"), st(["a", "b", "e", "d"], "e"), "replace right neighbour at max panes");
eq(openInSplit(st(["a", "b", "c", "d"], "b"), "e", "left"), st(["e", "b", "c", "d"], "e"), "replace left neighbour at max panes");

// Selecting in a split focuses an open pane or replaces the focused one.
eq(selectPane(st(["a", "b"], "a"), "b"), st(["a", "b"], "b"), "select open pane");
eq(selectPane(st(["a", "b"], "b"), "c"), st(["a", "c"], "c"), "select replaces focused");
eq(selectPane(st(["a", "b"], "a"), "c"), st(["c", "b"], "c"), "select replaces focused (left)");
eq(selectPane(st(["a", "b"], "b"), null), st([], "a"), "deselect closes the focused pane");

// Closing leaves remaining panes; down to 1 ends the split.
eq(closePane(st(["a", "b", "c"], "b"), "b"), st(["a", "c"], "c"), "close middle pane leaves 2");
eq(closePane(st(["a", "b"], "a"), "a"), st([], "b"), "close focused leaves single");
eq(closePane(st(["a", "b"], "a"), "b"), st([], "a"), "close other leaves single");
eq(closePane(st(["a", "b"], "a"), "z"), st(["a", "b"], "a"), "close unknown");
eq(closePane(st([], "a"), "a"), st([], "a"), "close without split");

// Pruning drops sessions that are gone, and ends the split when one is left.
const live = new Set(["a", "b", "c"]);
const same = st(["a", "b"], "a");
assert(prunePanes(same, live) === same, "nothing to prune keeps the same object");
eq(prunePanes(st(["a", "b", "x"], "x"), live), st(["a", "b"], "a"), "pruned 1 of 3 panes");
eq(prunePanes(st(["a", "x"], "x"), live), st([], "a"), "pruned focused pane");
eq(prunePanes(st(["x", "y"], "x"), live), st([], null), "both gone");
assert(prunePanes(st([], "q"), live).active === "q", "no split: active untouched");

// Focus cycling wraps; swapping keeps focus on its session.
eq(cyclePane(st(["a", "b", "c"], "a"), 1), st(["a", "b", "c"], "b"), "next");
eq(cyclePane(st(["a", "b", "c"], "c"), 1), st(["a", "b", "c"], "a"), "next wraps");
eq(cyclePane(st(["a", "b", "c"], "a"), -1), st(["a", "b", "c"], "c"), "previous wraps");
eq(cyclePane(st([], "a"), 1), st([], "a"), "cycle without split");
eq(swapPanes(st(["a", "b"], "a")), st(["b", "a"], "a"), "swap 2 panes");
eq(swapPanes(st(["a", "b", "c"], "a")), st(["c", "b", "a"], "a"), "reverse 3 panes");

// Multi-pane insert, replace, and move:
eq(insertPaneAt(st(["a", "b"], "a"), "c", 1), st(["a", "c", "b"], "c"), "insert between");
eq(insertPaneAt(st(["a", "b"], "a"), "c", 0), st(["c", "a", "b"], "c"), "insert at start");
eq(insertPaneAt(st(["a", "b", "c"], "a"), "d", 3), st(["a", "b", "c", "d"], "d"), "insert at end");
eq(insertPaneAt(st(["a", "b", "c", "d"], "a"), "e", 2), st(["a", "b", "c", "d"], "a"), "insert at max does not add and keeps active");
eq(replacePaneAt(st(["a", "b", "c"], "a"), "d", 1), st(["a", "d", "c"], "d"), "replace middle pane");
eq(movePane(st(["a", "b", "c"], "a"), 0, 2), st(["b", "c", "a"], "a"), "move pane 0 to 2");
eq(movePane(st(["a", "b", "c"], "c"), 2, 0), st(["c", "a", "b"], "c"), "move pane 2 to 0");
eq(movePane(st(["a", "b"], "a"), 0, 1), st(["b", "a"], "a"), "swap 2 panes with movePane");

// Restoring: full split of sessions that still exist (2 to 4).
eq(restorePanes(["a", "b"], live), ["a", "b"], "restore 2");
eq(restorePanes(["a", "b", "c"], live), ["a", "b", "c"], "restore 3");
eq(restorePanes(["a", "x"], live), [], "restore with a gone session");
eq(restorePanes(["a", "a"], live), [], "restore duplicates");
eq(restorePanes([], live), [], "restore nothing");

// "Open beside" replaces the pane you're not in.
eq([otherSide(st([], "a")), otherSide(st(["a", "b"], "a")), otherSide(st(["a", "b"], "b"))], ["right", "right", "left"], "other side");

// Width / height rules.
eq([columnsFor(200), columnsFor(479), columnsFor(480), columnsFor(720), columnsFor(960), columnsFor(2000)], [1, 1, 2, 3, 4, 4], "columns");
eq([rowsFor(100), rowsFor(159), rowsFor(160), rowsFor(320), rowsFor(2000)], [1, 1, 1, 2, 2], "rows");

// Platform caps and default orientation.
eq([maxPanesFor(false), maxPanesFor(true)], [4, MAX_PANES_REMOTE], "max panes by platform");
eq([defaultOrientation(false), defaultOrientation(true)], ["vertical", "horizontal"], "default orientation");

// Remote max 2: a third open-in-split replaces the neighbour, never adds a third pane.
eq(openInSplit(st(["a", "b"], "a"), "c", "right", 2), st(["a", "c"], "c"), "remote: replace right at max 2");
eq(openInSplit(st(["a", "b"], "b"), "c", "left", 2), st(["c", "b"], "c"), "remote: replace left at max 2");
// No left neighbour of the first pane: replace the other pane instead.
eq(openInSplit(st(["a", "b"], "a"), "c", "left", 2), st(["a", "c"], "c"), "remote: replace other when no left neighbour");
eq(insertPaneAt(st(["a", "b"], "a"), "c", 1, 2), st(["a", "b"], "a"), "remote: insert at max 2 does not add");
eq(restorePanes(["a", "b", "c"], live, 2), ["a", "b"], "remote: restore caps at 2");

// React keys: a pane's key (and so its terminal) survives the split opening, a swap, and the other pane closing.
{
  let k = { anchor: null as string | null, single: "single", last: new Map<string | null, string>() };
  const render = (ids: (string | null)[], inSplit: boolean) => {
    const r = paneKeys(k, ids, inSplit);
    k = r.next;
    return r.keys;
  };
  eq(render(["a"], false), ["single"], "single pane");
  eq(render(["d"], false), ["single"], "switching sessions keeps the single key");
  eq(render(["d", "b"], true), ["single", "b"], "opening a split keeps the open session's key");
  eq(render(["b", "d"], true), ["b", "single"], "a swap keeps both keys");
  eq(render(["b"], false), ["b"], "closing the first pane: the survivor keeps its key");
  eq(render(["c"], false), ["b"], "then switching sessions keeps that key");
  eq(render(["c", "b"], true), ["b", "b:pane"], "a pane for the session the key is named after gets its own");
  eq(render(["b"], false), ["b:pane"], "and keeps it when it is the one left");
}

console.log("panes: ok");
