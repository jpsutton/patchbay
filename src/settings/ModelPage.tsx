import { useEffect, useMemo, useState, type ReactNode } from "react";
import {
  api,
  errorText,
  estimateCost,
  type Model,
  type ModelParams,
  type Provider,
  type ReasoningEffort,
  type ReasoningSummary,
} from "../lib/ipc";
import { useStore } from "../lib/store";
import { input } from "./ProviderPage";

const EFFORTS: ReasoningEffort[] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const SUMMARIES: ReasoningSummary[] = ["none", "auto", "concise", "detailed"];

/** Form state: numbers stay as typed text until saved. */
interface Draft {
  displayName: string;
  modelId: string;
  vision: boolean;
  contextWindow: string;
  autoCompactPercent: string;
  maxOutputTokens: string;
  temperature: string;
  topP: string;
  reasoningEffort: ReasoningEffort | "";
  reasoningSummary: ReasoningSummary | "";
  priceInput: string;
  priceCached: string;
  priceCacheWrite: string;
  priceOutput: string;
}

const str = (n: number | null | undefined) => (n == null ? "" : String(n));

function toDraft(m: Model): Draft {
  const p = m.params;
  return {
    displayName: m.displayName,
    modelId: m.modelId,
    vision: m.vision,
    contextWindow: str(m.contextWindow),
    autoCompactPercent: str(p.autoCompactPercent),
    maxOutputTokens: str(p.maxOutputTokens),
    temperature: str(p.temperature),
    topP: str(p.topP),
    reasoningEffort: p.reasoningEffort ?? "",
    reasoningSummary: p.reasoningSummary ?? "",
    priceInput: str(p.pricing.input),
    priceCached: str(p.pricing.cachedInput),
    priceCacheWrite: str(p.pricing.cacheWrite),
    priceOutput: str(p.pricing.output),
  };
}

/** "" → null; otherwise a finite number (integer when `int`) within [min, max], or throws. */
function num(v: string, label: string, int = false, min = 0, max = Infinity): number | null {
  const t = v.trim().replace(/[_,$]/g, "");
  if (!t) return null;
  const n = Number(t);
  if (!Number.isFinite(n) || (int && !Number.isInteger(n))) {
    throw new Error(`${label} must be ${int ? "a whole number" : "a number"}`);
  }
  if (n < min || n > max) {
    throw new Error(max === Infinity ? `${label} must be at least ${min}` : `${label} must be between ${min} and ${max}`);
  }
  return n;
}

function fromDraft(d: Draft): { model: Partial<Model>; params: ModelParams } {
  return {
    model: {
      displayName: d.displayName.trim(),
      modelId: d.modelId.trim(),
      vision: d.vision,
      contextWindow: num(d.contextWindow, "Context window", true, 1),
    },
    params: {
      maxOutputTokens: num(d.maxOutputTokens, "Max output tokens", true, 1),
      temperature: num(d.temperature, "Temperature", false, 0, 2),
      topP: num(d.topP, "Top P", false, 0, 1),
      reasoningEffort: d.reasoningEffort || null,
      reasoningSummary: d.reasoningSummary || null,
      autoCompactPercent: num(d.autoCompactPercent, "Auto-compact threshold", true, 10, 99),
      pricing: {
        input: num(d.priceInput, "Input price"),
        cachedInput: num(d.priceCached, "Cached input price"),
        cacheWrite: num(d.priceCacheWrite, "Cache write price"),
        output: num(d.priceOutput, "Output price"),
      },
    },
  };
}

function Section({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-3">
      <div>
        <div className="text-sm font-medium">{title}</div>
        {hint && <div className="mt-0.5 text-xs text-muted">{hint}</div>}
      </div>
      <div className="grid grid-cols-2 gap-x-4 gap-y-3">{children}</div>
    </section>
  );
}

function Field({
  label,
  hint,
  wide,
  children,
}: {
  label: string;
  hint?: string;
  wide?: boolean;
  children: ReactNode;
}) {
  return (
    <label className={`flex flex-col gap-1 text-xs text-muted ${wide ? "col-span-2" : ""}`}>
      {label}
      {children}
      {hint && <span className="text-[12px] text-muted/80">{hint}</span>}
    </label>
  );
}

