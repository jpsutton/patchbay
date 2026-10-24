import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode, type RefObject } from "react";
import { ActionSheet, useHold, type ItemAction } from "../components/ItemMenu";
import { Dot } from "../components/Dot";
import { ChevronRightIcon, MonitorIcon, PlusIcon, StarIcon } from "../components/icons";
import { fmtShort } from "../lib/format";
import { api, errorText, type CodeSession } from "../lib/ipc";
import { promptRename, sessionActions } from "../lib/itemActions";
import { loadStringList, saveStringList, useManualOrder } from "../lib/manualOrder";
import { memberName, memberOnline, useStore } from "../lib/store";

/** What a gateway knows a Code session is doing. */
export interface Live {
  status: string;
  busy: boolean;
  permissions: string[];
  last: number;
}

const ORDER_KEY = "patchbay.remote.codeProjectOrder";
const COLLAPSED_KEY = "patchbay.remote.codeProjectCollapsed";
/** How far the pointer moves before a press on a grip becomes a drag. */
const DRAG_THRESHOLD = 4;

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

function projectKey(s: CodeSession): string {
  return `${s.hostNode ?? ""}:${s.cwd}`;
}

interface Project {
  key: string;
  host: string;
  cwd: string;
  label: string;
  where: string;
  /** Unpinned sessions, newest first. Pins are listed above the computers. */
  sessions: CodeSession[];
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
 * The phone's Code list. With `groupByProject`, sessions nest under the folder they run in,
 * and those folders drag into order within their computer. The order is this phone's alone.
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
  const [showArchived, setShowArchived] = useState(false);
  const [collapsed, setCollapsed] = useState(() => new Set(loadStringList(COLLAPSED_KEY)));
  const [menu, setMenu] = useState<{ title: string; actions: ItemAction[] } | null>(null);
  const [drag, setDrag] = useState<{ key: string; index: number } | null>(null);
  const headerEls = useRef(new Map<string, HTMLDivElement>());
  const dragged = useRef(false);

  const pinnedFirst = (a: CodeSession, b: CodeSession) => Number(b.pinned) - Number(a.pinned) || b.updatedAt - a.updatedAt;
  const active = useMemo(() => sessions.filter((s) => s.archivedAt == null), [sessions]);
  const archived = useMemo(() => sessions.filter((s) => s.archivedAt != null), [sessions]);

  const projects = useMemo<Project[]>(() => {
    const by = new Map<string, { host: string; cwd: string; list: CodeSession[] }>();
    for (const s of active) {
      const key = projectKey(s);
      const g = by.get(key) ?? { host: s.hostNode ?? "", cwd: s.cwd, list: [] };
      g.list.push(s);
      by.set(key, g);
    }
    return [...by.values()].map(({ host, cwd, list }) => ({
      key: `${host}:${cwd}`,
      host,
      cwd,
      label: basename(cwd),
      where: parentLabel(cwd),
      sessions: list.filter((s) => !s.pinned).sort((a, b) => b.updatedAt - a.updatedAt),
    }));
  }, [active]);

  // Every project, including one whose sessions are all pinned, so unpinning puts it back where it was.
  const rankKeys = useMemo(() => {
    const by = new Map<string, number>();
    for (const s of active) by.set(projectKey(s), Math.max(by.get(projectKey(s)) ?? 0, s.updatedAt));
    return [...by.entries()].sort((a, b) => b[1] - a[1]).map(([k]) => k);
  }, [active]);
  const { ordered, move } = useManualOrder(ORDER_KEY, rankKeys);
  const orderedProjects = useMemo(() => {
    const byKey = new Map(projects.map((p) => [p.key, p]));
    return ordered.flatMap((k) => {
      const p = byKey.get(k);
      return p && p.sessions.length > 0 ? [p] : [];
    });
  }, [projects, ordered]);

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

  const slotAt = (keys: string[], y: number) => {
    const i = keys.findIndex((k) => {
      const r = headerEls.current.get(k)?.getBoundingClientRect();
      return r != null && y < r.top + r.height / 2;
    });
    return i < 0 ? keys.length : i;
  };

