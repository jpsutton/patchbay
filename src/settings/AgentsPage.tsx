import { useEffect, useState, type ReactNode } from "react";
import {
  AGENT_LABELS,
  api,
  errorText,
  type AgentEngine,
  type AgentId,
  type AgentInfo,
  type ClaudeItem,
  type ClaudeOverview,
} from "../lib/ipc";
import { useStore } from "../lib/store";
import { PermissionRulesList } from "./PermissionRules";
import { Choice, Deferred, LongText, Row, Toggle } from "./PreferencesPage";
import { input } from "./ProviderPage";
import { Dot } from "../components/Dot";

const SOURCE: Record<NonNullable<AgentInfo["source"]>, string> = {
  builtin: "built in",
  override: "your path",
  path: "PATH",
  npx: "npx, downloaded on first use",
};

/** The installed agents, loaded once per Settings visit and shared by the overview and each agent's page. */
export function useAgentList() {
  const [agents, setAgents] = useState<AgentInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [wanted, setWanted] = useState(false);
  const load = () =>
    api.listAgents().then(
      (a) => {
        setAgents(a);
        setError(null);
      },
      (e) => setError(errorText(e)),
    );
  useEffect(() => {
    if (wanted) load();
  }, [wanted]);
  return { agents, error, reload: load, want: () => setWanted(true) };
}

export type AgentList = ReturnType<typeof useAgentList>;

function StatusDot({ on }: { on: boolean }) {
  return <Dot size={6} className={on ? "text-ok" : "text-faint"} />;
}

function Loading({ list }: { list: AgentList }) {
  if (list.error) {
    return (
      <p className="py-2 text-xs text-danger" role="alert">
        {list.error}
      </p>
    );
  }
  return list.agents ? null : <p className="py-3 text-xs text-muted">Looking for agents…</p>;
}

/**
 * Where an agent's models come from: its own setup (the default) or Patchbay's providers. Claude
 * Code speaks only the Anthropic Messages API, so it is offered Anthropic-compatible providers only.
 */
function ProviderChoice({ agent, label }: { agent: AgentId; label: string }) {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  const providers = useStore((s) => s.providers);
  const models = useStore((s) => s.models);
  const claude = agent === "claude-code";
  const usable = providers.filter((p) => !claude || p.kind === "anthropic");
  const value = prefs.agentProviders[agent] ?? "";
  const missing = value && !models.some((m) => m.id === value);

  const choose = (ref: string) => {
    const agentProviders = { ...prefs.agentProviders };
    if (ref) agentProviders[agent] = ref;
    else delete agentProviders[agent];
    return update({ agentProviders });
  };

  const hint = missing ? (
    <span className="text-danger">That model is gone from Settings; pick another.</span>
  ) : claude ? (
    value ? (
      "Every request runs on this model, including subagents and background tasks. It's billed by that provider, not your Claude plan."
    ) : (
      "Claude Code uses the Claude account or API key you signed in with in a terminal, and picks its own models. Pick one of your Anthropic-compatible models to run everything on it instead."
    )
  ) : value ? (
    `Your Patchbay providers are added to ${label}'s own, with this model as its default.`
  ) : (
    `${label} uses the providers and sign-in you set up in ${label} itself. Pick a model to add your Patchbay providers too, with that model as its default.`
  );

  return (
    <Row label="Models" hint={hint}>
      <select className={`${input} w-56`} value={value} onChange={(e) => choose(e.target.value)}>
        <option value="">{label}'s own setup</option>
        {usable.map((p) => {
          const list = models.filter((m) => m.providerId === p.id);
          return list.length === 0 ? null : (
            <optgroup key={p.id} label={p.name}>
              {list.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.displayName}
                </option>
              ))}
            </optgroup>
          );
        })}
        {missing && <option value={value}>Missing model</option>}
      </select>
    </Row>
  );
}

