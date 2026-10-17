import { useEffect, useRef, useState } from "react";
import type { ExportFormat } from "../lib/export";
import { HeaderButton } from "./HeaderButton";
import { DownloadIcon } from "./icons";

export function ExportMenu({ disabled, onExport }: { disabled?: boolean; onExport: (f: ExportFormat) => void }) {
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
      <HeaderButton
        label="Export"
        active={open}
        disabled={disabled}
        onClick={() => setOpen(!open)}
        aria-haspopup="menu"
        aria-expanded={open}
      >
        <DownloadIcon />
      </HeaderButton>
      {open && (
        <div role="menu" className="absolute right-0 top-full z-30 mt-1 w-44 rounded-xl border border-border bg-raised p-1 text-xs shadow-float">
          {(
            [
              ["markdown", "Markdown (.md)"],
              ["json", "JSON (.json)"],
            ] as const
          ).map(([f, label]) => (
            <button
              key={f}
              role="menuitem"
              className="block w-full rounded-md px-2.5 py-1.5 text-left hover:bg-panel-2"
              onClick={() => {
                setOpen(false);
                onExport(f);
              }}
            >
              {label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
