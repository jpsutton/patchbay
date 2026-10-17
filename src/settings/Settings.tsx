import { useEffect, useState } from "react";
import { api } from "../lib/ipc";
import { useStore } from "../lib/store";
import { ProviderPage } from "./ProviderPage";
import { ModelPage } from "./ModelPage";
import { PREF_SECTIONS, PreferencesPage, type PrefSection } from "./PreferencesPage";

type Selection =
  | { kind: "section"; id: PrefSection }
  | { kind: "provider"; id: string }
  | { kind: "model"; id: string }
  | { kind: "new" };

export function Settings() {
  const providers = useStore((s) => s.providers);
  const models = useStore((s) => s.models);
  const defaultModelRef = useStore((s) => s.defaultModelRef);
  const refreshCatalog = useStore((s) => s.refreshCatalog);
  const setSettingsOpen = useStore((s) => s.setSettingsOpen);
  const askConfirm = useStore((s) => s.askConfirm);
  // With no models yet, land where setup starts.
  const [sel, setSel] = useState<Selection>(() => {
    const st = useStore.getState();
    const wanted = PREF_SECTIONS.find((x) => x.id === st.settingsSection);
    if (wanted) {
      useStore.setState({ settingsSection: null });
      return { kind: "section", id: wanted.id };
    }
    return models.length > 0
      ? { kind: "section", id: "general" }
      : providers[0]
        ? { kind: "provider", id: providers[0].id }
        : { kind: "new" };
  });
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !useStore.getState().confirm) setSettingsOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [setSettingsOpen]);

  const selModel = sel.kind === "model" ? models.find((m) => m.id === sel.id) : undefined;
  const selProvider =
    sel.kind === "provider"
      ? providers.find((p) => p.id === sel.id)
      : selModel
        ? providers.find((p) => p.id === selModel.providerId)
        : undefined;

  // Fall back when the selected item disappears (deleted elsewhere).
  useEffect(() => {
    if ((sel.kind === "provider" && !selProvider) || (sel.kind === "model" && !selModel)) {
      setSel(providers[0] ? { kind: "provider", id: providers[0].id } : { kind: "new" });
    }
  }, [sel, selProvider, selModel, providers]);

  const toggle = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const title =
    sel.kind === "section"
      ? PREF_SECTIONS.find((x) => x.id === sel.id)?.label
      : sel.kind === "new"
        ? "New provider"
        : sel.kind === "model"
          ? "Model"
          : (selProvider?.name ?? "Provider");

  return (
    <div
      className="fixed inset-0 z-40 flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]"
      onClick={() => setSettingsOpen(false)}
    >
      <div
        className="flex h-[min(720px,92vh)] w-[min(1000px,95vw)] overflow-hidden rounded-2xl border border-border bg-bg shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <aside className="flex w-60 shrink-0 flex-col border-r border-border bg-panel">
          <nav className="flex-1 overflow-y-auto pb-2" aria-label="Settings sections">
            <div className="px-4 pb-1.5 pt-4 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">
              Preferences
            </div>
            {PREF_SECTIONS.map((x) => (
              <button
                key={x.id}
                className={`mx-2 flex w-[calc(100%-1rem)] items-center rounded-md px-3 py-1.5 text-left text-xs ${
                  sel.kind === "section" && sel.id === x.id ? "bg-panel-2 text-fg" : "text-fg/85 hover:bg-panel-2/60"
                }`}
                onClick={() => setSel({ kind: "section", id: x.id })}
              >
                {x.label}
              </button>
            ))}
            <div className="px-4 pb-1.5 pt-5 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">
              Providers & models
            </div>
            {providers.map((p) => {
              const ms = models.filter((m) => m.providerId === p.id);
              const open = !collapsed.has(p.id);
              const active = sel.kind === "provider" && sel.id === p.id;
              return (
                <div key={p.id} className="mb-1">
                  <div
                    className={`mx-2 flex items-center gap-1 rounded-md pr-1 ${active ? "bg-panel-2" : "hover:bg-panel-2/60"}`}
                  >
                    <button
                      className="flex h-6 w-5 shrink-0 items-center justify-center text-[11px] text-muted hover:text-fg"
                      onClick={() => toggle(p.id)}
                      aria-label={open ? `Collapse ${p.name}` : `Expand ${p.name}`}
                      aria-expanded={open}
                    >
                      {open ? "⌄" : "›"}
                    </button>
                    <button
                      className="flex min-w-0 flex-1 flex-col py-1.5 text-left"
                      onClick={() => setSel({ kind: "provider", id: p.id })}
                    >
                      <span className="truncate text-xs font-medium text-fg">{p.name}</span>
                      <span className="truncate text-[11px] text-muted">{p.baseUrl}</span>
                    </button>
                    <span className="shrink-0 text-[11px] text-muted">{ms.length}</span>
                  </div>
                  {open &&
                    ms.map((m) => {
                      const mActive = sel.kind === "model" && sel.id === m.id;
                      return (
                        <button
                          key={m.id}
                          className={`mx-2 flex w-[calc(100%-1rem)] items-center gap-2 rounded-md py-1 pl-7 pr-2 text-left text-xs ${
                            mActive ? "bg-panel-2 text-fg" : "text-fg/85 hover:bg-panel-2/60"
                          }`}
                          onClick={() => setSel({ kind: "model", id: m.id })}
                          title={m.modelId}
                        >
                          <span className="min-w-0 flex-1 truncate">{m.displayName}</span>
                          {m.id === defaultModelRef && (
                            <span className="shrink-0 font-mono text-[10px] text-faint" title="Default model">default</span>
                          )}
                        </button>
                      );
                    })}
                  {open && ms.length === 0 && (
                    <button
                      className="mx-2 w-[calc(100%-1rem)] py-1 pl-7 text-left text-[12px] text-muted hover:text-fg"
                      onClick={() => setSel({ kind: "provider", id: p.id })}
                    >
                      No models — add some
                    </button>
                  )}
                </div>
              );
            })}
          </nav>
          <button
            className={`m-3 rounded-md border border-dashed border-border px-3 py-1.5 text-xs hover:border-accent ${
              sel.kind === "new" ? "border-accent text-accent-ink" : "text-muted"
            }`}
            onClick={() => setSel({ kind: "new" })}
          >
            + Add provider
          </button>
        </aside>
        <main className="flex min-w-0 flex-1 flex-col">
          <div className="flex items-center justify-between border-b border-border px-5 py-3">
            <div className="text-sm font-semibold">{title}</div>
            <div className="flex items-center gap-3">
              {sel.kind === "provider" && selProvider && (
                <button
                  className="text-xs text-muted hover:text-danger"
                  onClick={async () => {
                    const { ok } = await askConfirm({
                      title: "Delete provider?",
                      body: `"${selProvider.name}", its models and its stored API key will be removed.`,
                      confirmLabel: "Delete",
                      danger: true,
                    });
                    if (!ok) return;
                    await api.deleteProvider(selProvider.id);
                    await refreshCatalog();
                    setSel({ kind: "new" });
                  }}
                >
                  Delete provider
                </button>
              )}
              <button
                className="text-lg leading-none text-muted hover:text-fg"
                onClick={() => setSettingsOpen(false)}
                aria-label="Close"
              >
                ×
              </button>
            </div>
          </div>
          <div className="flex-1 overflow-y-auto px-5 pt-4">
            {sel.kind === "section" ? (
              <div className="pb-4">
                <PreferencesPage section={sel.id} />
              </div>
            ) : sel.kind === "model" && selModel ? (
              <ModelPage
                model={selModel}
                provider={selProvider}
                onDeleted={() => setSel({ kind: "provider", id: selModel.providerId })}
              />
            ) : (
              <div className="pb-4">
                <ProviderPage
                  provider={sel.kind === "provider" ? (selProvider ?? null) : null}
                  onSaved={async (p) => {
                    await refreshCatalog();
                    setSel({ kind: "provider", id: p.id });
                  }}
                  onOpenModel={(id) => setSel({ kind: "model", id })}
                />
              </div>
            )}
          </div>
        </main>
      </div>
    </div>
  );
}
