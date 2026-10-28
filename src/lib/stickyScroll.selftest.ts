/**
 * Pure-function checks for the sticky-scroll pin decision.
 * Run: node --experimental-strip-types src/lib/stickyScroll.selftest.ts
 */
import { nextLastTop, nextStick, type ScrollSample } from "./stickyScroll.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const s = (scrollTop: number, scrollHeight = 5000, clientHeight = 800): ScrollSample => ({ scrollTop, scrollHeight, clientHeight });
const bottom = 5000 - 800;

// Pinned and content streams in: scrollTop only grows, stays pinned.
assert(nextStick(true, s(bottom), s(bottom + 40, 5040)), "growth while pinned stays pinned");

// Fractional-DPR readback: set to the bottom, reads back a hair lower. Must not detach.
assert(nextStick(true, s(bottom), s(bottom - 0.4)), "sub-pixel drop does not detach");
assert(nextStick(true, s(bottom), s(bottom - 1)), "1px drop does not detach");

// A real upward scroll detaches, even inside the near-bottom zone.
assert(!nextStick(true, s(bottom), s(bottom - 3)), "3px upward scroll inside near zone detaches");
assert(!nextStick(true, s(bottom), s(bottom - 400)), "large upward scroll detaches");

// Content shrinking drops scrollTop without the user: stays pinned.
assert(nextStick(true, s(bottom), s(bottom - 200, 4800)), "layout shrink does not detach");

// Detached stays detached while away from the bottom, re-pins on reaching the bottom.
assert(!nextStick(false, s(1000), s(1200)), "scrolling down but far away stays detached");
assert(nextStick(false, s(bottom - 200), s(bottom - 10)), "reaching the bottom re-pins");

// A slow smooth scroll up (2px/frame) is still caught every frame.
let stick = true;
let prev = s(bottom);
for (let i = 1; i <= 5; i++) {
  const cur = s(bottom - 2 * i);
  stick = nextStick(stick, prev, cur);
  prev = cur;
}
assert(!stick, "slow upward scroll detaches");

// A slow drag of 0.4px per event must accumulate into a detach, not be forgiven each time.
stick = true;
let ref = bottom;
for (let i = 1; i <= 6; i++) {
  const cur = s(bottom - 0.4 * i);
  stick = nextStick(stick, s(ref), cur);
  ref = nextLastTop(ref, cur.scrollTop);
}
assert(!stick, "sub-pixel-per-event drag eventually detaches");
assert(nextLastTop(100, 100.5) === 100.5, "growth advances the reference");
assert(nextLastTop(100, 99.6) === 100, "rounding drop keeps the reference");
assert(nextLastTop(100, 90) === 90, "real drop moves the reference");

console.log("stickyScroll selftest: ok");
