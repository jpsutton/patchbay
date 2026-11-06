/**
 * Pure-function checks for the cold-cache prompt.
 * Run: node --experimental-strip-types src/code/coldCache.selftest.ts
 */
import { COLD_CACHE_PROMPT_TOKENS, coldCacheExpiredAt, isCacheCommand } from "./coldCache.ts";

function eq(actual: unknown, expected: unknown, msg: string) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}: expected ${e}, got ${a}`);
}

const big = COLD_CACHE_PROMPT_TOKENS;
const clock = { lastAt: 1_000_000, ttlSecs: 300, auto: false };

eq(coldCacheExpiredAt(clock, big, 1_000_000 + 299_999), null, "still warm");
eq(coldCacheExpiredAt(clock, big, 1_300_000), 1_300_000, "expired at the ttl");
eq(coldCacheExpiredAt(clock, big - 1, 2_000_000), null, "small context");
eq(coldCacheExpiredAt(clock, null, 2_000_000), null, "unknown context");
eq(coldCacheExpiredAt({ ...clock, lastAt: null }, big, 2_000_000), null, "not known to be warm");
eq(coldCacheExpiredAt(null, big, 2_000_000), null, "no clock");

eq(isCacheCommand("/compact"), true, "compact");
eq(isCacheCommand("  /compact the API work"), true, "compact with focus");
eq(isCacheCommand("/keepwarm auto"), true, "keepwarm");
eq(isCacheCommand("/compactor"), false, "other command");
eq(isCacheCommand("please /compact"), false, "mid-sentence");

console.log("coldCache selftest ok");
