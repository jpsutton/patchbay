// The sidebar's usage gauge for a Code session whose agent has no plan window: Grok, Patchbay's
// own agent, and the ACP agents. Claude, Cursor and Antigravity sessions keep their plan gauges
// (App's SessionPlanUsageButton picks one of the two). Nothing new is fetched: the gauge reads the
// session's own `UsageView`, the same store entry the composer's UsageBar draws, priced by the same
// model resolution CodeView uses.
import { useEffect } from "react";
import {
  AGENT_LABELS,
  estimateCost,
  usesPatchbayModels,
  type CodeSession,
  type Model,
} from "../lib/ipc";
import { contextPct, hasUsage, usageToneClass } from "../lib/sessionUsage";
import { useStore } from "../lib/store";
import { GaugeIcon } from "./icons";
import { ContextRing, UsageBar, fmtTokens, usageCost, usageDetailRows, useCacheNote } from "./UsageBar";

/**
 * The catalog model id this session runs on: Patchbay's own agent and Grok follow the catalog
 * (the agent's own id carries a `pb-` prefix), the others keep a model the catalog doesn't know.
 * Null when nothing resolves — the same rule CodeView shows the model chip by.
 */
export function sessionModelRef(
  session: Pick<CodeSession, "agent" | "modelRef" | "agentModel"> | null | undefined,
  acpCurrentModelId: string | null | undefined,
): string | null {
  if (!session) return null;
  if (usesPatchbayModels(session.agent)) return acpCurrentModelId?.replace(/^pb-/, "") ?? session.modelRef;
  return acpCurrentModelId ?? session.agentModel;
}

/**
 * That id's entry in the catalog, for pricing and the context window. An agent that brings its own
 * model resolves to nothing: its usage is still counted, just not priced or sized by Patchbay.
 */
export function sessionModel(
  session: Pick<CodeSession, "agent" | "modelRef" | "agentModel"> | null | undefined,
  acpCurrentModelId: string | null | undefined,
  models: Model[],
): Model | undefined {
  if (!session || !usesPatchbayModels(session.agent)) return undefined;
  const ref = sessionModelRef(session, acpCurrentModelId);
  return ref ? models.find((m) => m.id === ref) : undefined;
}

/** The open Code session, its usage as the composer's bar reads it, and the model it runs on. */
function useSessionUsage() {
  const mode = useStore((s) => s.mode);
  const sessionId = useStore((s) => s.activeCodeSessionId);
  const session = useStore((s) => s.codeSessions.find((x) => x.id === sessionId));
  const state = useStore((s) => (sessionId ? s.code[sessionId] : undefined));
  const models = useStore((s) => s.models);
  const usage = state?.usage ?? null;
  return { mode, session, usage, model: sessionModel(session, state?.models?.currentModelId, models) };
}

/**
 * The sidebar footer's toggle for this session's usage pane: the context ring, the session's token
 * count and what it has cost, in the slot where plan gauges sit. Nothing reported yet means no
 * button at all, exactly as before this gauge existed.
 */
