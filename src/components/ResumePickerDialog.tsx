import { useEffect, useState } from "react";
import { useStore } from "../lib/store";

/**
 * The relaunch resume picker (pb-20261111-22): sessions whose active-turn flag was still set
 * when the app died — SIGKILL, crash, Force Quit, or Quit now — are offered with check marks.
 * Each checked session is re-attached and sent exactly one fixed continue prompt through the
 * normal send path; each unchecked session only loses its flag (no reminder next launch).
 * Per-session errors are shown in the result — never silent. Styled after ConfirmDialog.
 */
export function ResumePickerDialog() {
  const picker = useStore((s) => s.resumePicker);
  const [checked, setChecked] = useState<Record<string, boolean>>({});

  const open = picker !== null && picker.results === null;
  useEffect(() => {
    if (!open || !picker) return;
    // Default every candidate to checked: resuming is the point; uncheck to skip one.
    const next: Record<string, boolean> = {};
    for (const c of picker.candidates) next[c.id] = true;
    setChecked(next);
    const onKey = (e: KeyboardEvent) => {
      // Escape dismisses without changing any flag: the sessions come back next launch.
      if (e.key === "Escape") useStore.getState().closeResumePicker();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, picker?.candidates]);

  if (!picker) return null;

  const title = (id: string) => picker.candidates.find((c) => c.id === id)?.title ?? id;

  // Result view: every outcome, errors included (never silent).
  if (picker.results) {
    return (
      <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]">
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="resume-result-title"
          className="w-[min(440px,92vw)] rounded-2xl border border-border bg-raised p-5 shadow-float"
          onClick={(e) => e.stopPropagation()}
        >
          <div id="resume-result-title" className="mb-2 text-sm font-semibold">
            Resume result
          </div>
          <ul className="mb-4 max-h-[50vh] space-y-1.5 overflow-y-auto text-xs leading-5">
            {picker.results.map((r) => (
              <li key={r.id} className="flex items-start gap-2">
                <span
                  className={
                    r.action === "error" ? "text-danger" : r.action === "sent" ? "text-accent-ink" : "text-faint"
                  }
                  aria-hidden
                >
                  {r.action === "error" ? "!" : r.action === "sent" ? "\u2713" : "\u2013"}
                </span>
                <span className="min-w-0 flex-1">
                  <span className="truncate">{title(r.id)}</span>
                  {r.action === "error" && (
                    <span className="block break-words text-danger">{r.error ?? "resume failed"}</span>
                  )}
                  {r.action === "sent" && <span className="block text-muted">continuing with one prompt</span>}
                  {r.action === "running" && (
                    <span className="block text-muted">still working — left alone (no extra prompt)</span>
                  )}
                  {r.action === "skipped" && (
                    <span className="block text-muted">skipped — you won&apos;t be reminded next launch</span>
                  )}
                </span>
              </li>
            ))}
          </ul>
          <div className="flex justify-end gap-2">
            <button
              autoFocus
              className="h-[34px] rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg"
              onClick={() => useStore.getState().closeResumePicker()}
            >
              Close
            </button>
          </div>
        </div>
      </div>
    );
  }

  if (!open) return null;
  const busy = picker.busy;
  const checkedIds = picker.candidates.filter((c) => checked[c.id]).map((c) => c.id);
  return (
    <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]">
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="resume-title"
        className="w-[min(440px,92vw)] rounded-2xl border border-border bg-raised p-5 shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div id="resume-title" className="mb-2 text-sm font-semibold">
          Continue where you left off?
        </div>
        <div className="mb-3 text-xs leading-5 text-muted">
          Patchbay was closed while these sessions were working. Each one you check gets a single
          prompt to continue; unchecking skips it for good. Esc or &quot;Not now&quot; keeps this
          list for the next launch — nothing is sent and no flag is cleared.
        </div>
        <ul className="mb-4 max-h-[50vh] space-y-1.5 overflow-y-auto">
          {picker.candidates.map((c) => (
            <li key={c.id}>
              <label className="flex cursor-pointer items-start gap-2 rounded-lg border border-border bg-panel px-3 py-2 text-xs">
                <input
                  type="checkbox"
                  className="mt-0.5"
                  disabled={busy}
                  checked={!!checked[c.id]}
                  onChange={(e) => setChecked((prev) => ({ ...prev, [c.id]: e.target.checked }))}
                />
                <span className="min-w-0 flex-1">
                  <span className="block truncate font-medium">{c.title}</span>
                  <span className="block truncate text-[11px] text-muted">{c.agent} \u00b7 {c.cwd}</span>
                </span>
              </label>
            </li>
          ))}
        </ul>
        <div className="flex justify-end gap-2">
          <button
            autoFocus
            className="h-[34px] rounded-lg px-3.5 text-xs text-muted hover:bg-panel-2 hover:text-fg"
            disabled={busy}
            onClick={() => useStore.getState().closeResumePicker()}
          >
            Not now
          </button>
          <button
            disabled={busy}
            className="h-[34px] rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg disabled:opacity-50"
            onClick={() => useStore.getState().applyResume(checkedIds)}
          >
            {busy ? "Resuming\u2026" : `Resume (${checkedIds.length})`}
          </button>
        </div>
      </div>
    </div>
  );
}