/** Turns on a Claude plan: stop them before they run into extra usage. */
function UsageLimit({ agent }: { agent: string }) {
  const limit = useStore((s) => s.prefs.usageLimit);
  const update = useStore((s) => s.updatePrefs);
  return (
    <>
      <Row
        label="Stop at plan limit"
        hint={`Cancel running turns and hold new prompts when your Claude plan's 5-hour or weekly usage reaches the limit, or extra usage starts. Lifts when the window resets.${
          agent === "patchbay" ? " Applies to sessions on a model whose provider uses your Claude login. Shared with Claude Code." : ""
        }`}
      >
        <Toggle
          label="Stop at plan limit"
          checked={limit.enabled}
          onChange={(enabled) => update({ usageLimit: { ...limit, enabled } })}
        />
      </Row>
      {limit.enabled && (
        <Row label="Limit (%)" hint="Of the 5-hour or weekly window, whichever is higher.">
          <Deferred
            value={String(limit.percent)}
            placeholder="95"
            mono
            width="w-24"
            onCommit={(v) => {
              const t = v.trim() || "95";
              if (!/^\d+$/.test(t)) return Promise.resolve("Enter a whole number");
              return update({ usageLimit: { ...limit, percent: Number(t) } });
            }}
          />
        </Row>
      )}
    </>
  );
}

/** Settings for the agents that run on Patchbay's providers and models (Patchbay's own, Grok Build). */
function PatchbayModelSettings() {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  const [search, setSearch] = useState<{ configured: boolean; source: string | null } | null>(null);
  useEffect(() => {
    api.webSearchStatus().then(setSearch).catch(() => setSearch(null));
  }, []);
  return (
    <>
      <Row label="Web fetch tool" hint="Let the agent download web pages you or it reference.">
        <Toggle label="Web fetch tool" checked={prefs.webFetch} onChange={(webFetch) => update({ webFetch })} />
      </Row>
      <Row
        label="Compact prompts"
        hint="Send the agent's own prompts and tool descriptions in a shortened form (about a fifth less prompt text) and ask it to reply tersely, like the caveman plugin: no articles, filler or hedging, code and errors exact. Saves input and output tokens. Each session can switch it in its own options."
      >
        <Toggle label="Compact prompts" checked={prefs.minifyPrompts} onChange={(minifyPrompts) => update({ minifyPrompts })} />
      </Row>
      <Row
        label="Web search (Brave)"
        hint={
          search?.configured
            ? `A Brave Search API key is set (${search.source === "env" ? "from BRAVE_API_KEY" : search.source === "database" ? "stored in the database: no keychain" : "in the keychain"}). The agent can search the web.`
            : "Paste a Brave Search API key to let the agent search the web. It is stored in the OS keychain."
        }
      >
        <div className="flex flex-col items-end gap-1">
          <Deferred
            value=""
            placeholder={search?.configured ? "Replace the key" : "Brave Search API key"}
            mono
            onCommit={async (v) => {
              const key = v.trim();
              if (!key) return null;
              try {
                await api.setWebSearchKey(key);
                setSearch(await api.webSearchStatus());
                return null;
              } catch (e) {
                return errorText(e);
              }
            }}
          />
          {search?.configured && search.source !== "env" && (
            <button
              className="text-xs text-accent-ink underline underline-offset-2"
              onClick={async () => {
                await api.clearWebSearchKey();
                setSearch(await api.webSearchStatus());
              }}
            >
              Remove the key
            </button>
          )}
        </div>
      </Row>
      <Row label="Auto-compact at (%)" hint="Default for models without their own setting. Blank = 85.">
        <Deferred
          value={prefs.autoCompactPercent == null ? "" : String(prefs.autoCompactPercent)}
          placeholder="85"
          mono
          width="w-24"
          onCommit={(v) => {
            const t = v.trim();
            if (t && !/^\d+$/.test(t)) return Promise.resolve("Enter a whole number");
            return update({ autoCompactPercent: t ? Number(t) : null });
          }}
        />
      </Row>
    </>
  );
}

/** Settings Grok Build reads from the config Patchbay writes for it. */
function GrokSettings() {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  return (
    <>
      <Row
        label="Background titles and recaps"
        hint="Extra model calls that name sessions and summarize turns. Turn off to save tokens."
      >
        <Toggle
          label="Background titles and recaps"
          checked={prefs.backgroundSummaries}
          onChange={(backgroundSummaries) => update({ backgroundSummaries })}
        />
      </Row>
      <ClaudeSetup label="Grok Build" />
    </>
  );
}

