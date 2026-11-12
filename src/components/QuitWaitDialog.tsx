import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { useStore } from "../lib/store";
import { api } from "../lib/ipc";

/**
 * The quit wait flow (pb-20261111-22): on quit, if sessions are mid-turn, this dialog lists
 * them live — each disappears as its turn finishes — and the app quits when the list empties.
 * "Quit now" is always available (today's stop_all: flags stay set, the relaunch offers them);
 * "Keep Patchbay open" cancels. No timeout: the user decides. Shown only for the standalone
 * owner (the backend never emits `quit://wait` to an attached window — the daemon keeps
 * working and there is nothing to wait for). Styled after ConfirmDialog.
 */
export function QuitWaitDialog() {
  const [waiting, setWaiting] = useState(false);
  const busyIds = useStore((s) => Object.entries(s.code).filter(([, c]) => c.busy).map(([id]) => id));
  const sessions = useStore((s) => s.codeSessions);

  useEffect(() => {
    const un = listen("quit://wait", () => setWaiting(true));
    const onKey = (e: KeyboardEvent) => {
      // Escape keeps Patchbay open, like every other house dialog's cancel.
      if (e.key === "Escape") setWaiting((w) => w && (api.quitCancel().catch(() => {}), false));
    };
    window.addEventListener("keydown", onKey);
    return () => {
      un.then((f) => f(), () => {});
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  // The list empties as turns finish: then the app quits. Never a timeout — the other two
  // buttons are the user's decisions.
  useEffect(() => {
    if (waiting && busyIds.length === 0) {
      setWaiting(false);
      api.quitProceed().catch(() => {});
    }
  }, [waiting, busyIds.length]);

  if (!waiting) return null;
  const title = (id: string) => sessions.find((x) => x.id === id)?.title ?? id;
  const keepOpen = () => {
    setWaiting(false);
    api.quitCancel().catch(() => {});
  };
  return (
    <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]">
      <div
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="quit-wait-title"
        className="w-[min(440px,92vw)] rounded-2xl border border-border bg-raised p-5 shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div id="quit-wait-title" className="mb-2 text-sm font-semibold">
          Waiting for {busyIds.length} session{busyIds.length === 1 ? "" : "s"} to finish
        </div>
        <div className="mb-3 text-xs leading-5 text-muted">
          Patchbay will quit as soon as they are done. Quit now stops them instead — they stay
          on the resume list next launch.
        </div>
        <ul className="mb-4 max-h-[50vh] space-y-1.5 overflow-y-auto text-xs">
          {busyIds.map((id) => (
            <li key={id} className="flex items-center gap-2 rounded-lg border border-border bg-panel px-3 py-2">
              <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-accent" aria-hidden />
              <span className="min-w-0 flex-1 truncate">{title(id)}</span>
              <span className="shrink-0 text-[11px] text-muted">working…</span>
            </li>
          ))}
        </ul>
        <div className="flex justify-end gap-2">
          <button
            className="h-[34px] rounded-lg px-3.5 text-xs text-muted hover:bg-panel-2 hover:text-fg"
            onClick={keepOpen}
          >
            Keep Patchbay open
          </button>
          <button
            autoFocus
            className="h-[34px] rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg"
            onClick={() => {
              setWaiting(false);
              api.quitProceed().catch(() => {});
            }}
          >
            Quit now
          </button>
        </div>
      </div>
    </div>
  );
}
