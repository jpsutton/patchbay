import { useEffect, useState } from "react";
import type { CursorPlanLimits } from "../lib/ipc";
import { api } from "../lib/ipc";
import { useStore } from "../lib/store";
import { GaugeIcon } from "./icons";
import { fmtSpan } from "./PlanUsage";

const nowSecs = () => Math.floor(Date.now() / 1000);

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

function tone(pct: number, limit: number | null) {
  if (limit != null && pct >= limit) return "danger";
  if (pct >= (limit ?? 100) - 15) return "warn";
  return "ok";
}

const barClass = (t: ReturnType<typeof tone>) => (t === "danger" ? "bg-danger" : t === "warn" ? "bg-warn" : "bg-ok");

function livePct(limits: CursorPlanLimits | null, now: number): { label: string; pct: number }[] {
  if (!limits) return [];
  if (limits.resetsAt != null && limits.resetsAt <= now) return [];
  return [
    { label: "Auto", pct: limits.auto },
    { label: "API", pct: limits.api },
    { label: "Total", pct: limits.auto == null && limits.api == null ? limits.total : null },
  ].filter((r): r is { label: string; pct: number } => r.pct != null);
}

function peak(rows: { pct: number }[]) {
  return rows.length ? Math.round(Math.max(...rows.map((r) => r.pct))) : null;
}

/** Sidebar footer toggle for Cursor plan usage: Auto / API mini bars. */
export function CursorPlanUsageButton() {
  const limits = useStore((s) => s.cursorPlanLimits);
  const open = useStore((s) => s.cursorPlanOpen);
  const setOpen = useStore((s) => s.setCursorPlanOpen);
  const guard = useStore((s) => s.prefs.cursorUsageLimit);
  const now = useNow(true);
  const limit = guard.enabled ? guard.percent : null;
  const rows = livePct(limits, now);
  const pct = peak(rows);
  const t = pct == null ? null : tone(pct, limit);
  const title =
    rows.length > 0
      ? `Cursor plan usage: ${rows.map((r) => `${r.label} ${Math.round(r.pct)}%`).join(", ")}`
      : "Cursor plan usage";
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
      <GaugeIcon size={16} className={t === "danger" ? "text-danger" : t === "warn" ? "text-warn" : undefined} />
      {rows.length > 0 && (
        <span className="flex flex-col gap-[3px]" aria-hidden="true">
          {rows.map((r) => (
            <span key={r.label} className="flex items-center gap-[5px] text-[10.5px] leading-none">
              <span className="w-5 text-faint @max-[300px]/footer:hidden">{r.label === "Auto" ? "au" : r.label === "API" ? "api" : "tot"}</span>
              <span className="h-1 w-[30px] overflow-hidden rounded-full bg-border">
                <span
                  className={`block h-full rounded-full ${barClass(tone(Math.round(r.pct), limit))}`}
                  style={{ width: `${Math.min(100, Math.round(r.pct))}%` }}
                />
              </span>
              <span className="w-[4ch] text-right text-fg tabular-nums">{Math.round(r.pct)}%</span>
            </span>
          ))}
        </span>
      )}
      {pct != null && rows.length === 0 && (
        <span className={`meta ${t === "danger" ? "text-danger" : t === "warn" ? "text-warn" : ""}`}>{pct}%</span>
      )}
    </button>
  );
}

function PoolRow({ label, pct, now, resetsAt, limit }: { label: string; pct: number; now: number; resetsAt: number | null; limit: number | null }) {
  const rounded = Math.round(pct);
  const t = tone(rounded, limit);
  return (
    <div className="px-3 py-1.5">
      <div className="flex items-baseline justify-between">
        <span>{label}</span>
        <span className="meta text-faint">
          {rounded}%
          {resetsAt != null && resetsAt > now ? ` · resets in ${fmtSpan(resetsAt - now)}` : ""}
        </span>
      </div>
      <div className="relative mt-1.5 h-1.5 overflow-hidden rounded-full bg-panel-2" aria-hidden="true">
        <div className={`h-full rounded-full ${barClass(t)}`} style={{ width: `${Math.min(100, rounded)}%` }} />
        {limit != null && limit < 100 && (
          <div className="absolute inset-y-0 w-px bg-fg/50" style={{ left: `${limit}%` }} title={`Stops at ${limit}%`} />
        )}
      </div>
    </div>
  );
}

/**
 * Cursor plan usage from the dashboard poll (or a mesh peer on the same account). Stays open until
 * its button is clicked again.
 */
export function CursorPlanUsageBlade({ className }: { className: string }) {
  const limits = useStore((s) => s.cursorPlanLimits);
  const open = useStore((s) => s.cursorPlanOpen);
  const guard = useStore((s) => s.prefs.cursorUsageLimit);
  const openSettingsAt = useStore((s) => s.openSettingsAt);
  const now = useNow(open);
  useEffect(() => {
    api.cursorPlanWatching(open).catch(() => {});
  }, [open]);
  if (!open) return null;
  const limit = guard.enabled ? guard.percent : null;
  const rows = livePct(limits, now);
  const reset = limits?.resetsAt ?? null;
  return (
    <div
      role="dialog"
      aria-label="Cursor plan usage"
      className={`z-30 w-[280px] rounded-xl border border-border bg-raised py-1.5 text-[13px] shadow-float ${className}`}
    >
      <div className="flex items-baseline justify-between px-3 pb-1.5 pt-2">
        <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">Cursor plan</span>
        {limits && <span className="text-[11px] text-faint">updated {fmtSpan(now - limits.observedAt)} ago</span>}
      </div>
      {limits?.email && <p className="px-3 pb-1 text-[11px] text-faint">{limits.email}</p>}
      {rows.length === 0 ? (
        <p className="px-3 py-1.5 text-muted">
          No usage yet. It appears while a Cursor session is active here, or when a mesh peer on the same Cursor account reports.
        </p>
      ) : (
        rows.map((r) => <PoolRow key={r.label} label={r.label} pct={r.pct} now={now} resetsAt={reset} limit={limit} />)
      )}
      <div className="mt-1 flex items-baseline justify-between border-t border-line px-3 pb-1 pt-2 text-[12px] text-muted">
        <span>{limit != null ? `Turns stop at ${limit}%` : "Usage limit off"}</span>
        <button className="text-accent-ink hover:underline" onClick={() => openSettingsAt("agent:cursor")}>
          Change
        </button>
      </div>
    </div>
  );
}
