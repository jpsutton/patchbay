import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { api, type SessionPlanUsage } from "../lib/ipc";
import { isRemoteApp } from "../lib/platform";
import { sessionPlanRows, type InfoRow } from "../lib/sessionInfo";

const nowSecs = () => Math.floor(Date.now() / 1000);

function planKindLabel(plan: SessionPlanUsage): string {
  if (plan.kind === "claude") return "Claude plan";
  if (plan.kind === "cursor") return "Cursor plan";
  return "Antigravity plan";
}

/** One-line chip text: first couple of usage rows, without Account / Updated. */
function planChipSummary(rows: InfoRow[]): string {
  const bits = rows
    .filter((r) => r.label !== "Account" && r.label !== "Updated")
    .slice(0, 2)
    .map((r) => {
      const short = r.value.split(" · ")[0]?.trim() || r.value;
      return r.label ? `${r.label} ${short}` : short;
    });
  return bits.length ? bits.join(" · ") : "Tap for details";
}

/**
 * Phone-only plan meter under the composer. Desktop keeps the sidebar gauge; touch has no
 * sidebar footer, so this row opens a sheet with the same plan facts as Session info.
 */
export function MobilePlanUsage({ sessionId }: { sessionId: string }) {
  const remote = isRemoteApp();
  const [plan, setPlan] = useState<SessionPlanUsage | null | undefined>(undefined);
  const [sheetOpen, setSheetOpen] = useState(false);
  const [now, setNow] = useState(nowSecs);

  useEffect(() => {
    if (!remote) return;
    let live = true;
    const load = () => {
      api.sessionPlanUsage(sessionId).then(
        (p) => live && setPlan(p),
        () => live && setPlan(null),
      );
    };
    load();
    const poll = setInterval(load, 60_000);
    return () => {
      live = false;
      clearInterval(poll);
    };
  }, [remote, sessionId]);

  useEffect(() => {
    if (!sheetOpen) return;
    setNow(nowSecs());
    api.sessionPlanUsage(sessionId).then(setPlan, () => {});
    const t = setInterval(() => setNow(nowSecs()), 30_000);
    return () => clearInterval(t);
  }, [sheetOpen, sessionId]);

  if (!remote || plan == null) return null;

  const rows = sessionPlanRows(plan, now) ?? [{ label: "Usage", value: "Not reported yet" }];
  const title = planKindLabel(plan);
  const summary = planChipSummary(rows);

  return (
    <>
      <button
        type="button"
        className="meta mt-1 flex h-5 w-full items-center gap-2 rounded-md px-1.5 text-left text-faint active:bg-panel-2"
        aria-label={`${title}: ${summary}, tap for details`}
        onClick={() => setSheetOpen(true)}
      >
        <span className="shrink-0 text-muted">{title}</span>
        <span className="min-w-0 flex-1 truncate tabular-nums">{summary}</span>
      </button>
      {sheetOpen && <PlanDetailSheet title={title} rows={rows} onClose={() => setSheetOpen(false)} />}
    </>
  );
}

/** Phone bottom sheet: provider plan rows, same facts as Session info's Plan section. */
function PlanDetailSheet({ title, rows, onClose }: { title: string; rows: InfoRow[]; onClose: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return createPortal(
    <div className="fixed inset-0 z-[60] flex flex-col justify-end bg-black/45" onClick={onClose}>
      <div
        role="dialog"
        aria-modal="true"
        aria-label={title}
        className="rounded-t-2xl border-t border-border bg-raised px-2 pb-[calc(env(safe-area-inset-bottom)+8px)] pt-2"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="truncate px-3 pb-2 pt-1 text-[13px] font-medium text-fg">{title}</div>
        <dl className="px-1 pb-2">
          {rows.map((r, i) => (
            <div
              key={`${r.label}-${i}`}
              className="grid grid-cols-[7.5rem_1fr] items-start gap-x-2 border-t border-line px-2 py-2.5 first:border-t-0"
            >
              <dt className="text-[13px] text-muted">{r.label || "\u00a0"}</dt>
              <dd className="min-w-0 break-words text-[14px] text-fg">{r.value}</dd>
            </div>
          ))}
        </dl>
        <button
          type="button"
          className="mt-1 flex h-12 w-full items-center justify-center rounded-xl text-[16px] text-muted active:bg-panel-2"
          onClick={onClose}
        >
          Close
        </button>
      </div>
    </div>,
    document.body,
  );
}
