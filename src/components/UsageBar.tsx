import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import type { CacheClock, Tokens } from "../lib/ipc";
import { contextPct, hasUsage, usageToneClass } from "../lib/sessionUsage";
import { isRemoteApp } from "../lib/platform";

export function fmtTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 10_000) return `${(n / 1000).toFixed(1)}k`;
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
  return `${(n / 1_000_000).toFixed(n < 10_000_000 ? 2 : 1)}M`;
}

function fmtCost(usd: number): string {
  return usd < 0.01 ? `$${usd.toFixed(4)}` : `$${usd.toFixed(2)}`;
}

export type DetailRow = { label: string; value: string };

/**
 * What to say about cost: what the provider reported, else the estimate from the model's prices,
 * and the label that goes with it (`$0.07` reported, `~$0.07` estimated, null when neither).
 */
export function usageCost(
  totals: Tokens | null | undefined,
  estimated: number | null | undefined,
): { reported: number | null; cost: number | null; label: string | null } {
  const reported = totals?.costUsd != null && totals.costUsd > 0 ? totals.costUsd : null;
  const cost = reported ?? (estimated != null && estimated > 0 ? estimated : null);
  return { reported, cost, label: cost == null ? null : `${reported != null ? "" : "~"}${fmtCost(cost)}` };
}

/**
 * One-line usage summary under a composer: session token totals on the left, a context-window
 * meter on the right. `contextWindow` null means the model's window is unknown.
 *
 * On the phone (remote), tap the bar to open a bottom sheet with the same details desktop gets
 * from hover tooltips — touch has no hover.
 */
