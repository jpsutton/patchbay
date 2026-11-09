import { useMemo, useState, type ReactNode } from "react";
import { ActionSheet, useHold, type ItemAction } from "../components/ItemMenu";
import { Dot } from "../components/Dot";
import { ChevronRightIcon, MonitorIcon, PlusIcon, StarIcon } from "../components/icons";
import { fmtShort } from "../lib/format";
import { api, errorText, type CodeSession } from "../lib/ipc";
import { promptRename, sessionActions } from "../lib/itemActions";
import { loadStringList, saveStringList } from "../lib/manualOrder";
import { memberName, memberOnline, useStore } from "../lib/store";

/** What a gateway knows a Code session is doing. */
export interface Live {
  status: string;
  busy: boolean;
  permissions: string[];
  /** Open agent questions; absent from older hosts. */
  questions?: string[];
  /** Mid-turn with nothing from the agent for the host's stall limit; absent from older hosts. */
  stalled?: boolean;
  last: number;
}

/** Waiting on the user: an approval or an agent question. */
export const liveNeedsYou = (l?: Live) => (l?.permissions.length ?? 0) > 0 || (l?.questions?.length ?? 0) > 0;

const PROJECT_COLLAPSED_KEY = "patchbay.remote.codeProjectCollapsed";
const HOST_COLLAPSED_KEY = "patchbay.remote.codeHostCollapsed";

function basename(p: string): string {
  return p.split(/[\\/]/).filter(Boolean).pop() ?? p;
}

/** Parent of `p`, with `/home/name` and `/Users/name` shortened to `~`. */
function parentLabel(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  parts.pop();
  const home = parts.length >= 2 && parts[0] === "home" ? 2 : parts[0] === "Users" ? 2 : 0;
  return (home ? "~/" : "/") + parts.slice(home).join("/");
}

/** `/a/b` and `/a/b/` are one folder. */
function dirOf(s: CodeSession): string {
  return s.cwd.length > 1 ? s.cwd.replace(/[\\/]+$/, "") || s.cwd : s.cwd;
}

/**
 * The folder's key on this phone: host + cwd, so the same path on two computers stays two projects.
 */
function projectKey(s: CodeSession): string {
  return s.hostNode ? `${s.hostNode}:${dirOf(s)}` : dirOf(s);
}

interface Project {
  key: string;
  host: string;
  cwd: string;
  label: string;
  /** Parent path for the subtitle (host is already the section). */
  where: string;
  /** Unpinned sessions, newest first. Pins are listed above the folders when grouping by project. */
  sessions: CodeSession[];
  latest: number;
}

/** The collapsed "Archived" row at the end of a list. */
export function ArchivedRow({ count, open, onToggle }: { count: number; open: boolean; onToggle: () => void }) {
  if (count === 0) return null;
  return (
    <button
      className="flex w-full items-center gap-2 px-4 pb-1.5 pt-5 text-left font-mono text-[11px] uppercase tracking-[0.06em] text-faint"
      aria-expanded={open}
      onClick={onToggle}
    >
      <span className={`inline-block transition-transform ${open ? "rotate-90" : ""}`}>›</span>
      Archived · {count}
    </button>
  );
}

/**
 * The phone's Code list. Always nests under each mesh host (online first). With `groupByProject`,
 * projects nest under the host; both host and project headers collapse. `onlineOnly` hides
 * offline hosts and their sessions.
 */
