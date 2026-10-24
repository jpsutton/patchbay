import { useEffect, useMemo, useRef, useState } from "react";
import { api, type SearchHit } from "../lib/ipc";
import { useStore } from "../lib/store";
import { fmtTime, matchesQuery } from "../lib/format";
import { focusSidebarSearch, keyLabel } from "../lib/shortcuts";
import { SearchIcon } from "./icons";

type Item = { key: string; section: string; label: string; detail?: string; snippet?: string; run: () => void };

/** "\u0001match\u0002" markers from FTS5 snippets -> highlighted spans. */
function Snippet({ text }: { text: string }) {
  const parts = text.split(/(\u0001[^\u0002]*\u0002)/);
  return (
    <>
      {parts.map((p, i) =>
        p.startsWith("\u0001") ? (
          <mark key={i} className="rounded-[3px] bg-accent/25 px-0.5 text-fg">
            {p.slice(1, -1)}
          </mark>
        ) : (
          <span key={i}>{p}</span>
        ),
      )}
    </>
  );
}

export function CommandPalette() {
  const open = useStore((s) => s.paletteOpen);
  const setOpen = useStore((s) => s.setPaletteOpen);
  const conversations = useStore((s) => s.conversations);
  const sessions = useStore((s) => s.codeSessions);
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [idx, setIdx] = useState(0);
  const list = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (open) {
      setQ("");
      setHits([]);
      setIdx(0);
    }
  }, [open]);

  useEffect(() => {
    if (!open || q.trim().length < 2) {
      setHits([]);
      return;
    }
    const t = setTimeout(() => {
      api.searchMessages(q, 30).then(setHits).catch(() => setHits([]));
    }, 120);
    return () => clearTimeout(t);
  }, [q, open]);

  const close = () => setOpen(false);

  const items = useMemo<Item[]>(() => {
    const st = useStore.getState();
    const actions: Item[] = [
      { key: "a-new", section: "Actions", label: "New chat", detail: keyLabel("Mod+N"), run: () => { st.setMode("chat"); st.selectConversation(null); } },
      { key: "a-folder", section: "Actions", label: "New Code session…", run: () => useStore.getState().setNewSessionOpen(true) },
      { key: "a-chat", section: "Actions", label: "Go to Chat", run: () => st.setMode("chat") },
      { key: "a-code", section: "Actions", label: "Go to Code", run: () => st.setMode("code") },
      { key: "a-filter", section: "Actions", label: "Filter the sidebar", detail: keyLabel("Mod+Shift+F"), run: () => { st.setSidebarCollapsed(false); focusSidebarSearch(); } },
      { key: "a-sidebar", section: "Actions", label: "Show or hide the sidebar", detail: keyLabel("Mod+B"), run: () => st.setSidebarCollapsed(!st.sidebarCollapsed) },
      { key: "a-settings", section: "Actions", label: "Settings", detail: keyLabel("Mod+,"), run: () => st.setSettingsOpen(true) },
      { key: "a-usage", section: "Actions", label: "Usage and spend", run: () => st.openSettingsAt("usage") },
      { key: "a-mcp", section: "Actions", label: "MCP servers", run: () => st.openSettingsAt("mcp") },
      { key: "a-prompts", section: "Actions", label: "Saved prompts", run: () => st.openSettingsAt("prompts") },
      { key: "a-keys", section: "Actions", label: "Keyboard shortcuts", detail: keyLabel("Mod+/"), run: () => st.setShortcutsOpen(true) },
      {
        key: "a-info",
        section: "Actions",
        label: "Code session info (IDs, agent, model)",
        // Read at run time: this list isn't rebuilt when the open session changes.
        run: () => {
          const now = useStore.getState();
          if (now.activeCodeSessionId) now.setSessionInfoId(now.activeCodeSessionId);
          else now.setToast("Open a Code session first.");
        },
      },
    ].filter((a) => matchesQuery(q, a.label));
    const chats: Item[] = conversations
      .filter((c) => matchesQuery(q, c.title))
      .slice(0, q ? 6 : 5)
      .map((c) => ({
        key: `c-${c.id}`,
        section: "Chats",
        label: c.title,
        detail: fmtTime(c.updatedAt),
        run: () => {
          st.setMode("chat");
          st.selectConversation(c.id);
        },
      }));
    const code: Item[] = sessions
      .filter((s) => matchesQuery(q, s.title, s.cwd))
      .slice(0, q ? 6 : 3)
      .map((s) => ({
        key: `s-${s.id}`,
        section: "Code sessions",
        label: s.title,
        detail: s.cwd,
        run: () => {
          st.setMode("code");
          st.selectCodeSession(s.id);
        },
      }));
    const messages: Item[] = hits.map((h) => ({
      key: `m-${h.messageId}`,
      section: "Messages",
      label: h.conversationTitle,
      detail: `${h.role === "user" ? "You" : "Reply"} · ${fmtTime(h.createdAt)}`,
      snippet: h.snippet,
      run: () => {
        st.setMode("chat");
        st.setPendingJump({ conversationId: h.conversationId, messageId: h.messageId });
        st.selectConversation(h.conversationId);
      },
    }));
    // With a query, content first; without one, actions first.
    return q ? [...chats, ...code, ...messages, ...actions] : [...actions, ...chats, ...code];
  }, [q, hits, conversations, sessions]);

  useEffect(() => setIdx(0), [q, hits]);
  useEffect(() => {
    list.current?.querySelector(`[data-idx="${idx}"]`)?.scrollIntoView({ block: "nearest" });
  }, [idx]);

  if (!open) return null;
  const pick = (it: Item | undefined) => {
    if (!it) return;
    close();
    it.run();
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/55 p-6 pt-[12vh] backdrop-blur-[2px]"
      onClick={close}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Search and commands"
        className="flex max-h-[70vh] w-[min(640px,94vw)] flex-col overflow-hidden rounded-2xl border border-border bg-raised shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex h-14 shrink-0 items-center gap-3 border-b border-line px-4">
        <span className="flex text-muted">
          <SearchIcon size={18} />
        </span>
        <input
          autoFocus
          aria-label="Search"
          className="min-w-0 flex-1 bg-transparent text-[16px] outline-none placeholder:text-faint"
          placeholder="Search chats, sessions and messages, or run a command"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              e.preventDefault();
              close();
            } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
              e.preventDefault();
              const d = e.key === "ArrowDown" ? 1 : -1;
              setIdx((i) => (items.length ? (i + d + items.length) % items.length : 0));
            } else if (e.key === "Enter") {
              e.preventDefault();
              pick(items[idx]);
            }
          }}
        />
        <kbd className="rounded-[5px] border border-border px-1.5 font-mono text-[11px] leading-[18px] text-faint">Esc</kbd>
        </div>
        <div ref={list} className="min-h-0 flex-1 overflow-y-auto p-2" role="listbox">
          {items.map((it, i) => (
            <div key={it.key}>
              {(i === 0 || items[i - 1].section !== it.section) && (
                <div className={`px-2.5 pb-1.5 font-mono text-[11px] uppercase tracking-[0.06em] text-faint ${i === 0 ? "pt-1" : "pt-3"}`}>
                  {it.section}
                </div>
              )}
              <button
                data-idx={i}
                role="option"
                aria-selected={i === idx}
                className={`flex w-full flex-col rounded-[9px] px-2.5 py-2 text-left ${i === idx ? "bg-panel-2" : "hover:bg-panel"}`}
                onMouseEnter={() => setIdx(i)}
                onClick={() => pick(it)}
              >
                <span className="flex w-full items-baseline gap-2 text-xs">
                  <span className="min-w-0 flex-1 truncate">{it.label}</span>
                  {it.detail && <span className="meta max-w-60 shrink-0 truncate text-faint">{it.detail}</span>}
                </span>
                {it.snippet && (
                  <span className="mt-0.5 line-clamp-2 text-[13px] text-muted">
                    <Snippet text={it.snippet} />
                  </span>
                )}
              </button>
            </div>
          ))}
          {items.length === 0 && <div className="px-2.5 py-3 text-xs text-faint">No matches</div>}
        </div>
        <div className="flex h-10 shrink-0 items-center gap-4 border-t border-line px-4 text-[12px] text-faint">
          <span>
            <kbd className="font-mono text-[11px] text-muted">↑ ↓</kbd> move
          </span>
          <span>
            <kbd className="font-mono text-[11px] text-muted">Enter</kbd> open
          </span>
          <span className="flex-1" />
          <span className="meta">
            {items.length} {items.length === 1 ? "result" : "results"}
          </span>
        </div>
      </div>
    </div>
  );
}
