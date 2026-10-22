import { useEffect, useState } from "react";
import { api, errorText, type AuthMode, type DiscoveredModel, type Provider, type ProviderKind } from "../lib/ipc";
import { useStore } from "../lib/store";

const PRESETS: { label: string; kind: ProviderKind; baseUrl: string }[] = [
  { label: "OpenAI", kind: "openai_chat", baseUrl: "https://api.openai.com/v1" },
  { label: "Anthropic", kind: "anthropic", baseUrl: "https://api.anthropic.com/v1" },
  { label: "Google Gemini", kind: "gemini", baseUrl: "https://generativelanguage.googleapis.com/v1beta" },
  { label: "OpenRouter", kind: "openai_chat", baseUrl: "https://openrouter.ai/api/v1" },
  { label: "Ollama (local)", kind: "openai_chat", baseUrl: "http://localhost:11434/v1" },
  { label: "LM Studio (local)", kind: "openai_chat", baseUrl: "http://localhost:1234/v1" },
];

const KIND_LABEL: Record<ProviderKind, string> = {
  openai_chat: "OpenAI-compatible · Chat Completions",
  openai_responses: "OpenAI · Responses API",
  anthropic: "Anthropic-compatible · Messages",
  gemini: "Google · Gemini API",
};

export const input =
  "w-full rounded-md border border-border bg-bg px-2 py-1.5 text-sm outline-none focus:border-accent";

function ProviderEditor({ provider, onSaved }: { provider: Provider | null; onSaved: (p: Provider) => void }) {
  const [name, setName] = useState(provider?.name ?? "");
  const [kind, setKind] = useState<ProviderKind>(provider?.kind ?? "openai_chat");
  const [baseUrl, setBaseUrl] = useState(provider?.baseUrl ?? "");
  const [key, setKey] = useState("");
  const [authMode, setAuthMode] = useState<AuthMode>(provider?.authMode ?? "api_key");
  const [busy, setBusy] = useState(false);
  const setToast = useStore((s) => s.setToast);
  const useClaudeLogin = kind === "anthropic" && authMode === "claude_login";

  useEffect(() => {
    setName(provider?.name ?? "");
    setKind(provider?.kind ?? "openai_chat");
    setBaseUrl(provider?.baseUrl ?? "");
    setAuthMode(provider?.authMode ?? "api_key");
    setKey("");
  }, [provider?.id]);

  return (
    <div className="flex flex-col gap-3">
      {!provider && (
        <div className="flex flex-wrap gap-2">
          {PRESETS.map((p) => (
            <button
              key={p.label}
              className="rounded-md border border-border px-2 py-1 text-xs hover:border-accent"
              onClick={() => {
                setName(p.label);
                setKind(p.kind);
                setBaseUrl(p.baseUrl);
                setAuthMode("api_key");
              }}
            >
              {p.label}
            </button>
          ))}
        </div>
      )}
      <label className="flex flex-col gap-1 text-xs text-muted">
        Name
        <input className={input} value={name} onChange={(e) => setName(e.target.value)} placeholder="My provider" />
      </label>
      <label className="flex flex-col gap-1 text-xs text-muted">
        API type
        <select
          className={input}
          value={kind}
          onChange={(e) => {
            const next = e.target.value as ProviderKind;
            setKind(next);
            if (next !== "anthropic") setAuthMode("api_key");
          }}
        >
          {(Object.keys(KIND_LABEL) as ProviderKind[]).map((k) => (
            <option key={k} value={k}>
              {KIND_LABEL[k]}
            </option>
          ))}
        </select>
      </label>
      <label className="flex flex-col gap-1 text-xs text-muted">
        Base URL (with its version path, e.g. /v1)
        <input
          className={`${input} font-mono`}
          value={baseUrl}
          onChange={(e) => setBaseUrl(e.target.value)}
          placeholder="https://api.example.com/v1"
        />
      </label>
      {kind === "anthropic" && (
        <label className="flex items-start gap-2 text-xs text-muted">
          <input
            type="checkbox"
            className="mt-0.5"
            checked={useClaudeLogin}
            onChange={(e) => setAuthMode(e.target.checked ? "claude_login" : "api_key")}
          />
          <span>
            Use Claude Code login (subscription)
            <span className="mt-0.5 block text-muted">
              Reuses the login from claude on this machine. Uses your Max/Pro usage, not an API key.
            </span>
          </span>
        </label>
      )}
      {useClaudeLogin ? (
        <p className="text-xs text-ok">Signed in via Claude Code on this machine — no API key stored.</p>
      ) : (
        <label className="flex flex-col gap-1 text-xs text-muted">
          API key {provider?.hasKey && <span className="text-ok">(stored — leave blank to keep)</span>}
          <input
            className={`${input} font-mono`}
            type="password"
            value={key}
            onChange={(e) => setKey(e.target.value)}
            placeholder={provider?.hasKey ? "••••••••" : "optional for local servers"}
            autoComplete="off"
          />
        </label>
      )}
      <div className="flex gap-2">
        <button
          className="rounded-md bg-accent px-3 py-1.5 text-xs font-medium text-accent-fg disabled:opacity-50"
          disabled={busy || !name.trim() || !baseUrl.trim()}
          onClick={async () => {
            setBusy(true);
            try {
              const p = await api.saveProvider({
                id: provider?.id,
                name,
                kind,
                baseUrl,
                authMode: kind === "anthropic" ? authMode : "api_key",
                apiKey: useClaudeLogin ? "" : key ? key : undefined,
              });
              setKey("");
              onSaved(p);
            } catch (e) {
              setToast(errorText(e));
            } finally {
              setBusy(false);
            }
          }}
        >
          {provider ? "Save" : "Add provider"}
        </button>
        {provider?.hasKey && !useClaudeLogin && (
          <button
            className="rounded-md px-3 py-1.5 text-xs text-muted hover:text-danger"
            onClick={async () => {
              const p = await api.saveProvider({
                id: provider.id,
                name,
                kind,
                baseUrl,
                authMode: "api_key",
                apiKey: "",
              });
              onSaved(p);
            }}
          >
            Remove key
          </button>
        )}
      </div>
    </div>
  );
}


