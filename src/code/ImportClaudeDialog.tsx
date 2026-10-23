import { useEffect, useMemo, useState } from "react";
import { api, errorText, type AgentId, type AgentInfo, type ClaudeSession } from "../lib/ipc";
import { useStore } from "../lib/store";
import { XIcon } from "../components/icons";

const labelCls = "px-5 pb-1.5 pt-3 font-mono text-[11px] uppercase tracking-[0.06em] text-faint";

function tilde(path: string): string {
  return path.replace(/^\/home\/[^/]+/, "~").replace(/^\/Users\/[^/]+/, "~");
}

function ago(ms: number): string {
  const days = Math.floor((Date.now() - ms) / 86_400_000);
  if (days < 1) return "today";
  if (days === 1) return "yesterday";
  if (days < 60) return `${days} days ago`;
  return new Date(ms).toLocaleDateString();
}

const size = (bytes: number) => (bytes >= 1_000_000 ? `${(bytes / 1_000_000).toFixed(1)} MB` : `${Math.max(1, Math.round(bytes / 1000))} KB`);

/**
 * Bring sessions over from Claude Code. Each becomes a Code session with its conversation as
 * earlier history; the agent chosen here gets that conversation with its first message.
 */
export function ImportClaudeDialog() {
  const isOpen = useStore((s) => s.importClaudeOpen);
  return isOpen ? <Dialog /> : null;
}

function Dialog() {
  const setOpen = useStore((s) => s.setImportClaudeOpen);
  const [found, setFound] = useState<ClaudeSession[] | null>(null);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [agent, setAgent] = useState<AgentId | null>(null);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const close = () => setOpen(false);

  useEffect(() => {
    let live = true;
    api.listClaudeSessions().then(
      (l) => live && setFound(l),
      (e) => live && setErr(errorText(e)),
    );
    api.newSessionOptions(null).then((o) => {
      if (!live) return;
      const usable = o.agents.filter((a) => a.available);
      setAgents(usable);
      setAgent(usable.some((a) => a.id === o.defaultAgent) ? o.defaultAgent : (usable[0]?.id ?? null));
    }, () => {});
    return () => {
      live = false;
    };
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        close();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, []);

  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (found ?? []).filter((c) => !q || `${c.title} ${c.preview} ${c.cwd}`.toLowerCase().includes(q));
  }, [found, query]);

  const toggle = (id: string) =>
    setPicked((p) => {
      const n = new Set(p);
      if (!n.delete(id)) n.add(id);
      return n;
    });

  const run = async (ids: string[]) => {
    if (ids.length === 0) return;
    setBusy(true);
    setErr(null);
    try {
      const made = await api.importClaudeSessions(ids, agent);
      const st = useStore.getState();
      await st.refreshCodeSessions();
      st.setMode("code");
      if (made[0]) st.selectCodeSession(made[0].id);
      close();
    } catch (e) {
      setErr(errorText(e));
      setBusy(false);
    }
  };

  const n = picked.size;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]" onClick={close}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="import-claude-title"
        className="flex max-h-[min(700px,90vh)] w-[min(640px,94vw)] flex-col rounded-2xl border border-border bg-raised shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-1 px-5 pb-1 pt-4">
          <div id="import-claude-title" className="min-w-0 flex-1 truncate text-sm font-semibold">
            Import from Claude Code
          </div>
          <button
            className="flex h-7 w-7 items-center justify-center rounded-lg text-muted hover:bg-panel-2 hover:text-fg"
            aria-label="Close"
            onClick={close}
          >
            <XIcon size={15} />
          </button>
        </div>

        <div className="px-5 pt-2">
          <input
            className="h-9 w-full rounded-lg border border-border bg-panel px-2.5 text-sm text-fg outline-none focus:border-accent/60"
            placeholder="Search by title, prompt or folder"
            aria-label="Search Claude Code sessions"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            autoFocus
          />
        </div>

        <div className="mt-2 min-h-[200px] flex-1 overflow-y-auto border-y border-line py-1">
          {!found && !err && <div className="px-5 py-4 text-xs text-muted">Looking in Claude Code's folder…</div>}
          {found && shown.length === 0 && (
            <div className="px-5 py-4 text-xs text-muted">
              {found.length === 0 ? "No Claude Code sessions found on this computer." : "Nothing matches."}
            </div>
          )}
          {shown.map((c) => (
            <label
              key={c.id}
              className={`flex cursor-pointer items-start gap-3 px-5 py-2 hover:bg-panel-2 ${c.imported ? "opacity-60" : ""}`}
            >
              <input
                type="checkbox"
                className="mt-1"
                checked={picked.has(c.id)}
                disabled={busy}
                onChange={() => toggle(c.id)}
                aria-label={`Import ${c.title}`}
              />
              <span className="min-w-0 flex-1">
                <span className="flex items-baseline gap-2">
                  <span className="min-w-0 flex-1 truncate text-sm font-medium">{c.title}</span>
                  <span className="shrink-0 text-[11px] text-faint">
                    {ago(c.modified)} · {size(c.size)}
                  </span>
                </span>
                <span className="block truncate text-xs text-muted">{c.preview}</span>
                <span className="block truncate font-mono text-[11px] text-faint">
                  {tilde(c.cwd)}
                  {!c.cwdExists && " (folder is gone; opens in the nearest one that exists)"}
                  {c.imported && " · imported already: opens the copy"}
                </span>
              </span>
            </label>
          ))}
        </div>

        <div className={labelCls}>Continue with</div>
        <div className="px-5 pb-1">
          {agents.length === 0 ? (
            <div className="text-xs text-muted">No coding agent is installed.</div>
          ) : (
            <select
              className="h-9 w-full rounded-lg border border-border bg-panel px-2.5 text-sm text-fg outline-none focus:border-accent/60"
              value={agent ?? ""}
              aria-label="Coding agent"
              onChange={(e) => setAgent(e.target.value as AgentId)}
            >
              {agents.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.label}
                </option>
              ))}
            </select>
          )}
        </div>

        {err && <div className="px-5 pt-2 text-xs text-danger">{err}</div>}
        <div className="flex items-center justify-end gap-2 px-5 py-3">
          <button className="h-[34px] rounded-lg px-3.5 text-xs text-muted hover:bg-panel-2 hover:text-fg" onClick={close}>
            Cancel
          </button>
          <button
            disabled={n === 0 || busy || !agent}
            className="h-[34px] rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg disabled:opacity-50"
            onClick={() => run([...picked])}
          >
            {busy ? "Importing…" : n > 1 ? `Import ${n} sessions` : "Import session"}
          </button>
        </div>
      </div>
    </div>
  );
}
