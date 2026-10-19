import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { errorText, type AgentId, type AgentInfo, type CodeSession, type MeshMember } from "../lib/ipc";
import { useStore } from "../lib/store";
import { ChevronLeftIcon, ChevronRightIcon, MonitorIcon, XIcon } from "../components/icons";

/** What a computer offers for a new session (gateway.rs, `new_session_options_local`). */
interface Options {
  agents: AgentInfo[];
  defaultAgent: AgentId;
  home: string | null;
  recent: string[];
}

/** One folder listing on a computer (gateway.rs, `list_dirs_local`). */
interface Listing {
  path: string;
  parent: string | null;
  home: string;
  dirs: string[];
  truncated: boolean;
}

const call = {
  options: (host: string) => invoke<Options>("remote_new_session_options", { host }),
  dirs: (host: string, path: string | null) => invoke<Listing>("remote_list_dirs", { host, path }),
  create: (host: string, cwd: string, modelRef: string | null, agent: AgentId) =>
    invoke<CodeSession>("remote_create_code_session", { host, cwd, modelRef, agent }),
};

/** `~/…` for paths in the home folder. */
function tilde(path: string, home: string | null): string {
  if (home && (path === home || path.startsWith(home + "/"))) return "~" + path.slice(home.length);
  return path;
}

const rowCls = "flex w-full items-center gap-3 px-4 py-3 text-left text-[15px] active:bg-panel-2";
const labelCls = "px-4 pb-1.5 pt-5 font-mono text-[11px] uppercase tracking-[0.06em] text-faint";

/**
 * Start a Code session from the phone: pick a computer, an agent and a folder on that computer.
 * The session runs there, like one started at its own screen.
 */