export function UsageBar({
  totals,
  contextTokens,
  contextWindow,
  autoCompactPercent,
  estimatedCostUsd,
  cache,
  onConfigureWindow,
}: {
  totals: Tokens | null;
  contextTokens: number | null;
  contextWindow: number | null;
  autoCompactPercent?: number | null;
  /** From the model's configured prices; shown when the provider reports no cost. */
  estimatedCostUsd?: number | null;
  /** The agent's prompt cache clock; shown as a countdown while the cache is warm. */
  cache?: CacheClock | null;
  onConfigureWindow?: () => void;
}) {
  const remote = isRemoteApp();
  const [sheetOpen, setSheetOpen] = useState(false);
  const cacheNote = useCacheNote(cache ?? null, contextTokens);
  const hasTotals = !!totals && (totals.input > 0 || totals.output > 0);
  if (!hasUsage(totals, contextTokens)) return <div className="h-5" />;

  const { reported, cost } = usageCost(totals, estimatedCostUsd);
  const fresh = totals ? Math.max(0, totals.input - totals.cachedRead - totals.cacheWrite) : 0;
  const detail = totals
    ? [
        `Input: ${totals.input.toLocaleString()} tokens`,
        `  fresh: ${fresh.toLocaleString()}`,
        `  cache read: ${totals.cachedRead.toLocaleString()}`,
        `  cache write: ${totals.cacheWrite.toLocaleString()}`,
        `Output: ${totals.output.toLocaleString()} tokens`,
        totals.reasoning ? `  reasoning: ${totals.reasoning.toLocaleString()}` : null,
        reported != null
          ? `Cost: ${fmtCost(reported)} (reported by provider)`
          : cost != null
            ? `Cost: ~${fmtCost(cost)} (estimated from model prices in Settings)`
            : null,
        totals.apiMs ? `API time: ${(totals.apiMs / 1000).toFixed(1)}s` : null,
      ]
        .filter(Boolean)
        .join("\n")
    : "";

  const pct = contextPct(contextTokens, contextWindow);
  const limit = autoCompactPercent ?? 90;
  const tone = usageToneClass(pct, limit);
  const ctxTitle = [
    contextTokens == null
      ? "Context fill is reported after the next model call"
      : contextWindow
        ? `${contextTokens.toLocaleString()} of ${contextWindow.toLocaleString()} tokens in context (${Math.round(pct ?? 0)}%)` +
          (autoCompactPercent ? `\nAuto-compacts at ${autoCompactPercent}%` : "")
        : `${contextTokens.toLocaleString()} tokens in context. Set this model's context window in Settings to see a meter.`,
    cacheNote,
  ]
    .filter(Boolean)
    .join("\n\n");

  const rows = usageDetailRows({
    totals,
    hasTotals,
    fresh,
    reported,
    cost,
    contextTokens,
    contextWindow,
    pct,
    autoCompactPercent,
    cacheNote,
  });

  const openSheet = () => setSheetOpen(true);

  return (
    <>
      <div
        className={`meta @container mt-1.5 flex h-5 items-center gap-3 px-1.5 text-faint ${remote ? "cursor-pointer rounded-md active:bg-panel-2" : ""}`}
        aria-label={remote ? "Token usage, tap for details" : "Token usage"}
        role={remote ? "button" : undefined}
        tabIndex={remote ? 0 : undefined}
        onClick={remote ? openSheet : undefined}
        onKeyDown={
          remote
            ? (e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  openSheet();
                }
              }
            : undefined
        }
      >
        {/* Narrow screens (phones) leave out the cached count; it is in the tooltip / sheet. The context fill and the prompt cache clock are only in the meter's tooltip / sheet. */}
        {hasTotals && totals && (
          <div className="flex min-w-0 items-center gap-2 overflow-hidden whitespace-nowrap" title={remote ? undefined : detail}>
            <span>
              In <span className="text-muted">{fmtTokens(totals.input)}</span>
              {totals.cachedRead > 0 && <span className="hidden @sm:inline"> ({fmtTokens(totals.cachedRead)} cached)</span>}
            </span>
            <span>·</span>
            <span>
              Out <span className="text-muted">{fmtTokens(totals.output)}</span>
            </span>
            {cost != null && (
              <>
                <span className="hidden @[14rem]:inline">·</span>
                <span className="hidden @[14rem]:inline">{reported != null ? fmtCost(cost) : `~${fmtCost(cost)}`}</span>
              </>
            )}
          </div>
        )}
        <div className="flex-1" />
        <div className="flex shrink-0 items-center gap-1.5 whitespace-nowrap" title={remote ? undefined : ctxTitle}>
          {contextWindow ? (
            <ContextRing pct={pct ?? 0} tone={tone} compactAt={autoCompactPercent ?? null} />
          ) : (
            // No window, no meter: the bare count stays as the thing to hover / tap.
            <span>
              Context <span className="text-muted">{contextTokens == null ? "—" : fmtTokens(contextTokens)}</span>
            </span>
          )}
          {!contextWindow && onConfigureWindow && contextTokens != null && (
            <button
              type="button"
              className="hidden shrink-0 whitespace-nowrap font-sans text-accent-ink hover:underline sm:inline"
              onClick={(e) => {
                e.stopPropagation();
                onConfigureWindow();
              }}
            >
              set window
            </button>
          )}
        </div>
      </div>
      {sheetOpen && <UsageDetailSheet rows={rows} onClose={() => setSheetOpen(false)} />}
    </>
  );
}

export function usageDetailRows({
  totals,
  hasTotals,
  fresh,
  reported,
  cost,
  contextTokens,
  contextWindow,
  pct,
  autoCompactPercent,
  cacheNote,
}: {
  totals: Tokens | null;
  hasTotals: boolean;
  fresh: number;
  reported: number | null;
  cost: number | null;
  contextTokens: number | null;
  contextWindow: number | null;
  pct: number | null;
  autoCompactPercent?: number | null;
  cacheNote: string | null;
}): DetailRow[] {
  const rows: DetailRow[] = [];
  if (hasTotals && totals) {
    rows.push({ label: "Input", value: `${totals.input.toLocaleString()} tokens` });
    rows.push({ label: "Fresh", value: fresh.toLocaleString() });
    rows.push({ label: "Cache read", value: totals.cachedRead.toLocaleString() });
    rows.push({ label: "Cache write", value: totals.cacheWrite.toLocaleString() });
    rows.push({ label: "Output", value: `${totals.output.toLocaleString()} tokens` });
    if (totals.reasoning) rows.push({ label: "Reasoning", value: totals.reasoning.toLocaleString() });
    if (reported != null) rows.push({ label: "Cost", value: `${fmtCost(reported)} (reported)` });
    else if (cost != null) rows.push({ label: "Cost", value: `~${fmtCost(cost)} (estimated)` });
    else rows.push({ label: "Cost", value: "Not reported yet" });
    if (totals.apiMs) rows.push({ label: "API time", value: `${(totals.apiMs / 1000).toFixed(1)}s` });
  } else {
    rows.push({ label: "Tokens", value: "Not reported yet" });
  }

  if (contextTokens == null) {
    rows.push({ label: "Context", value: "Not reported yet" });
  } else if (contextWindow) {
    rows.push({
      label: "Context",
      value: `${contextTokens.toLocaleString()} of ${contextWindow.toLocaleString()} (${Math.round(pct ?? 0)}%)`,
    });
    if (autoCompactPercent) rows.push({ label: "Auto-compact", value: `At ${autoCompactPercent}%` });
  } else {
    rows.push({
      label: "Context",
      value: `${contextTokens.toLocaleString()} tokens (set window in Settings for a meter)`,
    });
  }

  if (cacheNote) {
    for (const line of cacheNote.split("\n").filter(Boolean)) {
      rows.push({ label: rows.some((r) => r.label === "Cache") ? "" : "Cache", value: line });
    }
  }

  return rows;
}

