/** Where Tab must send focus to stay inside a modal with `count` focusable controls: the index to
 *  focus, or null when the browser's own Tab step already stays inside. `active` is the focused
 *  control's index, or -1 when focus is outside the modal. */
export function trapTarget(count: number, active: number, shift: boolean): number | null {
  if (count <= 0) return null;
  if (active < 0) return shift ? count - 1 : 0;
  if (shift && active === 0) return count - 1;
  if (!shift && active === count - 1) return 0;
  return null;
}

const FOCUSABLE = 'a[href],button:not([disabled]),input:not([disabled]),select:not([disabled]),textarea:not([disabled]),[tabindex]:not([tabindex="-1"])';

/** The controls Tab can reach inside `root`, in order. */
export function focusables(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.getClientRects().length > 0);
}
