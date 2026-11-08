/**
 * A turn the host reported quiet (pb-20261107-17, `src-tauri/src/code/stall.rs`): the `stalled`
 * event marks the session, and any other event from it clears the mark, the same rule the host
 * applies. Nothing is cancelled; the transcript says so and the sidebar shows it.
 */

export interface StallMark {
  /** Seconds the turn had been quiet when the host reported it; null when it isn't stalled. */
  stalled: number | null;
}

/** The session's mark after event `type` (the `stalled` event itself sets it in the reducer). */
export function trackStall<S extends StallMark>(type: string, next: S): S {
  if (type === "stalled" || next.stalled == null) return next;
  return { ...next, stalled: null };
}
