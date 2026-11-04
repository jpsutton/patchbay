/**
 * Run: node --experimental-strip-types src/lib/panes.selftest.ts
 */
import {
  clampLeftWidth,
  closePane,
  columnsFor,
  cyclePane,
  dropAllowed,
  dropSide,
  openInSplit,
  otherSide,
  paneKeys,
  prunePanes,
  restorePanes,
  selectPane,
  swapAllowed,
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

// A full split: an open session is just focused, a new one replaces the pane on that side.
eq(openInSplit(st(["a", "b"], "a"), "b", "left"), st(["a", "b"], "b"), "already open: focus");
eq(openInSplit(st(["a", "b"], "a"), "c", "right"), st(["a", "c"], "c"), "replace right");
eq(openInSplit(st(["a", "b"], "b"), "c", "left"), st(["c", "b"], "c"), "replace left");

// Selecting in a split focuses an open pane or replaces the focused one.
eq(selectPane(st(["a", "b"], "a"), "b"), st(["a", "b"], "b"), "select open pane");
eq(selectPane(st(["a", "b"], "b"), "c"), st(["a", "c"], "c"), "select replaces focused");
eq(selectPane(st(["a", "b"], "a"), "c"), st(["c", "b"], "c"), "select replaces focused (left)");
eq(selectPane(st(["a", "b"], "b"), null), st([], "a"), "deselect closes the focused pane");

// Closing leaves the other pane as the single open session.
eq(closePane(st(["a", "b"], "a"), "a"), st([], "b"), "close focused");
eq(closePane(st(["a", "b"], "a"), "b"), st([], "a"), "close other");
eq(closePane(st(["a", "b"], "a"), "z"), st(["a", "b"], "a"), "close unknown");
eq(closePane(st([], "a"), "a"), st([], "a"), "close without split");

// Pruning drops sessions that are gone, and ends the split when one is left.
const live = new Set(["a", "b"]);
const same = st(["a", "b"], "a");
assert(prunePanes(same, live) === same, "nothing to prune keeps the same object");
eq(prunePanes(st(["a", "x"], "x"), live), st([], "a"), "pruned focused pane");
eq(prunePanes(st(["x", "y"], "x"), live), st([], null), "both gone");
assert(prunePanes(st([], "q"), live).active === "q", "no split: active untouched");

// Focus cycling wraps; swapping keeps focus on its session.
eq(cyclePane(st(["a", "b"], "a"), 1), st(["a", "b"], "b"), "next");
eq(cyclePane(st(["a", "b"], "a"), -1), st(["a", "b"], "b"), "previous wraps");
eq(cyclePane(st([], "a"), 1), st([], "a"), "cycle without split");
eq(swapPanes(st(["a", "b"], "a")), st(["b", "a"], "a"), "swap");

// Restoring: only a full split of sessions that still exist.
eq(restorePanes(["a", "b"], live), ["a", "b"], "restore");
eq(restorePanes(["a", "x"], live), [], "restore with a gone session");
eq(restorePanes(["a", "a"], live), [], "restore duplicates");
eq(restorePanes([], live), [], "restore nothing");

// "Open beside" replaces the pane you're not in.
eq([otherSide(st([], "a")), otherSide(st(["a", "b"], "a")), otherSide(st(["a", "b"], "b"))], ["right", "right", "left"], "other side");

// A drop only counts when there's room for two, and it would change something.
eq(
  [
    dropAllowed(st([], "a"), "b", 1000),
    dropAllowed(st([], "a"), "a", 1000),
    dropAllowed(st([], "a"), "b", 500),
    dropAllowed(st(["a", "b"], "a"), "b", 1000),
    dropAllowed(st(["a", "b"], "a"), "c", 1000),
    dropAllowed(st([], null), "b", 1000),
  ],
  [true, false, false, false, true, true],
  "drop allowed",
);

// Dragging a pane only swaps onto the other side, and only with both on screen.
eq(
  [
    swapAllowed(st(["a", "b"], "a"), "a", "right", 1000),
    swapAllowed(st(["a", "b"], "a"), "a", "left", 1000),
    swapAllowed(st(["a", "b"], "a"), "b", "left", 1000),
    swapAllowed(st(["a", "b"], "a"), "b", "right", 1000),
    swapAllowed(st(["a", "b"], "a"), "a", "right", 500),
    swapAllowed(st([], "a"), "a", "right", 1000),
  ],
  [true, false, true, false, false, false],
  "swap allowed",
);

// Width rules.
eq([columnsFor(300), columnsFor(719), columnsFor(720), columnsFor(2000)], [1, 1, 2, 2], "columns");
eq([dropSide(10, 0, 100), dropSide(50, 0, 100), dropSide(60, 0, 100)], ["left", "right", "right"], "drop side");
eq([dropSide(10, 256, 800), dropSide(700, 256, 800)], ["left", "right"], "drop side with an offset host");
eq(
  [clampLeftWidth(100, 1000), clampLeftWidth(500, 1000), clampLeftWidth(900, 1000), clampLeftWidth(300, 600)],
  [360, 500, 640, 300],
  "clamp left width",
);

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