/** Fetch the provider's model list, or add a model by id. */
function AddModels({ provider, onAdded }: { provider: Provider; onAdded: (modelRef: string) => void }) {
  const models = useStore((s) => s.models).filter((m) => m.providerId === provider.id);
  const refreshCatalog = useStore((s) => s.refreshCatalog);
  const defaultModelRef = useStore((s) => s.defaultModelRef);
  const setDefaultModel = useStore((s) => s.setDefaultModel);
  const setToast = useStore((s) => s.setToast);
  const [found, setFound] = useState<DiscoveredModel[] | null>(null);
  const [filter, setFilter] = useState("");
  const [manual, setManual] = useState("");
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    setFound(null);
    setFilter("");
  }, [provider.id]);

  const add = async (modelId: string, displayName?: string, contextWindow?: number | null, open = false) => {
    try {
      const m = await api.saveModel({ providerId: provider.id, modelId, displayName, contextWindow });
      await refreshCatalog();
      if (!defaultModelRef) await setDefaultModel(m.id);
      if (open) onAdded(m.id);
    } catch (e) {
      setToast(errorText(e));
    }
  };

  const have = new Set(models.map((m) => m.modelId));
  const shown = (found ?? []).filter((m) => m.id.toLowerCase().includes(filter.toLowerCase()));

  return (
    <div className="flex flex-col gap-3">
      <div className="text-sm font-medium">Add models</div>
      <div className="flex gap-2">
        <button
          className="rounded-md border border-border px-3 py-1.5 text-xs hover:border-accent disabled:opacity-50"
          disabled={loading}
          onClick={async () => {
            setLoading(true);
            try {
              setFound(await api.discoverModels(provider.id));
            } catch (e) {
              setToast(errorText(e));
            } finally {
              setLoading(false);
            }
          }}
        >
          {loading ? "Fetching…" : "Fetch available models"}
        </button>
        <input
          className={`${input} flex-1 font-mono text-xs`}
          placeholder="or type a model id and press Enter"
          value={manual}
          onChange={(e) => setManual(e.target.value)}
          onKeyDown={async (e) => {
            if (e.key === "Enter" && manual.trim()) {
              await add(manual.trim(), undefined, undefined, true);
              setManual("");
            }
          }}
        />
      </div>
      {found && (
        <div className="rounded-md border border-border">
          <input
            className="w-full border-b border-border bg-transparent px-2 py-1.5 text-xs outline-none"
            placeholder={`Filter ${found.length} models`}
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
          <div className="max-h-64 overflow-y-auto">
            {shown.map((m) => (
              <div key={m.id} className="flex items-center gap-2 px-2 py-1 text-xs hover:bg-panel-2">
                <span className="min-w-0 flex-1 truncate font-mono">{m.id}</span>
                {m.contextWindow && <span className="text-muted">{Math.round(m.contextWindow / 1000)}k ctx</span>}
                {have.has(m.id) ? (
                  <span className="text-muted">added</span>
                ) : (
                  <button className="text-accent-ink" onClick={() => add(m.id, m.displayName, m.contextWindow)}>
                    add
                  </button>
                )}
              </div>
            ))}
            {shown.length === 0 && <div className="px-2 py-2 text-xs text-muted">No matches</div>}
          </div>
        </div>
      )}
      <p className="text-xs text-muted">
        Added models appear under this provider in the sidebar. Select one to set its parameters and prices.
      </p>
    </div>
  );
}

export function ProviderPage({
  provider,
  onSaved,
  onOpenModel,
}: {
  provider: Provider | null;
  onSaved: (p: Provider) => void;
  onOpenModel: (modelRef: string) => void;
}) {
  return (
    <div className="flex max-w-xl flex-col gap-6">
      <ProviderEditor provider={provider} onSaved={onSaved} />
      {provider && <AddModels provider={provider} onAdded={onOpenModel} />}
      <p className="text-xs text-muted">
        Keys are stored in the OS keychain when available. Chat and Patchbay's coding agent call the
        provider directly from this app; other agents receive keys only through their process environment.
      </p>
    </div>
  );
}
