import { useEffect, useState } from "react";
import { Dot } from "../components/Dot";
import { fmtTokens } from "../components/UsageBar";
import type { AcpToolCall } from "../lib/ipc";
import { fmtElapsed, STALL_MS, workingStatus, type Phase, type WorkingStatus } from "../lib/workingStatus";

type Source = {
  toolCalls: Record<string, AcpToolCall>;
  lastForegroundAt: number | null;
  lastPhase: Phase | null;
  turnStartedAt: number | null;
  turnChars: number;
};

function label(w: WorkingStatus): string {
  switch (w.kind) {
    case "thinking":
      return "Thinking…";
    case "writing":
      return "Writing…";
    case "tool":
      return `${(w.tool?.title || "Running a tool").split("\n")[0]}…`;
    case "stalled":
      return `No activity for ${fmtElapsed(w.idleMs ?? STALL_MS)}. The model may be slow or stuck.`;
    default:
      return "Waiting for the model…";
  }
}

/**
 * The "Working…" line under the transcript. Says what the agent is doing, and keeps visible
 * proof of life (a turn timer, a streamed-token estimate, time since the last update), so a slow
 * model reads differently from a stopped one. Ticks on its own, so the transcript doesn't re-render.
 */
export function WorkingIndicator({ s, onStop }: { s: Source; onStop: () => void }) {
  const [, tick] = useState(0);
  useEffect(() => {
    const id = window.setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(id);
  }, []);

  const w = workingStatus(s);
  const stalled = w.kind === "stalled";
  // Under the stall threshold the label already says "Waiting"; the counter adds the how long.
  const showIdle = w.quiet && !stalled && w.idleMs != null;
  const meta = [
    w.turnMs != null ? fmtElapsed(w.turnMs) : null,
    w.tokens > 0 ? `~${fmtTokens(w.tokens)} tokens` : null,
    showIdle ? `last update ${fmtElapsed(w.idleMs!)} ago` : null,
  ].filter(Boolean);

  return (
    <div
      className={`flex items-center gap-2 text-xs ${stalled ? "text-warn" : "text-muted"}`}
      role="status"
      aria-live="off"
      data-working-kind={w.kind}
    >
      <Dot size={6} className={stalled ? "text-warn" : "signal-pulse text-live"} />
      <span className="min-w-0 truncate">{label(w)}</span>
      {meta.length > 0 && <span className="shrink-0 font-mono text-[11px] tabular-nums text-faint">{meta.join(" · ")}</span>}
      {stalled && (
        <button className="shrink-0 text-accent-ink underline underline-offset-2" onClick={onStop}>
          Stop
        </button>
      )}
    </div>
  );
}