/** The categories of the Claude Code setup, in the order Settings lists them. */
const CLAUDE_PARTS = [
  ["instructions", "Instructions", "CLAUDE.md files (user, project, CLAUDE.local.md) and .claude/rules, added to the agent's instructions. AGENTS.md is always read."],
  ["skills", "Skills and slash commands", "Skills from skills/ (the agent loads one when a task matches it, or you run it as /name) and commands from commands/."],
  ["agents", "Subagents", "Agent definitions from agents/, offered as subagent types with their own instructions and tools."],
  ["mcpServers", "MCP servers", "Servers from ~/.claude.json and the project's .mcp.json, in addition to the ones set in Patchbay."],
  ["hooks", "Hooks", "Commands from Claude's settings that run before and after tools, on each prompt, and when a turn or session starts or stops."],
  ["plugins", "Plugins", "Installed Claude Code plugins, including the skills, commands, agents, hooks and MCP servers they bundle."],
] as const;

function Source({ s }: { s: string }) {
  return <span className="rounded bg-panel-2 px-1.5 py-px text-[11px] text-muted">{s}</span>;
}

/** Names found for a category, folded when there are many. */
function Found({ items, off }: { items: ClaudeItem[]; off: boolean }) {
  const [all, setAll] = useState(false);
  if (items.length === 0) return <p className="pb-2 text-xs text-faint">None found.</p>;
  const shown = all ? items : items.slice(0, 6);
  return (
    <ul className={`flex flex-col gap-1 pb-2 text-xs ${off ? "opacity-50" : ""}`}>
      {shown.map((i) => (
        <li key={`${i.source}:${i.name}`} className="flex min-w-0 items-baseline gap-2" title={i.description}>
          <span className="shrink-0 font-mono">{i.name}</span>
          <Source s={i.source} />
          <span className="truncate text-muted">{i.description}</span>
        </li>
      ))}
      {items.length > shown.length && (
        <li>
          <button className="text-accent-ink underline underline-offset-2" onClick={() => setAll(true)}>
            {items.length - shown.length} more
          </button>
        </li>
      )}
    </ul>
  );
}

/** One item with a switch of its own (a plugin, an MCP server). */
function ItemSwitch({
  label,
  detail,
  on,
  off,
  onChange,
}: {
  label: string;
  detail: ReactNode;
  on: boolean;
  off: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className={`flex items-center justify-between gap-4 py-1 pl-3 text-xs ${off ? "opacity-50" : ""}`}>
      <span className="flex min-w-0 items-baseline gap-2">
        <span className="font-mono">{label}</span>
        {detail}
      </span>
      <Toggle label={label} checked={on && !off} onChange={(v) => !off && onChange(v)} />
    </div>
  );
}

/**
 * What an agent loads from the user's Claude Code setup. Patchbay's own agent and Grok Build
 * share the category switches; Patchbay's agent also lists what it finds, with switches for each
 * plugin and MCP server.
 */
