import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { AGENT_LABELS, api, type AgentInfo, type CodeSession } from "../lib/ipc";
import { memberName, remoteHost, useStore } from "../lib/store";
import { ArchivedToggle, SectionLabel, SidebarItem } from "../components/SidebarItem";
import { sessionActions, toggleSessionArchive } from "../lib/itemActions";
import { fmtShort, matchesQuery } from "../lib/format";
import { ChevronRightIcon, PlusIcon } from "../components/icons";
import { activityOf, strongest } from "../lib/activity";
import { installedAgents, newSessionIn } from "./CodeView";
import { ContextMenu } from "../components/ItemMenu";

const COLLAPSED_KEY = "patchbay.codeSidebar.collapsed";
const ORDER_KEY = "patchbay.codeSidebar.order";
/** How far the pointer moves before a press on a folder becomes a drag. */
const DRAG_THRESHOLD = 4;

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

/** Folder keys in the order the user arranged them, top first. */
function loadOrder(): string[] {
  try {
    const v = JSON.parse(localStorage.getItem(ORDER_KEY) ?? "[]");
    return Array.isArray(v) ? (v as string[]) : [];
  } catch {
    return [];
  }
}

function saveOrder(o: string[]) {
  try {
    localStorage.setItem(ORDER_KEY, JSON.stringify(o));
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
  /** `cwd`, or for a folder on another computer `host:cwd`. */
  key: string;
  cwd: string;
  /** The computer the folder is on, when that isn't this one. */
  host: string | null;
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
  const mesh = useStore((s) => s.mesh);
  const [collapsed, setCollapsed] = useState(loadCollapsed);
  const [showArchived, setShowArchived] = useState(false);
  const [order, setOrder] = useState(loadOrder);
  /** While dragging a folder: which one, and the slot it would drop into. */
  const [drag, setDrag] = useState<{ key: string; index: number } | null>(null);
  const folderEls = useRef(new Map<string, HTMLDivElement>());
  /** Set when a press turned into a drag, so the click that ends it doesn't toggle the folder. */
  const dragged = useRef(false);
  /** Right-click on a folder's "+": which agent to start there. */
  const [agentMenu, setAgentMenu] = useState<{
    x: number;
    y: number;
    group: Group;
    agents: AgentInfo[];
  } | null>(null);
  /** Right-click on a group header: collapse/expand actions. */
  const [groupMenu, setGroupMenu] = useState<{ x: number; y: number } | null>(null);

  const matching = useMemo(() => sessions.filter((s) => matchesQuery(query, s.title, s.cwd)), [sessions, query]);
  const hits = useMemo(() => matching.filter((s) => s.archivedAt == null), [matching]);
  const archived = useMemo(() => matching.filter((s) => s.archivedAt != null), [matching]);
  const pinned = useMemo(() => hits.filter((s) => s.pinned), [hits]);

  const byRecency = useMemo<Group[]>(() => {
    // The same path on two computers is two folders.
    const by = new Map<string, { cwd: string; host: string | null; list: CodeSession[] }>();
    for (const s of hits) {
      if (s.pinned) continue;
      const host = remoteHost(s, mesh);
      const key = host ? `${host}:${s.cwd}` : s.cwd;
      const g = by.get(key) ?? { cwd: s.cwd, host, list: [] };
      g.list.push(s);
      by.set(key, g);
    }
    return [...by.entries()]
      .map(([key, { cwd, host, list }]) => {
        const sorted = [...list].sort((a, b) => b.updatedAt - a.updatedAt);
        return {
          key,
          cwd,
          host,
          label: basename(cwd),
          // Same folder name in two places: show where each one lives.
          where: host ? `${memberName(mesh, host)}:${parentLabel(cwd)}` : parentLabel(cwd),
          sessions: sorted,
          latest: sorted[0]?.updatedAt ?? 0,
        };
      })
      .sort((a, b) => b.latest - a.latest);
  }, [hits, mesh]);

  // Folders keep the order the user gave them. A folder seen for the first time goes on top.
  const groups = useMemo<Group[]>(() => {
    const rank = new Map(order.map((k, i) => [k, i]));
    const fresh = byRecency.filter((g) => !rank.has(g.key));
    const known = byRecency.filter((g) => rank.has(g.key)).sort((a, b) => rank.get(a.key)! - rank.get(b.key)!);
    return [...fresh, ...known];
  }, [byRecency, order]);

  // Record first-seen folders so later activity doesn't move them. Folders that
  // drop out of view (all archived, or a computer offline) keep their place.
  useEffect(() => {
    const fresh = groups.filter((g) => !order.includes(g.key)).map((g) => g.key);
    if (fresh.length === 0) return;
    const next = [...fresh, ...order];
    saveOrder(next);
    setOrder(next);
  }, [groups, order]);

  /** Move `key` so it sits just above the visible folder now at `index` (or last, past the end). */
  const moveFolder = (key: string, index: number) => {
    const visible = groups.map((g) => g.key);
    const from = visible.indexOf(key);
    if (from < 0 || index < 0 || index === from || index === from + 1) return;
    const next = order.filter((k) => k !== key);
    const before = visible[index];
    if (before !== undefined) next.splice(next.indexOf(before), 0, key);
    else next.splice(next.indexOf(visible[visible.length - 1]) + 1, 0, key);
    saveOrder(next);
    setOrder(next);
  };

  /** Which slot the pointer is over: the folder whose upper half it is in, or past the last one. */
  const slotAt = (y: number) => {
    const i = groups.findIndex((g) => {
      const r = folderEls.current.get(g.key)?.getBoundingClientRect();
      return r != null && y < r.top + r.height / 2;
    });
    return i < 0 ? groups.length : i;
  };

  const startDrag = (e: ReactPointerEvent<HTMLButtonElement>, key: string) => {
    if (e.button !== 0) return;
    const startY = e.clientY;
    const el = e.currentTarget;
    let index: number | null = null;
    dragged.current = false;
    el.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      if (index == null) {
        if (Math.abs(ev.clientY - startY) < DRAG_THRESHOLD) return;
        document.body.style.cursor = "grabbing";
        document.body.style.userSelect = "none";
        dragged.current = true;
      }
      index = slotAt(ev.clientY);
      setDrag({ key, index });
    };
    const up = (ev: PointerEvent) => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
      if (index == null) return;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      setDrag(null);
      if (ev.type === "pointerup") moveFolder(key, index);
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
  };

  const toggle = (cwd: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(cwd)) next.delete(cwd);
      else next.add(cwd);
      saveCollapsed(next);
      return next;
    });

  const setAllCollapsed = (fold: boolean) =>
    setCollapsed(() => {
      // While searching everything is shown, so leave the saved state alone.
      const next = fold ? new Set(groups.map((g) => g.key)) : new Set<string>();
      if (!query.trim()) saveCollapsed(next);
      return next;
    });

  // No line where the drop would leave the folder in place.
  const from = drag ? groups.findIndex((g) => g.key === drag.key) : -1;
  const dropAt = drag && drag.index !== from && drag.index !== from + 1 ? drag.index : null;

  if (sessions.length === 0) {
    return <div className="px-4 py-2 text-xs text-muted">No code sessions yet</div>;
  }
  if (hits.length === 0 && archived.length === 0) {
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
        onSelect={async () => {
          // Opening an archived session brings it back.
          if (s.archivedAt != null) {
            await api.updateCodeSession(s.id, { archived: false });
            await refresh();
          }
          select(s.id);
        }}
        menu={(rename) =>
          sessionActions(s, {
            rename,
            desktop: true,
            running: !!st && (st.status === "ready" || st.status === "starting"),
            local: !remoteHost(s, mesh),
          })
        }
        onTogglePin={async () => {
          await api.updateCodeSession(s.id, { pinned: !s.pinned });
          refresh();
        }}
        onRename={async (t) => {
          await api.updateCodeSession(s.id, { title: t });
          refresh();
        }}
        onArchive={s.archivedAt == null ? toggleSessionArchive(s) : undefined}
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
      {groups.map((g, i) => {
        // While searching, show every match regardless of collapsed folders.
        const isCollapsed = !query.trim() && collapsed.has(g.key);
        const hasActive = g.sessions.some((s) => s.id === activeId);
        return (
          <div
            key={g.key}
            ref={(el) => {
              if (el) folderEls.current.set(g.key, el);
              else folderEls.current.delete(g.key);
            }}
            className={`relative mb-1 ${drag?.key === g.key ? "opacity-50" : ""}`}
          >
            {dropAt === i && <DropLine top />}
            {dropAt === groups.length && i === groups.length - 1 && <DropLine />}
            <div
              className="group/header mx-2 mt-2 flex items-center gap-1 rounded-lg py-1 pl-2.5 pr-1 hover:bg-panel-2/50"
              onContextMenu={(e) => {
                e.preventDefault();
                setAgentMenu(null);
                setGroupMenu({ x: e.clientX, y: e.clientY });
              }}
            >
              <button
                className="flex min-w-0 flex-1 items-baseline gap-1.5 text-left"
                onPointerDown={(e) => startDrag(e, g.key)}
                onClick={() => {
                  if (dragged.current) {
                    dragged.current = false;
                    return;
                  }
                  toggle(g.key);
                }}
                onKeyDown={(e) => {
                  // Alt+Up/Down moves the folder without a pointer.
                  if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
                  e.preventDefault();
                  moveFolder(g.key, e.key === "ArrowUp" ? i - 1 : i + 2);
                }}
                title={`${g.host ? `${g.cwd} on ${memberName(mesh, g.host)}` : g.cwd}\nDrag to reorder`}
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
                  if (isCollapsed) toggle(g.key);
                  newSessionIn(g.cwd, undefined, g.host);
                }}
                onContextMenu={(e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  const at = { x: e.clientX, y: e.clientY, group: g };
                  installedAgents(g.host).then((agents) =>
                    setAgentMenu({ ...at, agents: agents.filter((a) => a.available) }),
                  );
                }}
                title={
                  g.host
                    ? `New session in ${g.cwd} on ${memberName(mesh, g.host)} (right-click for another agent)`
                    : `New ${AGENT_LABELS[prefs.defaultAgent] ?? ""} session in ${g.cwd} (right-click for another agent)`
                }
                aria-label={`New session in ${g.label}`}
              >
                <PlusIcon size={14} />
              </button>
            </div>
            {!isCollapsed && g.sessions.map((s) => item(s, false))}
          </div>
        );
      })}
      {groupMenu && groups.length > 0 && (
        <ContextMenu
          x={groupMenu.x}
          y={groupMenu.y}
          actions={[
            { label: "Collapse all groups", onSelect: () => setAllCollapsed(true) },
            { label: "Expand all groups", onSelect: () => setAllCollapsed(false) },
          ]}
          onClose={() => setGroupMenu(null)}
        />
      )}
      {agentMenu && agentMenu.agents.length > 0 && (
        <ContextMenu
          x={agentMenu.x}
          y={agentMenu.y}
          title={`New session in ${agentMenu.group.cwd}`}
          actions={agentMenu.agents.map((a) => ({
            label: `New ${a.label} session${a.id === prefs.defaultAgent ? " (default)" : ""}`,
            onSelect: () => {
              if (collapsed.has(agentMenu.group.key)) toggle(agentMenu.group.key);
              newSessionIn(agentMenu.group.cwd, a.id, agentMenu.group.host);
            },
          }))}
          onClose={() => setAgentMenu(null)}
        />
      )}
      <ArchivedToggle count={archived.length} open={showArchived} onToggle={() => setShowArchived(!showArchived)} />
      {showArchived && archived.map((s) => item(s, true))}
    </>
  );
}

/** Where a dragged folder will land: a line along the top or bottom edge of a folder. */
function DropLine({ top }: { top?: boolean }) {
  return (
    <div
      className={`pointer-events-none absolute left-3 right-3 h-0.5 rounded-full bg-accent ${top ? "top-0.5" : "-bottom-1"}`}
    />
  );
}
