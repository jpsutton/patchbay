import { useEffect, useState } from "react";
import type { PlanLimits, PlanWindow } from "../lib/ipc";
import { useStore } from "../lib/store";
import { GaugeIcon } from "./icons";

const nowSecs = () => Math.floor(Date.now() / 1000);

/** Re-render every 30 s so "resets in" and "updated … ago" stay current. */
function useNow(active: boolean) {
  const [now, setNow] = useState(nowSecs);
  useEffect(() => {
    if (!active) return;
    setNow(nowSecs());
    const t = setInterval(() => setNow(nowSecs()), 30_000);
    return () => clearInterval(t);
  }, [active]);
  return now;
}

/** `1h 12m`, `3d 2h`, `4m`. */
export function fmtSpan(secs: number) {
  const m = Math.max(1, Math.ceil(secs / 60));
  if (m < 60) return `${m}m`;
  if (m < 1440) return `${Math.floor(m / 60)}h ${m % 60}m`;
  return `${Math.floor(m / 1440)}d ${Math.floor(m / 60) % 24}h`;
}

/** A window that has reset since its report says nothing about now. */
const live = (w: PlanWindow | null, now: number) => (w && w.resetsAt > now ? w : null);

/** The highest usage still in effect, as a whole percent. */
function peak(l: PlanLimits | null, now: number) {
  if (!l) return null;
  if (l.overageUntil != null && l.overageUntil > now) return 100;
  const ws = [live(l.fiveHour, now), live(l.sevenDay, now)].filter((w): w is PlanWindow => !!w);
  return ws.length ? Math.round(Math.max(...ws.map((w) => w.utilization)) * 100) : null;
}

function tone(pct: number, limit: number | null) {
  if (limit != null && pct >= limit) return "danger";
  if (pct >= (limit ?? 100) - 15) return "warn";
  return "ok";
}

/** The sidebar footer's toggle for the Claude plan usage blade. */
export function PlanUsageButton() {
  const limits = useStore((s) => s.planLimits);
  const open = useStore((s) => s.planOpen);
  const setOpen = useStore((s) => s.setPlanOpen);
  const guard = useStore((s) => s.prefs.usageLimit);
  const now = useNow(true);
  const pct = peak(limits, now);
  const t = pct == null ? null : tone(pct, guard.enabled ? guard.percent : null);
  return (
    <button
      className={`flex h-9 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-lg px-2 text-[13px] hover:bg-panel-2 hover:text-fg ${
        open ? "bg-panel-2 text-fg" : "text-muted"
      }`}
      title="Claude plan usage"
      aria-label="Claude plan usage"
      aria-expanded={open}
      onClick={() => setOpen(!open)}
    >
      <GaugeIcon size={16} />
      {pct != null && (
        <span className={`meta ${t === "danger" ? "text-danger" : t === "warn" ? "text-warn" : ""}`}>{pct}%</span>
      )}
    </button>
  );
}

function WindowRow({ label, w, now, limit }: { label: string; w: PlanWindow | null; now: number; limit: number | null }) {
  const current = live(w, now);
  if (!w) return null;
  const pct = current ? Math.round(current.utilization * 100) : 0;
  const t = tone(pct, limit);
  return (
    <div className="px-3 py-1.5">
      <div className="flex items-baseline justify-between">
        <span>{label}</span>
        <span className="meta text-faint">
          {current ? `${pct}% · resets in ${fmtSpan(current.resetsAt - now)}` : "reset since last report"}
        </span>
      </div>
      <div className="relative mt-1.5 h-1.5 overflow-hidden rounded-full bg-panel-2" aria-hidden="true">
        <div
          className={`h-full rounded-full ${t === "danger" ? "bg-danger" : t === "warn" ? "bg-warn" : "bg-ok"}`}
          style={{ width: `${Math.min(100, pct)}%` }}
        />
        {limit != null && limit < 100 && (
          <div className="absolute inset-y-0 w-px bg-fg/50" style={{ left: `${limit}%` }} title={`Stops at ${limit}%`} />
        )}
      </div>
    </div>
  );
}

/**
 * Claude plan usage from the last report of Claude Code, or of Patchbay's agent on a Claude login. Stays open until its button is clicked
 * again, so it can sit beside a running session.
 */
export function PlanUsageBlade({ className }: { className: string }) {
  const limits = useStore((s) => s.planLimits);
  const open = useStore((s) => s.planOpen);
  const guard = useStore((s) => s.prefs.usageLimit);
  const openSettingsAt = useStore((s) => s.openSettingsAt);
  const defaultAgent = useStore((s) => s.prefs.defaultAgent);
  const now = useNow(open);
  if (!open) return null;
  const limit = guard.enabled ? guard.percent : null;
  const overage = limits?.overageUntil != null && limits.overageUntil > now;
  return (
    <div
      role="dialog"
      aria-label="Claude plan usage"
      className={`z-30 w-[280px] rounded-xl border border-border bg-raised py-1.5 text-[13px] shadow-float ${className}`}
    >
      <div className="flex items-baseline justify-between px-3 pb-1.5 pt-2">
        <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">Claude plan</span>
        {limits && <span className="text-[11px] text-faint">updated {fmtSpan(now - limits.observedAt)} ago</span>}
      </div>
      {!limits || (!limits.fiveHour && !limits.sevenDay && !overage) ? (
        <p className="px-3 py-1.5 text-muted">
          No usage reported yet. It shows up after a turn on a Claude plan: Claude Code signed in with one, or Patchbay's agent on a Claude login provider.
        </p>
      ) : (
        <>
          <WindowRow label="5-hour" w={limits.fiveHour} now={now} limit={limit} />
          <WindowRow label="Weekly" w={limits.sevenDay} now={now} limit={limit} />
          {overage && (
            <div className="mx-2 mb-1 mt-1.5 rounded-lg bg-danger/10 px-2.5 py-2 text-[12.5px] leading-[1.4] text-danger" role="alert">
              Using extra usage, billed past the plan, for {fmtSpan(limits.overageUntil! - now)} more.
            </div>
          )}
        </>
      )}
      <div className="mt-1 flex items-baseline justify-between border-t border-line px-3 pb-1 pt-2 text-[12px] text-muted">
        <span>{limit != null ? `Turns stop at ${limit}%` : "Usage limit off"}</span>
        <button className="text-accent-ink hover:underline" onClick={() => openSettingsAt(defaultAgent === "patchbay" ? "agent:patchbay" : "agent:claude-code")}>
          Change
        </button>
      </div>
    </div>
  );
}