function ClaudeSetup({ label }: { label: string }) {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  const sessions = useStore((s) => s.codeSessions);
  const grok = label === "Grok Build";
  const cc = prefs.claudeConfig;
  // Projects to show the setup of: recent Code sessions' folders, newest first.
  const projects = [...new Set([...sessions].sort((a, b) => b.updatedAt - a.updatedAt).map((s) => s.cwd))];
  const [project, setProject] = useState<string>(projects[0] ?? "");
  const [overview, setOverview] = useState<ClaudeOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (grok) return;
    api.claudeOverview(project || null).then(
      (o) => {
        setOverview(o);
        setError(null);
      },
      (e) => setError(errorText(e)),
    );
  }, [grok, project, cc.disabledPlugins, cc.disabledMcpServers]);

  const setList = (key: "disabledPlugins" | "disabledMcpServers", name: string, on: boolean) => {
    const rest = cc[key].filter((x) => x !== name);
    return update({ claudeConfig: { ...cc, [key]: on ? rest : [...rest, name] } });
  };

  const details = (key: (typeof CLAUDE_PARTS)[number][0]) => {
    if (grok || !overview) return null;
    const off = !cc[key];
    switch (key) {
      case "instructions":
        return overview.instructions.length === 0 ? (
          <p className="pb-2 text-xs text-faint">None found.</p>
        ) : (
          <ul className={`pb-2 text-xs ${off ? "opacity-50" : ""}`}>
            {overview.instructions.map((p) => (
              <li key={p} className="truncate font-mono text-muted">
                {p}
              </li>
            ))}
          </ul>
        );
      case "skills":
        return <Found items={[...overview.skills, ...overview.commands.map((c) => ({ ...c, name: `/${c.name}` }))]} off={off} />;
      case "agents":
        return <Found items={overview.agents} off={off} />;
      case "mcpServers":
        return overview.mcpServers.length === 0 ? (
          <p className="pb-2 text-xs text-faint">None found.</p>
        ) : (
          <div className="pb-2">
            {overview.mcpServers.map((m) => (
              <ItemSwitch
                key={m.name}
                label={m.name}
                detail={
                  <>
                    <Source s={m.source} />
                    <span className="text-muted">{m.transport === "sse" ? "SSE (not supported)" : m.transport}</span>
                  </>
                }
                on={!m.disabled}
                off={off}
                onChange={(v) => setList("disabledMcpServers", m.name, v)}
              />
            ))}
          </div>
        );
      case "hooks":
        return overview.hooks.length === 0 ? (
          <p className="pb-2 text-xs text-faint">None found.</p>
        ) : (
          <ul className={`flex flex-col gap-1.5 pb-2 text-xs ${off ? "opacity-50" : ""}`}>
            {overview.hooks.map((h, i) => (
              <li key={i} className="min-w-0">
                <div className="flex items-baseline gap-2">
                  <span>
                    {h.event}
                    {h.matcher && h.matcher !== "*" ? ` · ${h.matcher}` : ""}
                  </span>
                  <Source s={h.source} />
                </div>
                <div className="truncate font-mono text-muted" title={h.command}>
                  {h.command}
                </div>
              </li>
            ))}
          </ul>
        );
      case "plugins":
        return overview.plugins.length === 0 ? (
          <p className="pb-2 text-xs text-faint">None installed.</p>
        ) : (
          <div className="pb-2">
            {overview.plugins.map((p) => (
              <ItemSwitch
                key={p.id}
                label={p.name}
                detail={!p.enabledInClaude && <span className="text-muted">off in Claude Code</span>}
                on={p.enabledInClaude && !p.disabled}
                off={off || !p.enabledInClaude}
                onChange={(v) => setList("disabledPlugins", p.id, v)}
              />
            ))}
          </div>
        );
    }
  };

  return (
    <>
      <div className="pt-4">
        <div className="text-sm">From your Claude Code setup</div>
        <div className="mt-0.5 text-xs text-muted">
          What {label} loads from <span className="font-mono">~/.claude</span> and each project's{" "}
          <span className="font-mono">.claude/</span> folder, alongside its own configuration.
          {grok ? "" : " The switches for each part are shared with Grok Build; the ones for single plugins and servers are Patchbay's own."}
        </div>
      </div>
      {!grok && (
        <Row label="Show the setup of" hint="A project adds its own .claude/ folder, CLAUDE.md, .mcp.json and settings.">
          <select className={`${input} w-56`} value={project} onChange={(e) => setProject(e.target.value)}>
            <option value="">Your user setup only</option>
            {projects.map((p) => (
              <option key={p} value={p}>
                {p}
              </option>
            ))}
          </select>
        </Row>
      )}
      {error && (
        <p className="py-2 text-xs text-danger" role="alert">
          {error}
        </p>
      )}
      {CLAUDE_PARTS.map(([key, name, hint]) => (
        <div key={key} className="border-b border-border last:border-b-0">
          <div className="flex items-start justify-between gap-6 py-3">
            <div className="min-w-0">
              <div className="text-sm">{name}</div>
              <div className="mt-0.5 text-xs text-muted">
                {key === "plugins" && grok
                  ? `${hint} Only builds of Grok Build that honour GROK_CLAUDE_PLUGINS_ENABLED can turn this off.`
                  : key === "hooks"
                    ? <><span className="text-danger">Hooks run commands on your machine</span> during sessions. {hint}</>
                    : hint}
              </div>
            </div>
            <div className="shrink-0">
              <Toggle
                label={`Claude Code ${name}`}
                checked={cc[key]}
                onChange={(v) => update({ claudeConfig: { ...cc, [key]: v } })}
              />
            </div>
          </div>
          {details(key)}
        </div>
      ))}
      <p className="pt-2 text-xs text-muted">Idle sessions restart to pick up changes; busy ones restart when their turn ends.</p>
    </>
  );
}

