import { useEffect, useState } from "react";
import type { AntigravityPlanBucket, AntigravityPlanLimits } from "../lib/ipc";
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

function tone(pct: number) {
  if (pct >= 100) return "danger";
  if (pct >= 85) return "warn";
  return "ok";
}

const barClass = (t: ReturnType<typeof tone>) => (t === "danger" ? "bg-danger" : t === "warn" ? "bg-warn" : "bg-ok");

function liveBuckets(limits: AntigravityPlanLimits | null, now: number): AntigravityPlanBucket[] {
  if (!limits || limits.unavailable) return [];
  return (limits.buckets ?? []).filter((b) => b.resetsAt == null || b.resetsAt > now);
}

/** Compact footer labels for the common Antigravity buckets. */
function shortLabel(b: AntigravityPlanBucket): string {
  const id = b.bucketId;
  if (id === "gemini-5h") return "5h";
  if (id === "gemini-weekly") return "wk";
  if (id === "3p-5h") return "3p5";
  if (id === "3p-weekly") return "3pw";
  if (id.includes("argon") && id.includes("5h")) return "ar5";
  if (id.includes("argon")) return "arg";
  if (b.window === "5h") return "5h";
  if (b.window === "weekly") return "wk";
  return b.displayName.slice(0, 3).toLowerCase();
}

function rowLabel(b: AntigravityPlanBucket): string {
  const group = b.groupName?.trim();
  if (group) {
    if (b.bucketId === "gemini-5h" || b.window === "5h") return `${group} · 5-hour`;
    if (b.bucketId === "gemini-weekly" || b.window === "weekly") return `${group} · weekly`;
    return `${group} · ${b.displayName}`;
  }
  return b.displayName;
}

/** Prefer Gemini 5h/weekly in the footer; fall back to 3p, then anything else (max 3 rows). */
function footerBuckets(buckets: AntigravityPlanBucket[]): AntigravityPlanBucket[] {
  const gemini = buckets.filter((b) => b.bucketId === "gemini-5h" || b.bucketId === "gemini-weekly");
  if (gemini.length) return gemini.slice(0, 3);
  const third = buckets.filter((b) => b.bucketId.startsWith("3p-"));
  if (third.length) return third.slice(0, 3);
  return buckets.slice(0, 3);
}

function peakUsed(buckets: AntigravityPlanBucket[]): number | null {
  if (!buckets.length) return null;
  return Math.round(Math.max(...buckets.map((b) => (1 - b.remainingFraction) * 100)));
}

/** Sidebar footer toggle for Antigravity / Gemini plan quota. */
export function AntigravityPlanUsageButton() {
  const limits = useStore((s) => s.antigravityPlanLimits);
  const open = useStore((s) => s.antigravityPlanOpen);
  const setOpen = useStore((s) => s.setAntigravityPlanOpen);
  const now = useNow(true);
  const live = liveBuckets(limits, now);
  const rows = footerBuckets(live).map((b) => ({
    key: b.bucketId,
    label: shortLabel(b),
    pct: Math.round((1 - b.remainingFraction) * 100),
  }));
  const pct = peakUsed(live);
  const t = pct == null ? null : tone(pct);
  const unavailable = limits?.unavailable;
  const title = unavailable
    ? `Antigravity plan: ${unavailable}`
    : rows.length > 0
      ? `Antigravity plan usage: ${rows.map((r) => `${r.label} ${r.pct}% used`).join(", ")}`
      : "Antigravity plan usage";
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
        className={t === "danger" ? "text-danger" : t === "warn" ? "text-warn" : undefined}
      />
      {rows.length > 0 && (
        <span className="flex flex-col gap-[3px]" aria-hidden="true">
          {rows.map((r) => (
            <span key={r.key} className="flex items-center gap-[5px] text-[10.5px] leading-none">
              <span className="w-5 text-faint @max-[300px]/footer:hidden">{r.label}</span>
              <span className="h-1 w-[30px] overflow-hidden rounded-full bg-border">
                <span
                  className={`block h-full rounded-full ${barClass(tone(r.pct))}`}
                  style={{ width: `${Math.min(100, r.pct)}%` }}
                />
              </span>
              <span className="w-[4ch] text-right text-fg tabular-nums">{r.pct}%</span>
            </span>
          ))}
        </span>
      )}
      {unavailable && <span className="meta text-muted">n/a</span>}
      {!unavailable && pct != null && rows.length === 0 && (
        <span className={`meta ${t === "danger" ? "text-danger" : t === "warn" ? "text-warn" : ""}`}>{pct}%</span>
      )}
    </button>
  );
}

function PoolRow({ bucket, now }: { bucket: AntigravityPlanBucket; now: number }) {
  const used = Math.round((1 - bucket.remainingFraction) * 100);
  const left = Math.round(bucket.remainingFraction * 100);
  const t = tone(used);
  const reset = bucket.resetsAt;
  return (
    <div className="px-3 py-1.5">
      <div className="flex items-baseline justify-between gap-2">
        <span>{rowLabel(bucket)}</span>
        <span className="meta shrink-0 text-faint">
          {left}% left
          {reset != null && reset > now ? ` · resets in ${fmtSpan(reset - now)}` : ""}
        </span>
      </div>
      <div className="relative mt-1.5 h-1.5 overflow-hidden rounded-full bg-panel-2" aria-hidden="true">
        <div className={`h-full rounded-full ${barClass(t)}`} style={{ width: `${Math.min(100, used)}%` }} />
      </div>
    </div>
  );
}

/**
 * Antigravity plan usage from the Cloud Code quota poll (or a mesh peer on the same Google
 * account). Full-width sidebar pane above the footer; stays open until its button is clicked again.
 */
export function AntigravityPlanUsageBlade({ className }: { className?: string }) {
  const limits = useStore((s) => s.antigravityPlanLimits);
  const open = useStore((s) => s.antigravityPlanOpen);
  const setOpen = useStore((s) => s.setAntigravityPlanOpen);
  const now = useNow(open);
  useEffect(() => {
    api.antigravityPlanWatching(open).catch(() => {});
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
  const buckets = liveBuckets(limits, now);
  const empty = !limits?.unavailable && buckets.length === 0;
  return (
    <div
      role="dialog"
      aria-label="Antigravity plan usage"
      className={`max-h-[40vh] w-full shrink-0 overflow-y-auto border-t border-line bg-panel py-1.5 text-[13px] ${className ?? ""}`}
    >
      <div className="flex items-baseline justify-between px-3 pb-1.5 pt-2">
        <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">Antigravity plan</span>
        {limits && <span className="text-[11px] text-faint">updated {fmtSpan(now - limits.observedAt)} ago</span>}
      </div>
      {limits?.email && <p className="px-3 pb-1 text-[11px] text-faint">{limits.email}</p>}
      {limits?.unavailable ? (
        <p className="px-3 py-1.5 text-muted">{limits.unavailable}</p>
      ) : empty ? (
        <p className="px-3 py-1.5 text-muted">
          No usage yet. It appears while an Antigravity session is active here (Google account sign-in), or when a mesh
          peer on the same account reports.
        </p>
      ) : (
        buckets.map((b) => <PoolRow key={b.bucketId} bucket={b} now={now} />)
      )}
    </div>
  );
}
