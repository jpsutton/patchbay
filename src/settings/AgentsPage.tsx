import { useEffect, useState } from "react";
import { api, errorText, type AgentId, type AgentInfo } from "../lib/ipc";
import { useStore } from "../lib/store";
import { PermissionRulesList } from "./PermissionRules";
import { Deferred, Row } from "./PreferencesPage";

const SOURCE: Record<NonNullable<AgentInfo["source"]>, string> = {
  override: "your path",
  bundled: "bundled",
  path: "PATH",
  npx: "npx, downloaded on first use",
};

/** Settings → Coding agents: which ACP agents are installed, the default, and path overrides. */
export function AgentsPage() {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  const [agents, setAgents] = useState<AgentInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = () =>
    api.listAgents().then(
      (a) => {
        setAgents(a);
        setError(null);
      },
      (e) => setError(errorText(e)),
    );
  useEffect(() => {
    load();
  }, []);

  const setPath = async (id: AgentId, path: string) => {
    const agentPaths = { ...prefs.agentPaths };
    if (path) agentPaths[id] = path;
    else delete agentPaths[id];
    const err = await update({ agentPaths });
    if (!err) load();
    return err;
  };

  return (
    <div className="flex max-w-xl flex-col">
      <p className="pb-2 text-xs text-muted">
        Code sessions run a coding agent over the Agent Client Protocol. Grok Build ships with Patchbay and uses your
        providers and models. The others use their own sign-in and model list; install and sign in to them in a
        terminal. A session keeps the agent it started with.
      </p>
      {error && (
        <p className="py-2 text-xs text-danger" role="alert">
          {error}
        </p>
      )}
      {!agents && !error && <p className="py-3 text-xs text-muted">Looking for agents…</p>}
      {agents?.map((a) => (
        <div key={a.id} className="border-b border-border py-3 last:border-b-0">
          <div className="flex items-start justify-between gap-6">
            <div className="min-w-0">
              <div className="flex items-center gap-2 text-sm">
                <span
                  className={`h-1.5 w-1.5 shrink-0 rounded-full ${a.available ? "bg-ok" : "bg-faint"}`}
                  aria-hidden
                />
                {a.label}
                {prefs.defaultAgent === a.id && (
                  <span className="rounded bg-panel-2 px-1.5 py-px text-[11px] text-muted">default</span>
                )}
              </div>
              <div className="mt-0.5 break-all text-xs text-muted">
                {a.available ? (
                  <>
                    {a.version ? `${a.version} · ` : ""}
                    <span className="font-mono">{a.program}</span>
                    {a.source ? ` (${SOURCE[a.source]})` : ""}
                  </>
                ) : (
                  <span className={prefs.agentPaths[a.id] ? "text-danger" : ""}>{a.error ?? a.installHint}</span>
                )}
              </div>
              {!a.supportsInstructions && (
                <div className="mt-0.5 text-xs text-faint">Doesn't receive Patchbay's custom instructions.</div>
              )}
            </div>
            <button
              className="shrink-0 rounded-md border border-border px-2.5 py-1 text-xs text-muted hover:text-fg disabled:opacity-50"
              disabled={prefs.defaultAgent === a.id}
              onClick={() => update({ defaultAgent: a.id })}
            >
              Make default
            </button>
          </div>
          {a.id !== "grok" || prefs.agentPaths[a.id] ? (
            <div className="mt-2">
              <Row label="Executable" hint="Absolute path. Blank looks it up on PATH.">
                <Deferred
                  value={prefs.agentPaths[a.id] ?? ""}
                  placeholder={a.id === "claude-code" ? "claude-agent-acp" : (a.program ?? "")}
                  mono
                  onCommit={(v) => setPath(a.id, v.trim())}
                />
              </Row>
            </div>
          ) : null}
        </div>
      ))}
      <div className="pt-3">
        <button className="text-xs text-accent-ink underline underline-offset-2" onClick={load}>
          Check again
        </button>
      </div>
      <PermissionRulesList />
    </div>
  );
}
