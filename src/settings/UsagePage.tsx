import { useEffect, useMemo, useState } from "react";
import { AGENT_LABELS, api, errorText, estimateCost, type Tokens, type UsageReport } from "../lib/ipc";
import { useStore } from "../lib/store";
import { fmtTokens } from "../components/UsageBar";
import { Choice } from "./PreferencesPage";

const zero = (): Tokens => ({ input: 0, cachedRead: 0, cacheWrite: 0, output: 0, reasoning: 0, costUsd: null, apiMs: 0 });

function add(a: Tokens, b: Tokens): Tokens {
  return {
    input: a.input + b.input,
    cachedRead: a.cachedRead + b.cachedRead,
    cacheWrite: a.cacheWrite + b.cacheWrite,
    output: a.output + b.output,
    reasoning: a.reasoning + b.reasoning,
    costUsd: a.costUsd == null && b.costUsd == null ? null : (a.costUsd ?? 0) + (b.costUsd ?? 0),
    apiMs: a.apiMs + b.apiMs,
  };
}

function fmtUsd(n: number): string {
  return n < 0.01 && n > 0 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`;
}

function localDay(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="flex-1 rounded-lg border border-border bg-panel px-3 py-2">
      <div className="text-[12px] text-muted">{label}</div>
      <div className="text-lg font-semibold tabular-nums">{value}</div>
      {sub && <div className="text-[12px] text-muted">{sub}</div>}
    </div>
  );
}

/** One-series daily bars: thin marks, rounded tops, hover tooltip, recessive axis. */
function DailyBars({ days, values, format }: { days: string[]; values: number[]; format: (n: number) => string }) {
  const [hover, setHover] = useState<number | null>(null);
  const max = Math.max(...values, 0);
  const H = 120;
  const W = 600;
  const slot = W / Math.max(days.length, 1);
  const bw = Math.max(2, Math.min(14, slot * 0.6));
  const ticks = [0, max / 2, max];
  return (
    <div className="relative">
      <svg viewBox={`0 0 ${W} ${H + 18}`} className="w-full" role="img" aria-label="Usage per day">
        {ticks.map((t, i) => (
          <line key={i} x1={0} x2={W} y1={H - (max ? (t / max) * H : 0)} y2={H - (max ? (t / max) * H : 0)} stroke="var(--color-border)" strokeWidth={1} />
        ))}
        {values.map((v, i) => {
          const h = max ? (v / max) * H : 0;
          const x = i * slot + (slot - bw) / 2;
          const r = Math.min(4, bw / 2, h);
          return (
            <g key={days[i]} onMouseEnter={() => setHover(i)} onMouseLeave={() => setHover(null)}>
              {/* Hit target wider and taller than the mark. */}
              <rect x={i * slot} y={0} width={slot} height={H} fill="transparent" />
              {h > 0 && (
                <path
                  d={`M${x},${H} V${H - h + r} Q${x},${H - h} ${x + r},${H - h} H${x + bw - r} Q${x + bw},${H - h} ${x + bw},${H - h + r} V${H} Z`}
                  fill="var(--color-accent)"
                  opacity={hover === null || hover === i ? 1 : 0.45}
                />
              )}
            </g>
          );
        })}
        {[0, Math.floor((days.length - 1) / 2), days.length - 1].filter((v, i, a) => a.indexOf(v) === i && days[v]).map((i) => (
          <text
            key={i}
            x={i === 0 ? 0 : i === days.length - 1 ? W : i * slot + slot / 2}
            y={H + 14}
            textAnchor={i === 0 ? "start" : i === days.length - 1 ? "end" : "middle"}
            fontSize={10}
            fill="var(--color-muted)"
          >
            {days[i].slice(5)}
          </text>
        ))}
      </svg>
      {hover !== null && (
        <div
          className="pointer-events-none absolute -top-2 rounded-md border border-border bg-bg px-2 py-1 text-[12px] shadow-md"
          style={{ left: `min(calc(${((hover + 0.5) / days.length) * 100}% - 40px), calc(100% - 110px))` }}
        >
          <div className="text-muted">{days[hover]}</div>
          <div className="font-medium tabular-nums">{format(values[hover])}</div>
        </div>
      )}
    </div>
  );
}

export function UsagePage() {
  const models = useStore((s) => s.models);
  const providers = useStore((s) => s.providers);
  const setToast = useStore((s) => s.setToast);
  const [days, setDays] = useState<"7" | "30" | "90">("30");
  const [report, setReport] = useState<UsageReport | null>(null);
  const [metric, setMetric] = useState<"cost" | "tokens">("cost");

  useEffect(() => {
    api.usageReport(Number(days)).then(setReport).catch((e) => setToast(errorText(e)));
  }, [days, setToast]);

  const costOf = (t: Tokens, modelRef: string | null) =>
    t.costUsd != null && t.costUsd > 0 ? t.costUsd : (estimateCost(t, models.find((m) => m.id === modelRef)?.params.pricing) ?? 0);
  const label = (ref: string | null) => {
    const m = models.find((x) => x.id === ref);
    if (!m) return ref ? "Removed model" : "Unknown";
    const p = providers.find((x) => x.id === m.providerId);
    return p ? `${m.displayName} · ${p.name}` : m.displayName;
  };

  const view = useMemo(() => {
    if (!report) return null;
    const n = Number(days);
    const dayList: string[] = [];
    for (let i = n - 1; i >= 0; i--) dayList.push(localDay(new Date(Date.now() - i * 86_400_000)));
    const perDay = new Map(dayList.map((d) => [d, { cost: 0, tokens: 0 }]));
    const byModel = new Map<string, { ref: string | null; replies: number; usage: Tokens; cost: number }>();
    let total = zero();
    let cost = 0;
    let replies = 0;
    for (const r of report.chat) {
      const c = costOf(r.usage, r.modelRef);
      const d = perDay.get(r.day);
      if (d) {
        d.cost += c;
        d.tokens += r.usage.input + r.usage.output;
      }
      const key = r.modelRef ?? "";
      const m = byModel.get(key) ?? { ref: r.modelRef, replies: 0, usage: zero(), cost: 0 };
      m.replies += r.replies;
      m.usage = add(m.usage, r.usage);
      m.cost += c;
      byModel.set(key, m);
      total = add(total, r.usage);
      cost += c;
      replies += r.replies;
    }
    // Only Grok Build runs on Patchbay's models; other agents bring their own, so `modelRef` is
    // just the default it was created with and its prices don't apply.
    const code = report.code.map((s) => {
      const own = s.agent !== "grok";
      return {
        ...s,
        label: own ? [s.agentModel, AGENT_LABELS[s.agent] ?? s.agent].filter(Boolean).join(" · ") : label(s.modelRef),
        cost: own ? (s.usage.costUsd ?? 0) : costOf(s.usage, s.modelRef),
      };
    });
    return {
      dayList,
      perDay: dayList.map((d) => perDay.get(d)!),
      models: [...byModel.values()].sort((a, b) => b.cost - a.cost || b.usage.input - a.usage.input),
      total,
      cost,
      replies,
      code,
      codeCost: code.reduce((s, c) => s + c.cost, 0),
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [report, models, providers, days]);

  if (!view) return <div className="text-xs text-muted">Loading…</div>;
  const anyCost = view.cost > 0;
  const shown = anyCost ? metric : "tokens";

  return (
    <div className="flex max-w-3xl flex-col gap-4 pb-4">
      <div className="flex items-center gap-3">
        <Choice
          value={days}
          onChange={setDays}
          options={[
            { value: "7", label: "7 days" },
            { value: "30", label: "30 days" },
            { value: "90", label: "90 days" },
          ]}
        />
        <span className="flex-1" />
        {anyCost && (
          <Choice
            value={metric}
            onChange={setMetric}
            options={[
              { value: "cost", label: "Cost" },
              { value: "tokens", label: "Tokens" },
            ]}
          />
        )}
      </div>
      <div className="flex gap-3">
        <Stat label="Chat spend" value={anyCost ? fmtUsd(view.cost) : "—"} sub={anyCost ? "reported or estimated from model prices" : "add prices to models to estimate"} />
        <Stat label="Chat tokens" value={`${fmtTokens(view.total.input)} in`} sub={`${fmtTokens(view.total.output)} out · ${fmtTokens(view.total.cachedRead)} cached`} />
        <Stat label="Replies" value={String(view.replies)} />
      </div>
      <div>
        <div className="mb-1 text-xs font-medium">Chat {shown === "cost" ? "spend" : "tokens"} per day</div>
        <DailyBars
          days={view.dayList}
          values={view.perDay.map((d) => (shown === "cost" ? d.cost : d.tokens))}
          format={shown === "cost" ? fmtUsd : (n) => `${fmtTokens(n)} tokens`}
        />
      </div>
      <table className="w-full text-xs">
        <thead className="text-left text-muted">
          <tr>
            <th className="pb-1 font-normal">Model</th>
            <th className="pb-1 text-right font-normal">Replies</th>
            <th className="pb-1 text-right font-normal">Input</th>
            <th className="pb-1 text-right font-normal">Output</th>
            <th className="pb-1 text-right font-normal">Cost</th>
          </tr>
        </thead>
        <tbody className="tabular-nums">
          {view.models.map((m) => (
            <tr key={m.ref ?? ""} className="border-t border-border">
              <td className="py-1.5">{label(m.ref)}</td>
              <td className="py-1.5 text-right">{m.replies}</td>
              <td className="py-1.5 text-right">{fmtTokens(m.usage.input)}</td>
              <td className="py-1.5 text-right">{fmtTokens(m.usage.output)}</td>
              <td className="py-1.5 text-right">{m.cost > 0 ? fmtUsd(m.cost) : "—"}</td>
            </tr>
          ))}
          {view.models.length === 0 && (
            <tr>
              <td colSpan={5} className="py-2 text-muted">
                No chat usage in this period.
              </td>
            </tr>
          )}
        </tbody>
      </table>
      <div>
        <div className="mb-1 text-xs font-medium">
          Code sessions <span className="font-normal text-muted">(all time{view.codeCost > 0 ? `, ${fmtUsd(view.codeCost)}` : ""})</span>
        </div>
        <table className="w-full text-xs">
          <tbody className="tabular-nums">
            {view.code.slice(0, 20).map((s) => (
              <tr key={s.id} className="border-t border-border">
                <td className="max-w-64 truncate py-1.5" title={s.cwd}>
                  {s.title}
                </td>
                <td className="py-1.5 text-muted">{s.label}</td>
                <td className="py-1.5 text-right">{fmtTokens(s.usage.input)} in</td>
                <td className="py-1.5 text-right">{fmtTokens(s.usage.output)} out</td>
                <td className="py-1.5 text-right">{s.cost > 0 ? fmtUsd(s.cost) : "—"}</td>
              </tr>
            ))}
            {view.code.length === 0 && (
              <tr>
                <td className="py-2 text-muted">No code session usage yet.</td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
