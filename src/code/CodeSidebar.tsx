import { useMemo, useState } from "react";
import { AGENT_LABELS, api, type CodeSession } from "../lib/ipc";
import { useStore } from "../lib/store";
import { SectionLabel, SidebarItem } from "../components/SidebarItem";
import { fmtShort, matchesQuery } from "../lib/format";
import { ChevronRightIcon, PlusIcon } from "../components/icons";
import { activityOf, strongest } from "../lib/activity";
import { newSessionIn } from "./CodeView";

const COLLAPSED_KEY = "patchbay.codeSidebar.collapsed";

function loadCollapsed(): Set<string> {
  try {
    return new Set(JSON.parse(localStorage.getItem(COLLAPSED_KEY) ?? "[]") as string[]);
  } catch {
    return new Set();
  }
}

function saveCollapsed(s: Set<string>) {
  try {
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...s]));
  } catch {
    /* storage unavailable */
  }
}

function basename(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() ?? p;
}

function parentLabel(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  parts.pop();
  const home = parts.length >= 2 && parts[0] === "home" ? 2 : parts[0] === "Users" ? 2 : 0;
  return (home ? "~/" : "/") + parts.slice(home).join("/");
}

interface Group {
  cwd: string;
  label: string;
  where: string;
  sessions: CodeSession[];
  latest: number;
}

export function CodeSidebar() {
  const sessions = useStore((s) => s.codeSessions);
  const activeId = useStore((s) => s.activeCodeSessionId);
  const select = useStore((s) => s.selectCodeSession);
  const refresh = useStore((s) => s.refreshCodeSessions);
  const code = useStore((s) => s.code);
  const askConfirm = useStore((s) => s.askConfirm);
  const query = useStore((s) => s.sidebarQuery);
  const prefs = useStore((s) => s.prefs);
  const [collapsed, setCollapsed] = useState(loadCollapsed);

  const hits = useMemo(() => sessions.filter((s) => matchesQuery(query, s.title, s.cwd)), [sessions, query]);
  const pinned = useMemo(() => hits.filter((s) => s.pinned), [hits]);

  const groups = useMemo<Group[]>(() => {
    const by = new Map<string, CodeSession[]>();
    for (const s of hits) if (!s.pinned) by.set(s.cwd, [...(by.get(s.cwd) ?? []), s]);
    const names = new Map<string, number>();
    for (const cwd of by.keys()) names.set(basename(cwd), (names.get(basename(cwd)) ?? 0) + 1);
    return [...by.entries()]
      .map(([cwd, list]) => {
        const sorted = [...list].sort((a, b) => b.updatedAt - a.updatedAt);
        const name = basename(cwd);
        return {
          cwd,
          // Same folder name in two places: show where each one lives.
          label: name,
          where: parentLabel(cwd),
          sessions: sorted,
          latest: sorted[0]?.updatedAt ?? 0,
        };
      })
      .sort((a, b) => b.latest - a.latest);
  }, [hits]);

  const toggle = (cwd: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(cwd)) next.delete(cwd);
      else next.add(cwd);
      saveCollapsed(next);
      return next;
    });

  if (sessions.length === 0) {
    return <div className="px-4 py-2 text-xs text-muted">No code sessions yet</div>;
  }
  if (hits.length === 0) {
    return <div className="px-4 py-2 text-xs text-muted">No matches</div>;
  }

  const item = (s: CodeSession, inPinned: boolean) => {
    const st = code[s.id];
    // Live tool calls show a new edit or commit before the turn ends and it's saved.
    const live = st ? activityOf(Object.values(st.toolCalls)) : null;
    return (
      <SidebarItem
        key={s.id}
        icon={strongest(s.activity, live) ?? "chat"}
        indent={!inPinned}
        title={s.title}
        subtitle={inPinned ? basename(s.cwd) : undefined}
        active={s.id === activeId}
        busy={st?.busy}
        attention={(st?.permissions.length ?? 0) > 0}
        meta={fmtShort(s.updatedAt)}
        pinned={s.pinned}
        status={st && (st.status === "ready" || st.status === "starting") ? "running" : "idle"}
        onSelect={() => select(s.id)}
        onTogglePin={async () => {
          await api.updateCodeSession(s.id, { pinned: !s.pinned });
          refresh();
        }}
        onRename={async (t) => {
          await api.updateCodeSession(s.id, { title: t });
          refresh();
        }}
        onDelete={async () => {
          const { ok } = await askConfirm({
            title: "Remove session?",
            body: `"${s.title}" will be removed from Patchbay. Files in ${s.cwd} are not touched.`,
            confirmLabel: "Remove",
            danger: true,
          });
          if (!ok) return;
          await api.deleteCodeSession(s.id);
          if (s.id === activeId) select(null);
          refresh();
        }}
      />
    );
  };

  return (
    <>
      {pinned.length > 0 && (
        <div className="mb-1 flex flex-col gap-0.5">
          <SectionLabel>Pinned</SectionLabel>
          {pinned.map((s) => item(s, true))}
        </div>
      )}
      {pinned.length > 0 && groups.length > 0 && <SectionLabel>Folders</SectionLabel>}
      {groups.map((g) => {
        // While searching, show every match regardless of collapsed folders.
        const isCollapsed = !query.trim() && collapsed.has(g.cwd);
        const hasActive = g.sessions.some((s) => s.id === activeId);
        return (
          <div key={g.cwd} className="mb-1">
            <div className="group/header mx-2 mt-2 flex items-center gap-1 rounded-lg py-1 pl-2.5 pr-1 hover:bg-panel-2/50">
              <button
                className="flex min-w-0 flex-1 items-baseline gap-1.5 text-left"
                onClick={() => toggle(g.cwd)}
                title={g.cwd}
                aria-expanded={!isCollapsed}
              >
                <span className="shrink-0 truncate text-[13px] font-medium text-fg/90">{g.label}</span>
                <span className="min-w-0 truncate font-mono text-[11px] text-faint">{g.where}</span>
                <span
                  className={`flex shrink-0 self-center text-faint transition-transform ${isCollapsed ? "" : "rotate-90"}`}
                >
                  <ChevronRightIcon size={12} />
                </span>
                {isCollapsed && hasActive && <span className="h-1.5 w-1.5 shrink-0 self-center rounded-full bg-accent" />}
              </button>
              <button
                className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-faint opacity-0 hover:bg-panel-2 hover:text-fg focus-visible:opacity-100 group-hover/header:opacity-100"
                onClick={() => {
                  if (isCollapsed) toggle(g.cwd);
                  newSessionIn(g.cwd);
                }}
                title={`New ${AGENT_LABELS[prefs.defaultAgent] ?? ""} session in ${g.cwd}`}
                aria-label={`New session in ${g.label}`}
              >
                <PlusIcon size={14} />
              </button>
            </div>
            {!isCollapsed && g.sessions.map((s) => item(s, false))}
          </div>
        );
      })}
    </>
  );
}
