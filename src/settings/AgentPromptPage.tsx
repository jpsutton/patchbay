import { useEffect, useState, type ReactNode } from "react";
import { api, errorText, type AgentPromptPreview, type PromptSection } from "../lib/ipc";
import { useStore } from "../lib/store";
import { Toggle } from "../components/Toggle";
import { LongText, Row } from "./PreferencesPage";
import { input } from "./ProviderPage";

const fmt = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));

function Tokens({ n }: { n: number }) {
  return <span className="shrink-0 font-mono text-[11px] text-faint">~{fmt(n)} tokens</span>;
}

function Heading({ n, title, hint, tokens }: { n?: number; title: string; hint: ReactNode; tokens?: number }) {
  return (
    <div className="pt-5">
      <div className="flex items-baseline justify-between gap-3 border-b border-border pb-1">
        <div className="text-sm font-semibold">
          {n != null && <span className="mr-1.5 text-muted">{n}.</span>}
          {title}
        </div>
        {tokens != null && <Tokens n={tokens} />}
      </div>
      <div className="pt-1.5 text-xs text-muted">{hint}</div>
    </div>
  );
}

/** One piece of the prompt: its title, where it comes from, its size, and its text on demand. */
function Block({ s, right, note }: { s: PromptSection; right?: ReactNode; note?: ReactNode }) {
  const [open, setOpen] = useState(false);
  return (
    <div className={`border-b border-border py-2 last:border-b-0 ${s.enabled ? "" : "opacity-60"}`}>
      <div className="flex items-center gap-3">
        <button
          className="flex min-w-0 flex-1 items-baseline gap-2 text-left text-xs"
          onClick={() => setOpen(!open)}
          aria-expanded={open}
        >
          <span className="w-3 shrink-0 text-muted">{open ? "⌄" : "›"}</span>
          <span className="shrink-0 font-medium">{s.title}</span>
          {s.source && <span className="truncate font-mono text-[11px] text-muted">{s.source}</span>}
        </button>
        <Tokens n={s.tokens} />
        {right}
      </div>
      {note && <div className="pl-5 pt-0.5 text-[11px] text-muted">{note}</div>}
      {open && (
        <pre className="mt-2 max-h-72 overflow-auto whitespace-pre-wrap rounded-md border border-border bg-panel p-2 font-mono text-[11px] text-fg/90">
          {s.text || "(empty)"}
        </pre>
      )}
    </div>
  );
}

/**
 * What ends up in Patchbay's agent prompt, in the order the model reads it: the standard prompt,
 * the instructions (custom ones, then CLAUDE.md, AGENTS.md and rules files) and the skills. The
 * text comes from the agent's own code for the chosen project and model, so it is what a session
 * there would send. The switches that decide what is in it sit beside each part.
 */
