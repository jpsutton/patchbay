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
  const tone = pct == null ? "bg-muted" : pct >= limit ? "bg-danger" : pct >= limit * 0.75 ? "bg-warn" : "bg-live";
  const ctxTitle =
    contextTokens == null
      ? "Context fill is reported after the next model call"
      : contextWindow
        ? `${contextTokens.toLocaleString()} of ${contextWindow.toLocaleString()} tokens in context` +
          (autoCompactPercent ? `\nAuto-compacts at ${autoCompactPercent}%` : "")
        : `${contextTokens.toLocaleString()} tokens in context. Set this model's context window in Settings to see a meter.`;

  return (
    <div className="meta mt-1.5 flex h-5 items-center gap-3 px-1.5 text-faint" aria-label="Token usage">
      {hasTotals && totals && (
        <div className="flex min-w-0 items-center gap-2 truncate" title={detail}>
          <span>
            In <span className="text-muted">{fmtTokens(totals.input)}</span>
            {totals.cachedRead > 0 && <span> ({fmtTokens(totals.cachedRead)} cached)</span>}
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
      <div className="flex shrink-0 items-center gap-2" title={ctxTitle}>
        <span>
          Context{" "}
          {contextTokens == null ? (
            "—"
          ) : (
            <>
              <span className="text-muted">{fmtTokens(contextTokens)}</span>
              {contextWindow ? <> / {fmtTokens(contextWindow)}</> : null}
              {pct != null && <> · {Math.round(pct)}%</>}
            </>
          )}
        </span>
        {contextWindow ? (
          <div
            className="relative h-1 w-24 overflow-hidden rounded-full bg-panel-2"
            role="meter"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={pct == null ? undefined : Math.round(pct)}
            aria-label="Context window used"
          >
            <div className={`h-full rounded-full ${tone}`} style={{ width: `${pct ?? 0}%` }} />
            {autoCompactPercent ? (
              <div className="absolute top-0 h-full w-px bg-faint" style={{ left: `${autoCompactPercent}%` }} />
            ) : null}
          </div>
        ) : (
          onConfigureWindow &&
          contextTokens != null && (
            <button className="font-sans text-accent-ink hover:underline" onClick={onConfigureWindow}>
              set window
            </button>
          )
        )}
      </div>
    </div>
  );
}
