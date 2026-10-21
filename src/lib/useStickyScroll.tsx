import { useCallback, useEffect, useRef, useState, type DependencyList, type UIEvent } from "react";

const NEAR_BOTTOM_PX = 80;

/**
 * Keeps a scroller pinned to the bottom while content streams in, until the user scrolls up.
 * `pin()` re-attaches it (e.g. when the user sends a message).
 */
export function useStickyScroll(deps: DependencyList) {
  const [el, setEl] = useState<HTMLDivElement | null>(null);
  const elRef = useRef<HTMLDivElement | null>(null);
  const stick = useRef(true);
  const [atBottom, setAtBottom] = useState(true);
  const [atTop, setAtTop] = useState(true);

  const ref = useCallback((node: HTMLDivElement | null) => {
    elRef.current = node;
    setEl(node);
  }, []);

  useEffect(() => {
    const node = elRef.current;
    if (node && stick.current) node.scrollTop = node.scrollHeight;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  // Content also grows after a render (highlighting, diagrams, images); follow it while pinned.
  // The scroller itself shrinks when a phone's keyboard opens (the app is padded by the keyboard)
  // and grows as the composer resizes: stay at the bottom then too, or the latest messages slip
  // out of view behind the keyboard.
  useEffect(() => {
    const content = el?.firstElementChild;
    if (!el || !content) return;
    const ro = new ResizeObserver(() => {
      if (stick.current) el.scrollTop = el.scrollHeight;
    });
    ro.observe(content);
    ro.observe(el);
    return () => ro.disconnect();
  }, [el]);

  const lastTop = useRef(0);
  const onScroll = (e: UIEvent<HTMLDivElement>) => {
    const node = e.currentTarget;
    const near = node.scrollHeight - node.scrollTop - node.clientHeight < NEAR_BOTTOM_PX;
    // Only an upward scroll detaches; layout shifts while content renders must not.
    if (near) stick.current = true;
    else if (node.scrollTop < lastTop.current) stick.current = false;
    lastTop.current = node.scrollTop;
    setAtBottom(near);
    setAtTop(node.scrollTop < 1);
  };

  /** Re-attach and go to the bottom now and once the layout settles (composer clearing etc.). */
  const pin = () => {
    stick.current = true;
    setAtBottom(true);
    const node = elRef.current;
    if (!node) return;
    node.scrollTop = node.scrollHeight;
    requestAnimationFrame(() => {
      if (stick.current) node.scrollTop = node.scrollHeight;
    });
  };

  return { ref, el, onScroll, atBottom, atTop, pin, toBottom: pin };
}

export function JumpToBottom({ show, onClick }: { show: boolean; onClick: () => void }) {
  if (!show) return null;
  return (
    <button
      className="absolute bottom-3 left-1/2 z-10 -translate-x-1/2 rounded-full border border-border bg-raised px-3 py-1 text-xs text-muted shadow-raised hover:text-fg"
      onClick={onClick}
      aria-label="Jump to latest"
    >
      ↓ Jump to latest
    </button>
  );
}

/**
 * Light blur over content scrolling under the top edge. Place it as a sibling just before the
 * scroller, inside a `relative` wrapper, and give the scroller `fade-top` while scrolled.
 */
export function ScrollFade({ show }: { show: boolean }) {
  return <div className={`scroll-fade ${show ? "opacity-100" : "opacity-0"}`} aria-hidden="true" />;
}