/** Limits of Patchbay's own agent. Blank keeps the default; changes apply from the next request. */
function EngineSettings() {
  const engine = useStore((s) => s.prefs.agentEngine);
  const update = useStore((s) => s.updatePrefs);
  const field = (key: keyof AgentEngine, label: string, hint: string, def: number, max: number) => (
    <Row key={key} label={label} hint={`${hint} Blank = ${def}.`}>
      <Deferred
        value={engine[key] == null ? "" : String(engine[key])}
        placeholder={String(def)}
        mono
        width="w-24"
        onCommit={(v) => {
          const t = v.trim();
          if (t && !/^\d+$/.test(t)) return Promise.resolve("Enter a whole number");
          const n = t ? Number(t) : null;
          if (n != null && (n < (key === "requestRetries" ? 0 : 1) || n > max)) return Promise.resolve(`Between ${key === "requestRetries" ? 0 : 1} and ${max}`);
          return update({ agentEngine: { ...engine, [key]: n } });
        }}
      />
    </Row>
  );
  return (
    <>
      <div className="pt-4">
        <div className="text-sm">Engine</div>
        <div className="mt-0.5 text-xs text-muted">
          How far the agent goes on its own. Model settings (temperature, max output, reasoning effort, context window)
          are per model under Models; each session can change its thinking effort in the composer.
        </div>
      </div>
      {field("maxSteps", "Steps per prompt", "Model requests one prompt may make before the turn stops.", 100, 10000)}
      {field("subagentMaxSteps", "Steps per subagent", "Model requests one subagent run may make.", 40, 10000)}
      {field("requestRetries", "Request retries", "Retries of a model request that fails before any output.", 2, 10)}
      {field("idleTimeoutSecs", "Stream timeout (s)", "Give up on a reply that sends nothing for this long.", 180, 3600)}
      {field("shellTimeoutSecs", "Shell timeout (s)", "Commands stop after this long unless the agent asks for more (up to 600).", 120, 600)}
    </>
  );
}

/** Settings → Coding agents: what applies to every agent, and a summary linking to each agent's page. */
export function AgentsPage({ list, onOpen }: { list: AgentList; onOpen: (id: AgentId) => void }) {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  const setToast = useStore((s) => s.setToast);
  useEffect(list.want, []);
  const ids = list.agents?.map((a) => a.id) ?? (Object.keys(AGENT_LABELS) as AgentId[]);

  return (
    <div className="flex max-w-xl flex-col">
      <p className="pb-2 text-xs text-muted">
        Code sessions run a coding agent over the Agent Client Protocol. Patchbay's own agent is built in and uses your
        providers and models. The others bring their own sign-in and models; install and sign in to them in a
        terminal. A session keeps the agent it started with. The settings here apply to every agent; open an agent for
        its own.
      </p>
      <Row label="Default agent" hint="New Code sessions start with it. You can pick another for each session.">
        <select
          className={`${input} w-56`}
          value={prefs.defaultAgent}
          onChange={async (e) => {
            const err = await update({ defaultAgent: e.target.value as AgentId });
            if (err) setToast(err);
          }}
        >
          {ids.map((id) => (
            <option key={id} value={id}>
              {AGENT_LABELS[id]}
            </option>
          ))}
        </select>
      </Row>
      <Row
        label="Default approval mode"
        hint={
          prefs.permissionMode === "always-approve" ? (
            <span className="text-danger">
              New sessions run edits, commands and network access without asking. Plans and questions still reach you,
              and deny rules in an agent's own config still apply.
            </span>
          ) : prefs.permissionMode === "auto" ? (
            "New sessions read and edit inside the project without asking; deletes, shell, network and anything outside the project still ask."
          ) : (
            "New sessions ask before anything the agent asks permission for. Each session can change it in its header."
          )
        }
      >
        <Choice
          value={prefs.permissionMode}
          onChange={async (permissionMode) => {
            const err = await update({ permissionMode });
            if (err) setToast(err);
          }}
          options={[
            { value: "default", label: "Ask first" },
            { value: "auto", label: "Auto edits" },
            { value: "always-approve", label: "Approve all" },
          ]}
        />
      </Row>
      <div className="pt-3">
        <div className="text-sm">Custom instructions</div>
        <div className="mt-0.5 text-xs text-muted">
          Added to the agent's system prompt in every Code session, alongside the project's AGENTS.md. Each agent's page
          says whether it receives them.
        </div>
        <LongText
          value={prefs.agentInstructions}
          placeholder="e.g. Run the test suite after changing code. Never commit without asking."
          onCommit={(v) => update({ agentInstructions: v })}
        />
      </div>

      <div className="pt-3 text-sm">Agents</div>
      <Loading list={list} />
      {list.agents?.map((a) => (
        <button
          key={a.id}
          className="flex items-center justify-between gap-6 border-b border-border py-2.5 text-left last:border-b-0 hover:bg-panel-2/40"
          onClick={() => onOpen(a.id)}
        >
          <span className="flex min-w-0 items-center gap-2 text-sm">
            <StatusDot on={a.available} />
            {a.label}
            {prefs.defaultAgent === a.id && (
              <span className="rounded bg-panel-2 px-1.5 py-px text-[11px] text-muted">default</span>
            )}
          </span>
          <span className="flex shrink-0 items-center gap-2 text-xs text-muted">
            {a.available ? (a.version ?? "installed") : "not found"}
            <span aria-hidden>›</span>
          </span>
        </button>
      ))}
      <div className="pt-3">
        <button className="text-xs text-accent-ink underline underline-offset-2" onClick={list.reload}>
          Check again
        </button>
      </div>
      <PermissionRulesList />
    </div>
  );
}