export function CodeSessionList({
  live,
  groupByProject,
  onlineOnly,
  activeId,
  onOpen,
  onNewIn,
}: {
  live: Record<string, Live>;
  groupByProject: boolean;
  onlineOnly?: boolean;
  activeId?: string | null;
  onOpen: (id: string) => void;
  /** Start a session already pointed at this computer and folder. */
  onNewIn: (host: string, cwd: string) => void;
}) {
  const sessions = useStore((s) => s.codeSessions);
  const mesh = useStore((s) => s.mesh);
  const [showArchived, setShowArchived] = useState(false);
  const [collapsedProjects, setCollapsedProjects] = useState(() => new Set(loadStringList(PROJECT_COLLAPSED_KEY)));
  const [collapsedHosts, setCollapsedHosts] = useState(() => new Set(loadStringList(HOST_COLLAPSED_KEY)));
  const [menu, setMenu] = useState<{ title: string; actions: ItemAction[] } | null>(null);

  const pinnedFirst = (a: CodeSession, b: CodeSession) => Number(b.pinned) - Number(a.pinned) || b.updatedAt - a.updatedAt;
  const hostOf = (s: CodeSession) => s.hostNode ?? "";
  const isOnline = (host: string) => memberOnline(mesh, host);

  const active = useMemo(() => {
    const list = sessions.filter((s) => s.archivedAt == null);
    return onlineOnly ? list.filter((s) => isOnline(hostOf(s))) : list;
  }, [sessions, mesh, onlineOnly]);

  const archived = useMemo(() => {
    const list = sessions.filter((s) => s.archivedAt != null);
    return onlineOnly ? list.filter((s) => isOnline(hostOf(s))) : list;
  }, [sessions, mesh, onlineOnly]);

  const projects = useMemo(() => {
    const by = new Map<string, CodeSession[]>();
    for (const s of active) {
      const k = projectKey(s);
      const list = by.get(k);
      if (list) list.push(s);
      else by.set(k, [s]);
    }
    const out = new Map<string, Project>();
    for (const [key, list] of by) {
      const newest = [...list].sort((a, b) => b.updatedAt - a.updatedAt);
      const top = newest[0];
      const host = hostOf(top);
      const cwd = dirOf(top);
      out.set(key, {
        key,
        host,
        cwd,
        label: basename(cwd),
        where: parentLabel(cwd),
        sessions: newest.filter((s) => !s.pinned),
        latest: top.updatedAt,
      });
    }
    return out;
  }, [active]);

  const hosts = useMemo(() => {
    const ids: string[] = [];
    for (const s of [...active].sort((a, b) => b.updatedAt - a.updatedAt)) {
      const h = hostOf(s);
      if (!ids.includes(h)) ids.push(h);
    }
    ids.sort((a, b) => Number(isOnline(b)) - Number(isOnline(a)));
    return ids;
  }, [active, mesh]);

  const pinned = useMemo(() => active.filter((s) => s.pinned).sort((a, b) => b.updatedAt - a.updatedAt), [active]);

  const toggleProject = (key: string) =>
    setCollapsedProjects((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      saveStringList(PROJECT_COLLAPSED_KEY, [...next]);
      return next;
    });

  const toggleHost = (key: string) =>
    setCollapsedHosts((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      saveStringList(HOST_COLLAPSED_KEY, [...next]);
      return next;
    });

  const openSession = async (s: CodeSession) => {
    try {
      if (s.archivedAt != null) {
        await api.updateCodeSession(s.id, { archived: false });
        await useStore.getState().refreshCodeSessions();
      }
      onOpen(s.id);
    } catch (e) {
      useStore.getState().setToast(errorText(e));
    }
  };

  const row = (s: CodeSession, indent: boolean) => {
    const l = live[s.id];
    return (
      <Row
        key={s.id}
        title={s.title}
        sub={indent ? fmtShort(s.updatedAt) : `${basename(s.cwd)} · ${fmtShort(s.updatedAt)}`}
        indent={indent}
        here={s.id === activeId}
        leading={s.pinned ? <StarIcon size={13} /> : null}
        trailing={<SessionState live={l} online={isOnline(hostOf(s))} />}
        onClick={() => openSession(s)}
        onHold={() =>
          setMenu({
            title: s.title,
            actions: sessionActions(s, {
              desktop: false,
              local: false,
              running: !!l && (l.busy || l.status === "ready" || l.status === "starting"),
              rename: async () => {
                const t = await promptRename(s.title);
                if (!t) return;
                await api.updateCodeSession(s.id, { title: t });
                await useStore.getState().refreshCodeSessions();
              },
            }),
          })
        }
      />
    );
  };

  /** A folder with the sessions in it, or nothing when all of them are pinned. */
  const folder = (p: Project) => {
    if (p.sessions.length === 0) return null;
    return (
      <ProjectBlock
        key={p.key}
        project={p}
        collapsed={collapsedProjects.has(p.key)}
        online={isOnline(p.host)}
        onToggle={() => toggleProject(p.key)}
        onNew={() => onNewIn(p.host, p.cwd)}
      >
        {p.sessions.map((s) => row(s, true))}
      </ProjectBlock>
    );
  };

  const hostSection = (host: string) => {
    const online = isOnline(host);
    const hostSessions = active.filter((s) => hostOf(s) === host);
    const hostProjects = [...projects.values()]
      .filter((p) => p.host === host && p.sessions.length > 0)
      .sort((a, b) => b.latest - a.latest);
    // When grouping by project, pinned sessions live in the Pinned section; a host with only
    // pins has nothing left to show here.
    if (groupByProject ? hostProjects.length === 0 : hostSessions.length === 0) return null;
    const collapsed = collapsedHosts.has(host);
    const bodyCount = groupByProject ? hostProjects.length : hostSessions.length;

    return (
      <section key={host || "local"}>
        <HostHeader
          name={memberName(mesh, host) || "unknown computer"}
          online={online}
          collapsed={collapsed}
          count={bodyCount}
          onToggle={() => toggleHost(host)}
        />
        {!collapsed &&
          (groupByProject
            ? hostProjects.map((p) => folder(p))
            : hostSessions.sort(pinnedFirst).map((s) => row(s, false)))}
      </section>
    );
  };

  return (
    <>
      {/* The fixed ActionSheet overlay stays outside the clip wrapper, so it can't be clipped. */}
      {menu && <ActionSheet title={menu.title} actions={menu.actions} onClose={() => setMenu(null)} />}
      {/*
       * The list's scroller is a horizontal scroll container (overflow-y:auto coerces overflow-x to
       * auto). Clip one level inside it so a child wider than the phone (a long title or mono path)
       * can't extend scrollWidth and leave the list offset or show a horizontal scrollbar — the same
       * phantom-scroll root cause as the desktop sidebar. `clip` creates no scroll container.
       */}
      <div className="min-w-0 overflow-x-clip">
        {groupByProject && pinned.length > 0 && (
          <section>
            <div className="flex items-center gap-2 px-4 pb-1.5 pt-4 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">
              Pinned
            </div>
            {pinned.map((s) => row(s, false))}
          </section>
        )}
        {hosts.map((host) => hostSection(host))}
        {hosts.length === 0 && active.length === 0 && archived.length === 0 && (
          <div className="px-4 py-8 text-center text-[14px] text-muted">
            {onlineOnly ? "No sessions on online computers." : "No code sessions yet."}
          </div>
        )}
        <ArchivedRow count={archived.length} open={showArchived} onToggle={() => setShowArchived(!showArchived)} />
        {showArchived && archived.map((s) => row(s, false))}
      </div>
    </>
  );
}

