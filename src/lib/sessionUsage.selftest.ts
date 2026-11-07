/**
 * Run: node --experimental-strip-types src/lib/sessionUsage.selftest.ts
 */
import { contextPct, hasUsage, usageToneClass } from "./sessionUsage.ts";
import type { Tokens } from "./ipc";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const tokens = (over: Partial<Tokens> = {}): Tokens => ({
  input: 0,
  cachedRead: 0,
  cacheWrite: 0,
  output: 0,
  reasoning: 0,
  costUsd: null,
  apiMs: 0,
  ...over,
});

// The empty state the sidebar slot had before this gauge existed: nothing reported, nothing shown.
assert(!hasUsage(null, null), "no usage state at all stays hidden");
assert(!hasUsage(tokens(), null), "zero totals and no context count stay hidden");
assert(hasUsage(tokens({ input: 1200 }), null), "input alone is worth drawing");
assert(hasUsage(tokens({ output: 900 }), null), "output alone is worth drawing");
assert(hasUsage(tokens({ input: 0, output: 0 }), 4000), "a context count with no totals is worth drawing");

assert(contextPct(null, 128_000) == null, "no context count, no percent");
assert(contextPct(24_000, null) == null, "no window, no percent");
assert(Math.round(contextPct(24_000, 128_000)!) === 19, "fill is a percent of the window");
assert(contextPct(200_000, 128_000) === 100, "a count past the window reads as full");

assert(usageToneClass(null, 90) === "stroke-muted", "an unknown fill is muted");
assert(usageToneClass(10, 90) === "stroke-live", "a low fill is green");
assert(usageToneClass(67, 90) === "stroke-live", "just under three quarters of the limit is still green");
assert(usageToneClass(68, 90) === "stroke-warn", "three quarters of the limit turns amber");
assert(usageToneClass(90, 90) === "stroke-danger", "the limit turns red");
// The limit is the agent's own auto-compact percent when it reports one, not always 90.
assert(usageToneClass(80, 85) === "stroke-warn", "amber is relative to that limit too");
assert(usageToneClass(85, 85) === "stroke-danger", "and so is red");

console.log("sessionUsage selftest ok");
