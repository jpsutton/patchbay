/**
 * Pure-function checks for adopting a saved earlier history.
 * Run: node --experimental-strip-types src/lib/earlierHistory.selftest.ts
 */
import { adoptEarlier } from "./earlierHistory.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const part = (...entries: string[]) => ({ entries, toolCalls: {} });

const saved = part("imported");
assert(adoptEarlier(null, saved) === saved, "a window with no earlier history takes the saved copy");
assert(adoptEarlier(part(), saved) === saved, "an empty one does too");
const shown = part("already here");
assert(adoptEarlier(shown, saved) === shown, "what the window has is kept");
assert(adoptEarlier(null, null) === null, "nothing saved, nothing shown");
const empty = part();
assert(adoptEarlier(empty, part()) === empty, "an empty saved copy changes nothing");

console.log("earlierHistory selftest ok");
