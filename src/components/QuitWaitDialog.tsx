import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { useStore } from "../lib/store";
import { api } from "../lib/ipc";

interface ActiveSession {
  id: string;
  title: string;
  waiting: boolean;
}

/**
 * The quit wait flow (pb-20261111-22): on quit, if sessions are mid-turn, this dialog lists
 * them live — each disappears as its turn finishes — and the app quits when the list empties.
 * "Quit now" is always available (today's stop_all: flags stay set, the relaunch offers them);
 * "Keep Patchbay open" + Escape cancel. No timeout: the user decides.
 *
 * The list comes from the BACKEND's own busy_sessions via quit_wait_status (polled), never
 * from this window's store — a stale store could auto-quit an empty list on a reload. Every
 * mount re-queries too, so a reload (or a second window) re-shows a pending wait instead of
 * leaving the gate stuck with nothing on screen. The backend raises/focuses the window before
 * emitting quit://wait, so a hidden or minimized window cannot make Quit look dead.
 */
export function QuitWaitDialog() {
  const [waiting, setWaiting] = useState(false);
  const [active, setActive] = useState<ActiveSession[]>([]);
  const sessions = useStore((s) => s.codeSessions);

  useEffect(() => {
    let dead = false;
    const sync = () => {
      api
        .quitWaitStatus()
        .then((st) => {
          if (dead) return;
          setWaiting(st.showing);
          setActive(st.active);
          // The backend says a quit is pending and its own list is empty: the turns finished
          // (even across a reload) — carry the quit out.
          if (st.showing && st.active.length === 0) api.quitProceed().catch(() => {});
        })
        .catch(() => {});
    };
    sync();
    const iv = setInterval(sync, 500);
    const un = listen("quit://wait", sync);
    return () => {
      dead = true;
      clearInterval(iv);
      un.then((f) => f(), () => {});
    };
  }, []);

  // Escape = Keep Patchbay open, like every other house dialog's cancel.
  useEffect(() => {
    if (!waiting) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setWaiting(false);
        api.quitCancel().catch(() => {});
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [waiting]);

  if (!waiting) return null;
  const title = (s: ActiveSession) => sessions.find((x) => x.id === s.id)?.title ?? s.title;
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
          Waiting for {active.length} session{active.length === 1 ? "" : "s"} to finish
        </div>
        <div className="mb-3 text-xs leading-5 text-muted">
          Patchbay will quit as soon as they are done. Quit now stops them instead — they stay
          on the resume list next launch.
        </div>
        <ul className="mb-4 max-h-[50vh] space-y-1.5 overflow-y-auto text-xs">
          {active.map((s) => (
            <li key={s.id} className="flex items-center gap-2 rounded-lg border border-border bg-panel px-3 py-2">
              <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-accent" aria-hidden />
              <span className="min-w-0 flex-1 truncate">{title(s)}</span>
              <span className="shrink-0 text-[11px] text-muted">{s.waiting ? "waiting for you…" : "working…"}</span>
            </li>
          ))}
        </ul>
        <div className="flex justify-end gap-2">
          <button
            autoFocus
            className="h-[34px] rounded-lg px-3.5 text-xs text-muted hover:bg-panel-2 hover:text-fg"
            onClick={keepOpen}
          >
            Keep Patchbay open
          </button>
          <button
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
