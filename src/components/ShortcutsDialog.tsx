import { useEffect } from "react";
import { useStore } from "../lib/store";
import { keyLabel, SHORTCUTS } from "../lib/shortcuts";

export function ShortcutsDialog() {
  const open = useStore((s) => s.shortcutsOpen);
  const setOpen = useStore((s) => s.setShortcutsOpen);
  const sendKey = useStore((s) => s.prefs.sendKey);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        setOpen(false);
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [open, setOpen]);

  if (!open) return null;
  const rows = [
    { keys: sendKey === "mod-enter" ? "Mod+Enter" : "Enter", label: "Send message" },
    { keys: sendKey === "mod-enter" ? "Enter" : "Shift+Enter", label: "New line" },
    { keys: "/", label: "Slash commands (Code, at the start of a message)" },
    ...SHORTCUTS,
  ];
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]" onClick={() => setOpen(false)}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="shortcuts-title"
        className="w-[min(460px,92vw)] rounded-2xl border border-border bg-raised p-5 shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-3 flex items-center">
          <div id="shortcuts-title" className="flex-1 text-sm font-semibold">
            Keyboard shortcuts
          </div>
          <button className="text-muted hover:text-fg" onClick={() => setOpen(false)} aria-label="Close">
            ×
          </button>
        </div>
        <table className="w-full text-xs">
          <tbody>
            {rows.map((r) => (
              <tr key={r.keys + r.label} className="border-t border-line first:border-t-0">
                <td className="py-1.5 pr-4 text-muted">{r.label}</td>
                <td className="whitespace-nowrap py-1.5 text-right">
                  <kbd className="rounded-[5px] border border-border bg-panel px-1.5 py-0.5 font-mono text-[11.5px]">
                    {keyLabel(r.keys)}
                  </kbd>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
