import type { Tokens } from "../lib/ipc";

export function fmtTokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 10_000) return `${(n / 1000).toFixed(1)}k`;
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
  return `${(n / 1_000_000).toFixed(n < 10_000_000 ? 2 : 1)}M`;
}

function fmtCost(usd: number): string {
  return usd < 0.01 ? `$${usd.toFixed(4)}` : `$${usd.toFixed(2)}`;
}

/**
 * One-line usage summary under a composer: session token totals on the left, a context-window
 * meter on the right. `contextWindow` null means the model's window is unknown.
 */
export function UsageBar({
  totals,
  contextTokens,
  contextWindow,
  autoCompactPercent,
  estimatedCostUsd,
  onConfigureWindow,
}: {
  totals: Tokens | null;
  contextTokens: number | null;
  contextWindow: number | null;
  autoCompactPercent?: number | null;
  /** From the model's configured prices; shown when the provider reports no cost. */
  estimatedCostUsd?: number | null;
  onConfigureWindow?: () => void;
}) {
  const hasTotals = !!totals && (totals.input > 0 || totals.output > 0);
  if (!hasTotals && contextTokens == null) return <div className="h-5" />;

  const reported = totals?.costUsd != null && totals.costUsd > 0 ? totals.costUsd : null;
  const cost = reported ?? (estimatedCostUsd != null && estimatedCostUsd > 0 ? estimatedCostUsd : null);
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

  const pct =
    contextTokens != null && contextWindow ? Math.min(100, (contextTokens / contextWindow) * 100) : null;
  const limit = autoCompactPercent ?? 90;
  const tone =
    pct == null ? "stroke-muted" : pct >= limit ? "stroke-danger" : pct >= limit * 0.75 ? "stroke-warn" : "stroke-live";
  const ctxTitle =
    contextTokens == null
      ? "Context fill is reported after the next model call"
      : contextWindow
        ? `${contextTokens.toLocaleString()} of ${contextWindow.toLocaleString()} tokens in context` +
          (autoCompactPercent ? `\nAuto-compacts at ${autoCompactPercent}%` : "")
        : `${contextTokens.toLocaleString()} tokens in context. Set this model's context window in Settings to see a meter.`;

  return (
    <div className="meta mt-1.5 flex h-5 items-center gap-3 px-1.5 text-faint" aria-label="Token usage">
      {/* Narrow screens (phones) leave out the cached count and the context counts; both are in the tooltips. */}
      {hasTotals && totals && (
        <div className="flex shrink-0 items-center gap-2 whitespace-nowrap" title={detail}>
          <span>
            In <span className="text-muted">{fmtTokens(totals.input)}</span>
            {totals.cachedRead > 0 && <span className="hidden sm:inline"> ({fmtTokens(totals.cachedRead)} cached)</span>}
          </span>
          <span>·</span>
          <span>
            Out <span className="text-muted">{fmtTokens(totals.output)}</span>
          </span>
          {cost != null && (
            <>
              <span>·</span>
              <span>{reported != null ? fmtCost(cost) : `~${fmtCost(cost)}`}</span>
            </>
          )}
        </div>
      )}
      <div className="flex-1" />
      <div className="flex shrink-0 items-center gap-1.5 whitespace-nowrap" title={ctxTitle}>
        {contextWindow ? <ContextRing pct={pct ?? 0} tone={tone} compactAt={autoCompactPercent ?? null} /> : null}
        <span>
          {/* Next to its meter a bare percentage reads as context; a bare count wouldn't. */}
          <span className={pct != null ? "hidden sm:inline" : ""}>Context </span>
          {contextTokens == null ? (
            "—"
          ) : pct != null ? (
            <>
              <span className="hidden sm:inline">
                <span className="text-muted">{fmtTokens(contextTokens)}</span> / {fmtTokens(contextWindow!)} ·{" "}
              </span>
              <span className="text-muted">{Math.round(pct)}%</span>
            </>
          ) : (
            <span className="text-muted">{fmtTokens(contextTokens)}</span>
          )}
        </span>
        {!contextWindow && onConfigureWindow && contextTokens != null && (
          <button className="hidden shrink-0 whitespace-nowrap font-sans text-accent-ink hover:underline sm:inline" onClick={onConfigureWindow}>
            set window
          </button>
        )}
      </div>
    </div>
  );
}

const RING_R = 5.5;
const RING_C = 2 * Math.PI * RING_R;

/** The context window as a ring filling clockwise from the top, with a tick where it auto-compacts. */
function ContextRing({ pct, tone, compactAt }: { pct: number; tone: string; compactAt: number | null }) {
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
