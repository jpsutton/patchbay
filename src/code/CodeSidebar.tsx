import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { AGENT_LABELS, api, type AgentInfo, type CodeSession } from "../lib/ipc";
import { memberName, remoteHost, useStore } from "../lib/store";
import { ArchivedToggle, SectionLabel, SidebarItem } from "../components/SidebarItem";
import { archiveSessions, sessionActions, toggleSessionArchive } from "../lib/itemActions";
import { fmtShort, matchesQuery } from "../lib/format";
import { ChevronRightIcon, GripVerticalIcon, PlusIcon } from "../components/icons";
import { activityOf, strongest } from "../lib/activity";
import { installedAgents, newSessionIn } from "./CodeView";
import { ContextMenu, type ItemAction } from "../components/ItemMenu";
import { Dot } from "../components/Dot";
import { applyOrder, loadStringList, saveStringList } from "../lib/manualOrder";
import {
  GROUP_PREFIX,
  arrange,
  locate,
  planMove,
  planNewGroup,
  planProjectsToFolders,
  planRename,
  planUngroup,
  recordFresh,
  seedOrder,
  type Target,
  type TopItem,
} from "../lib/sidebarLayout";

const COLLAPSED_KEY = "patchbay.codeSidebar.collapsed";
/** The folder order from before the layout synced. It seeds the first layout. */
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

/** A session's folder. `/a/b` and `/a/b/` are one folder. */
function dirOf(s: CodeSession): string {
  return s.cwd.length > 1 ? s.cwd.replace(/[\\/]+$/, "") || s.cwd : s.cwd;
}

/** A session's folder's key in the synced layout. It names the computer, so it is the same on every one. */
function folderLayoutKey(s: CodeSession): string {
  return s.hostNode ? `${s.hostNode}:${dirOf(s)}` : dirOf(s);
}

/** A session's project's key. The prefix keeps it apart from a folder's. */
function projectLayoutKey(s: CodeSession): string {
  return `project:${basename(dirOf(s))}`;
}

/** Set once this computer has moved projects back to their folders' places (`planProjectsToFolders`). */
const PROJECTS_PLACED_KEY = "patchbay.codeSidebar.projectsPlaced";

function parentLabel(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  parts.pop();
  const home = parts.length >= 2 && parts[0] === "home" ? 2 : parts[0] === "Users" ? 2 : 0;
  return (home ? "~/" : "/") + parts.slice(home).join("/");
}

interface Group {
  /** `cwd`, or for a folder on another computer `host:cwd`. */
  key: string;
  /** The key in the synced layout: the same on every computer, so it always names the computer. */
  layoutKey: string;
  /** Layout keys whose place this one takes while it has none: a project's folders, newest first, or a folder's project. */
  inherit: string[];
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
  const layout = useStore((s) => s.sidebarLayout);
  const changeLayout = useStore((s) => s.changeSidebarLayout);
  /** While dragging a folder or a group: which one, where it would drop, and whether that is onto a group. */
  const [drag, setDrag] = useState<{ key: string; target: Target; into: boolean } | null>(null);
  /** Each folder's and each group's block, by layout key, and each group's header. */
  const blockEls = useRef(new Map<string, HTMLDivElement>());
  const headerEls = useRef(new Map<string, HTMLDivElement>());
  /** Set when a press turned into a drag, so the click that ends it doesn't toggle the folder. */
  const dragged = useRef(false);
  /** Right-click on a folder's "+": which agent to start there. */
  const [agentMenu, setAgentMenu] = useState<{
    x: number;
    y: number;
    group: Group;
    agents: AgentInfo[];
  } | null>(null);
  /** Right-click on a folder or group header: what to do with it. `key` is its layout key. */
  const [menu, setMenu] = useState<{ x: number; y: number; key: string } | null>(null);
  /** Rows picked with Shift/Ctrl/Cmd-click, for archiving together. */
  const [picked, setPicked] = useState<Set<string>>(new Set());
  /** The row a Shift-click extends from: the last one clicked. */
  const anchor = useRef<string | null>(null);

  const matching = useMemo(() => sessions.filter((s) => matchesQuery(query, s.title, s.cwd)), [sessions, query]);
  const hits = useMemo(() => matching.filter((s) => s.archivedAt == null), [matching]);
  const archived = useMemo(() => matching.filter((s) => s.archivedAt != null), [matching]);
  const pinned = useMemo(() => hits.filter((s) => s.pinned), [hits]);