export function ModelPage({
  model,
  provider,
  onDeleted,
}: {
  model: Model;
  provider: Provider | undefined;
  onDeleted: () => void;
}) {
  const refreshCatalog = useStore((s) => s.refreshCatalog);
  const defaultModelRef = useStore((s) => s.defaultModelRef);
  const setDefaultModel = useStore((s) => s.setDefaultModel);
  const askConfirm = useStore((s) => s.askConfirm);
  const [draft, setDraft] = useState<Draft>(() => toDraft(model));
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setDraft(toDraft(model));
    setError(null);
    setSaved(false);
  }, [model]);

  const dirty = JSON.stringify(draft) !== JSON.stringify(toDraft(model));
  const set = <K extends keyof Draft>(k: K) => (v: Draft[K]) => {
    setDraft((d) => ({ ...d, [k]: v }));
    setSaved(false);
  };
  const text = (k: keyof Draft, placeholder: string, mono = true) => (
    <input
      className={`${input} ${mono ? "font-mono" : ""}`}
      value={draft[k] as string}
      placeholder={placeholder}
      inputMode="decimal"
      onChange={(e) => set(k)(e.target.value as never)}
    />
  );

  // A worked example so prices per million are easy to sanity-check.
  const example = useMemo(() => {
    try {
      const { params } = fromDraft(draft);
      const c = estimateCost(
        { input: 10_000, cachedRead: 0, cacheWrite: 0, output: 1_000, reasoning: 0, costUsd: null, apiMs: 0 },
        params.pricing,
      );
      return c == null ? null : `10k input + 1k output ≈ $${c.toFixed(4)}`;
    } catch {
      return null;
    }
  }, [draft]);

  const save = async () => {
    setError(null);
    let parsed;
    try {
      parsed = fromDraft(draft);
    } catch (e) {
      setError(errorText(e));
      return;
    }
    if (!parsed.model.modelId) {
      setError("Model ID is required");
      return;
    }
    setBusy(true);
    try {
      await api.saveModel({
        id: model.id,
        providerId: model.providerId,
        modelId: parsed.model.modelId,
        displayName: parsed.model.displayName,
        vision: parsed.model.vision,
        contextWindow: parsed.model.contextWindow ?? null,
        params: parsed.params,
      });
      await refreshCatalog();
      setSaved(true);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const isResponses = provider?.kind === "openai_responses";

  return (
    <div className="flex max-w-xl flex-col gap-6">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="truncate text-base font-semibold">{model.displayName}</div>
          <div className="truncate font-mono text-xs text-muted">
            {provider?.name} · {model.modelId}
          </div>
        </div>
        {defaultModelRef === model.id ? (
          <span className="rounded bg-accent/15 px-2 py-0.5 text-[12px] text-accent-ink">Default</span>
        ) : (
          <button
            className="rounded-md border border-border px-2 py-1 text-xs hover:border-accent"
            onClick={() => setDefaultModel(model.id)}
          >
            Make default
          </button>
        )}
        <button
          className="rounded-md px-2 py-1 text-xs text-muted hover:text-danger"
          onClick={async () => {
            const { ok } = await askConfirm({
              title: "Delete model?",
              body: `"${model.displayName}" will be removed from ${provider?.name ?? "this provider"}. Conversations that used it keep their history.`,
              confirmLabel: "Delete",
              danger: true,
            });
            if (!ok) return;
            await api.deleteModel(model.id);
            await refreshCatalog();
            onDeleted();
          }}
        >
          Delete
        </button>
      </div>

      <Section title="General">
        <Field label="Display name">{text("displayName", model.modelId, false)}</Field>
        <Field label="Model ID" hint="Sent to the API as `model`">
          {text("modelId", "model-id")}
        </Field>
        <label className="col-span-2 flex items-center gap-2 text-xs">
          <input type="checkbox" checked={draft.vision} onChange={(e) => set("vision")(e.target.checked)} />
          Accepts image input
        </label>
      </Section>

      <Section title="Context">
        <Field label="Context window (tokens)" hint="Drives the context meter and agent compaction">
          {text("contextWindow", "e.g. 128000")}
        </Field>
        <Field label="Auto-compact at (%)" hint="Code agents on Patchbay models (Patchbay, Grok Build). Default 85">
          {text("autoCompactPercent", "85")}
        </Field>
      </Section>

      <Section title="Generation" hint="Leave blank to use the provider's default.">
        <Field label="Max output tokens" hint={provider?.kind === "anthropic" ? "Chat defaults to 8192, or 32000 and up for current Claude models" : undefined}>
          {text("maxOutputTokens", "provider default")}
        </Field>
        <Field label="Temperature" hint="0 – 2">
          {text("temperature", "provider default")}
        </Field>
        <Field label="Top P" hint="0 – 1">
          {text("topP", "provider default")}
        </Field>
        <Field label="Reasoning effort" hint="For reasoning models">
          <select
            className={input}
            value={draft.reasoningEffort}
            onChange={(e) => set("reasoningEffort")(e.target.value as Draft["reasoningEffort"])}
          >
            <option value="">Provider default</option>
            {EFFORTS.map((e) => (
              <option key={e} value={e}>
                {e}
              </option>
            ))}
          </select>
        </Field>
        {isResponses && (
          <Field label="Reasoning summary" hint="Responses API">
            <select
              className={input}
              value={draft.reasoningSummary}
              onChange={(e) => set("reasoningSummary")(e.target.value as Draft["reasoningSummary"])}
            >
              <option value="">Provider default</option>
              {SUMMARIES.map((e) => (
                <option key={e} value={e}>
                  {e}
                </option>
              ))}
            </select>
          </Field>
        )}
      </Section>

      <Section
        title="Pricing"
        hint="Optional, in USD per 1M tokens. Used to estimate cost under the chat input when the provider doesn't report it."
      >
        <Field label="Input">{text("priceInput", "$ / 1M")}</Field>
        <Field label="Output">{text("priceOutput", "$ / 1M")}</Field>
        <Field label="Cached input (read)" hint="Blank = input price">
          {text("priceCached", "$ / 1M")}
        </Field>
        <Field label="Cache write" hint="Blank = input price">
          {text("priceCacheWrite", "$ / 1M")}
        </Field>
        {example && <div className="col-span-2 text-[12px] text-muted">{example}</div>}
      </Section>

      <div className="sticky bottom-0 -mx-5 flex items-center gap-3 border-t border-border bg-bg px-5 py-3">
        <button
          className="rounded-md bg-accent px-3 py-1.5 text-xs font-medium text-accent-fg disabled:opacity-40"
          disabled={!dirty || busy}
          onClick={save}
        >
          {busy ? "Saving…" : "Save"}
        </button>
        <button
          className="rounded-md px-3 py-1.5 text-xs text-muted hover:text-fg disabled:opacity-40"
          disabled={!dirty || busy}
          onClick={() => {
            setDraft(toDraft(model));
            setError(null);
          }}
        >
          Revert
        </button>
        {error ? (
          <span className="text-xs text-danger" role="alert">
            {error}
          </span>
        ) : saved && !dirty ? (
          <span className="text-xs text-ok">Saved. Running Code sessions use it after Restart.</span>
        ) : null}
      </div>
    </div>
  );
}
