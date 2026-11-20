import { useEffect, useLayoutEffect, useRef, useState, type RefObject } from "react";

/**
 * How much a header bar has to give up to fit: 0 when everything fits, up to `max` when it still
 * overflows after every step. Measured from the header element, not from the window width, since
 * what has to fit varies (window controls, a long title, the agent's modes).
 *
 * Callers read the step to decide what to render, dropping the least important things first.
 * Hysteresis: a step is only given back once the width that made it overflow is available again,
 * so the header does not flicker between two steps at the boundary.
 *
 * `resetKey` (e.g. the pane layout or the session) clears what was measured, since widths from
 * another layout mean nothing here.
 */
export function useHeaderFit(
  ref: RefObject<HTMLElement | null>,
  max: number,
  resetKey?: unknown,
): number {
  const [step, setStep] = useState(0);
  /** The header width each step needed when it overflowed, to know when a wider one lets it go back. */
  const needed = useRef<number[]>([]);
  const fit = useRef<() => void>(() => {});
  fit.current = () => {
    const el = ref.current;
    if (!el) return;
    const room = el.clientWidth;
    // The title keeps a minimum width, so a header with too much in it overflows.
    if (el.scrollWidth > room + 1) {
      if (step < max) {
        needed.current[step] = el.scrollWidth;
        setStep(step + 1);
      }
    } else if (step > 0 && room >= (needed.current[step - 1] ?? Infinity)) setStep(step - 1);
  };
  // After every render, so a longer title or a new badge is noticed; the observer catches resizes.
  useLayoutEffect(() => fit.current());
  useEffect(() => {
    needed.current = [];
    setStep(0);
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(() => fit.current());
    ro.observe(el);
    return () => ro.disconnect();
  }, [resetKey]);
  return step;
}
