import { useEffect, useState } from "react";
import { Dot } from "../components/Dot";
import { api, errorText } from "../lib/ipc";
import type { Subagent } from "../lib/ipc";
import { useStore } from "../lib/store";
import { health, idleSecs, shown, stoppable, type Health } from "../lib/subagents";
import { fmtElapsed } from "../lib/workingStatus";

const TONE: Record<Health, string> = {
  active: "signal-pulse text-live",
  quiet: "text-warn",
  waiting: "text-warn",
  done: "text-faint",
  failed: "text-danger",
  stopped: "text-faint",
};

function note(a: Subagent, h: Health, idle: number): string {
  switch (h) {
    case "waiting":
      return `${a.current}. Answer the prompt above, or stop it.`;
    case "quiet":
      return `No activity for ${fmtElapsed(idle * 1000)}: ${a.current}. It may be stuck.`;
    case "failed":
      return a.error ?? "Failed";
    default:
      return a.current;
  }
}

/**
 * One row per running subagent of Patchbay's own agent: what it is doing, how long it has run
 * and how long since it last did anything, with Stop. A subagent that has gone quiet, or waits
 * on a permission prompt nobody answered, shows in the warning color. Ticks on its own, so the
 * transcript doesn't re-render.
 */
export function SubagentsPanel({ sessionId }: { sessionId: string }) {
  const agents = useStore((s) => s.code[sessionId]?.subagents);
  const at = useStore((s) => s.code[sessionId]?.subagentsAt ?? null);
  const canStop = useStore((s) => !!s.code[sessionId]?.extensions.cancelTool);
  const [, tick] = useState(0);
  const rows = agents ? shown(agents) : [];
  useEffect(() => {
    if (rows.length === 0) return;
    const id = window.setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(id);
  }, [rows.length]);
  if (rows.length === 0) return null;
  const now = Date.now();

  return (
    <ul className="flex flex-col gap-1" aria-label="Subagents">
      {rows.map((a) => {
        const idle = idleSecs(a, at, now);
        const h = health(a, idle);
        const ran = a.elapsed_secs + (a.status === "running" && at != null ? Math.max(0, (now - at) / 1000) : 0);
        const warn = h === "quiet" || h === "waiting";
        return (
          <li key={a.id} className={`flex items-center gap-2 text-xs ${warn ? "text-warn" : "text-muted"}`} data-subagent-health={h}>
            <Dot size={6} className={TONE[h]} />
            <span className="shrink-0 font-medium text-fg/80">{a.name ?? a.id}</span>
            <span className="min-w-0 flex-1 truncate" title={`${a.description}\n${note(a, h, idle)}`}>
              {note(a, h, idle)}
            </span>
            <span className="shrink-0 font-mono text-[11px] tabular-nums text-faint">
              {[fmtElapsed(ran * 1000), a.steps > 0 ? `${a.steps} steps` : null, `idle ${fmtElapsed(idle * 1000)}`]
                .filter(Boolean)
                .join(" · ")}
            </span>
            {canStop && stoppable(a) && (
              <button
                type="button"
                className="shrink-0 text-accent-ink underline underline-offset-2"
                onClick={() => api.codeCancelTool(sessionId, `subagent:${a.id}`).catch((e) => useStore.getState().setToast(errorText(e)))}
                aria-label={`Stop subagent ${a.name ?? a.id}`}
              >
                Stop
              </button>
            )}
          </li>
        );
      })}
    </ul>
  );
}
