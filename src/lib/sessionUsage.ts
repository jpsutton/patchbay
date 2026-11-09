// What a session's usage looks like when it is drawn: whether there is anything to draw at all,
// how full the context window is, and what colour that fill is. The composer's UsageBar and the
// sidebar's session gauge (components/SessionUsage) both go through these three rules, so the two
// can't disagree about a session. Types only from ipc, so the selftest runs under plain node.
import type { Tokens } from "./ipc";

/** Anything worth drawing: session totals, or a context count to put a ring around. */
export function hasUsage(totals: Tokens | null, contextTokens: number | null): boolean {
  const hasTotals = !!totals && (totals.input > 0 || totals.output > 0);
  return hasTotals || contextTokens != null;
}

/** Context fill as a whole percent; null while either number is unknown. */
export function contextPct(contextTokens: number | null, contextWindow: number | null): number | null {
  return contextTokens != null && contextWindow ? Math.min(100, (contextTokens / contextWindow) * 100) : null;
}

/** The fill's colour at `limit` percent: red at it, amber a quarter before it, green under that. */
export function usageToneClass(pct: number | null, limit: number): string {
  if (pct == null) return "stroke-muted";
  return pct >= limit ? "stroke-danger" : pct >= limit * 0.75 ? "stroke-warn" : "stroke-live";
}