function HostHeader({
  name,
  online,
  collapsed,
  count,
  onToggle,
}: {
  name: string;
  online: boolean;
  collapsed: boolean;
  count: number;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      className="flex w-full items-center gap-2 px-4 pb-1.5 pt-4 text-left font-mono text-[11px] uppercase tracking-[0.06em] text-faint"
      aria-expanded={!collapsed}
      onClick={onToggle}
    >
      <ChevronRightIcon size={13} className={`shrink-0 transition-transform ${collapsed ? "" : "rotate-90"}`} />
      <MonitorIcon size={12} className="shrink-0" />
      <span className="min-w-0 truncate">{name}</span>
      {!online && <span className="shrink-0 font-sans normal-case tracking-normal">· offline</span>}
      {collapsed && <span className="shrink-0">{count}</span>}
    </button>
  );
}

function ProjectBlock({
  project,
  collapsed,
  online,
  onToggle,
  onNew,
  children,
}: {
  project: Project;
  collapsed: boolean;
  online: boolean;
  onToggle: () => void;
  onNew: () => void;
  children: ReactNode;
}) {
  return (
    <div>
      <div className="mx-2 mt-0.5 flex items-center gap-0.5 rounded-lg py-0.5 pl-2 pr-1">
        <button className="flex min-w-0 flex-1 items-center gap-1.5 py-2 text-left" aria-expanded={!collapsed} onClick={onToggle}>
          <span className="min-w-0 truncate text-[13px] font-medium text-fg/90">{project.label}</span>
          <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-faint">{project.where}</span>
          <ChevronRightIcon size={14} className={`shrink-0 text-faint transition-transform ${collapsed ? "" : "rotate-90"}`} />
          {collapsed && <span className="font-mono text-[11px] text-faint">{project.sessions.length}</span>}
        </button>
        {online && (
          <button
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg text-faint active:bg-panel-2 active:text-fg"
            aria-label={`New session in ${project.label}`}
            title={`New session in ${project.cwd}`}
            onClick={onNew}
          >
            <PlusIcon size={16} />
          </button>
        )}
      </div>
      {!collapsed && children}
    </div>
  );
}

function SessionState({ live, online }: { live?: Live; online: boolean }) {
  if (!online) return <span className="text-[12px] text-faint">Offline</span>;
  if (liveNeedsYou(live))
    return <span className="rounded-full bg-accent px-2 py-0.5 text-[12px] font-semibold text-accent-fg">Needs you</span>;
  if (live?.busy && live.stalled)
    return (
      <span className="flex items-center gap-1.5 text-[12px] text-warn">
        <Dot size={6} className="text-warn" />
        Gone quiet
      </span>
    );
  if (live?.busy)
    return (
      <span className="flex items-center gap-1.5 text-[12px] text-live">
        <Dot size={6} className="signal-pulse text-live" />
        Working
      </span>
    );
  if (live?.status === "ready") return <span className="text-[12px] text-ok">Ready</span>;
  return null;
}

function Row({
  title,
  sub,
  indent,
  here,
  leading,
  trailing,
  onClick,
  onHold,
}: {
  title: string;
  sub?: string;
  indent?: boolean;
  here?: boolean;
  leading?: ReactNode;
  trailing?: ReactNode;
  onClick: () => void;
  onHold?: () => void;
}) {
  const hold = useHold(onHold ?? (() => {}));
  return (
    <button
      className={`flex w-full select-none items-center gap-3 py-3 text-left [-webkit-touch-callout:none] active:bg-panel-2 ${
        indent ? "py-3 pl-8 pr-4" : "px-4"
      } ${here ? "bg-panel" : ""}`}
      onClick={onClick}
      {...(onHold ? hold : {})}
      aria-current={here ? "true" : undefined}
    >
      {leading && <span className="shrink-0 text-accent-ink">{leading}</span>}
      <span className="flex min-w-0 flex-1 flex-col gap-0.5">
        <span className="truncate text-[15px]">{title}</span>
        {sub && <span className="truncate text-[12.5px] text-faint">{sub}</span>}
      </span>
      {trailing && <span className="shrink-0">{trailing}</span>}
    </button>
  );
}
