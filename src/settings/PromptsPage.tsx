import { useEffect, useState } from "react";
import { api, errorText, type Prompt } from "../lib/ipc";
import { useStore } from "../lib/store";
import { input } from "./ProviderPage";

/** Saved system prompts ("styles"), applied per conversation from the System prompt panel. */
export function PromptsPage() {
  const setToast = useStore((s) => s.setToast);
  const askConfirm = useStore((s) => s.askConfirm);
  const [prompts, setPrompts] = useState<Prompt[]>([]);
  const [sel, setSel] = useState<string | "new" | null>(null);
  const [draft, setDraft] = useState({ name: "", content: "" });

  const reload = async (select?: string) => {
    const ps = await api.listPrompts();
    setPrompts(ps);
    if (select) setSel(select);
  };
  useEffect(() => {
    reload().catch((e) => setToast(errorText(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  useEffect(() => {
    const p = prompts.find((x) => x.id === sel);
    setDraft(p ? { name: p.name, content: p.content } : { name: "", content: "" });
  }, [sel, prompts]);

  const current = prompts.find((x) => x.id === sel);
  const dirty = sel === "new" || (current && (current.name !== draft.name || current.content !== draft.content));

  return (
    <div className="flex max-w-2xl gap-4 pb-4">
      <div className="flex w-48 shrink-0 flex-col gap-0.5">
        {prompts.map((p) => (
          <button
            key={p.id}
            className={`truncate rounded-md px-2 py-1.5 text-left text-xs ${sel === p.id ? "bg-panel-2" : "hover:bg-panel-2/60"}`}
            onClick={() => setSel(p.id)}
          >
            {p.name}
          </button>
        ))}
        {prompts.length === 0 && <div className="px-2 py-1.5 text-xs text-muted">No saved prompts</div>}
        <button
          className="mt-2 rounded-md border border-dashed border-border px-2 py-1.5 text-left text-xs text-muted hover:border-accent hover:text-fg"
          onClick={() => setSel("new")}
        >
          + New prompt
        </button>
      </div>
      <div className="min-w-0 flex-1">
        {sel ? (
          <div className="flex flex-col gap-2">
            <input
              className={input}
              placeholder="Name, e.g. Concise reviewer"
              value={draft.name}
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
            <textarea
              className={`${input} h-64 resize-y font-mono text-xs`}
              placeholder="Instructions to use as a conversation's system prompt"
              value={draft.content}
              onChange={(e) => setDraft({ ...draft, content: e.target.value })}
            />
            <div className="flex items-center gap-2">
              <button
                className="rounded-md bg-accent px-3 py-1.5 text-xs font-medium text-accent-fg disabled:opacity-40"
                disabled={!dirty || !draft.name.trim()}
                onClick={async () => {
                  try {
                    const p = await api.savePrompt({ id: sel === "new" ? undefined : sel, ...draft });
                    await reload(p.id);
                  } catch (e) {
                    setToast(errorText(e));
                  }
                }}
              >
                Save
              </button>
              {current && (
                <button
                  className="text-xs text-muted hover:text-danger"
                  onClick={async () => {
                    const { ok } = await askConfirm({
                      title: "Delete prompt?",
                      body: `"${current.name}" will be removed. Conversations that use its text keep it.`,
                      confirmLabel: "Delete",
                      danger: true,
                    });
                    if (!ok) return;
                    await api.deletePrompt(current.id);
                    setSel(null);
                    reload();
                  }}
                >
                  Delete
                </button>
              )}
            </div>
          </div>
        ) : (
          <p className="text-xs text-muted">
            Saved prompts are reusable system prompts. Pick one in a conversation's System prompt panel, or save the
            current one from there.
          </p>
        )}
      </div>
    </div>
  );
}
