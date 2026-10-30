import { useEffect, useState } from "react";
import type { PlanLimits, PlanSample, PlanWindow } from "../lib/ipc";
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

/** A window's usage as a whole percent; 0 once it has reset, null if never reported. */
const windowPct = (w: PlanWindow | null, now: number) => {
  if (!w) return null;
  const current = live(w, now);
  return current ? Math.round(current.utilization * 100) : 0;
};

const WINDOW_SECS = { fiveHour: 5 * 3600, sevenDay: 7 * 86400 };

/**
 * How fast a window is being used, relative to an even spend: 1 uses exactly the whole window by its reset, 2 uses it
 * up halfway through. Null when the window is new (too little elapsed to say) or has reset.
 */
function burnPace(w: PlanWindow | null, span: number, now: number) {
  const current = live(w, now);
  if (!current) return null;
  const elapsed = (span - (current.resetsAt - now)) / span;
  return elapsed < 0.03 ? null : current.utilization / Math.min(1, elapsed);
}

/** A stretch of a window's bar, from one usage level to the next, and how fast usage grew across it. */
export interface BarSegment {
  from: number;
  to: number;
  pace: number;
  /** When the stretch began and ended (unix seconds). */
  t0: number;
  t1: number;
}

/**
 * Splits a window's progress bar by how fast it filled: each report since the window opened marks a level
 * (0 to 1) reached at a time, and the pace between two reports is the growth over the share of the window
 * that passed. Idle time between reports counts, so a long quiet stretch before a burst reads as moderate.
 */
export function barSegments(history: PlanSample[], key: "fiveHour" | "sevenDay", w: PlanWindow, span: number): BarSegment[] {
  const start = w.resetsAt - span;
  let at = start;
  let level = 0;
  const out: BarSegment[] = [];
  for (const s of history) {
    const r = s[key];
    // Reports of this window only; its reset time can drift by a few seconds between them.
    if (!r || Math.abs(r.resetsAt - w.resetsAt) > 120 || s.at <= at) continue;
    const u = Math.min(1, Math.max(level, r.utilization));
    if (u > level) out.push({ from: level, to: u, pace: (u - level) / ((s.at - at) / span), t0: at, t1: s.at });
    at = s.at;
    level = u;
  }
  return out;
}

/** `14:05`, or `Mon 14:05` once the span is longer than a day. */
const fmtClock = (secs: number, weekday: boolean) =>
  new Date(secs * 1000).toLocaleString([], { ...(weekday && { weekday: "short" }), hour: "numeric", minute: "2-digit" });

const paceClass = (pace: number) => (pace > 2 ? "bg-danger" : pace > 1 ? "bg-warn" : "bg-ok");

/**
 * What the pace means for a window: usage projected at its reset, and when it would run out first, if it would.
 * Null once the window is used up: usage can't grow past full, so there is nothing left to project.
 */
function burnOutlook(w: PlanWindow | null, span: number, now: number) {
  const pace = burnPace(w, span, now);
  const current = live(w, now);
  if (pace == null || !current || current.utilization <= 0 || current.utilization >= 1) return null;
  const runOutIn = ((1 - current.utilization) / current.utilization) * (span - (current.resetsAt - now));
  return { pace, projected: Math.round(pace * 100), runOutIn: pace > 1 ? runOutIn : null };
}

/** The faster of the two windows' paces. */
const worstPace = (l: PlanLimits | null, now: number) => {
  const paces = [burnPace(l?.fiveHour ?? null, WINDOW_SECS.fiveHour, now), burnPace(l?.sevenDay ?? null, WINDOW_SECS.sevenDay, now)].filter(
    (p): p is number => p != null,
  );
  return paces.length ? Math.max(...paces) : null;
};

/** The gauge icon with its needle at the burn pace: left at none, upright at an even spend, right at double or more. */
function BurnGauge({ pace, className }: { pace: number; className?: string }) {
  const angle = (Math.min(2, pace) / 2) * 240 - 120;
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width={16}
      height={16}
      viewBox="2 2 20 20"
      overflow="visible"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      className={className}
    >
      <path d="M3.34 19a10 10 0 1 1 17.32 0" />
      <path d="M12 14V8" transform={`rotate(${angle} 12 14)`} />
    </svg>
  );
}

const barClass = (t: ReturnType<typeof tone>) => (t === "danger" ? "bg-danger" : t === "warn" ? "bg-warn" : "bg-ok");

