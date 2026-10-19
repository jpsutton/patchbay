import { useState } from "react";
import { api, errorText, type AuthChoice } from "../lib/ipc";
import { useStore } from "../lib/store";
import { TerminalPane } from "../terminal/TerminalPane";

/**
 * The agent wants a sign-in: before it starts a session, or (`again`) mid-session, after it
 * refused a prompt. Offers the methods it advertised. A browser or API-key method goes to ACP
 * `authenticate` and the agent does the rest. A terminal method runs the agent's login command
 * in a terminal here; when that succeeds, Patchbay restarts the agent so it picks up the login.
 */
export function SignInCard({
  sessionId,
  cwd,
  message,
  methods,
  again,
}: {
  sessionId: string;
  cwd: string;
  message: string;
  methods: AuthChoice[];
  again: boolean;
}) {
  const [keyFor, setKeyFor] = useState<string | null>(null);
  const [key, setKey] = useState("");
  const [pending, setPending] = useState<AuthChoice | null>(null);
  const [terminal, setTerminal] = useState<AuthChoice | null>(null);
  const [error, setError] = useState<string | null>(null);

  const signIn = async (m: AuthChoice, apiKey?: string) => {
    setPending(m);
    setError(null);
    try {
      // Resolves once the agent's session is up, or mid-session once it accepts the sign-in. The
      // card goes away with the sessionReady or authResolved event.
      await api.codeAuthenticate(sessionId, m.id, apiKey);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setPending(null);
    }
  };

  // Stopping the agent ends a sign-in it is waiting on (a browser flow); a fresh one asks again.
  const cancel = () => {
    setPending(null);
    useStore.getState().startAgent(sessionId);
  };

  // The login command ended. Signed in: a fresh agent process reads the new login (and resumes
  // the session); the card goes away as the old one exits.
  const terminalDone = (m: AuthChoice, code: number | null) => {
    if (code === 0) useStore.getState().startAgent(sessionId);
    else setError(`${m.name}: the sign-in ${code == null ? "didn't start" : `exited with code ${code}`}.`);
  };

  const keyMethod = methods.find((m) => m.id === keyFor);
  return (
    <section
      aria-label="Sign-in required"
      className="flex flex-col gap-3 rounded-xl border border-accent/45 bg-accent/[0.06] p-3.5"
    >
      <div className="flex items-center gap-2">
        <span className="signal-pulse h-2 w-2 rounded-full bg-accent" />
        <span className="text-xs font-medium text-accent-ink">
          {again ? "Sign in to continue" : "Sign in to start the agent"}
        </span>
      </div>
      <div className="text-sm text-muted">{message}</div>
      {terminal ? (
        <div className="flex flex-col gap-2">
          <div className="h-56 overflow-hidden rounded-lg border border-border">
            <TerminalPane
              id={sessionId}
              cwd={cwd}
              visible
              authMethod={terminal.id}
              onExit={(code) => terminalDone(terminal, code)}
            />
          </div>
          <div className="flex flex-wrap items-center gap-3 text-sm">
            <span className="min-w-0 flex-1 text-muted">
              Finish signing in ({terminal.name}) in the terminal. When it's done, Patchbay restarts the agent
              {again ? " and you can resend your message" : ""}.
            </span>
            <button
              className="h-[34px] rounded-lg border border-border bg-panel px-3 text-xs text-fg hover:bg-panel-2"
              onClick={() => {
                setTerminal(null);
                setError(null);
              }}
            >
              Back
            </button>
          </div>
        </div>
      ) : pending ? (
        <div className="flex flex-wrap items-center gap-3 text-sm">
          <span className="signal-pulse h-1.5 w-1.5 shrink-0 rounded-full bg-live" />
          <span className="min-w-0 flex-1">
            Signing in ({pending.name})…{" "}
            <span className="text-muted">If a browser window opened, finish signing in there.</span>
          </span>
          <button
            className="h-[34px] rounded-lg border border-border bg-panel px-3 text-xs text-fg hover:bg-panel-2"
            onClick={cancel}
          >
            Cancel
          </button>
        </div>
      ) : keyMethod ? (
        <form
          className="flex flex-wrap items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (key.trim()) signIn(keyMethod, key.trim());
          }}
        >
          <input
            type="password"
            autoFocus
            autoComplete="off"
            spellCheck={false}
            aria-label={keyMethod.name}
            placeholder={keyMethod.name}
            className="h-[34px] min-w-0 flex-1 rounded-lg border border-border bg-bg px-3 font-mono text-[13px] text-fg outline-none focus:border-accent"
            value={key}
            onChange={(e) => setKey(e.target.value)}
          />
          <button
            type="submit"
            disabled={!key.trim()}
            className="h-[34px] rounded-lg bg-accent px-3.5 text-xs font-semibold text-accent-fg hover:brightness-105 disabled:opacity-50"
          >
            Sign in
          </button>
          <button
            type="button"
            className="h-[34px] rounded-lg px-3 text-xs text-muted hover:bg-panel-2"
            onClick={() => {
              setKeyFor(null);
              setKey("");
            }}
          >
            Back
          </button>
          <div className="basis-full text-xs text-faint">Goes to the agent. Patchbay doesn't store it.</div>
        </form>
      ) : (
        <div className="flex flex-wrap items-center gap-2">
          {methods.map((m, i) => (
            <button
              key={m.id}
              title={m.description ?? undefined}
              className={`h-[34px] rounded-lg px-3.5 text-xs ${
                i === 0
                  ? "bg-accent font-semibold text-accent-fg hover:brightness-105"
                  : "border border-border bg-panel text-fg hover:bg-panel-2"
              }`}
              onClick={() => {
                setError(null);
                if (m.terminal) setTerminal(m);
                else if (m.apiKey) setKeyFor(m.id);
                else signIn(m);
              }}
            >
              {m.name}
            </button>
          ))}
        </div>
      )}
      {error && <div className="text-xs text-danger">{error}</div>}
    </section>
  );
}
