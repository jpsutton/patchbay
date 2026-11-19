import { Children, Fragment, isValidElement, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { ChipOverflow } from "./ChipSelect";
import { EllipsisIcon } from "./icons";

/** Width of the overflow button (h-9 w-9). */
const OVERFLOW_BTN_PX = 36;

/**
 * The footer's controls in order, with fragments, arrays and empty slots flattened away. Callers
 * pass `<>…</>` with conditionals; the row needs each control on its own to move it as a unit.
 */
export function footerItems(node: ReactNode): ReactNode[] {
  const out: ReactNode[] = [];
  const walk = (n: ReactNode) => {
    Children.forEach(n, (child) => {
      if (child == null || typeof child === "boolean" || child === "") return;
      if (isValidElement(child) && child.type === Fragment) {
        walk((child.props as { children?: ReactNode }).children);
        return;
      }
      out.push(child);
    });
  };
  walk(node);
  return out;
}

/**
 * The desktop composer's footer row. Controls never overlap: each one stays in the row only while
 * it fits at its minimum width, and the first that doesn't fit moves into the overflow menu with
 * everything after it (so the controls on the right go first). The menu shows each moved control
 * as a labelled row.
 *
 * A control's minimum is its natural width unless it can shrink (`flex-shrink` above 0), then its
 * CSS `min-width`: the model chip shrinks, the other chips and buttons fit whole or move.
 * Measuring reads the DOM, so each control is one element that renders nothing when it has nothing
 * to show (an agent picker with one agent): an empty slot takes no room and is never moved.
 */
export function ComposerFooterRow({ items, label }: { items: ReactNode[]; label: string }) {
  const rowRef = useRef<HTMLDivElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const [moved, setMoved] = useState<number[]>([]);
  const [open, setOpen] = useState(false);
  // The menu is 20rem wide and opens rightward from the button; near the right edge it opens leftward.
  const [alignRight, setAlignRight] = useState(false);

  const measure = () => {
    const row = rowRef.current;
    if (!row) return;
    const slots = Array.from(row.querySelectorAll<HTMLElement>(":scope > [data-footer-item]"));
    // Show every slot, at its natural size, so each reports its width.
    const entries = slots.flatMap((slot) => {
      const el = slot.firstElementChild as HTMLElement | null;
      return el ? [{ index: Number(slot.dataset.footerItem), slot, el }] : [];
    });
    const floors = entries.map(({ el }) => {
      const cs = getComputedStyle(el);
      const css = parseFloat(cs.minWidth);
      return cs.flexShrink !== "0" && Number.isFinite(css) ? css : null;
    });
    entries.forEach(({ slot, el }) => {
      slot.style.display = "contents";
      el.style.flex = "none";
      el.style.minWidth = "0";
    });
    const natural = entries.map(({ el }) => el.getBoundingClientRect().width);
    entries.forEach(({ slot, el }) => {
      slot.style.display = "";
      el.style.flex = "";
      el.style.minWidth = "";
    });
    const mins = natural.map((w, i) => (floors[i] === null ? w : Math.min(floors[i]!, w)));
    const gap = parseFloat(getComputedStyle(row).columnGap) || 0;
    const fits = (room: number) => {
      let used = 0;
      let count = 0;
      for (const w of mins) {
        const next = used + (count > 0 ? gap : 0) + w;
        if (next > room) break;
        used = next;
        count++;
      }
      return count;
    };
    let keep = fits(row.clientWidth);
    // Something has to move, so the overflow button takes its place in the row.
    if (keep < entries.length) keep = fits(row.clientWidth - OVERFLOW_BTN_PX - gap);
    const next = entries.slice(keep).map((e) => e.index);
    setMoved((prev) => (prev.length === next.length && prev.every((v, i) => v === next[i]) ? prev : next));
  };

  // Measure before paint on mount. Later changes arrive through the observers below, not on every
  // render: the composer re-renders on each keystroke, and measuring forces a layout.
  useLayoutEffect(measure, [items.length]);
  useEffect(() => {
    const row = rowRef.current;
    if (!row) return;
    const resize = new ResizeObserver(measure);
    resize.observe(row);
    // A chip's label changing (a new model, a new effort) changes its width without resizing the row.
    const mutate = new MutationObserver(measure);
    mutate.observe(row, { childList: true, characterData: true, subtree: true });
    void document.fonts?.ready.then(measure);
    return () => {
      resize.disconnect();
      mutate.disconnect();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const hasMoved = moved.length > 0;
  useEffect(() => {
    if (!hasMoved) setOpen(false);
  }, [hasMoved]);

  useLayoutEffect(() => {
    if (!open || !wrapRef.current) return;
    setAlignRight(wrapRef.current.getBoundingClientRect().left + 320 > window.innerWidth - 8);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      const t = e.target as Node;
      if (wrapRef.current?.contains(t)) return;
      // A chip's list is portaled to <body>: a click in it is outside this menu, but not a click-away.
      if (t instanceof Element && t.closest("[data-chip-menu]")) return;
      setOpen(false);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      // An open chip list takes Escape first; closing this menu too would unmount that chip.
      if (document.querySelector("[data-chip-menu]")) return;
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", esc, true);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", esc, true);
    };
  }, [open]);

  return (
    <div ref={rowRef} className="flex min-w-0 flex-1 items-center gap-1.5">
      {items.map((item, i) => (
        // `contents` keeps each control a direct flex item of the row; `hidden` takes a moved one out.
        <div key={i} data-footer-item={i} className={moved.includes(i) ? "hidden" : "contents"}>
          {item}
        </div>
      ))}
      {hasMoved && (
        <div ref={wrapRef} className="relative shrink-0">
          <button
            className={`relative flex h-9 w-9 items-center justify-center rounded-[10px] border border-border hover:bg-panel-2/60 hover:text-fg ${
              open ? "bg-panel-2 text-fg" : "text-muted"
            }`}
            onClick={() => setOpen(!open)}
            title={`${label}: ${moved.length} more`}
            aria-label={label}
            aria-haspopup="dialog"
            aria-expanded={open}
          >
            <EllipsisIcon size={16} />
            <span className="pointer-events-none absolute -right-1 -top-1 flex h-3.5 min-w-3.5 items-center justify-center rounded-full bg-accent px-1 font-mono text-[10px] font-semibold leading-none text-accent-fg">
              {moved.length}
            </span>
          </button>
          {open && (
            <div
              role="dialog"
              aria-label={label}
              className={`absolute bottom-full z-30 mb-1.5 flex w-80 max-w-[calc(100vw-16px)] flex-col rounded-xl border border-border bg-raised p-1 shadow-float ${
                alignRight ? "right-0" : "left-0"
              }`}
            >
              <ChipOverflow.Provider value={true}>
                {moved.map((i) => (
                  <Fragment key={i}>{items[i]}</Fragment>
                ))}
              </ChipOverflow.Provider>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