/** The sidebar footer's toggle for the Claude plan usage blade: one mini bar per window. */
export function PlanUsageButton() {
  const limits = useStore((s) => s.planLimits);
  const open = useStore((s) => s.planOpen);
  const setOpen = useStore((s) => s.setPlanOpen);
  const guard = useStore((s) => s.prefs.usageLimit);
  const now = useNow(true);
  const limit = guard.enabled ? guard.percent : null;
  const rows = [
    { label: "5h", pct: windowPct(limits?.fiveHour ?? null, now) },
    { label: "wk", pct: windowPct(limits?.sevenDay ?? null, now) },
  ].filter((r): r is { label: string; pct: number } => r.pct != null);
  const overage = limits?.overageUntil != null && limits.overageUntil > now;
  // Overage with no window reported still deserves a number.
  const pct = rows.length ? null : peak(limits, now);
  const t = pct == null ? null : tone(pct, limit);
  const pace = worstPace(limits, now);
  const paceTone = pace == null ? null : tone(Math.round(pace * 100), limit);
  const title = rows.length
    ? `Claude plan usage: ${rows.map((r) => `${r.label === "5h" ? "5-hour" : "weekly"} ${r.pct}%`).join(", ")}${
        pace == null ? "" : `. Burning at ${pace.toFixed(1)}x an even pace`
      }`
    : "Claude plan usage";
  return (
    <button
      className={`flex h-9 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-lg px-2 text-[13px] hover:bg-panel-2 hover:text-fg ${
        open ? "bg-panel-2 text-fg" : "text-muted"
      }`}
      title={title}
      aria-label={title}
      aria-expanded={open}
      onClick={() => setOpen(!open)}
    >
      {pace != null ? (
        <BurnGauge
          pace={pace}
          className={overage || paceTone === "danger" ? "text-danger" : paceTone === "warn" ? "text-warn" : undefined}
        />
      ) : (
        <GaugeIcon size={16} className={overage ? "text-danger" : undefined} />
      )}
      {rows.length > 0 && (
        <span className="flex flex-col gap-[3px]" aria-hidden="true">
          {rows.map((r) => (
            <span key={r.label} className="flex items-center gap-[5px] text-[10.5px] leading-none">
              <span className="w-3 text-faint @max-[300px]/footer:hidden">{r.label}</span>
              <span className="h-1 w-[30px] overflow-hidden rounded-full bg-border @max-[260px]/footer:w-5">
                <span className={`block h-full rounded-full ${barClass(tone(r.pct, limit))}`} style={{ width: `${Math.min(100, r.pct)}%` }} />
              </span>
              <span className="w-[4ch] text-right text-fg tabular-nums">{r.pct}%</span>
            </span>
          ))}
        </span>
      )}
      {pct != null && (
        <span className={`meta ${t === "danger" ? "text-danger" : t === "warn" ? "text-warn" : ""}`}>{pct}%</span>
      )}
    </button>
  );
}

function WindowRow({ label, w, which, now, limit }: { label: string; w: PlanWindow | null; which: "fiveHour" | "sevenDay"; now: number; limit: number | null }) {
  const span = WINDOW_SECS[which];
  const current = live(w, now);
  const burn = burnOutlook(w, span, now);
  const history = useStore((s) => s.planHistory);
  const segments = current ? barSegments(history, which, current, span) : [];
  const covered = segments.length ? segments[segments.length - 1].to : 0;
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
      <div
        className="relative mt-1.5 h-1.5 overflow-hidden rounded-full bg-panel-2"
        title={segments.length ? "Color shows how fast usage grew: green at an even pace or slower, amber up to twice that, red faster" : undefined}
        aria-hidden="true"
      >
        {segments.map((g, i) => (
          <div
            key={i}
            className={`absolute inset-y-0 ${paceClass(g.pace)}`}
            style={{ left: `${g.from * 100}%`, width: `${(g.to - g.from) * 100}%` }}
            title={`${fmtClock(g.t0, span > 86400)} to ${fmtClock(g.t1, span > 86400)} · ${g.pace.toFixed(1)}x pace · ${Math.round((g.to - g.from) * 100)}% used`}
          />
        ))}
        {/* Usage the history doesn't account for, or all of it when there is none. */}
        {current && current.utilization > covered && (
          <div
            className={`absolute inset-y-0 ${barClass(t)}`}
            style={{ left: `${covered * 100}%`, width: `${(Math.min(1, current.utilization) - covered) * 100}%` }}
          />
        )}
        {limit != null && limit < 100 && (
          <div className="absolute inset-y-0 w-px bg-fg/50" style={{ left: `${limit}%` }} title={`Stops at ${limit}%`} />
        )}
      </div>
      {burn && (
        <div className={`meta mt-1 ${burn.runOutIn != null ? "text-warn" : "text-faint"}`}>
          {burn.pace.toFixed(1)}x pace ·{" "}
          {burn.runOutIn != null ? `runs out in ${fmtSpan(burn.runOutIn)}` : `${burn.projected}% by reset`}
        </div>
      )}
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
          <WindowRow label="5-hour" w={limits.fiveHour} which="fiveHour" now={now} limit={limit} />
          <WindowRow label="Weekly" w={limits.sevenDay} which="sevenDay" now={now} limit={limit} />
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
