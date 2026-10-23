import { useEffect, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, errorText, type AgentId, type DirListing, type NewSessionOptions } from "../lib/ipc";
import { memberOnline, remoteHost, useStore } from "../lib/store";
import { isWeb } from "../lib/platform";
import { ChevronLeftIcon, ChevronRightIcon, FolderPlusIcon, MonitorIcon, XIcon } from "../components/icons";
import { Dot } from "../components/Dot";

/** `~/…` for paths in the home folder. */
function tilde(path: string, home: string | null): string {
  if (home && (path === home || path.startsWith(home + "/"))) return "~" + path.slice(home.length);
  return path;
}

const labelCls = "px-5 pb-1.5 pt-4 font-mono text-[11px] uppercase tracking-[0.06em] text-faint";
const rowCls = "flex w-full items-center gap-2.5 px-5 py-1.5 text-left text-sm hover:bg-panel-2";

/**
 * Start a Code session: pick a computer (this one, or another in the mesh), an agent installed
 * there and a folder there. Like the phone's New session sheet, sized for a window.
 */
export function NewSessionDialog() {
  const isOpen = useStore((s) => s.newSessionOpen);
  return isOpen ? <Dialog /> : null;
}

function Dialog() {
  const setOpen = useStore((s) => s.setNewSessionOpen);
  const mesh = useStore((s) => s.mesh);
  const me = mesh?.nodeId ?? null;
  // Computers that can run a session now: this one, then the online members (phones run none).
  const others = (mesh?.member ? mesh.members : []).filter(
    (m) => m.nodeId !== me && m.role !== "remote" && memberOnline(mesh, m.nodeId),
  );
  // Start where the open session is, in its folder.
  const [initial] = useState(() => {
    const st = useStore.getState();
    const cur = st.mode === "code" ? st.codeSessions.find((s) => s.id === st.activeCodeSessionId) : undefined;
    const host = cur ? remoteHost(cur, st.mesh) : null;
    return { host: host && memberOnline(st.mesh, host) ? host : null, cwd: cur?.cwd ?? null };
  });
  /** null: this computer. */
  const [host, setHost] = useState<string | null>(initial.host);
  const [opts, setOpts] = useState<NewSessionOptions | null>(null);
  const [agent, setAgent] = useState<AgentId | null>(null);
  const [folder, setFolder] = useState<string | null>(null);
  const [browse, setBrowse] = useState<DirListing | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const close = () => setOpen(false);

  useEffect(() => {
    let live = true;
    setOpts(null);
    setFolder(null);
    setBrowse(null);
    setErr(null);
    api.newSessionOptions(host).then(
      (o) => {
        if (!live) return;
        setOpts(o);
        const usable = o.agents.filter((a) => a.available);
        setAgent(usable.some((a) => a.id === o.defaultAgent) ? o.defaultAgent : (usable[0]?.id ?? null));
        setFolder(host === initial.host && initial.cwd ? initial.cwd : (o.recent[0] ?? null));
      },
      (e) => live && setErr(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [host]);

  const listDirs = async (path: string | null) => {
    try {
      setBrowse(await api.listDirs(path, host));
      setErr(null);
    } catch (e) {
      setErr(errorText(e));
    }
  };

  const pickFolder = async () => {
    // This computer's own dialog when there is one; otherwise browse in here.
    if (host || isWeb()) return listDirs(null);
    const dir = await openDialog({ directory: true, multiple: false, title: "Choose the project folder" });
    if (dir && !Array.isArray(dir)) setFolder(dir);
  };

  const ready = !!agent && !!folder && !busy;
  const start = async () => {
    if (!agent || !folder) return;
    setBusy(true);
    try {
      const st = useStore.getState();
      const s = await api.createCodeSession(folder, st.defaultModelRef, agent, host);
      await st.refreshCodeSessions();
      st.setMode("code");
      st.selectCodeSession(s.id);
      close();
    } catch (e) {
      setErr(errorText(e));
      setBusy(false);
    }
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        if (browse) setBrowse(null);
        else close();
      } else if (e.key === "Enter" && !browse && ready && !(e.target instanceof HTMLSelectElement)) {
        e.preventDefault();
        start();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  });

  const home = opts?.home ?? browse?.home ?? null;
  const agents = opts?.agents.filter((a) => a.available) ?? [];
  const folders = folder && opts && !opts.recent.includes(folder) ? [folder, ...opts.recent] : (opts?.recent ?? []);
  const where = host ? (mesh?.members.find((m) => m.nodeId === host)?.name ?? "that computer") : "this computer";

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]" onClick={close}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="new-session-title"
        className="flex max-h-[min(620px,88vh)] w-[min(500px,92vw)] flex-col rounded-2xl border border-border bg-raised shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-1 px-5 pb-1 pt-4">
          {browse && (
            <button
              className="-ml-2 flex h-7 w-7 items-center justify-center rounded-lg text-muted hover:bg-panel-2 hover:text-fg disabled:opacity-40"
              aria-label="Up one folder"
              disabled={!browse.parent}
              onClick={() => browse.parent && listDirs(browse.parent)}
            >
              <ChevronLeftIcon size={16} />
            </button>
          )}
          <div id="new-session-title" className="min-w-0 flex-1 truncate text-sm font-semibold">
            {browse ? <span className="font-mono text-xs font-normal text-muted">{tilde(browse.path, home)}</span> : "New session"}
          </div>
          <button
            className="flex h-7 w-7 items-center justify-center rounded-lg text-muted hover:bg-panel-2 hover:text-fg"
            aria-label={browse ? "Back to the new session" : "Close"}
            onClick={() => (browse ? setBrowse(null) : close())}
          >
            <XIcon size={15} />
          </button>
        </div>

        {browse ? (
          <div className="min-h-[200px] flex-1 overflow-y-auto border-y border-line py-1">
            {browse.dirs.length === 0 && <div className="px-5 py-4 text-xs text-muted">No folders here.</div>}
            {browse.dirs.map((d) => (
              <button key={d} className={rowCls} onClick={() => listDirs(`${browse.path}/${d}`)}>
                <span className="min-w-0 flex-1 truncate">{d}</span>
                <ChevronRightIcon size={12} />
              </button>
            ))}
            {browse.truncated && <div className="px-5 py-2 text-xs text-faint">Only the first 500 folders are shown.</div>}
          </div>
        ) : (
          <div className="min-h-0 flex-1 overflow-y-auto pb-2">
            {others.length > 0 && (
              <>
                <div className={labelCls}>Computer</div>
                <div className="flex flex-wrap gap-1.5 px-5">
                  {[{ nodeId: null as string | null, name: "This computer" }, ...others].map((m) => (
                    <button
                      key={m.nodeId ?? ""}
                      aria-pressed={host === m.nodeId}
                      onClick={() => setHost(m.nodeId)}
                      className={`flex h-8 items-center gap-1.5 rounded-lg border px-2.5 text-xs ${
                        host === m.nodeId
                          ? "border-accent/60 bg-accent/10 text-fg"
                          : "border-border text-muted hover:bg-panel-2 hover:text-fg"
                      }`}
                    >
                      <MonitorIcon size={13} />
                      {m.name}
                    </button>
                  ))}
                </div>
              </>
            )}

            {!opts && !err && <div className="px-5 pt-4 text-xs text-muted">Asking {where}…</div>}
            {opts && (
              <>
                <div className={labelCls}>Agent</div>
                <div className="px-5">
                  {agents.length === 0 ? (
                    <div className="text-xs text-muted">No coding agent is installed on {where}.</div>
                  ) : (
                    <select
                      className="h-9 w-full rounded-lg border border-border bg-panel px-2.5 text-sm text-fg outline-none focus:border-accent/60"
                      value={agent ?? ""}
                      aria-label="Coding agent"
                      onChange={(e) => setAgent(e.target.value as AgentId)}
                    >
                      {agents.map((a) => (
                        <option key={a.id} value={a.id}>
                          {a.label}
                          {a.id === opts.defaultAgent ? " (default)" : ""}
                        </option>
                      ))}
                    </select>
                  )}
                </div>

                <div className={labelCls}>Folder</div>
                <div role="radiogroup" aria-label="Folder">
                  {folders.map((f) => (
                    <button key={f} role="radio" aria-checked={folder === f} className={rowCls} onClick={() => setFolder(f)}>
                      {folder === f ? <Dot size={12} className="text-accent" /> : <Dot size={12} hollow={1} className="text-border" />}
                      <span className="min-w-0 flex-1 truncate font-mono text-[12.5px]">{tilde(f, home)}</span>
                    </button>
                  ))}
                </div>
                <button className={`${rowCls} text-accent-ink`} onClick={pickFolder}>
                  <FolderPlusIcon size={14} />
                  {folders.length ? "Another folder…" : "Choose a folder…"}
                </button>
              </>
            )}
          </div>
        )}

        {err && <div className="px-5 pt-2 text-xs text-danger">{err}</div>}
        <div className="flex items-center justify-end gap-2 px-5 py-3">
          {!host && !browse && !isWeb() && (
            <button
              className="mr-auto h-[34px] rounded-lg px-2 text-xs text-accent-ink hover:bg-panel-2"
              onClick={() => {
                close();
                useStore.getState().setImportClaudeOpen(true);
              }}
            >
              Import from Claude Code…
            </button>
          )}
          <button className="h-[34px] rounded-lg px-3.5 text-xs text-muted hover:bg-panel-2 hover:text-fg" onClick={close}>
            Cancel
          </button>
          {browse ? (
            <button
              className="h-[34px] max-w-[60%] truncate rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg"
              onClick={() => {
                setFolder(browse.path);
                setBrowse(null);
              }}
            >
              Use {tilde(browse.path, home)}
            </button>
          ) : (
            <button
              disabled={!ready}
              className="h-[34px] rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg disabled:opacity-50"
              onClick={start}
            >
              {busy ? "Starting…" : "Start session"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
