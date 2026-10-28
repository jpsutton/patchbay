/** Distance from the bottom that still counts as "at the bottom". */
export const NEAR_BOTTOM_PX = 80;
/**
 * A scrollTop drop this small is rounding, not the user. On fractional-DPR screens (most Android
 * phones) a scrollTop set to the bottom can read back a fraction lower.
 */
export const SCROLL_UP_SLACK_PX = 1;

export type ScrollSample = { scrollTop: number; scrollHeight: number; clientHeight: number };

/**
 * Whether the scroller should stay pinned after a scroll event. `prev` is the sample from the
 * previous scroll event (zeros before the first), `stick` the current pin state.
 */
export function nextStick(stick: boolean, prev: ScrollSample, cur: ScrollSample): boolean {
  const near = cur.scrollHeight - cur.scrollTop - cur.clientHeight < NEAR_BOTTOM_PX;
  // Any real upward scroll detaches, even inside the near-bottom zone: smooth scrolling moves a few
  // pixels per frame, so checking `near` first would re-pin on every streamed chunk and the user
  // could never scroll away. A scrollTop drop from content shrinking is a layout shift, not the user.
  const shrank = cur.scrollHeight < prev.scrollHeight;
  if (cur.scrollTop < prev.scrollTop - SCROLL_UP_SLACK_PX && !shrank) return false;
  return near ? true : stick;
}

/**
 * The scrollTop to compare the next event against. A rounding-sized drop keeps the old reference, so
 * a slow drag of under a pixel per event still adds up to a detach instead of being forgiven each time.
 */
export function nextLastTop(prevTop: number, curTop: number): number {
  return curTop < prevTop && prevTop - curTop <= SCROLL_UP_SLACK_PX ? prevTop : curTop;
}