  const byRecency = useMemo<Group[]>(() => {
    // Normally the same path on two computers is two folders. Grouping by project name makes
    // every folder with the same name one group, wherever it lives.
    const by = new Map<string, CodeSession[]>();
    const layoutKeys = new Map<string, string>();
    for (const s of hits) {
      if (s.pinned) continue;
      const host = remoteHost(s, mesh);
      const dir = dirOf(s);
      const key = prefs.groupByProject ? projectLayoutKey(s) : host ? `${host}:${dir}` : dir;
      layoutKeys.set(key, prefs.groupByProject ? key : folderLayoutKey(s));
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
          layoutKey: layoutKeys.get(key) ?? key,
          inherit: prefs.groupByProject ? [...new Set(sorted.map(folderLayoutKey))] : [projectLayoutKey(top)],
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

  // Folders and groups keep the layout the user gave them, which syncs across the mesh. A folder
  // seen for the first time goes on top, and one that drops out of view keeps its place.
  const byLayoutKey = useMemo(() => new Map(byRecency.map((g) => [g.layoutKey, g])), [byRecency]);
  const arranged = useMemo(
    () => arrange(byRecency.map((g) => g.layoutKey), layout ?? [], new Map(byRecency.map((g) => [g.layoutKey, g.inherit]))),
    [byRecency, layout],
  );
  const searching = query.trim() !== "";
  // While searching, a group with no match is left out.
  const top = useMemo<TopItem[]>(
    () => arranged.top.filter((t) => t.type === "folder" || !searching || t.children.length > 0),
    [arranged, searching],
  );
  /** Every folder on screen, in the order shown. */
  const groups = useMemo<Group[]>(
    () =>
      top.flatMap((t) => (t.type === "folder" ? [t.key] : t.children)).flatMap((k) => {
        const g = byLayoutKey.get(k);
        return g ? [g] : [];
      }),
    [top, byLayoutKey],
  );

  // Record folders the layout has not seen, so later activity doesn't move them. The first layout
  // starts from the order this computer kept before the layout synced. A search shows only some
  // folders, so it records nothing. Each folder is tried once: a failed write must not loop.
  const tried = useRef(new Set<string>());
  useEffect(() => {
    if (layout == null || searching) return;
    const fresh = arranged.fresh.filter((k) => !tried.current.has(k));
    if (fresh.length === 0) return;
    for (const k of fresh) tried.current.add(k);
    const legacy = layout.length === 0 ? loadStringList(ORDER_KEY) : [];
    if (legacy.length > 0) {
      const keyOf = new Map(byRecency.map((g) => [g.key, g.layoutKey]));
      const order = applyOrder(byRecency.map((g) => g.key), legacy).map((k) => keyOf.get(k)!);
      changeLayout({ put: seedOrder(order), remove: [] });
    } else {
      changeLayout({ put: recordFresh(layout, fresh), remove: [] });
    }
  }, [layout, arranged.fresh, byRecency, searching, changeLayout]);

  // Once per computer: projects recorded at the top level by builds that didn't place them with
  // their folders go back to their folders' places, in or out of a group.
  useEffect(() => {
    if (layout == null || !prefs.groupByProject || sessions.length === 0) return;
    if (localStorage.getItem(PROJECTS_PLACED_KEY)) return;
    localStorage.setItem(PROJECTS_PLACED_KEY, "1");
    const sources = new Map<string, string[]>();
    for (const s of [...sessions].sort((a, b) => b.updatedAt - a.updatedAt)) {
      const list = sources.get(projectLayoutKey(s)) ?? [];
      const f = folderLayoutKey(s);
      if (!list.includes(f)) sources.set(projectLayoutKey(s), [...list, f]);
    }
    const change = planProjectsToFolders(layout, sources);
    if (change) changeLayout(change);
  }, [layout, prefs.groupByProject, sessions, changeLayout]);

  // The drag handlers outlive the render that started them, so they read the latest of these.
  const live = useRef({ top, layout });
  live.current = { top, layout };

  const moveTo = (key: string, target: Target) => {
    const change = planMove(live.current.layout ?? [], live.current.top, key, target);
    if (change) changeLayout(change);
  };

  /** Move one slot up or down in its list, without a pointer. */
  const nudge = (key: string, dir: -1 | 1) => {
    const at = locate(top, key);
    if (at) moveTo(key, { parent: at.parent, index: dir < 0 ? at.index - 1 : at.index + 2 });
  };

  /** Where a dragged item would land with the pointer at `y`. */
  const dropAt = (y: number, key: string): { target: Target; into: boolean } => {
    const rect = (k: string) => blockEls.current.get(k)?.getBoundingClientRect();
    const items = live.current.top;
    for (let i = 0; i < items.length; i++) {
      const t = items[i];
      const r = rect(t.key);
      if (!r || y >= r.bottom) continue;
      if (t.type === "group" && !key.startsWith(GROUP_PREFIX)) {
        const h = headerEls.current.get(t.key)?.getBoundingClientRect();
        // The top of a header is the gap above the group; the rest of it, or a gap between
        // folders, drops into the group.
        if (h && y < h.top + h.height * 0.3) return { target: { parent: "", index: i }, into: false };
        if (!h || y < h.bottom) return { target: { parent: t.key, index: 0 }, into: true };
        const j = t.children.findIndex((c) => {
          const cr = rect(c);
          return cr != null && y < cr.top + cr.height / 2;
        });
        return { target: { parent: t.key, index: j < 0 ? t.children.length : j }, into: false };
      }
      return { target: { parent: "", index: y < r.top + r.height / 2 ? i : i + 1 }, into: false };
    }
    return { target: { parent: "", index: items.length }, into: false };
  };

  const startDrag = (e: ReactPointerEvent<HTMLButtonElement>, key: string) => {
    if (e.button !== 0) return;
    const startY = e.clientY;
    const el = e.currentTarget;
    let last: { target: Target; into: boolean } | null = null;
    dragged.current = false;
    el.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      if (last == null) {
        if (Math.abs(ev.clientY - startY) < DRAG_THRESHOLD) return;
        document.body.style.cursor = "grabbing";
        document.body.style.userSelect = "none";
        dragged.current = true;
      }
      last = dropAt(ev.clientY, key);
      setDrag({ key, ...last });
    };
    const up = (ev: PointerEvent) => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
      if (last == null) return;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      setDrag(null);
      if (ev.type === "pointerup") moveTo(key, last.target);
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
      const next = fold
        ? new Set([...groups.map((g) => g.key), ...top.flatMap((t) => (t.type === "group" ? [t.key] : []))])
        : new Set<string>();
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

  // Show a drop mark only where the move changes something.
  const dropTarget = drag && planMove(layout ?? [], top, drag.key, drag.target) ? drag.target : null;
  let line: { key: string; edge: "top" | "bottom" } | null = null;
  let ring: string | null = null;
  if (drag && dropTarget) {
    const into = top.find((t): t is Extract<TopItem, { type: "group" }> => t.type === "group" && t.key === dropTarget.parent);
    const siblings = into ? into.children : top.map((t) => t.key);
    if (drag.into) ring = dropTarget.parent;
    else if (dropTarget.index < siblings.length) line = { key: siblings[dropTarget.index], edge: "top" };
    else if (siblings.length > 0) line = { key: siblings[siblings.length - 1], edge: "bottom" };
  }

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

  /** A ref callback that files an element under `key`. */
  const blockRef = (key: string, map = blockEls) => (el: HTMLDivElement | null) => {
    if (el) map.current.set(key, el);
    else map.current.delete(key);
  };

  const renderFolder = (g: Group) => {
    // While searching, show every match regardless of collapsed folders.
    const isCollapsed = !searching && collapsed.has(g.key);
    const hasActive = g.sessions.some((s) => s.id === activeId);
    return (
      <div
        key={g.key}
        ref={blockRef(g.layoutKey)}
        className={`relative mb-1 ${drag?.key === g.layoutKey ? "opacity-50" : ""}`}
      >
        {line?.key === g.layoutKey && <DropLine top={line.edge === "top"} />}
        <div
          className="group/header mx-2 mt-2 flex items-center gap-1 rounded-lg py-1 pl-2.5 pr-1 hover:bg-panel-2/50"
          onContextMenu={(e) => {
            e.preventDefault();
            setAgentMenu(null);
            setMenu({ x: e.clientX, y: e.clientY, key: g.layoutKey });
          }}
        >
          <button
            className="-ml-2 flex h-6 w-3 shrink-0 cursor-grab touch-none items-center justify-center rounded text-faint opacity-0 hover:text-fg focus-visible:opacity-100 group-hover/header:opacity-100"
            onPointerDown={(e) => startDrag(e, g.layoutKey)}
            tabIndex={-1}
            title="Drag to reorder"
            aria-label={`Drag to reorder ${g.label}`}
          >
            <GripVerticalIcon size={12} />
          </button>
          <button
            className="flex min-w-0 flex-1 items-baseline gap-1.5 text-left"
            onPointerDown={(e) => startDrag(e, g.layoutKey)}
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
              nudge(g.layoutKey, e.key === "ArrowUp" ? -1 : 1);
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
  };

  const renderGroup = (t: Extract<TopItem, { type: "group" }>) => {
    const kids = t.children.flatMap((k) => {
      const g = byLayoutKey.get(k);
      return g ? [g] : [];
    });
    const isCollapsed = !searching && collapsed.has(t.key);
    const hasActive = kids.some((g) => g.sessions.some((s) => s.id === activeId));
    return (
      <div
        key={t.key}
        ref={blockRef(t.key)}
        className={`relative mb-1 ${drag?.key === t.key ? "opacity-50" : ""}`}
      >
        {line?.key === t.key && <DropLine top={line.edge === "top"} />}
        <div
          ref={blockRef(t.key, headerEls)}
          className={`group/header mx-2 mt-2 flex items-center gap-1 rounded-lg py-1 pl-2.5 pr-1 hover:bg-panel-2/50 ${
            ring === t.key ? "ring-1 ring-accent" : ""
          }`}
          onContextMenu={(e) => {
            e.preventDefault();
            setAgentMenu(null);
            setMenu({ x: e.clientX, y: e.clientY, key: t.key });
          }}
        >
          <button
            className="-ml-2 flex h-6 w-3 shrink-0 cursor-grab touch-none items-center justify-center rounded text-faint opacity-0 hover:text-fg focus-visible:opacity-100 group-hover/header:opacity-100"
            onPointerDown={(e) => startDrag(e, t.key)}
            tabIndex={-1}
            title="Drag to reorder"
            aria-label={`Drag to reorder ${t.name}`}
          >
            <GripVerticalIcon size={12} />
          </button>
          <button
            className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
            onPointerDown={(e) => startDrag(e, t.key)}
            onClick={() => {
              if (dragged.current) {
                dragged.current = false;
                return;
              }
              toggle(t.key);
            }}
            onKeyDown={(e) => {
              if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
              e.preventDefault();
              nudge(t.key, e.key === "ArrowUp" ? -1 : 1);
            }}
            title={`${t.name}: ${kids.length} ${kids.length === 1 ? "folder" : "folders"}\nDrag to reorder`}
            aria-expanded={!isCollapsed}
          >
            <span className="min-w-0 truncate text-[12px] font-semibold uppercase tracking-wide text-muted">{t.name}</span>
            <span className="shrink-0 text-[11px] text-faint">{kids.length}</span>
            <span className={`flex shrink-0 text-faint transition-transform ${isCollapsed ? "" : "rotate-90"}`}>
              <ChevronRightIcon size={12} />
            </span>
            {isCollapsed && hasActive && <Dot size={6} className="self-center text-accent" />}
          </button>
        </div>
        {!isCollapsed &&
          (kids.length > 0 ? (
            <div className="ml-3 border-l border-line/60">{kids.map(renderFolder)}</div>
          ) : (
            <div className="px-4 py-1 text-xs text-faint">No folders. Drag one here.</div>
          ))}
      </div>
    );
  };

  /** Names a group, then makes it, holding `folder` when given. */
  const newGroup = async (folder?: string) => {
    const { ok, text } = await askConfirm({
      title: "New group",
      body: folder ? "Name a group to put this folder in." : "Name the group.",
      confirmLabel: "Create",
      input: { value: "", label: "Group name" },
    });
    if (ok && text) changeLayout(planNewGroup(live.current.layout ?? [], live.current.top, text, folder));
  };

  const renameGroup = async (key: string, name: string) => {
    const { ok, text } = await askConfirm({
      title: "Rename group",
      body: `Rename "${name}".`,
      confirmLabel: "Rename",
      input: { value: name, label: "New name" },
    });
    if (ok && text) changeLayout(planRename(live.current.layout ?? [], key, text));
  };

  /** What the right-click menu on a folder or group header offers. */
  const menuActions = (key: string): ItemAction[] => {
    const fold: ItemAction[] = [
      { label: "Collapse all", onSelect: () => setAllCollapsed(true) },
      { label: "Expand all", onSelect: () => setAllCollapsed(false) },
    ];
    if (key.startsWith(GROUP_PREFIX)) {
      const t = top.find((x) => x.key === key);
      const name = t?.type === "group" ? t.name : "";
      return [
        { label: "Rename group…", onSelect: () => renameGroup(key, name) },
        { label: "Ungroup", onSelect: () => changeLayout(planUngroup(live.current.layout ?? [], live.current.top, key)) },
        ...fold,
      ];
    }
    const at = locate(top, key);
    const into = top.flatMap((t) =>
      t.type === "group" && t.key !== at?.parent
        ? [{ label: `Move to ${t.name}`, onSelect: () => moveTo(key, { parent: t.key, index: t.children.length }) }]
        : [],
    );
    const out: ItemAction[] =
      at && at.parent !== ""
        ? [
            {
              label: "Remove from group",
              onSelect: () => moveTo(key, { parent: "", index: top.findIndex((t) => t.key === at.parent) + 1 }),
            },
          ]
        : [];
    return [...into, { label: "New group…", onSelect: () => newGroup(key) }, ...out, ...fold];
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
      {top.map((t) => {
        if (t.type === "group") return renderGroup(t);
        const g = byLayoutKey.get(t.key);
        return g ? renderFolder(g) : null;
      })}
      {menu && (
        <ContextMenu x={menu.x} y={menu.y} actions={menuActions(menu.key)} onClose={() => setMenu(null)} />
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
