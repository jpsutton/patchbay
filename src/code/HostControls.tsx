import { useEffect, useState } from "react";
import { api, errorText, type CodeSession } from "../lib/ipc";
import { memberName, memberOnline, remoteHost, useStore } from "../lib/store";
import { input } from "../settings/ProviderPage";
import { MonitorIcon } from "../components/icons";
import { isRemoteApp } from "../lib/platform";
import { Dot } from "../components/Dot";

/** "on <computer>" next to a session hosted elsewhere in the mesh. */
export function HostBadge({ session }: { session: CodeSession }) {
  const mesh = useStore((s) => s.mesh);
  const host = remoteHost(session, mesh);
  if (!host) return null;
  const online = memberOnline(mesh, host);
  const name = memberName(mesh, host);
  return (
    <span
      className="flex shrink-0 items-center gap-1.5 rounded-md border border-line px-1.5 py-0.5 text-[12px] text-muted"
      title={online ? `The agent runs on ${name}` : `${name} is offline`}
    >
      <MonitorIcon size={13} />
      {name}
      <Dot size={6} className={online ? "text-ok" : "text-faint"} aria-label={online ? "online" : "offline"} />
    </span>
  );
}

/** Restart the view on a session's new host after a move or take-over. */
async function reattach(id: string) {
  const st = useStore.getState();
  await st.refreshCodeSessions();
  st.resetCode(id);
  await st.startAgent(id);
}

/** Move a session to another computer in the mesh, or (`takeOver`) continue it here. */
export function MoveDialog({
  session,
  takeOver,
  onClose,
}: {
  session: CodeSession;
  takeOver?: boolean;
  onClose: () => void;
}) {
  const mesh = useStore((s) => s.mesh);
  const setToast = useStore((s) => s.setToast);
  const host = session.hostNode ?? mesh?.nodeId;
  const targets = (mesh?.members ?? []).filter((m) => m.nodeId !== host && m.state !== "removed");
  const [target, setTarget] = useState(() =>
    takeOver ? (mesh?.nodeId ?? "") : (targets.find((m) => m.state === "self")?.nodeId ?? targets.find((m) => m.state !== "offline")?.nodeId ?? ""),
  );
  const [cwd, setCwd] = useState(session.cwd);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const hostName = memberName(mesh, host);
  const targetName = memberName(mesh, target);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && !busy && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  const go = async () => {
    setBusy(true);
    setError(null);
    try {
      const note = takeOver ? await api.codeTakeOver(session.id, cwd) : await api.codeMove(session.id, target, cwd);
      setToast(note ?? (takeOver ? "The session continues on this computer" : `Moved to ${targetName}`));
      onClose();
      reattach(session.id);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/55 p-6 backdrop-blur-[2px]" onClick={() => !busy && onClose()}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="move-title"
        className="flex w-[min(480px,92vw)] flex-col gap-3 rounded-2xl border border-border bg-raised p-5 shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div id="move-title" className="text-sm font-semibold">
          {takeOver ? `Take over from ${hostName}` : "Move session to another computer"}
        </div>
        <p className="text-xs leading-5 text-muted">
          {takeOver
            ? `${hostName} is offline. The session continues here from the last checkpoint this computer received, losing only a turn that was still running. If ${hostName} comes back, it keeps any later work on a separate git ref.`
            : `The agent stops briefly while its work moves. Uncommitted changes come along; ${hostName}'s branch and files stay as they are.`}
        </p>
        {!takeOver && (
          <label className="flex flex-col gap-1 text-xs">
            <span className="text-muted">Computer</span>
            <select className={input} value={target} onChange={(e) => setTarget(e.target.value)}>
              {targets.map((m) => (
                <option key={m.nodeId} value={m.nodeId} disabled={m.state === "offline"}>
                  {m.name}
                  {m.state === "self" ? " (this computer)" : m.state === "offline" ? " (offline)" : ""}
                </option>
              ))}
            </select>
          </label>
        )}
        <label className="flex flex-col gap-1 text-xs">
          <span className="text-muted">Folder on {takeOver ? "this computer" : targetName || "that computer"}</span>
          <input className={`${input} font-mono text-xs`} value={cwd} onChange={(e) => setCwd(e.target.value)} />
          <span className="text-[12px] text-faint">
            An existing checkout of the same repository is reused; it must have no uncommitted changes. A missing
            folder is created.
          </span>
        </label>
        {error && (
          <div className="rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-xs text-danger" role="alert">
            {error}
          </div>
        )}
        <div className="flex justify-end gap-2">
          <button className="h-[34px] rounded-lg px-3.5 text-xs text-muted hover:bg-panel-2 hover:text-fg" disabled={busy} onClick={onClose}>
            Cancel
          </button>
          <button
            className="h-[34px] rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg disabled:opacity-40"
            disabled={busy || !cwd.trim() || (!takeOver && !target)}
            onClick={go}
          >
            {busy ? (takeOver ? "Taking over…" : "Moving…") : takeOver ? "Take over here" : "Move"}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Shown in a session whose host is offline. */
export function OfflineHostBanner({ session, onTakeOver }: { session: CodeSession; onTakeOver: () => void }) {
  const mesh = useStore((s) => s.mesh);
  const [checkpoint, setCheckpoint] = useState<number | null>(null);
  const host = remoteHost(session, mesh);
  const offline = !!host && !memberOnline(mesh, host);
  useEffect(() => {
    if (!offline) return;
    api
      .codeCheckpoints()
      .then((c) => setCheckpoint(c[session.id] ?? null))
      .catch(() => {});
  }, [offline, session.id]);
  if (!offline) return null;
  return (
    <div className="flex items-center gap-3 rounded-lg border border-line bg-panel px-3 py-2.5 text-xs">
      <MonitorIcon size={16} />
      <div className="min-w-0 flex-1 text-muted">
        <span className="text-fg">{memberName(mesh, host)}</span> runs this session and is offline.
        {isRemoteApp()
          ? " Take it over from another computer to keep working."
          : checkpoint
            ? ` A checkpoint from ${new Date(checkpoint).toLocaleString()} is on this computer.`
            : ""}
      </div>
      {!isRemoteApp() && (
        <button className="shrink-0 rounded-md bg-accent px-3 py-1.5 font-medium text-accent-fg" onClick={onTakeOver}>
          Take over here
        </button>
      )}
    </div>
  );
}