export function SessionUsageButton() {
  const open = useStore((s) => s.sessionUsageOpen);
  const setOpen = useStore((s) => s.setSessionUsageOpen);
  const { mode, session, usage, model } = useSessionUsage();
  // No session, or the agent has reported nothing yet: the slot stays empty, as it was before.
  if (mode !== "code" || !session || !usage || !hasUsage(usage.totals, usage.contextTokens)) return null;

  const totals = usage.totals;
  const contextTokens = usage.contextTokens;
  const hasTotals = totals.input > 0 || totals.output > 0;
  const contextWindow = usage.contextWindow ?? model?.contextWindow ?? null;
  const pct = contextPct(contextTokens, contextWindow);
  // hasUsage means one of the two is set: the count that went through, else the one in the window.
  const tokensShown = hasTotals ? totals.input : contextTokens!;
  const { reported, label } = usageCost(totals, estimateCost(totals, model?.params.pricing));
  const title = [
    hasTotals ? `Session usage: In ${totals.input.toLocaleString()} tokens, Out ${totals.output.toLocaleString()}` : "Session usage",
    label
      ? `Cost ${label}${reported != null ? " (reported by provider)" : " (estimated from model prices in Settings)"}`
      : null,
    contextTokens == null
      ? null
      : contextWindow
        ? `Context ${contextTokens.toLocaleString()} of ${contextWindow.toLocaleString()} tokens (${Math.round(pct ?? 0)}%)`
        : `Context ${contextTokens.toLocaleString()} tokens`,
  ]
    .filter(Boolean)
    .join("\n");
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
      {pct != null ? (
        <ContextRing pct={pct} tone={usageToneClass(pct, usage.autoCompactPercent ?? 90)} compactAt={usage.autoCompactPercent} />
      ) : (
        <GaugeIcon size={16} />
      )}
      {/* The ring says how full the window is; these say what has gone through it. */}
      <span className="flex flex-col gap-[3px]" aria-hidden="true">
        <span className="meta leading-none text-fg">{fmtTokens(tokensShown)}</span>
        {label && <span className="meta leading-none">{label}</span>}
      </span>
    </button>
  );
}

/** The pane the gauge opens: this session's usage, full width above the footer. */
export function SessionUsageBlade() {
  const open = useStore((s) => s.sessionUsageOpen);
  const setOpen = useStore((s) => s.setSessionUsageOpen);
  const { mode, session, usage, model } = useSessionUsage();
  const totals = usage?.totals ?? null;
  const contextTokens = usage?.contextTokens ?? null;
  // The cache clock only counts down while the pane is open; a closed pane has nothing to tick.
  const cacheNote = useCacheNote(open ? (usage?.cache ?? null) : null, contextTokens);
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
  if (!open || mode !== "code" || !session || !usage || !hasUsage(totals, contextTokens)) return null;

  const contextWindow = usage.contextWindow ?? model?.contextWindow ?? null;
  const hasTotals = usage.totals.input > 0 || usage.totals.output > 0;
  const pct = contextPct(contextTokens, contextWindow);
  const { reported, cost } = usageCost(usage.totals, estimateCost(usage.totals, model?.params.pricing));
  const rows = usageDetailRows({
    totals: usage.totals,
    hasTotals,
    fresh: Math.max(0, usage.totals.input - usage.totals.cachedRead - usage.totals.cacheWrite),
    reported,
    cost,
    contextTokens,
    contextWindow,
    pct,
    autoCompactPercent: usage.autoCompactPercent,
    cacheNote,
  });
  return (
    <div
      role="dialog"
      aria-label="Session usage"
      className="max-h-[40vh] w-full shrink-0 overflow-y-auto border-t border-line bg-panel py-1.5 text-[13px]"
    >
      <div className="flex items-baseline justify-between gap-2 px-3 pb-1.5 pt-2">
        <span className="font-mono text-[11px] uppercase tracking-[0.08em] text-faint">Session usage</span>
        <span className="shrink-0 text-[11px] text-faint">{model?.displayName ?? AGENT_LABELS[session.agent]}</span>
      </div>
      <UsageBar
        totals={usage.totals}
        contextTokens={usage.contextTokens}
        contextWindow={contextWindow}
        autoCompactPercent={usage.autoCompactPercent}
        estimatedCostUsd={estimateCost(usage.totals, model?.params.pricing)}
        cache={usage.cache ?? null}
        onConfigureWindow={() => useStore.getState().setSettingsOpen(true)}
      />
      <dl className="px-1 pb-1">
        {rows.map((r, i) => (
          <div
            key={`${r.label}-${i}`}
            className="grid grid-cols-[6.5rem_1fr] items-start gap-x-2 border-t border-line px-2 py-1.5 first:border-t-0"
          >
            <dt className="text-[12.5px] text-muted">{r.label || "\u00a0"}</dt>
            <dd className="min-w-0 break-words text-[12.5px] text-fg">{r.value}</dd>
          </div>
        ))}
      </dl>
    </div>
  );
}
