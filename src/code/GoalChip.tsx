import { useEffect, useRef, useState } from "react";
import type { GoalView } from "../lib/ipc";

const LABEL: Record<GoalView["status"], string> = {
  active: "Goal",
  paused: "Goal paused",
  blocked: "Goal blocked",
  complete: "Goal done",
};

const DOT: Record<GoalView["status"], string> = {
  active: "bg-accent",
  paused: "bg-muted",
  blocked: "bg-danger",
  complete: "bg-ok",
};

/**
 * The session's `/goal`: a chip with its state, and a small panel with the objective, why it
 * stopped and the actions that fit. The actions run slash commands, so the agent stays the one
 * that changes the goal. `working`: a turn is running, where Stop is what pauses a goal.
 */
export function GoalChip({
  goal,
  working,
  ready,
  onCommand,
  onStop,
  className = "",
}: {
  goal: GoalView;
  working: boolean;
  ready: boolean;
  onCommand: (text: string) => void;
  onStop: () => void;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const away = (e: PointerEvent) => {
      if (root.current && !root.current.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("pointerdown", away);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("pointerdown", away);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);

  const run = (text: string) => {
    setOpen(false);
    onCommand(text);
  };
  const stop = () => {
    setOpen(false);
    onStop();
  };
  const tokens = (n: number) => (n >= 1000 ? `${Math.round(n / 100) / 10}k` : String(n));
  const resumable = goal.status === "paused" || goal.status === "blocked";
  const btn = "rounded-[7px] border border-line px-2 py-1 text-[12px] text-muted hover:text-fg disabled:opacity-50";

  return (
    <div ref={root} className={`relative mr-1 shrink-0 ${className}`}>
      <button
        type="button"
        data-testid="goal-chip"
        className="flex h-[26px] max-w-56 items-center gap-1.5 rounded-[9px] border border-line bg-panel px-2.5 text-[12px] text-fg"
        title={`${goal.objective}\n${goal.reason}`.trim()}
        aria-label={`${LABEL[goal.status]}: ${goal.objective}`}
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${DOT[goal.status]}`} />
        <span className="shrink-0">{LABEL[goal.status]}</span>
        {goal.status === "active" && goal.maxRounds > 0 && (
          <span className="shrink-0 text-faint">
            {goal.stretchRounds}/{goal.maxRounds}
          </span>
        )}
      </button>
      {open && (
        <div
          role="dialog"
          aria-label="Goal"
          className="absolute right-0 top-full z-30 mt-1 w-72 max-w-[calc(100vw-1.5rem)] rounded-[10px] border border-line bg-panel p-3 text-[12.5px] shadow-lg"
        >
          <div className="whitespace-pre-wrap break-words text-fg">{goal.objective}</div>
          {goal.reason && (
            <div className="mt-2 whitespace-pre-wrap break-words text-muted">
              {goal.status === "complete" ? `Evidence: ${goal.reason}` : goal.reason}
            </div>
          )}
          {goal.status === "active" && goal.lastGaps && (
            <div className="mt-2 whitespace-pre-wrap break-words text-muted">Last check found: {goal.lastGaps}</div>
          )}
          <div className="mt-2 text-faint">
            {goal.stretchRounds} of {goal.maxRounds} checks this run · {tokens(goal.tokensUsed)} tokens
            {goal.budget ? ` · budget ${tokens(goal.budget)}` : ""}
          </div>
          <div className="mt-3 flex flex-wrap gap-1.5">
            {goal.status === "active" && (
              <button type="button" className={btn} disabled={!working} onClick={stop}>
                Pause (stop the turn)
              </button>
            )}
            {resumable && (
              <button type="button" className={btn} disabled={!ready || working} onClick={() => run("/goal resume")}>
                Resume
              </button>
            )}
            <button type="button" className={btn} disabled={!ready || working} onClick={() => run("/goal clear")}>
              Clear
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
