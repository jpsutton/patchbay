/**
 * Checks for the stalled-turn mark.
 * Run: node --experimental-strip-types src/lib/stall.selftest.ts
 */
import { trackStall } from "./stall.ts";

function assert(ok: boolean, what: string) {
  if (!ok) throw new Error(`stall selftest failed: ${what}`);
}

const marked = { stalled: 600, other: 1 };
assert(trackStall("stalled", marked) === marked, "the stalled event keeps its mark");
assert(trackStall("update", marked).stalled === null, "an update clears it");
assert(trackStall("promptDone", marked).stalled === null, "the turn ending clears it");
assert(trackStall("update", marked).other === 1, "the rest of the state is kept");
const clear = { stalled: null };
assert(trackStall("update", clear) === clear, "nothing to clear: same object, no re-render");
console.log("stall selftest ok");
