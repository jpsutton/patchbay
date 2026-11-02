import { useEffect, useState, type ReactNode } from "react";
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

function fmtUsdCents(cents: number) {
  return `$${(cents / 100).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

function hasFreeCapacity(limits: CursorPlanLimits | null): boolean {
  if (!limits) return false;
  return limits.remainingBonus === true || (limits.grantsRemainingCents ?? 0) > 0;
}

function bonusLabel(limits: CursorPlanLimits | null): string | null {
  if (!limits) return null;
  const used = limits.bonusSpendCents;
  if (used != null && used > 0) {
    return limits.remainingBonus
      ? `${fmtUsdCents(used)} free on us · still available`
      : `${fmtUsdCents(used)} free on us · used up`;
  }
  if (limits.remainingBonus) return "Free on us available";
  if (limits.remainingBonus === false && (used == null || used === 0)) return "No free-on-us left";
  return null;
}

function grantsLabel(limits: CursorPlanLimits | null): string | null {
  if (!limits || limits.grantsRemainingCents == null) return null;
  const left = limits.grantsRemainingCents;
  if (left > 0) {
    return limits.slowPool
      ? `${fmtUsdCents(left)} free credits left · slow pool`
      : `${fmtUsdCents(left)} free credits left`;
  }
  return limits.slowPool ? "Free credits used up · slow pool" : "No free credits left";
}

/** Sidebar footer toggle for Cursor plan usage: Auto / API mini bars. */
export function CursorPlanUsageButton() {
  const limits = useStore((s) => s.cursorPlanLimits);
  const open = useStore((s) => s.cursorPlanOpen);
  const setOpen = useStore((s) => s.setCursorPlanOpen);
  const now = useNow(true);
  const rows = livePct(limits, now);
  const pct = peak(rows);
  const t = pct == null ? null : tone(pct, null);
  const bonus = bonusLabel(limits);
  const grants = grantsLabel(limits);
  const freeOk = hasFreeCapacity(limits);
  const titleParts = [
    rows.length > 0 ? `Cursor plan usage: ${rows.map((r) => `${r.label} ${Math.round(r.pct)}%`).join(", ")}` : "Cursor plan usage",
    bonus,
    grants,
  ].filter(Boolean);
  const title = titleParts.join(". ");
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
      <GaugeIcon
        size={16}
        className={
          freeOk
            ? "text-ok"
            : t === "danger"
              ? "text-danger"
              : t === "warn"
                ? "text-warn"
                : undefined
        }
      />
      {rows.length > 0 && (
        <span className="flex flex-col gap-[3px]" aria-hidden="true">
          {rows.map((r) => (
            <span key={r.label} className="flex items-center gap-[5px] text-[10.5px] leading-none">
              <span className="w-5 text-faint @max-[300px]/footer:hidden">{r.label === "Auto" ? "au" : r.label === "API" ? "api" : "tot"}</span>
              <span className="h-1 w-[30px] overflow-hidden rounded-full bg-border">
                <span
                  className={`block h-full rounded-full ${barClass(tone(Math.round(r.pct), null))}`}
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

function PoolRow({ label, pct, now, resetsAt }: { label: string; pct: number; now: number; resetsAt: number | null }) {
  const rounded = Math.round(pct);
  const t = tone(rounded, null);
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
      </div>
    </div>
  );
}

function StatusChip({ ok, children }: { ok: boolean; children: ReactNode }) {
  return (
    <div
      className={`mx-2 mb-1 mt-1.5 rounded-lg px-2.5 py-2 text-[12.5px] leading-[1.4] ${
        ok ? "bg-ok/10 text-ok" : "bg-panel-2 text-muted"
      }`}
      role="status"
    >
      {children}
    </div>
  );
}

/**
 * Cursor plan usage from the dashboard poll (or a mesh peer on the same account). Full-width sidebar
 * pane above the footer; stays open until its button is clicked again.
 */
export function CursorPlanUsageBlade({ className }: { className?: string }) {
  const limits = useStore((s) => s.cursorPlanLimits);
  const open = useStore((s) => s.cursorPlanOpen);
  const setOpen = useStore((s) => s.setCursorPlanOpen);
  const guard = useStore((s) => s.prefs.cursorUsageLimit);
  const openSettingsAt = useStore((s) => s.openSettingsAt);
  const now = useNow(open);
  useEffect(() => {
    api.cursorPlanWatching(open).catch(() => {});
  }, [open]);
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        setOpen(false);
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [open, setOpen]);
  if (!open) return null;
  const rows = livePct(limits, now);
  const reset = limits?.resetsAt ?? null;
  const bonusUsed = limits?.bonusSpendCents;
  const onBonus = limits?.remainingBonus === true;
  const bonusGone = limits?.remainingBonus === false;
  const grantsLeft = limits?.grantsRemainingCents;
  const grantTotal = limits?.grants?.reduce((sum, g) => sum + g.totalCents, 0) ?? null;
  const onGrants = grantsLeft != null && grantsLeft > 0;
  const grantsKnown = grantsLeft != null;
  const included = limits?.includedSpendCents;
  const limit = limits?.limitCents;
  const empty =
    rows.length === 0 && bonusUsed == null && !grantsKnown && included == null && !limits?.displayMessage;
  return (
    <div
      role="dialog"
      aria-label="Cursor plan usage"
      className={`max-h-[40vh] w-full shrink-0 overflow-y-auto border-t border-line bg-panel py-1.5 text-[13px] ${className ?? ""}`}
    >
      <div className="flex items-baseline justify-between px-3 pb-1.5 pt-2">
        <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">Cursor plan</span>
        {limits && <span className="text-[11px] text-faint">updated {fmtSpan(now - limits.observedAt)} ago</span>}
      </div>
      {limits?.email && <p className="px-3 pb-1 text-[11px] text-faint">{limits.email}</p>}
      {empty ? (
        <p className="px-3 py-1.5 text-muted">
          No usage yet. It appears while a Cursor session is active here, or when a mesh peer on the same Cursor account reports.
        </p>
      ) : (
        <>
          {limits?.displayMessage && (
            <p className="px-3 pb-1 text-[12px] text-muted">{limits.displayMessage}</p>
          )}
          {included != null && limit != null && limit > 0 && (
            <div className="px-3 py-1.5">
              <div className="flex items-baseline justify-between">
                <span>Included</span>
                <span className="meta text-faint">
                  {fmtUsdCents(included)} / {fmtUsdCents(limit)}
                  {reset != null && reset > now ? ` · resets in ${fmtSpan(reset - now)}` : ""}
                </span>
              </div>
              <div className="relative mt-1.5 h-1.5 overflow-hidden rounded-full bg-panel-2" aria-hidden="true">
                <div
                  className={`h-full rounded-full ${barClass(tone(Math.round((100 * included) / limit), null))}`}
                  style={{ width: `${Math.min(100, Math.round((100 * included) / limit))}%` }}
                />
              </div>
            </div>
          )}
          {rows.map((r) => (
            <PoolRow key={r.label} label={r.label} pct={r.pct} now={now} resetsAt={reset} />
          ))}
          {(bonusUsed != null || onBonus || bonusGone) && (
            <StatusChip ok={onBonus}>
              {bonusUsed != null && bonusUsed > 0 ? (
                <>
                  <span className="text-fg">{fmtUsdCents(bonusUsed)}</span> free usage on us used
                  {onBonus ? " · still available" : bonusGone ? " · no longer available" : ""}
                </>
              ) : onBonus ? (
                "Free usage on us is still available"
              ) : (
                "Free usage on us is no longer available"
              )}
            </StatusChip>
          )}
          {grantsKnown && (
            <StatusChip ok={onGrants}>
              {onGrants ? (
                <>
                  <span className="text-fg">{fmtUsdCents(grantsLeft)}</span>
                  {grantTotal != null && grantTotal > 0 ? ` of ${fmtUsdCents(grantTotal)}` : ""} free credits left
                  {limits?.slowPool ? " · slow pool" : ""}
                </>
              ) : (
                <>Free credits are used up{limits?.slowPool ? " · slow pool" : ""}</>
              )}
            </StatusChip>
          )}
          {limits?.onDemandAllowed != null && (
            <p className="px-3 pb-1 pt-0.5 text-[11px] text-faint">
              On-demand spending {limits.onDemandAllowed ? "on" : "off"}
            </p>
          )}
        </>
      )}
      <div className="mt-1 flex items-baseline justify-between border-t border-line px-3 pb-1 pt-2 text-[12px] text-muted">
        <span>{guard.enabled ? "Stops when plan, bonus, and credits are used up" : "Usage limit off"}</span>
        <button className="text-accent-ink hover:underline" onClick={() => openSettingsAt("agent:cursor")}>
          Change
        </button>
      </div>
    </div>
  );
}