export function AgentPromptPage() {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  const models = useStore((s) => s.models);
  const providers = useStore((s) => s.providers);
  const defaultModelRef = useStore((s) => s.defaultModelRef);
  const sessions = useStore((s) => s.codeSessions);
  const setToast = useStore((s) => s.setToast);
  const cc = prefs.claudeConfig;

  // Projects to preview: recent Code sessions' folders, newest first.
  const projects = [...new Set([...sessions].sort((a, b) => b.updatedAt - a.updatedAt).map((s) => s.cwd))];
  const [project, setProject] = useState<string>(projects[0] ?? "");
  const [modelRef, setModelRef] = useState<string>(defaultModelRef ?? models[0]?.id ?? "");
  const [pv, setPv] = useState<AgentPromptPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);

  useEffect(() => {
    let live = true;
    api.agentPromptPreview(project || null, modelRef || null, null).then(
      (p) => {
        if (!live) return;
        setPv(p);
        setError(null);
      },
      (e) => live && setError(errorText(e)),
    );
    return () => {
      live = false;
    };
  }, [project, modelRef, tick, cc, prefs.agentInstructions, prefs.minifyPrompts, prefs.webFetch]);

  const setList = (key: "disabledInstructions" | "disabledSkills", name: string, on: boolean) => {
    const rest = cc[key].filter((x) => x !== name);
    return update({ claudeConfig: { ...cc, [key]: on ? rest : [...rest, name] } });
  };

  const standard = pv?.sections.filter((s) => s.group === "standard") ?? [];
  const files = pv?.sections.filter((s) => s.group === "instructions" && s.id.startsWith("instructions:")) ?? [];
  const skillsText = pv?.sections.find((s) => s.id === "skills");
  const sum = (xs: PromptSection[]) => xs.filter((s) => s.enabled).reduce((n, s) => n + s.tokens, 0);
  const instructionTokens = pv ? sum(pv.sections.filter((s) => s.group === "instructions")) : 0;
  const modelLabel = (id: string) => {
    const m = models.find((x) => x.id === id);
    return m ? `${m.displayName} (${providers.find((p) => p.id === m.providerId)?.name ?? "?"})` : id;
  };

  return (
    <div className="max-w-3xl pb-4">
      <div className="text-xs text-muted">
        What a Code session on Patchbay's own agent sends the model as its system prompt, in the order the model reads
        it. Other agents (Claude Code, Codex and the rest) bring their own prompt.
      </div>

      <div className="mt-3 flex flex-wrap items-center gap-2 text-xs">
        <label className="flex items-center gap-1.5">
          <span className="text-muted">Project</span>
          <select
            className={`${input} max-w-60 py-1`}
            value={project}
            onChange={(e) => setProject(e.target.value)}
            aria-label="Project to preview"
          >
            <option value="">None (user setup only)</option>
            {projects.map((p) => (
              <option key={p} value={p}>
                {p}
              </option>
            ))}
          </select>
        </label>
        <label className="flex items-center gap-1.5">
          <span className="text-muted">Model</span>
          <select
            className={`${input} max-w-60 py-1`}
            value={modelRef}
            onChange={(e) => setModelRef(e.target.value)}
            aria-label="Model to preview"
          >
            <option value="">Not chosen</option>
            {models.map((m) => (
              <option key={m.id} value={m.id}>
                {modelLabel(m.id)}
              </option>
            ))}
          </select>
        </label>
        <button className="rounded-md border border-border px-2 py-1 hover:border-accent" onClick={() => setTick(tick + 1)}>
          Refresh
        </button>
        <button
          className="rounded-md border border-border px-2 py-1 hover:border-accent disabled:opacity-40"
          disabled={!pv}
          onClick={() => {
            const text = (pv?.sections ?? [])
              .filter((s) => s.enabled)
              .map((s) => s.text)
              .join("\n\n");
            navigator.clipboard.writeText(text).then(
              () => setToast("Copied the system prompt"),
              () => setToast("Could not copy"),
            );
          }}
        >
          Copy full prompt
        </button>
        {pv && (
          <span className="ml-auto text-muted">
            System prompt ~{fmt(pv.tokens)} tokens, plus ~{fmt(pv.toolsTokens)} for the built-in tools
          </span>
        )}
      </div>
      {error && (
        <div className="mt-2 text-xs text-danger" role="alert">
          {error}
        </div>
      )}

      <Heading
        n={1}
        title="Standard prompt"
        hint="The agent's own instructions, the reply style, and a note on the machine and project. The provider can add a line ahead of them."
        tokens={sum(standard)}
      />
      <Row
        label="Compact prompts"
        hint="Send the agent's own prompts and tool descriptions in a shortened form (about a fifth less prompt text) and ask it to reply tersely, like the caveman plugin: no articles, filler or hedging, code and errors exact. Each session can switch it in its own options."
      >
        <Toggle label="Compact prompts" checked={prefs.minifyPrompts} onChange={(minifyPrompts) => update({ minifyPrompts })} />
      </Row>
      {standard.map((s, i) => (
        <Block key={`${s.id}${i}`} s={s} />
      ))}

      <Heading
        n={2}
        title="Instructions"
        hint="Your own instructions, then every instruction file found, broadest first: the nearest file to the project has the last word."
        tokens={instructionTokens}
      />
      <div className="pt-3">
        <div className="text-sm">Custom instructions</div>
        <div className="mt-0.5 text-xs text-muted">
          Added to the system prompt in every Code session, alongside the project's AGENTS.md. Each agent's page says
          whether it receives them.
        </div>
        <LongText
          value={prefs.agentInstructions}
          placeholder="e.g. Run the test suite after changing code. Never commit without asking."
          onCommit={(v) => update({ agentInstructions: v })}
        />
      </div>
      <Row
        label="Claude Code instruction files"
        hint="CLAUDE.md files (user, project, CLAUDE.local.md) and .claude/rules. AGENTS.md is read whatever this says; switch it off below."
      >
        <Toggle label="Claude Code instruction files" checked={cc.instructions} onChange={(instructions) => update({ claudeConfig: { ...cc, instructions } })} />
      </Row>
      {pv && files.length === 0 && <p className="pt-2 text-xs text-faint">No instruction files found.</p>}
      {files.map((s) => {
        const path = s.path ?? s.id;
        const on = !cc.disabledInstructions.includes(path);
        return (
          <Block
            key={s.id}
            s={s}
            note={on && !s.enabled ? "Left out: the Claude Code instruction files switch is off." : undefined}
            right={<Toggle label={`Load ${path}`} checked={on} onChange={(v) => setList("disabledInstructions", path, v)} />}
          />
        );
      })}

      <Heading
        n={3}
        title="Skills"
        hint="Only each skill's name and description go into the prompt. The agent loads a skill's full text when a task matches it, or when you run it as /name."
        tokens={skillsText?.enabled ? skillsText.tokens : 0}
      />
      <Row label="Claude Code skills and slash commands" hint="Skills from skills/ and commands from commands/, in ~/.claude, the project and plugins.">
        <Toggle label="Claude Code skills" checked={cc.skills} onChange={(skills) => update({ claudeConfig: { ...cc, skills } })} />
      </Row>
      {skillsText && <Block s={skillsText} />}
      {pv && pv.skills.length === 0 && <p className="pt-2 text-xs text-faint">No skills found.</p>}
      {pv?.skills.map((k) => {
        const on = !cc.disabledSkills.includes(k.name);
        return (
          <div key={`${k.source}:${k.name}`} className={`flex items-center gap-3 border-b border-border py-1.5 pl-5 text-xs last:border-b-0 ${k.enabled ? "" : "opacity-60"}`}>
            <span className="flex min-w-0 flex-1 items-baseline gap-2" title={k.description}>
              <span className="shrink-0 font-mono">{k.name}</span>
              <span className="shrink-0 rounded bg-panel-2 px-1.5 py-px text-[11px] text-muted">{k.source}</span>
              <span className="truncate text-muted">{k.description}</span>
            </span>
            <Tokens n={k.tokens} />
            <Toggle label={`Load skill ${k.name}`} checked={on} onChange={(v) => setList("disabledSkills", k.name, v)} />
          </div>
        );
      })}

      <Heading
        title="Also sent"
        hint="Not part of the system prompt, but the model gets it too: short reminders added to the conversation, and the tool descriptions sent with every request. Tools from MCP servers come on top."
        tokens={pv?.toolsTokens}
      />
      {pv?.reminders.map((s) => (
        <Block key={s.id} s={s} />
      ))}
      {pv && (
        <details className="border-b border-border py-2 text-xs">
          <summary className="cursor-pointer">
            Built-in tools ({pv.tools.length}) <span className="ml-2 text-faint">~{fmt(pv.toolsTokens)} tokens</span>
          </summary>
          <ul className="mt-2 flex flex-col gap-1">
            {pv.tools.map((t) => (
              <li key={t.name} className="flex items-baseline gap-2" title={t.description}>
                <span className="shrink-0 font-mono">{t.name}</span>
                <span className="truncate text-muted">{t.description}</span>
                <span className="ml-auto shrink-0">
                  <Tokens n={t.tokens} />
                </span>
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  );
}