  const startDrag = (e: ReactPointerEvent<HTMLButtonElement>, key: string, hostKeys: string[]) => {
    if (e.button !== 0) return;
    e.stopPropagation();
    const startY = e.clientY;
    const el = e.currentTarget;
    let index: number | null = null;
    dragged.current = false;
    el.setPointerCapture(e.pointerId);
    const onMove = (ev: PointerEvent) => {
      if (index == null) {
        if (Math.abs(ev.clientY - startY) < DRAG_THRESHOLD) return;
        document.body.style.cursor = "grabbing";
        document.body.style.userSelect = "none";
        dragged.current = true;
      }
      index = slotAt(hostKeys, ev.clientY);
      setDrag({ key, index });
    };
    const up = (ev: PointerEvent) => {
      el.removeEventListener("pointermove", onMove);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
      if (index == null) return;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      setDrag(null);
      if (ev.type === "pointerup") move(key, index, hostKeys);
    };
    el.addEventListener("pointermove", onMove);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
  };

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
          {hosts.map((host) => {
            const list = orderedProjects.filter((p) => p.host === host);
            if (list.length === 0) return null;
            const hostKeys = list.map((p) => p.key);
            return (
              <section key={host}>
                <Section>
                  <MonitorIcon size={12} />
                  {memberName(mesh, host) || "unknown computer"}
                  {!memberOnline(mesh, host) && <span className="font-sans normal-case tracking-normal">· offline</span>}
                </Section>
                {list.map((p, i) => (
                  <ProjectBlock
                    key={p.key}
                    project={p}
                    index={i}
                    hostKeys={hostKeys}
                    collapsed={collapsed.has(p.key)}
                    online={memberOnline(mesh, p.host)}
                    dragging={drag?.key === p.key}
                    dropAt={dropIndex(drag, hostKeys)}
                    headerEls={headerEls}
                    onToggle={() => toggleCollapsed(p.key)}
                    onDrag={(e) => startDrag(e, p.key, hostKeys)}
                    onReorder={() =>
                      setMenu({
                        title: p.label,
                        actions: [
                          { label: "Move up", onSelect: () => move(p.key, i - 1, hostKeys) },
                          { label: "Move down", onSelect: () => move(p.key, i + 2, hostKeys) },
                        ],
                      })
                    }
                    onNew={() => onNewIn(p.host, p.cwd)}
                  >
                    {p.sessions.map((s) => row(s, true))}
                  </ProjectBlock>
                ))}
              </section>
            );
          })}
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

/** Where the dragged project will land, or null when the pointer is still over its own slot. */
function dropIndex(drag: { key: string; index: number } | null, hostKeys: string[]): number | null {
  if (!drag || !hostKeys.includes(drag.key)) return null;
  const from = hostKeys.indexOf(drag.key);
  if (drag.index === from || drag.index === from + 1) return null;
  return drag.index;
}

function ProjectBlock({
  project,
  index,
  hostKeys,
  collapsed,
  online,
  dragging,
  dropAt,
  headerEls,
  onToggle,
  onDrag,
  onReorder,
  onNew,
  children,
}: {
  project: Project;
  index: number;
  hostKeys: string[];
  collapsed: boolean;
  online: boolean;
  dragging: boolean;
  dropAt: number | null;
  headerEls: RefObject<Map<string, HTMLDivElement>>;
  onToggle: () => void;
  onDrag: (e: ReactPointerEvent<HTMLButtonElement>) => void;
  onReorder: () => void;
  onNew: () => void;
  children: ReactNode;
}) {
  const hold = useHold(onReorder);
  useEffect(() => {
    const els = headerEls.current;
    const key = project.key;
    return () => {
      els.delete(key);
    };
  }, [headerEls, project.key]);
  return (
    <div
      ref={(el) => {
        if (el) headerEls.current.set(project.key, el);
      }}
      className={dragging ? "opacity-40" : ""}
    >
      {dropAt === index && <div className="mx-4 my-0.5 h-0.5 rounded-full bg-accent" />}
      <div className="mx-2 mt-0.5 flex items-center gap-0.5 rounded-lg py-0.5 pl-1 pr-1">
        <button
          className="flex h-9 w-7 shrink-0 touch-none items-center justify-center"
          aria-label={`Reorder ${project.label}`}
          onPointerDown={onDrag}
        >
          <span className="grid grid-cols-2 gap-[3px]" aria-hidden>
            {Array.from({ length: 6 }, (_, n) => (
              <span key={n} className="h-[3px] w-[3px] rounded-full bg-faint/80" />
            ))}
          </span>
        </button>
        <button className="flex min-w-0 flex-1 items-center gap-1.5 py-1.5 text-left" aria-expanded={!collapsed} onClick={onToggle} {...hold}>
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
      {dropAt === hostKeys.length && index === hostKeys.length - 1 && <div className="mx-4 my-0.5 h-0.5 rounded-full bg-accent" />}
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
