/**
 * Placement of a composer chip menu. The failure: a model chip on the bottom edge of a
 * maximized window, menu opening downward, most of the list past the screen.
 * Run: node --experimental-strip-types src/lib/chipMenu.selftest.ts
 */
import { matchByPrefix, placeChipMenu, type AnchorBox } from "./chipMenu.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const box = (top: number, bottom: number, left: number, right: number): AnchorBox => ({ top, bottom, left, right });

// Maximized window, chip on the bottom edge, list taller than the sliver below it.
// Must open upward, fully inside the viewport, and not cover the chip.
{
  const anchor = box(940, 972, 400, 560);
  const menu = { width: 280, height: 264 };
  const viewport = { width: 1280, height: 1000 };
  const place = placeChipMenu(anchor, menu, viewport);
  assert(place.side === "up", "bottom chip opens upward");
  assert(place.top === 670, `upward top is 670, got ${place.top}`);
  assert(place.top >= 8, "menu stays below the viewport inset");
  assert(place.top + menu.height <= anchor.top - 6, "upward menu ends above the chip");
  assert(place.top + menu.height <= viewport.height - 8, "upward menu stays inside the viewport");
  assert(place.maxHeight >= menu.height, "list that fits above is not clipped");
}

// Chip with room below keeps the downward open.
{
  const place = placeChipMenu(box(100, 132, 40, 160), { width: 120, height: 80 }, { width: 1280, height: 800 });
  assert(place.side === "down", "mid-window chip opens downward");
  assert(place.top === 138, `downward top is 138, got ${place.top}`);
  assert(place.top >= 132 + 6, "downward menu starts below the chip");
}

// A short list that does not fit below still flips up.
{
  const place = placeChipMenu(box(700, 732, 40, 160), { width: 120, height: 80 }, { width: 1280, height: 780 });
  assert(place.side === "up", "list taller than the space below opens upward");
}

// Right edge: shift left so the menu stays inside.
{
  const place = placeChipMenu(box(100, 132, 1200, 1270), { width: 200, height: 40 }, { width: 1280, height: 800 });
  assert(place.left === 1072, `right-edge left is 1072, got ${place.left}`);
  assert(place.left + 200 <= 1280 - 8, "right-edge menu stays inside the viewport");
}

// Left edge: don't start past the inset.
{
  const place = placeChipMenu(box(100, 132, 0, 80), { width: 100, height: 40 }, { width: 800, height: 600 });
  assert(place.left === 8, `left-edge left is 8, got ${place.left}`);
}

// List taller than the window: cap height, pin the top to the inset, scroll the rest.
{
  const place = placeChipMenu(box(900, 932, 20, 100), { width: 100, height: 2000 }, { width: 800, height: 960 });
  assert(place.side === "up", "over-tall list on a bottom chip opens upward");
  assert(place.maxHeight === 886, `capped height is 886, got ${place.maxHeight}`);
  assert(place.top === 8, `capped top is the inset, got ${place.top}`);
  assert(place.maxHeight < 2000, "over-tall list scrolls instead of leaving the window");
}

const labels = ["Claude Haiku", "Claude Opus", "Claude Sonnet", "GLM Flash"];
assert(matchByPrefix(labels, 0, "c", true) === 1, "repeated first letter moves to the next match");
assert(matchByPrefix(labels, 0, "cl", false) === 0, "growing prefix stays on the current match");
assert(matchByPrefix(labels, 2, "g", true) === 3, "letter jumps to the matching label");
assert(matchByPrefix(labels, 1, "zzz", false) === 1, "no match keeps the current index");
assert(matchByPrefix(labels, 3, "c", true) === 0, "typeahead wraps");
