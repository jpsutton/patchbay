import { useMemo, useState, type ReactNode } from "react";
import { ActionSheet, useHold, type ItemAction } from "../components/ItemMenu";
import { Dot } from "../components/Dot";
import { ChevronRightIcon, MonitorIcon, PlusIcon, StarIcon } from "../components/icons";
import { fmtShort } from "../lib/format";
import { api, errorText, type CodeSession } from "../lib/ipc";
import { promptRename, sessionActions } from "../lib/itemActions";
import { loadStringList, saveStringList } from "../lib/manualOrder";
import { arrange } from "../lib/sidebarLayout";
import { memberName, memberOnline, useStore } from "../lib/store";

/** What a gateway knows a Code session is doing. */
export interface Live {
  status: string;
  busy: boolean;
  permissions: string[];
  last: number;
}

const COLLAPSED_KEY = "patchbay.remote.codeProjectCollapsed";

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
 * The folder's key in the Code sidebar layout. It is the same on every computer, so it names
 * the computer the folder is on (see `CodeSidebar`).
 */
function projectKey(s: CodeSession): string {
  return s.hostNode ? `${s.hostNode}:${dirOf(s)}` : dirOf(s);
}

interface Project {
  key: string;
  host: string;
  cwd: string;
  label: string;
  where: string;
  /** Unpinned sessions, newest first. Pins are listed above the folders. */
  sessions: CodeSession[];
  /** When the folder's newest session, pinned or not, last changed. */
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
 * The phone's Code list. With `groupByProject`, sessions nest under the folder they run in, and
 * the folders follow the Code sidebar's layout: the groups and the order set on the computers.
 * The phone only shows that layout; it is changed on a computer.
 */
export function CodeSessionList({
  live,
  groupByProject,
  activeId,
  onOpen,
  onNewIn,
}: {
  live: Record<string, Live>;
  groupByProject: boolean;
  activeId?: string | null;
  onOpen: (id: string) => void;
  /** Start a session already pointed at this computer and folder. */
  onNewIn: (host: string, cwd: string) => void;
}) {
  const sessions = useStore((s) => s.codeSessions);
  const mesh = useStore((s) => s.mesh);
  const layout = useStore((s) => s.sidebarLayout);
  const [showArchived, setShowArchived] = useState(false);
  const [collapsed, setCollapsed] = useState(() => new Set(loadStringList(COLLAPSED_KEY)));
  const [menu, setMenu] = useState<{ title: string; actions: ItemAction[] } | null>(null);

  const pinnedFirst = (a: CodeSession, b: CodeSession) => Number(b.pinned) - Number(a.pinned) || b.updatedAt - a.updatedAt;
  const active = useMemo(() => sessions.filter((s) => s.archivedAt == null), [sessions]);
  const archived = useMemo(() => sessions.filter((s) => s.archivedAt != null), [sessions]);

  // Every folder, including one whose sessions are all pinned, so unpinning puts it back where it was.
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
      const host = top.hostNode ?? "";
      const cwd = dirOf(top);
      out.set(key, {
        key,
        host,
        cwd,
        label: basename(cwd),
        // The same folder name can live on two computers: say which one this is.
        where: host ? `${memberName(mesh, host) || "unknown computer"}:${parentLabel(cwd)}` : parentLabel(cwd),
        sessions: newest.filter((s) => !s.pinned),
        latest: top.updatedAt,
      });
    }
    return out;
  }, [active, mesh]);

  // Folders and groups in the layout's order. A folder it has never seen goes on top, newest first.
  const top = useMemo(
    () => arrange([...projects.values()].sort((a, b) => b.latest - a.latest).map((p) => p.key), layout ?? []).top,
    [projects, layout],
  );

  const hosts = useMemo(() => {
    const ids: string[] = [];
    for (const s of [...active].sort((a, b) => b.updatedAt - a.updatedAt)) {
      const h = s.hostNode ?? "";
      if (!ids.includes(h)) ids.push(h);
    }
    ids.sort((a, b) => Number(memberOnline(mesh, b)) - Number(memberOnline(mesh, a)));
    return ids;
  }, [active, mesh]);

  const pinned = useMemo(() => active.filter((s) => s.pinned).sort((a, b) => b.updatedAt - a.updatedAt), [active]);

  const toggleCollapsed = (key: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      saveStringList(COLLAPSED_KEY, [...next]);
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
        trailing={<SessionState live={l} online={memberOnline(mesh, s.hostNode ?? "")} />}
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
  const folder = (key: string) => {
    const p = projects.get(key);
    if (!p || p.sessions.length === 0) return null;
    return (
      <ProjectBlock
        key={p.key}
        project={p}
        collapsed={collapsed.has(p.key)}
        online={memberOnline(mesh, p.host)}
        onToggle={() => toggleCollapsed(p.key)}
        onNew={() => onNewIn(p.host, p.cwd)}
      >
        {p.sessions.map((s) => row(s, true))}
      </ProjectBlock>
    );
  };

  return (
    <>
      {menu && <ActionSheet title={menu.title} actions={menu.actions} onClose={() => setMenu(null)} />}
      {groupByProject ? (
        <>
          {pinned.length > 0 && (
            <section>
              <Section>Pinned</Section>
              {pinned.map((s) => row(s, false))}
            </section>
          )}
          <div className="pt-2">
            {top.map((t) => {
              if (t.type === "folder") return folder(t.key);
              // A group with no folder to show is left out.
              const kids = t.children.filter((k) => (projects.get(k)?.sessions.length ?? 0) > 0);
              if (kids.length === 0) return null;
              const open = !collapsed.has(t.key);
              return (
                <div key={t.key}>
                  <GroupHeader name={t.name} count={kids.length} collapsed={!open} onToggle={() => toggleCollapsed(t.key)} />
                  {open && <div className="ml-3 border-l border-line">{kids.map((k) => folder(k))}</div>}
                </div>
              );
            })}
          </div>
        </>
      ) : (
        hosts.map((host) => (
          <section key={host}>
            <Section>
              <MonitorIcon size={12} />
              {memberName(mesh, host) || "unknown computer"}
              {!memberOnline(mesh, host) && <span className="font-sans normal-case tracking-normal">· offline</span>}
            </Section>
            {active
              .filter((s) => (s.hostNode ?? "") === host)
              .sort(pinnedFirst)
              .map((s) => row(s, false))}
          </section>
        ))
      )}
      <ArchivedRow count={archived.length} open={showArchived} onToggle={() => setShowArchived(!showArchived)} />
      {showArchived && archived.map((s) => row(s, false))}
    </>
  );
}

/** A user-made group of folders, shown above them. */
function GroupHeader({
  name,
  count,
  collapsed,
  onToggle,
}: {
  name: string;
  count: number;
  collapsed: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      className="mt-2 flex w-full items-center gap-2 px-4 py-2 text-left font-mono text-[11px] uppercase tracking-[0.06em] text-faint"
      aria-expanded={!collapsed}
      onClick={onToggle}
    >
      <ChevronRightIcon size={13} className={`shrink-0 transition-transform ${collapsed ? "" : "rotate-90"}`} />
      <span className="min-w-0 truncate">{name || "Group"}</span>
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
          <span className="shrink-0 truncate text-[13px] font-medium text-fg/90">{project.label}</span>
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

function Section({ children }: { children: ReactNode }) {
  return (
    <div className="flex items-center gap-2 px-4 pb-1.5 pt-4 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">
      {children}
    </div>
  );
}

function SessionState({ live, online }: { live?: Live; online: boolean }) {
  if (!online) return <span className="text-[12px] text-faint">Offline</span>;
  if (live && live.permissions.length > 0)
    return <span className="rounded-full bg-accent px-2 py-0.5 text-[12px] font-semibold text-accent-fg">Needs you</span>;
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
