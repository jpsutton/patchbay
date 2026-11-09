import type { CacheClock } from "../lib/ipc";

/**
 * A context this big, sent after the prompt cache expired, costs enough at the cache-write price
 * that the user gets to compact first. Higher than the agent's own "big context" mark (30k), which
 * only adds a note to the transcript.
 */
export const COLD_CACHE_PROMPT_TOKENS = 100_000;

/** Unix milliseconds the cache expired, when it has and the context is big enough to ask about; else null. */
export function coldCacheExpiredAt(cache: CacheClock | null | undefined, contextTokens: number | null | undefined, now: number): number | null {
  if (!cache || cache.lastAt == null || contextTokens == null || contextTokens < COLD_CACHE_PROMPT_TOKENS) return null;
  const expiresAt = cache.lastAt + cache.ttlSecs * 1000;
  return now >= expiresAt ? expiresAt : null;
}

/** `/compact` and `/keepwarm` talk about the cache themselves, so they go straight through. */
export function isCacheCommand(text: string): boolean {
  return /^\/(compact|keepwarm)(\s|$)/.test(text.trim());
}