/** Phone bottom sheet: same usage facts as the desktop hover tooltips, as readable rows. */
function UsageDetailSheet({ rows, onClose }: { rows: DetailRow[]; onClose: () => void }) {
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
        aria-label="Session usage"
        className="rounded-t-2xl border-t border-border bg-raised px-2 pb-[calc(env(safe-area-inset-bottom)+8px)] pt-2"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="truncate px-3 pb-2 pt-1 text-[13px] font-medium text-fg">Session usage</div>
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

function fmtClock(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

/**
 * The prompt cache clock as tooltip text: warm for N more minutes, or cold. Anthropic bills a
 * prompt that arrives after the cache expires at the cache-write price for the whole context, and
 * `/keepwarm` refreshes it. Null while no cache clock is known.
 */
export function useCacheNote(cache: CacheClock | null, contextTokens: number | null): string | null {
  const lastAt = cache?.lastAt ?? null;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (lastAt == null) return;
    setNow(Date.now());
    const t = setInterval(() => setNow(Date.now()), 15_000);
    return () => clearInterval(t);
  }, [lastAt]);
  if (!cache || lastAt == null) return null;

  const expiresAt = lastAt + cache.ttlSecs * 1000;
  const left = expiresAt - now;
  const auto = cache.auto ? "\nAuto keep-warm is on: /keepwarm auto turns it off." : "\n/keepwarm refreshes it; /keepwarm auto does it for you.";
  if (left <= 0) {
    return `Prompt cache expired at ${fmtClock(expiresAt)}.${
      contextTokens != null ? ` The next prompt re-caches about ${fmtTokens(contextTokens)} tokens at the cache-write price.` : ""
    }${auto}`;
  }
  return `Prompt cache is warm for ${Math.ceil(left / 60_000)}m, until ${fmtClock(expiresAt)}. A prompt sent before then reads it at the cache-read price.${auto}`;
}

const RING_R = 5.5;
const RING_C = 2 * Math.PI * RING_R;

/** The context window as a ring filling clockwise from the top, with a tick where it auto-compacts. */
export function ContextRing({ pct, tone, compactAt }: { pct: number; tone: string; compactAt: number | null }) {
  const tick = compactAt != null ? (compactAt / 100) * 2 * Math.PI : null;
  return (
    <svg
      viewBox="0 0 16 16"
      className="h-3.5 w-3.5 shrink-0 -rotate-90"
      role="meter"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(pct)}
      aria-label="Context window used"
    >
      <circle cx="8" cy="8" r={RING_R} fill="none" strokeWidth="2.5" className="stroke-border" />
      {pct > 0 && (
        <circle
          cx="8"
          cy="8"
          r={RING_R}
          fill="none"
          strokeWidth="2.5"
          className={tone}
          // A sliver stays visible however small the share.
          strokeDasharray={`${Math.max(0.6, (pct / 100) * RING_C)} ${RING_C}`}
        />
      )}
      {tick != null && (
        <line
          x1={8 + (RING_R - 1.75) * Math.cos(tick)}
          y1={8 + (RING_R - 1.75) * Math.sin(tick)}
          x2={8 + (RING_R + 1.75) * Math.cos(tick)}
          y2={8 + (RING_R + 1.75) * Math.sin(tick)}
          strokeWidth="1"
          className="stroke-faint"
        />
      )}
    </svg>
  );
}
