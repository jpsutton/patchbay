import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { AGENT_LABELS, api, type AgentInfo, type CodeSession } from "../lib/ipc";
import { memberName, remoteHost, useStore } from "../lib/store";
import { ArchivedToggle, SectionLabel, SidebarItem } from "../components/SidebarItem";
import { archiveSessions, sessionActions, toggleSessionArchive } from "../lib/itemActions";
import { fmtShort, matchesQuery } from "../lib/format";
import { ChevronRightIcon, GripVerticalIcon, PlusIcon } from "../components/icons";
import { activityOf, strongest } from "../lib/activity";
import { installedAgents, newSessionIn } from "./CodeView";
import { ContextMenu } from "../components/ItemMenu";
import { Dot } from "../components/Dot";
import { loadStringList, saveStringList, useManualOrder } from "../lib/manualOrder";

const COLLAPSED_KEY = "patchbay.codeSidebar.collapsed";
const ORDER_KEY = "patchbay.codeSidebar.order";
/** How far the pointer moves before a press on a folder becomes a drag. */
const DRAG_THRESHOLD = 4;

function loadCollapsed(): Set<string> {
  return new Set(loadStringList(COLLAPSED_KEY));
}

function saveCollapsed(s: Set<string>) {
  saveStringList(COLLAPSED_KEY, [...s]);
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
  /** Every folder the group's sessions are in, with the computer for those on another one. */
  places: string[];
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
  /** Rows picked with Shift/Ctrl/Cmd-click, for archiving together. */
  const [picked, setPicked] = useState<Set<string>>(new Set());
  /** The row a Shift-click extends from: the last one clicked. */
  const anchor = useRef<string | null>(null);

  const matching = useMemo(() => sessions.filter((s) => matchesQuery(query, s.title, s.cwd)), [sessions, query]);
  const hits = useMemo(() => matching.filter((s) => s.archivedAt == null), [matching]);
  const archived = useMemo(() => matching.filter((s) => s.archivedAt != null), [matching]);
  const pinned = useMemo(() => hits.filter((s) => s.pinned), [hits]);

  const byRecency = useMemo<Group[]>(() => {
    // `/a/b` and `/a/b/` are one folder.
    const dirOf = (s: CodeSession) => (s.cwd.length > 1 ? s.cwd.replace(/[\\/]+$/, "") || s.cwd : s.cwd);
    // Normally the same path on two computers is two folders. Grouping by project name makes
    // every folder with the same name one group, wherever it lives.
    const by = new Map<string, CodeSession[]>();
    for (const s of hits) {
      if (s.pinned) continue;
      const host = remoteHost(s, mesh);
      const dir = dirOf(s);
      // The prefix keeps a project's key apart from a folder's.
      const key = prefs.groupByProject ? `project:${basename(dir)}` : host ? `${host}:${dir}` : dir;
      const list = by.get(key);
      if (list) list.push(s);
      else by.set(key, [s]);
    }
    return [...by.entries()]
      .map(([key, list]) => {
        const sorted = [...list].sort((a, b) => b.updatedAt - a.updatedAt);
        // A project can span folders: "+" starts a session where its latest one is.
        const top = sorted[0];
        const cwd = dirOf(top);
        const host = remoteHost(top, mesh);
        return {
          key,
          cwd,
          host,
          label: basename(cwd),
          // Same folder name in two places: show where each one lives. A project has no one place.
          where: prefs.groupByProject ? "" : host ? `${memberName(mesh, host)}:${parentLabel(cwd)}` : parentLabel(cwd),
          places: [...new Set(sorted.map((s) => {
            const h = remoteHost(s, mesh);
            return h ? `${dirOf(s)} on ${memberName(mesh, h)}` : dirOf(s);
          }))],
          sessions: sorted,
          latest: top.updatedAt,
        };
      })
      .sort((a, b) => b.latest - a.latest);
  }, [hits, mesh, prefs.groupByProject]);

  // Folders keep the order the user gave them. A folder seen for the first time goes on top,
  // and one that drops out of view keeps its place in storage.
  const rankKeys = useMemo(() => byRecency.map((g) => g.key), [byRecency]);
  const { ordered, move: moveKey } = useManualOrder(ORDER_KEY, rankKeys);
  const groups = useMemo<Group[]>(() => {
    const byKey = new Map(byRecency.map((g) => [g.key, g]));
    return ordered.flatMap((k) => {
      const g = byKey.get(k);
      return g ? [g] : [];
    });
  }, [byRecency, ordered]);

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
      if (ev.type === "pointerup") moveKey(key, index);
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

  /** The rows a Shift-click can span: the pinned list, or one folder's. */
  const listOf = (id: string): CodeSession[] =>
    pinned.some((s) => s.id === id) ? pinned : (groups.find((g) => g.sessions.some((s) => s.id === id))?.sessions ?? []);

  /** Shift-click picks everything from the last click to this row; Ctrl/Cmd-click adds or drops one. */
  const pick = (s: CodeSession, shift: boolean) => {
    const list = listOf(s.id);
    const from = list.findIndex((x) => x.id === anchor.current);
    if (shift && from >= 0) {
      const to = list.findIndex((x) => x.id === s.id);
      const range = list.slice(Math.min(from, to), Math.max(from, to) + 1).map((x) => x.id);
      setPicked(new Set(range));
      return;
    }
    // No usable anchor: the first pick starts one.
    anchor.current = s.id;
    setPicked((prev) => {
      const next = new Set(prev);
      if (next.has(s.id)) next.delete(s.id);
      else next.add(s.id);
      return next;
    });
  };

  // Rows that were archived, removed or filtered out drop from the selection.
  const chosen = hits.filter((s) => picked.has(s.id));

  const hasChosen = chosen.length > 0;
  // Esc drops the selection, unless a field (rename, search) wants it.
  useEffect(() => {
    if (!hasChosen) return;
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (e.key === "Escape" && !e.defaultPrevented && t?.tagName !== "INPUT" && t?.tagName !== "TEXTAREA") {
        setPicked(new Set());
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [hasChosen]);

  // No line where the drop would leave the folder in place.
  const from = drag ? groups.findIndex((g) => g.key === drag.key) : -1;
  const dropAt = drag && drag.index !== from && drag.index !== from + 1 ? drag.index : null;

  if (sessions.length === 0) {
    return <div className="px-4 py-2 text-xs text-muted">No code sessions yet</div>;
  }
  if (hits.length === 0 && archived.length === 0) {
    return <div className="px-4 py-2 text-xs text-muted">No matches</div>;
  }

  /** The other computer hosting `s`, by name. */
  const hostLabel = (s: CodeSession) => {
    const h = remoteHost(s, mesh);
    return h ? memberName(mesh, h) : undefined;
  };

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
        selected={picked.has(s.id) && s.archivedAt == null}
        busy={st?.busy}
        attention={(st?.permissions.length ?? 0) > 0}
        meta={fmtShort(s.updatedAt)}
        host={prefs.groupByProject ? hostLabel(s) : undefined}
        pinned={s.pinned}
        status={st && (st.status === "ready" || st.status === "starting") ? "running" : "idle"}
        onSelect={async (e) => {
          if (s.archivedAt == null && (e.shiftKey || e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            // A Shift-click with nothing clicked yet starts from the open session.
            if (e.shiftKey && anchor.current == null) anchor.current = activeId;
            pick(s, e.shiftKey);
            return;
          }
          anchor.current = s.id;
          setPicked(new Set());
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
      {chosen.length > 0 && (
        <div className="sticky top-0 z-10 mx-2 mb-1 flex items-center gap-1 rounded-lg border border-line bg-panel px-2 py-1 text-[13px]">
          <span className="min-w-0 flex-1 truncate text-fg/90">{chosen.length} selected</span>
          <button
            className="rounded-md px-2 py-1 text-fg hover:bg-panel-2"
            onClick={() => {
              archiveSessions(chosen);
              setPicked(new Set());
            }}
          >
            Archive
          </button>
          <button className="rounded-md px-2 py-1 text-muted hover:bg-panel-2 hover:text-fg" onClick={() => setPicked(new Set())}>
            Clear
          </button>
        </div>
      )}
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
                className="-ml-2 flex h-6 w-3 shrink-0 cursor-grab touch-none items-center justify-center rounded text-faint opacity-0 hover:text-fg focus-visible:opacity-100 group-hover/header:opacity-100"
                onPointerDown={(e) => startDrag(e, g.key)}
                tabIndex={-1}
                title="Drag to reorder"
                aria-label={`Drag to reorder ${g.label}`}
              >
                <GripVerticalIcon size={12} />
              </button>
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
                  moveKey(g.key, e.key === "ArrowUp" ? i - 1 : i + 2);
                }}
                title={`${g.places.join("\n")}\nDrag to reorder`}
                aria-expanded={!isCollapsed}
              >
                <span className="shrink-0 truncate text-[13px] font-medium text-fg/90">{g.label}</span>
                <span className="min-w-0 truncate font-mono text-[11px] text-faint">{g.where}</span>
                <span
                  className={`flex shrink-0 self-center text-faint transition-transform ${isCollapsed ? "" : "rotate-90"}`}
                >
                  <ChevronRightIcon size={12} />
                </span>
                {isCollapsed && hasActive && <Dot size={6} className="self-center text-accent" />}
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