export function NewSessionSheet({
  members,
  gateway,
  onClose,
  onCreated,
}: {
  members: MeshMember[];
  gateway: string | null;
  onClose: () => void;
  onCreated: (id: string) => void;
}) {
  // Online computers, as the settings list shows them: the gateway, and members reachable directly or through it.
  const computers = members.filter(
    (m) => m.role !== "remote" && (m.nodeId === gateway || m.state === "direct" || m.state === "relay"),
  );
  const [host, setHost] = useState<string | null>(gateway ?? computers[0]?.nodeId ?? null);
  const [opts, setOpts] = useState<Options | null>(null);
  const [agent, setAgent] = useState<AgentId | null>(null);
  const [folder, setFolder] = useState<string | null>(null);
  const [browse, setBrowse] = useState<Listing | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!host) return;
    let live = true;
    setOpts(null);
    setFolder(null);
    setBrowse(null);
    setErr(null);
    call.options(host).then(
      (o) => {
        if (!live) return;
        setOpts(o);
        const usable = o.agents.filter((a) => a.available);
        setAgent(usable.some((a) => a.id === o.defaultAgent) ? o.defaultAgent : (usable[0]?.id ?? null));
        setFolder(o.recent[0] ?? null);
      },
      (e) => live && setErr(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [host]);

  const open = async (path: string | null) => {
    if (!host) return;
    try {
      setBrowse(await call.dirs(host, path));
      setErr(null);
    } catch (e) {
      setErr(errorText(e));
    }
  };

  const start = async () => {
    if (!host || !agent || !folder) return;
    setBusy(true);
    try {
      const st = useStore.getState();
      const s = await call.create(host, folder, st.defaultModelRef, agent);
      await st.refreshCodeSessions();
      // The gateway's list can lag the host that made it: show the new session either way.
      if (!useStore.getState().codeSessions.some((x) => x.id === s.id)) {
        useStore.setState((cur) => ({ codeSessions: [s, ...cur.codeSessions] }));
      }
      onCreated(s.id);
    } catch (e) {
      setErr(errorText(e));
      setBusy(false);
    }
  };

  const home = opts?.home ?? browse?.home ?? null;

  if (browse) {
    return (
      <div className="fixed inset-0 z-40 flex flex-col bg-bg pt-[env(safe-area-inset-top)]">
        <header className="flex h-14 shrink-0 items-center gap-1 px-2">
          <button
            className="flex h-10 w-10 items-center justify-center rounded-lg text-muted active:bg-panel-2 disabled:opacity-40"
            aria-label="Up one folder"
            disabled={!browse.parent}
            onClick={() => open(browse.parent)}
          >
            <ChevronLeftIcon size={20} />
          </button>
          <div className="min-w-0 flex-1 truncate font-mono text-[13px]">{tilde(browse.path, home)}</div>
          <button className="flex h-10 w-10 items-center justify-center rounded-lg text-muted active:bg-panel-2" aria-label="Close" onClick={() => setBrowse(null)}>
            <XIcon size={18} />
          </button>
        </header>
        {err && <div className="px-4 pb-2 text-[13px] text-danger">{err}</div>}
        <div className="min-h-0 flex-1 overflow-y-auto">
          {browse.dirs.length === 0 && <div className="px-4 py-6 text-[14px] text-muted">No folders here.</div>}
          {browse.dirs.map((d) => (
            <button key={d} className={rowCls} onClick={() => open(`${browse.path}/${d}`)}>
              <span className="min-w-0 flex-1 truncate">{d}</span>
              <ChevronRightIcon size={14} />
            </button>
          ))}
          {browse.truncated && <div className="px-4 py-3 text-[12.5px] text-faint">Only the first 500 folders are shown.</div>}
        </div>
        <div className="border-t border-line px-4 pb-[calc(env(safe-area-inset-bottom)+12px)] pt-3">
          <button
            className="flex h-11 w-full items-center justify-center rounded-[10px] bg-accent px-4 text-[15px] font-semibold text-accent-fg active:brightness-95"
            onClick={() => {
              setFolder(browse.path);
              setBrowse(null);
            }}
          >
            Use {tilde(browse.path, home)}
          </button>
        </div>
      </div>
    );
  }

  const agents = opts?.agents.filter((a) => a.available) ?? [];
  const folders = folder && opts && !opts.recent.includes(folder) ? [folder, ...opts.recent] : (opts?.recent ?? []);

  return (
    <div className="fixed inset-0 z-40 flex flex-col bg-bg pt-[env(safe-area-inset-top)]">
      <header className="flex h-14 shrink-0 items-center gap-2 px-4">
        <div className="flex-1 text-[18px] font-semibold">New session</div>
        <button className="flex h-10 w-10 items-center justify-center rounded-lg text-muted active:bg-panel-2" aria-label="Close" onClick={onClose}>
          <XIcon size={18} />
        </button>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto pb-4">
        <div className={labelCls}>Computer</div>
        {computers.length === 0 && <div className="px-4 py-2 text-[14px] text-muted">No computer is online.</div>}
        {computers.map((m) => (
          <button key={m.nodeId} className={rowCls} aria-pressed={host === m.nodeId} onClick={() => setHost(m.nodeId)}>
            <span className="shrink-0 text-faint">
              <MonitorIcon size={15} />
            </span>
            <span className="min-w-0 flex-1 truncate">{m.name}</span>
            {host === m.nodeId && <span className="h-2 w-2 shrink-0 rounded-full bg-accent" aria-label="Selected" />}
          </button>
        ))}

        {host && !opts && !err && <div className="px-4 pt-5 text-[14px] text-muted">Asking the computer…</div>}
        {opts && (
          <>
            <div className={labelCls}>Agent</div>
            <div className="px-4">
              {agents.length === 0 ? (
                <div className="text-[14px] text-muted">No coding agent is installed on this computer.</div>
              ) : (
                <select
                  className="h-11 w-full rounded-[10px] border border-border bg-raised px-3 text-[15px] text-fg outline-none"
                  value={agent ?? ""}
                  aria-label="Coding agent"
                  onChange={(e) => setAgent(e.target.value as AgentId)}
                >
                  {agents.map((a) => (
                    <option key={a.id} value={a.id}>
                      {a.label}
                    </option>
                  ))}
                </select>
              )}
            </div>

            <div className={labelCls}>Folder</div>
            {folders.map((f) => (
              <button key={f} className={rowCls} aria-pressed={folder === f} onClick={() => setFolder(f)}>
                <span className="min-w-0 flex-1 truncate font-mono text-[13.5px]">{tilde(f, home)}</span>
                {folder === f && <span className="h-2 w-2 shrink-0 rounded-full bg-accent" aria-label="Selected" />}
              </button>
            ))}
            <button className={`${rowCls} text-accent-ink`} onClick={() => open(null)}>
              Browse folders…
            </button>
          </>
        )}
        {err && <div className="px-4 pt-4 text-[13px] text-danger">{err}</div>}
      </div>
      <div className="border-t border-line px-4 pb-[calc(env(safe-area-inset-bottom)+12px)] pt-3">
        <button
          className="flex h-11 w-full items-center justify-center rounded-[10px] bg-accent px-4 text-[15px] font-semibold text-accent-fg active:brightness-95 disabled:opacity-50"
          disabled={!host || !agent || !folder || busy}
          onClick={start}
        >
          {busy ? "Starting…" : "Start session"}
        </button>
      </div>
    </div>
  );
}