/** Settings → Coding agents → one agent: where it was found, its models, and its own settings. */
export function AgentPage({ id, list }: { id: AgentId; list: AgentList }) {
  const prefs = useStore((s) => s.prefs);
  const update = useStore((s) => s.updatePrefs);
  const providers = useStore((s) => s.providers);
  useEffect(list.want, []);
  const a = list.agents?.find((x) => x.id === id);
  const onClaudeLogin = providers.some((p) => p.authMode === "claude_login");

  const setPath = async (path: string) => {
    const agentPaths = { ...prefs.agentPaths };
    if (path) agentPaths[id] = path;
    else delete agentPaths[id];
    const err = await update({ agentPaths });
    if (!err) list.reload();
    return err;
  };

  if (!a) return <Loading list={list} />;

  return (
    <div className="flex max-w-xl flex-col">
      <div className="flex items-start justify-between gap-6 border-b border-border pb-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2 text-sm">
            <StatusDot on={a.available} />
            {a.available ? "Installed" : "Not found"}
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
          <div className="mt-0.5 text-xs text-faint">
            {a.usesPatchbayModels ? "Runs on your Patchbay providers and models. " : ""}
            {a.supportsInstructions ? "Receives your custom instructions." : "Doesn't receive your custom instructions."}
          </div>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1.5">
          <button
            className="rounded-md border border-border px-2.5 py-1 text-xs text-muted hover:text-fg disabled:opacity-50"
            disabled={prefs.defaultAgent === a.id}
            onClick={() => update({ defaultAgent: a.id })}
          >
            Make default
          </button>
          <button className="text-xs text-accent-ink underline underline-offset-2" onClick={list.reload}>
            Check again
          </button>
        </div>
      </div>
      {a.takesProviders && <ProviderChoice agent={a.id} label={a.label} />}
      {((a.id === "claude-code" && !prefs.agentProviders[a.id]) || (a.id === "patchbay" && onClaudeLogin)) && (
        <UsageLimit agent={a.id} />
      )}
      {(a.id !== "grok" && a.id !== "patchbay") || prefs.agentPaths[a.id] ? (
        <Row label="Executable" hint="Absolute path. Blank looks it up on PATH.">
          <Deferred
            value={prefs.agentPaths[a.id] ?? ""}
            placeholder={a.id === "claude-code" ? "claude-agent-acp" : (a.program ?? "")}
            mono
            onCommit={(v) => setPath(v.trim())}
          />
        </Row>
      ) : null}
      {a.usesPatchbayModels && <PatchbayModelSettings />}
      {a.id === "patchbay" && <ClaudeSetup label="Patchbay" />}
      {a.id === "patchbay" && <EngineSettings />}
      {a.id === "grok" && <GrokSettings />}
    </div>
  );
}
