import { useEffect, useState } from "react";
import { api, errorText, type Conversation, type Prompt } from "../lib/ipc";
import { useStore } from "../lib/store";

/** Per-conversation system prompt, with the saved prompt library to pick from or add to. */
export function SystemPromptPanel({ conv }: { conv: Conversation }) {
  const refreshConversations = useStore((s) => s.refreshConversations);
  const setToast = useStore((s) => s.setToast);
  const [draft, setDraft] = useState(conv.systemPrompt ?? "");
  const [prompts, setPrompts] = useState<Prompt[]>([]);
  const [naming, setNaming] = useState<string | null>(null);

  useEffect(() => setDraft(conv.systemPrompt ?? ""), [conv.id, conv.systemPrompt]);
  useEffect(() => {
    api.listPrompts().then(setPrompts).catch(() => {});
  }, []);

  const commit = async (text: string) => {
    if (text === (conv.systemPrompt ?? "")) return;
    await api.updateConversation(conv.id, { systemPrompt: text });
    refreshConversations();
  };

  const saveAs = async (name: string) => {
    try {
      const p = await api.savePrompt({ name, content: draft });
      setPrompts((ps) => [...ps.filter((x) => x.id !== p.id), p].sort((a, b) => a.name.localeCompare(b.name)));
      setToast(`Saved prompt "${p.name}"`);
      setNaming(null);
    } catch (e) {
      setToast(errorText(e));
    }
  };

  return (
    <div className="border-b border-border bg-panel px-4 py-3">
      <textarea
        className="h-24 w-full resize-y rounded-md border border-border bg-bg p-2 text-xs outline-none focus:border-accent"
        placeholder="Instructions applied to every turn of this conversation"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => commit(draft)}
      />
      <div className="mt-2 flex items-center gap-2 text-xs">
        <select
          className="max-w-56 rounded-md border border-border bg-bg px-2 py-1 outline-none"
          value=""
          onChange={(e) => {
            const p = prompts.find((x) => x.id === e.target.value);
            if (!p) return;
            setDraft(p.content);
            commit(p.content);
          }}
          disabled={prompts.length === 0}
          aria-label="Use a saved prompt"
        >
          <option value="">{prompts.length ? "Use a saved prompt…" : "No saved prompts"}</option>
          {prompts.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name}
            </option>
          ))}
        </select>
        {naming === null ? (
          <button
            className="rounded-md px-2 py-1 text-muted hover:text-fg disabled:opacity-40"
            disabled={!draft.trim()}
            onClick={() => setNaming("")}
          >
            Save as prompt…
          </button>
        ) : (
          <form
            className="flex items-center gap-1"
            onSubmit={(e) => {
              e.preventDefault();
              if (naming.trim()) saveAs(naming.trim());
            }}
          >
            <input
              autoFocus
              className="w-44 rounded-md border border-border bg-bg px-2 py-1 outline-none focus:border-accent"
              placeholder="Prompt name"
              value={naming}
              onChange={(e) => setNaming(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.preventDefault();
                  setNaming(null);
                }
              }}
            />
            <button type="submit" className="rounded-md bg-accent px-2 py-1 font-medium text-accent-fg disabled:opacity-40" disabled={!naming.trim()}>
              Save
            </button>
          </form>
        )}
        <span className="flex-1" />
        <span className="text-muted">Manage prompts in Settings → Prompts</span>
      </div>
    </div>
  );
}
