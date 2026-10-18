import { useEffect, useState } from "react";
import { useStore } from "../lib/store";

export function ConfirmDialog() {
  const req = useStore((s) => s.confirm);
  const [checked, setChecked] = useState(false);
  const [text, setText] = useState("");

  useEffect(() => {
    setChecked(req?.checkbox?.checked ?? false);
    setText(req?.input?.value ?? "");
    if (!req) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") req.resolve({ ok: false, checked: false });
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [req]);

  if (!req) return null;
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]"
      onClick={() => req.resolve({ ok: false, checked: false })}
    >
      <div
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-title"
        className="w-[min(440px,92vw)] rounded-2xl border border-border bg-raised p-5 shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div id="confirm-title" className="mb-2 text-sm font-semibold">
          {req.title}
        </div>
        <div className="mb-4 whitespace-pre-wrap text-xs leading-5 text-muted">{req.body}</div>
        {req.input && (
          <input
            autoFocus
            aria-label={req.input.label}
            className="mb-4 h-9 w-full rounded-lg border border-border bg-bg px-3 text-sm outline-none focus:border-accent"
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && text.trim()) req.resolve({ ok: true, checked, text: text.trim() });
            }}
          />
        )}
        {req.checkbox && (
          <label className="mb-4 flex items-start gap-2 text-xs">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={checked}
              onChange={(e) => setChecked(e.target.checked)}
            />
            <span>{req.checkbox.label}</span>
          </label>
        )}
        <div className="flex justify-end gap-2">
          <button
            className="h-[34px] rounded-lg px-3.5 text-xs text-muted hover:bg-panel-2 hover:text-fg"
            onClick={() => req.resolve({ ok: false, checked: false })}
          >
            Cancel
          </button>
          <button
            autoFocus={!req.input}
            disabled={!!req.input && !text.trim()}
            className={`h-[34px] rounded-lg px-3.5 text-xs font-semibold disabled:opacity-50 ${
              req.danger ? "bg-danger text-bg" : "bg-accent text-accent-fg"
            }`}
            onClick={() => req.resolve({ ok: true, checked, text: req.input ? text.trim() : undefined })}
          >
            {req.confirmLabel ?? "Continue"}
          </button>
        </div>
      </div>
    </div>
  );
}
