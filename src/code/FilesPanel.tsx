import { useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { api } from "../lib/ipc";
import { useStore } from "../lib/store";
import { canOpen, FileLinkContext, openLocalPath } from "../components/FileLink";
import { normalizePath, type FileChange } from "./changes";
import {
  CheckIcon,
  ChevronDownIcon,
  ChevronRightIcon,
  FileIcon,
  FolderIcon,
  RestartIcon,
  XIcon,
} from "../components/icons";

/**
 * The Files blade: the session folder as a browsable tree.
 *
 * Files come from the same recursive list the @-mentions use
 * (`listProjectFiles` — honors .gitignore, skips hidden files), grouped
 * client-side by `/` segments. Select files and attach them to the
 * composer's draft as @-mentions; rows the agent changed carry the same
 * A/M/D badge as the Changes blade.
 */

const STATUS = {
  added: { letter: "A", tone: "text-ok" },
  modified: { letter: "M", tone: "text-accent-ink" },
  deleted: { letter: "D", tone: "text-danger" },
} as const;

interface TreeDir {
  name: string;
  path: string;
  dirs: TreeDir[];
  files: string[];
}

function buildTree(paths: string[]): TreeDir {
  const root: TreeDir = { name: "", path: "", dirs: [], files: [] };
  for (const p of paths) {
    const parts = p.split("/");
    let node = root;
    let cur = "";
    for (let i = 0; i < parts.length - 1; i++) {
      cur = cur ? `${cur}/${parts[i]}` : parts[i];
      let child = node.dirs.find((d) => d.path === cur);
      if (!child) {
        child = { name: parts[i], path: cur, dirs: [], files: [] };
        node.dirs.push(child);
      }
      node = child;
    }
    node.files.push(p);
  }
  return root;
}

function countFiles(dir: TreeDir): number {
  return dir.files.length + dir.dirs.reduce((n, d) => n + countFiles(d), 0);
}

function dirMatches(dir: TreeDir, q: string): boolean {
  if (dir.files.some((f) => f.toLowerCase().includes(q))) return true;
  return dir.dirs.some((d) => dirMatches(d, q));
}

export function FilesPanel({
  cwd,
  sessionId,
  changes,
  onClose,
}: {
  cwd: string;
  sessionId: string;
  changes: FileChange[];
  onClose: () => void;
}) {
  const [files, setFiles] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refreshTick, setRefreshTick] = useState(0);
  const [expanded, setExpanded] = useState<Set<string> | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [filter, setFilter] = useState("");
  // Opening needs the session's files on this computer and the desktop app (as file links do).
  const { local } = useContext(FileLinkContext);
  const opens = local && canOpen();

  useEffect(() => {
    let cancelled = false;
    setError(null);
    api
      .listProjectFiles(cwd, sessionId)
      .then((list) => {
        if (!cancelled) setFiles(list);
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : "Could not list files");
      });
    return () => {
      cancelled = true;
    };
    // Re-list when the agent's changed files grow: files it created appear.
  }, [cwd, sessionId, refreshTick, changes.length]);

  const tree = useMemo(() => buildTree(files ?? []), [files]);
  const effectiveExpanded = useMemo(
    () => expanded ?? new Set(tree.dirs.map((d) => d.path)),
    [expanded, tree],
  );

  const changedByRel = useMemo(() => {
    const root = normalizePath(cwd, "/");
    const prefix = root.endsWith("/") ? root : root + "/";
    const map = new Map<string, FileChange["status"]>();
    for (const c of changes) {
      const rel =
        c.path.startsWith(prefix) && c.path.length > prefix.length
          ? c.path.slice(prefix.length)
          : c.path;
      map.set(rel, c.status);
    }
    return map;
  }, [changes, cwd]);

  const q = filter.trim().toLowerCase();
  const toggleDir = (path: string) =>
    setExpanded((prev) => {
      const next = new Set(prev ?? effectiveExpanded);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  const toggleFile = (path: string) =>
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });

  const attach = () => {
    if (selected.size === 0) return;
    const paths = [...selected].sort();
    useStore
      .getState()
      .insertIntoComposer(paths.map((p) => `@${p}`).join(" ") + " ", paths);
    setSelected(new Set());
  };

  const basename = cwd.split(/[\\/]/).filter(Boolean).pop() ?? cwd;

  const renderDir = (dir: TreeDir, depth: number): ReactNode => {
    // The root has no row of its own to toggle, so it is always open.
    const open = q || dir.path === "" ? true : effectiveExpanded.has(dir.path);
    const visibleFiles = dir.files.filter((f) => !q || f.toLowerCase().includes(q));
    const visibleDirs = dir.dirs.filter((d) => !q || dirMatches(d, q));
    if (q && visibleFiles.length === 0 && visibleDirs.length === 0) return null;
    return (
      <div key={dir.path || "/"}>
        {dir.path !== "" && (
          <button
            className="flex h-8 w-full items-center gap-1.5 rounded-md px-2 text-left text-fg/85 hover:bg-panel-2"
            style={{ paddingLeft: 8 + depth * 14 }}
            onClick={() => toggleDir(dir.path)}
          >
            {open ? <ChevronDownIcon size={14} /> : <ChevronRightIcon size={14} />}
            <FolderIcon size={14} className="shrink-0 text-muted" />
            <span className="min-w-0 flex-1 truncate text-[12px] font-medium">{dir.name}</span>
            <span className="meta">{countFiles(dir)}</span>
          </button>
        )}
        {open && (
          <>
            {visibleDirs.map((d) => renderDir(d, depth + 1))}
            {visibleFiles.map((f) => {
              const name = f.slice(f.lastIndexOf("/") + 1);
              const status = changedByRel.get(f);
              const sel = selected.has(f);
              return (
                <button
                  key={f}
                  className={`flex h-8 w-full items-center gap-1.5 rounded-md px-2 text-left hover:bg-panel-2 ${
                    sel ? "bg-panel-2 text-fg" : "text-fg/75"
                  }`}
                  style={{ paddingLeft: 8 + (depth + 1) * 14 }}
                  onClick={() => toggleFile(f)}
                  onDoubleClick={opens ? () => openLocalPath(f, cwd) : undefined}
                  title={f}
                >
                  <span
                    className={`flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded-[4px] border ${
                      sel ? "border-accent bg-accent text-accent-fg" : "border-fg/30"
                    }`}
                  >
                    {sel && <CheckIcon size={10} strokeWidth={3} />}
                  </span>
                  <FileIcon size={14} className="shrink-0 text-muted" />
                  <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{name}</span>
                  {status && (
                    <span className={`font-mono text-[11px] ${STATUS[status].tone}`} title={status}>
                      {STATUS[status].letter}
                    </span>
                  )}
                </button>
              );
            })}
          </>
        )}
      </div>
    );
  };

  const noMatch =
    files !== null &&
    files.length > 0 &&
    q &&
    tree.dirs.every((d) => !dirMatches(d, q)) &&
    !tree.files.some((f) => f.toLowerCase().includes(q));

  return (
    <div className="flex h-full flex-col">
      <div className="flex h-11 shrink-0 items-center gap-2 border-b border-line pl-4 pr-2">
        <span className="text-[13px] font-medium">Files</span>
        <span className="meta min-w-0 flex-1 truncate">{basename}</span>
        <button
          className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-panel-2 hover:text-fg"
          onClick={() => setRefreshTick((t) => t + 1)}
          aria-label="Refresh file list"
          title="Refresh"
        >
          <RestartIcon size={14} />
        </button>
        <button
          className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-panel-2 hover:text-fg"
          onClick={onClose}
          aria-label="Close files"
          title="Close"
        >
          <XIcon size={14} />
        </button>
      </div>
      <div className="shrink-0 border-b border-line p-2">
        <input
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Filter files…"
          aria-label="Filter files"
          className="w-full rounded-md border border-line bg-raised px-2.5 py-1.5 text-[12px] text-fg outline-none placeholder:text-faint focus:border-fg/20"
        />
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto p-1.5">
        {files === null && !error && <div className="p-4 text-xs text-faint">Loading files…</div>}
        {error && (
          <div className="p-4 text-xs text-danger">
            {error}{" "}
            <button className="underline" onClick={() => setRefreshTick((t) => t + 1)}>
              Retry
            </button>
          </div>
        )}
        {files !== null && files.length === 0 && (
          <div className="p-4 text-xs text-faint">No files in this folder.</div>
        )}
        {files !== null && files.length > 0 && renderDir(tree, 0)}
        {noMatch && (
          <div className="p-4 text-xs text-faint">No files match “{filter.trim()}”.</div>
        )}
      </div>
      <div className="shrink-0 border-t border-line p-2">
        <div className="flex items-center gap-2">
          <span className="min-w-0 flex-1 truncate text-xs text-muted">
            {selected.size > 0 ? `${selected.size} selected` : "Click files to select"}
          </span>
          {selected.size > 0 && (
            <>
              <button
                className="rounded-md px-2.5 py-1.5 text-xs text-muted hover:bg-panel-2 hover:text-fg"
                onClick={() => setSelected(new Set())}
              >
                Clear
              </button>
              <button
                className="rounded-md bg-accent px-2.5 py-1.5 text-xs font-medium text-accent-fg hover:brightness-105"
                onClick={attach}
              >
                Attach to composer
              </button>
            </>
          )}
        </div>
        <div className="mt-1.5 text-[11px] leading-snug text-faint">
          Lists files the agent can see — honors .gitignore, skips hidden files.{opens && " Double-click opens a file."}
        </div>
      </div>
    </div>
  );
}
