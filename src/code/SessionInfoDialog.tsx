import { useEffect, useMemo, useState } from "react";
import { AGENT_LABELS, api, type AgentInfo, type HostInfo, type SessionPlanUsage } from "../lib/ipc";
import { memberName, remoteHost, useStore } from "../lib/store";
import { sessionInfoSections, sessionInfoText } from "../lib/sessionInfo";
import { CopyButton } from "../components/CopyButton";
import { ErrorBoundary } from "../components/ErrorBoundary";
import { CopyIcon, XIcon } from "../components/icons";
import { installedAgents } from "./CodeView";

/**
 * The raw ids and settings of one Code session, with copy buttons. Opened from a session's row
 * menu, its header, or the command palette, on the desktop and the phone alike. Plan usage is
 * fetched for this session only (Claude or Cursor), so the phone sees the same Plan section as
 * the computer.
 */
export function SessionInfoDialog() {
  const id = useStore((s) => s.sessionInfoId);
  // A row that can't be built (odd data from an agent or another computer) shows its error here
  // instead of taking the whole window down.
  return (
    <ErrorBoundary resetKey={id} fallback={(e) => <SessionInfoError error={e} />}>
      <SessionInfoContent />
    </ErrorBoundary>
  );
}

function SessionInfoError({ error }: { error: Error }) {
  const close = () => useStore.getState().setSessionInfoId(null);
  const text = `${error.name}: ${error.message}\n${error.stack ?? ""}`.trim();
  return (
    <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/55 p-4 backdrop-blur-[2px]" onClick={close}>
      <div
        role="alertdialog"
        aria-modal="true"
        className="flex max-h-[min(85vh,720px)] w-[min(560px,94vw)] flex-col rounded-2xl border border-border bg-raised p-4 shadow-float sm:p-5"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-2">
          <div className="min-w-0 flex-1 text-sm font-semibold">Session info can't be shown</div>
          <span className="px-1 text-xs text-muted">
            <CopyButton label="Copy error" text={text} />
          </span>
          <button
            className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-panel-2 hover:text-fg"
            onClick={close}
            aria-label="Close"
            title="Close"
          >
            <XIcon size={15} />
          </button>
        </div>
        <pre className="mt-3 min-h-0 flex-1 select-text overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] text-muted">
          {text}
        </pre>
      </div>
    </div>
  );
}

function SessionInfoContent() {
  const id = useStore((s) => s.sessionInfoId);
  const close = () => useStore.getState().setSessionInfoId(null);
  const session = useStore((s) => s.codeSessions.find((x) => x.id === s.sessionInfoId) ?? null);
  const state = useStore((s) => (s.sessionInfoId ? s.code[s.sessionInfoId] : undefined));
  const mesh = useStore((s) => s.mesh);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [hostInfo, setHostInfo] = useState<HostInfo | null>(null);
  const [plan, setPlan] = useState<SessionPlanUsage | null | undefined>(undefined);
  const host = remoteHost(session, mesh);

  useEffect(() => {
    if (!id) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") useStore.getState().setSessionInfoId(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [id]);

  useEffect(() => {
    if (!id) return;
    let live = true;
    installedAgents(host).then((list) => live && setAgents(Array.isArray(list) ? list : []));
    // An older host doesn't know the request, an offline one can't answer: leave the row out.
    setHostInfo(null);
    api.hostInfo(host).then((info) => live && setHostInfo(info), () => {});
    return () => {
      live = false;
    };
  }, [id, host]);

  useEffect(() => {
    if (!id) return;
    let live = true;
    setPlan(undefined);
    api.sessionPlanUsage(id).then(
      (p) => live && setPlan(p),
      () => live && setPlan(null),
    );
    return () => {
      live = false;
    };
  }, [id]);

  const sections = useMemo(
    () =>
      session
        ? sessionInfoSections({
            session,
            state,
            agentLabel: AGENT_LABELS[session.agent] ?? session.agent,
            build: agents.find((a) => a.id === session.agent) ?? null,
            hostInfo,
            hostName: host ? memberName(mesh, host) : null,
            isLocalHost: !host,
            plan,
          })
        : [],
    [session, state, agents, hostInfo, host, mesh, plan],
  );

  if (!id || !session) return null;
  return (
    <div className="fixed inset-0 z-[70] flex items-center justify-center bg-black/55 p-4 backdrop-blur-[2px]" onClick={close}>
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="session-info-title"
        className="flex max-h-[min(85vh,720px)] w-[min(560px,94vw)] flex-col rounded-2xl border border-border bg-raised shadow-float"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-2 px-4 pb-2 pt-4 sm:px-5">
          <div id="session-info-title" className="min-w-0 flex-1 truncate text-sm font-semibold">
            Session info
          </div>
          <span className="px-1 text-xs text-muted">
            <CopyButton label="Copy all" text={() => sessionInfoText(sections)} />
          </span>
          <button
            className="flex h-7 w-7 items-center justify-center rounded-md text-muted hover:bg-panel-2 hover:text-fg"
            onClick={close}
            aria-label="Close"
            title="Close"
          >
            <XIcon size={15} />
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto px-4 pb-5 sm:px-5">
          {sections.map((sec) => (
            <section key={sec.title} className="mt-3">
              <h2 className="pb-1 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">{sec.title}</h2>
              <dl className="flex flex-col">
                {sec.rows.map((r, rowIdx) => (
                  <div
                    key={`${r.label}:${rowIdx}`}
                    // Narrow screens stack the label over the value, so a UUID fits on one line.
                    className="grid grid-cols-[1fr_auto] items-start gap-x-2 gap-y-0.5 border-t border-line py-1.5 first:border-t-0 sm:grid-cols-[9.5rem_1fr_auto]"
                  >
                    <dt className="col-span-2 text-[12px] text-muted sm:col-span-1">{r.label}</dt>
                    <dd
                      className={`min-w-0 select-text text-[13px] ${r.mono ? "break-all font-mono text-[11px] sm:text-[12px]" : "break-words"}`}
                    >
                      {r.value}
                    </dd>
                    {r.copy ? (
                      <CopyButton label={`Copy ${r.label}`} icon={<CopyIcon size={14} />} text={r.value} />
                    ) : (
                      <span className="hidden w-7 sm:block" />
                    )}
                  </div>
                ))}
              </dl>
            </section>
          ))}
        </div>
      </div>
    </div>
  );
}
