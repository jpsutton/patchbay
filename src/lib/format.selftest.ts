/**
 * Run: node --experimental-strip-types src/lib/format.selftest.ts
 */
import { fmtAgo } from "./format.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}

const now = Date.parse("2026-10-22T16:00:00Z");
assert(fmtAgo(now - 10_000, now) === "just now", "under a minute");
assert(fmtAgo(now - 12 * 60_000, now) === "12 minutes ago", "minutes");
assert(fmtAgo(now - 60_000, now) === "1 minute ago", "one minute");
assert(fmtAgo(now - 3 * 3_600_000, now) === "3 hours ago", "hours");
assert(fmtAgo(now - 26 * 3_600_000, now) === "yesterday", "one day");
assert(fmtAgo(now - 3 * 86_400_000, now) === "3 days ago", "days");
assert(!fmtAgo(now - 14 * 86_400_000, now).includes("ago"), "older than a week is a calendar time");

console.log("format.selftest: ok");
