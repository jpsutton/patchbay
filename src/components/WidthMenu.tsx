import { useEffect, useRef, useState } from "react";
import {
  CHAT_WIDTH_MAX,
  CHAT_WIDTH_MIN,
  CHAT_WIDTH_STEP,
  resetChatWidth,
  setChatWidth,
  useChatWidth,
} from "../lib/chatWidth";
import { HeaderButton } from "./HeaderButton";
import { WidthIcon } from "./icons";

/** Header button with a slider for the width of the conversation column. */
export function WidthMenu() {
  const width = useChatWidth();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        setOpen(false);
      }
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", esc, true);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", esc, true);
    };
  }, [open]);
  return (
    <div ref={ref} className="relative">
      <HeaderButton label="Conversation width" active={open} onClick={() => setOpen(!open)} aria-haspopup="menu" aria-expanded={open}>
        <WidthIcon />
      </HeaderButton>
      {open && (
        <div className="absolute right-0 top-full z-30 mt-1 w-64 rounded-xl border border-border bg-raised p-3 text-xs shadow-float">
          <div className="mb-2 flex items-center justify-between">
            <span className="text-muted">Conversation width</span>
            <span className="font-mono tabular-nums">{width === CHAT_WIDTH_MIN ? "Default" : `${width} px`}</span>
          </div>
          <input
            type="range"
            aria-label="Conversation width"
            className="w-full accent-accent"
            min={CHAT_WIDTH_MIN}
            max={CHAT_WIDTH_MAX}
            step={CHAT_WIDTH_STEP}
            value={width}
            onChange={(e) => setChatWidth(Number(e.target.value))}
            onDoubleClick={resetChatWidth}
          />
          <div className="mt-2 flex items-center justify-between text-muted">
            <span>Wider tables, less scrolling</span>
            <button className="rounded-md px-1.5 py-0.5 hover:bg-panel-2 hover:text-fg disabled:opacity-40" disabled={width === CHAT_WIDTH_MIN} onClick={resetChatWidth}>
              Reset
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
