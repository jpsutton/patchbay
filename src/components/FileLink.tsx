import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { api, errorText } from "../lib/ipc";
import { useStore } from "../lib/store";
import { isRemoteApp, isWeb } from "../lib/platform";

/**
 * Where paths in a transcript live. A Code session resolves relative paths against its folder;
 * elsewhere only absolute and `~/` paths link. `local: false` (a session on another computer)
 * links nothing, since its files aren't on this one.
 */
export const FileLinkContext = createContext<{ cwd: string | null; local: boolean }>({ cwd: null, local: true });

/** Set while rendering inside a Markdown link, so its inline code doesn't nest a second one. */
export const InLinkContext = createContext(false);

// A Unix, `~/` or Windows path, or a relative one with a folder or an extension. No spaces or
// code punctuation, so `foo.bar()` and `a = b` stay plain.
const PATH = /^(?:~\/|\/|[A-Za-z]:[\\/]|\.{1,2}[\\/])?[\w.@+~-]+(?:[\\/][\w.@+~-]*)*$/;
// A trailing line reference: `:12`, `:12:4`, `#L12` or `#L12-L20`.
const LINE = /(?::\d+(?::\d+)?|#L\d+(?:-L?\d+)?)$/;

/** The file part of `text` if it reads like a path, without any line reference. */
export function pathOf(text: string): string | null {
  const t = text.trim().replace(LINE, "");
  if (t.length < 2 || t.length > 1024 || t.includes("://") || !PATH.test(t)) return null;
  if (!/[\\/]/.test(t) && !/\.[A-Za-z]\w*$/.test(t)) return null;
  if (/^\.+$/.test(t) || /^\d+(?:\.\d+)+$/.test(t)) return null;
  return t;
}

/** Whether links can open anything here: only the desktop app reaches this computer's files. */
function canOpen(): boolean {
  return !isWeb() && !isRemoteApp();
}

// One check per path, shared by every mention of it. A miss is forgotten after a few seconds, so
// a path an agent mentions before it writes the file links once it's there.
const checks = new Map<string, { at: number; exists: Promise<boolean> }>();
const RECHECK_MS = 3000;

function exists(path: string, cwd: string | null): Promise<boolean> {
  const key = `${cwd ?? ""}\u0000${path}`;
  const hit = checks.get(key);
  if (hit && Date.now() - hit.at < RECHECK_MS) return hit.exists;
  if (hit) {
    // Kept for good once found; a stale miss is checked again.
    const found = hit.exists.then((e) => e || api.localPathExists(path, cwd).catch(() => false));
    checks.set(key, { at: Date.now(), exists: found });
    return found;
  }
  if (checks.size > 2000) checks.clear();
  const check = api.localPathExists(path, cwd).catch(() => false);
  checks.set(key, { at: Date.now(), exists: check });
  return check;
}

function useExists(path: string | null, cwd: string | null, enabled: boolean): boolean {
  const [found, setFound] = useState<string | null>(null);
  const key = `${cwd ?? ""}\u0000${path}`;
  useEffect(() => {
    if (!path || !enabled) return;
    let live = true;
    exists(path, cwd).then((e) => live && setFound(e ? key : null));
    return () => {
      live = false;
    };
  }, [key, path, cwd, enabled]);
  return !!path && enabled && found === key;
}

export function openLocalPath(path: string, cwd: string | null) {
  api.openLocalPath(path, cwd).catch((e) => useStore.getState().setToast(`Could not open ${path}: ${errorText(e)}`));
}

/**
 * `children` as a link that opens `text`'s file in the OS's default app, once the file is known
 * to exist on this computer. Anything else renders `children` unchanged. `exact` takes `text` as
 * a path as is, for places that only ever hold one (a diff's file name).
 */
export function FileLink({
  text,
  exact,
  children,
  className,
}: {
  text: string;
  exact?: boolean;
  children: ReactNode;
  className?: string;
}) {
  const { cwd, local } = useContext(FileLinkContext);
  const inLink = useContext(InLinkContext);
  const path = exact ? text || null : pathOf(text);
  const enabled = local && !inLink && canOpen();
  if (!useExists(path, cwd, enabled) || !path) return <>{children}</>;
  return (
    <a
      href="#"
      className={`file-link ${className ?? ""}`}
      title={`Open ${path}`}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        openLocalPath(path, cwd);
      }}
    >
      {children}
    </a>
  );
}
