import { useEffect, useMemo, useState } from "react";
import { AGENT_LABELS, type AgentInfo } from "../lib/ipc";
import { memberName, remoteHost, useStore } from "../lib/store";
import { sessionInfoSections, sessionInfoText } from "../lib/sessionInfo";
import { CopyButton } from "../components/CopyButton";
import { CopyIcon, XIcon } from "../components/icons";
import { installedAgents } from "./CodeView";

/**
 * The raw ids and settings of one Code session, with copy buttons. Opened from a session's row
 * menu, its header, or the command palette, on the desktop and the phone alike; it reads only
 * what every surface already has (the saved session, its live state, the agent list).
 */
export function SessionInfoDialog() {
  const id = useStore((s) => s.sessionInfoId);
  const close = () => useStore.getState().setSessionInfoId(null);
  const session = useStore((s) => s.codeSessions.find((x) => x.id === s.sessionInfoId) ?? null);
  const state = useStore((s) => (s.sessionInfoId ? s.code[s.sessionInfoId] : undefined));
  const mesh = useStore((s) => s.mesh);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
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
    installedAgents(host).then((list) => live && setAgents(list));
    return () => {
      live = false;
    };
  }, [id, host]);

  const sections = useMemo(
    () =>
      session
        ? sessionInfoSections({
            session,
            state,
            agentLabel: AGENT_LABELS[session.agent] ?? session.agent,
            build: agents.find((a) => a.id === session.agent) ?? null,
            hostName: host ? memberName(mesh, host) : null,
            isLocalHost: !host,
          })
        : [],
    [session, state, agents, host, mesh],
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
        <div className="flex items-center gap-2 px-5 pb-2 pt-4">
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
        <div className="min-h-0 flex-1 overflow-y-auto px-5 pb-5">
          {sections.map((sec) => (
            <section key={sec.title} className="mt-3">
              <h2 className="pb-1 font-mono text-[11px] uppercase tracking-[0.06em] text-faint">{sec.title}</h2>
              <dl className="flex flex-col">
                {sec.rows.map((r) => (
                  <div
                    key={r.label}
                    className="flex items-start gap-2 border-t border-line py-1.5 first:border-t-0 sm:grid sm:grid-cols-[9.5rem_1fr_auto]"
                  >
                    <dt className="w-32 shrink-0 text-[12px] text-muted sm:w-auto">{r.label}</dt>
                    <dd
                      className={`min-w-0 flex-1 select-text text-[13px] ${r.mono ? "break-all font-mono text-[12px]" : "break-words"}`}
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
