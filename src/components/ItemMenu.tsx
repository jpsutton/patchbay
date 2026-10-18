import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";

/** One thing to do with a chat or Code session. */
export interface ItemAction {
  label: string;
  onSelect: () => void;
  danger?: boolean;
}

/** A desktop context menu at the pointer, kept inside the window. */
export function ContextMenu({
  x,
  y,
  title,
  actions,
  onClose,
}: {
  x: number;
  y: number;
  title?: string;
  actions: ItemAction[];
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: x, top: y });
  const [focus, setFocus] = useState(0);

  useLayoutEffect(() => {
    const r = ref.current?.getBoundingClientRect();
    if (!r) return;
    setPos({
      left: Math.max(4, Math.min(x, window.innerWidth - r.width - 4)),
      top: Math.max(4, Math.min(y, window.innerHeight - r.height - 4)),
    });
  }, [x, y]);

  useEffect(() => {
    const away = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
      else if (e.key === "ArrowDown") setFocus((f) => (f + 1) % actions.length);
      else if (e.key === "ArrowUp") setFocus((f) => (f - 1 + actions.length) % actions.length);
      else if (e.key === "Enter") {
        actions[focus]?.onSelect();
        onClose();
      } else return;
      e.preventDefault();
    };
    window.addEventListener("pointerdown", away, true);
    window.addEventListener("keydown", key);
    window.addEventListener("blur", onClose);
    window.addEventListener("resize", onClose);
    return () => {
      window.removeEventListener("pointerdown", away, true);
      window.removeEventListener("keydown", key);
      window.removeEventListener("blur", onClose);
      window.removeEventListener("resize", onClose);
    };
  }, [actions, focus, onClose]);

  return (
    <div
      ref={ref}
      role="menu"
      aria-label={title}
      className="fixed z-50 min-w-44 rounded-xl border border-border bg-raised p-1 text-[13px] shadow-float"
      style={pos}
      // Rendered inside the row: keep its clicks from reaching the row's own handlers.
      onClick={(e) => e.stopPropagation()}
      onDoubleClick={(e) => e.stopPropagation()}
      onContextMenu={(e) => {
        e.preventDefault();
        e.stopPropagation();
      }}
    >
      {actions.map((a, i) => (
        <button
          key={a.label}
          role="menuitem"
          className={`flex h-8 w-full items-center rounded-lg px-2.5 text-left ${a.danger ? "text-danger" : "text-fg"} ${
            i === focus ? "bg-panel-2" : ""
          }`}
          onPointerEnter={() => setFocus(i)}
          onClick={() => {
            a.onSelect();
            onClose();
          }}
        >
          {a.label}
        </button>
      ))}
    </div>
  );
}

/** A phone's bottom sheet of actions, opened by holding a row. */
export function ActionSheet({ title, actions, onClose }: { title: string; actions: ItemAction[]; onClose: () => void }) {
  return (
    <div className="fixed inset-0 z-[60] flex flex-col justify-end bg-black/45" onClick={onClose}>
      <div
        role="menu"
        aria-label={title}
        className="rounded-t-2xl border-t border-border bg-raised px-2 pb-[calc(env(safe-area-inset-bottom)+8px)] pt-2"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="truncate px-3 pb-2 pt-1 text-[13px] text-muted">{title}</div>
        {actions.map((a) => (
          <button
            key={a.label}
            role="menuitem"
            className={`flex h-12 w-full items-center rounded-xl px-3 text-left text-[16px] active:bg-panel-2 ${a.danger ? "text-danger" : "text-fg"}`}
            onClick={() => {
              onClose();
              a.onSelect();
            }}
          >
            {a.label}
          </button>
        ))}
        <button className="mt-1 flex h-12 w-full items-center justify-center rounded-xl text-[16px] text-muted active:bg-panel-2" onClick={onClose}>
          Cancel
        </button>
      </div>
    </div>
  );
}

const HOLD_MS = 500;
/** How far a finger may drift and still count as holding, not scrolling. */
const HOLD_SLOP = 8;

/**
 * Hold-to-open for touch rows. Returns props for the row: a hold of half a second calls `onHold`,
 * and the tap that ends it doesn't also open the row. A right click (or Android's own long-press
 * `contextmenu`) opens it too.
 */
export function useHold(onHold: () => void) {
  const timer = useRef<number | null>(null);
  const start = useRef<{ x: number; y: number } | null>(null);
  const held = useRef(false);
  const cancel = () => {
    if (timer.current != null) window.clearTimeout(timer.current);
    timer.current = null;
    start.current = null;
  };
  const fire = () => {
    cancel();
    if (held.current) return;
    held.current = true;
    navigator.vibrate?.(10);
    onHold();
  };
  return {
    onPointerDown: (e: ReactPointerEvent) => {
      held.current = false;
      start.current = { x: e.clientX, y: e.clientY };
      timer.current = window.setTimeout(fire, HOLD_MS);
    },
    onPointerMove: (e: ReactPointerEvent) => {
      const s = start.current;
      if (s && Math.hypot(e.clientX - s.x, e.clientY - s.y) > HOLD_SLOP) cancel();
    },
    onPointerUp: cancel,
    onPointerCancel: cancel,
    onContextMenu: (e: ReactMouseEvent) => {
      e.preventDefault();
      fire();
    },
    /** Swallow the click that ends a hold. */
    onClickCapture: (e: ReactMouseEvent) => {
      if (held.current) {
        e.preventDefault();
        e.stopPropagation();
        held.current = false;
      }
    },
  };
}
