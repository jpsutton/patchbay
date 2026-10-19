# Pluggable coding agents

Status: first version on `feat/agent-backends`. This note records how the Code tab drives more than one coding agent, what each agent supports, and what is still open.

## Goal

A Code session runs one coding agent. Grok Build ships with Patchbay and stays the default. A session can instead run an agent the user already has installed and signed in to:

| Agent | id | Launch | Sign-in |
|---|---|---|---|
| Grok Build | `grok` | bundled sidecar: `grok agent --no-leader [-m pb-<model>] stdio` | Patchbay's providers (rendered `config.toml`) |
| Claude Code | `claude-code` | `claude-agent-acp`, or `npx -y @agentclientprotocol/claude-agent-acp@<pinned>` | the user's Claude Code login |
| OpenCode | `opencode` | `opencode acp` | `opencode auth login` and its own config |
| Cursor CLI | `cursor` | `cursor-agent acp` | `cursor-agent login` |
| Kilo Code | `kilo` | `kilo acp` (npm `@kilocode/cli`) | `kilo auth login` and its own config |
| Codex | `codex` | `codex-acp`, or `npx -y @agentclientprotocol/codex-acp@<pinned>` | `codex login`, or `OPENAI_API_KEY` / `CODEX_API_KEY` |
| Gemini CLI | `gemini` | `gemini --acp`, or `npx -y @google/gemini-cli@<pinned> --acp` | `gemini` sign-in, or `GEMINI_API_KEY` |
| Antigravity | `antigravity` | `agy_acp_server [--uid=]` (Google's archive from the ACP registry; path set in Settings) | `auth.type` in `~/.gemini/antigravity-acp/settings.json` |

All eight speak ACP over stdio, so one client (`code/acp.rs`) drives them all. Per-agent differences live in `code/agents.rs`. The UI branches on what the agent advertises, never on which agent it is. The one exception is model handling, below.

## Shape

- `code/agents.rs`
  - `AgentKind`: the ids above, labels, install and sign-in hints.
  - `resolve()`: finds the executable. It checks the Settings override first, then the bundled sidecar (Grok), then PATH, then the user bin directories that a desktop launch can leave out of PATH (`~/.local/bin`, `~/.npm-global/bin`, `~/.bun/bin`, `~/.opencode/bin` and others). Last comes `npx` for Claude Code, Codex and Gemini CLI (`AgentKind::npx_package`).
  - `prepare()`: builds a `Launch` with the program, arguments, environment, `Extensions`, session `_meta`, the model to reselect, and the sign-in hint. The Grok-only steps moved here from `CodeManager::start` and `acp::run`: `GROK_HOME`, `config.toml`, telemetry env and the `-m pb-…` model key.
  - `detect_all()`: backs Settings → Coding agents. It runs `--version` on each agent it finds, but never through npx, which would download the adapter.
- `code/acp.rs`
  - Launches whatever `Launch` says.
  - Sends the `x.ai/statusLine` client capability only when `Extensions.status_line` is set.
  - Refuses `Rewind` unless `Extensions.rewind` is set.
  - Normalizes session config (next section).
  - Folds standard ACP usage (`usage_update`, `PromptResponse.usage`) into the same `Usage` event as Grok's status line.
  - Turns ACP `auth_required` (-32000) into a message that tells the user how to sign in.
- DB: `code_sessions.agent` (default `grok`) and `code_sessions.agent_model`. ACP session ids only mean something to the agent that made them, so switching agents starts a new ACP session (next section).
- Preferences: `defaultAgent` and `agentPaths` (id -> executable). Changing `agentPaths` restarts idle sessions, like the other agent settings.

## Models, modes and config options

Agents report these three ways:

- Legacy `models` / `modes` on `session/new` and `session/load`. Grok and Cursor do this.
- `configOptions` only, with `category: model | mode | thought_level`. OpenCode and Kilo do this. Kilo lists about 330 models.
- Both. Claude's adapter sends legacy `modes` plus a `model` config option.

`SessionConfig` in `acp.rs` derives `models` / `modes` from the `model` and `mode` config options when present, so the UI keeps one model picker and one mode switch. It flattens option groups. It remembers which option ids carry model and mode, so `SetModel` / `SetMode` go through `session/set_config_option` for those agents and through `session/set_model` / `set_mode` for the others. The remaining options (reasoning effort and the like) reach the UI as `configOptions` and render as extra chips in the composer footer. Each `config_option_update`, and each `set_config_option` response, sends a `config` event with the full new state.

Models are the one place where agent identity shows in the logic. Grok runs on Patchbay's models (`pb-<model id>`, with pricing, vision and context window from Settings). The other agents bring their own model ids. For those, the chosen model is saved in `agent_model` and reselected after `session/new` or `session/load` when the agent comes back on a different one. `AgentKind::uses_patchbay_models` gates this.

## Switching agents mid-session

The composer's agent picker works at any point in a session, on the desktop and on the phone. It lists the agents installed on the computer that runs the session: `list_agents` takes a `host`, and asks that computer through `code.new_options`. While the agent is mid-turn, the switch is refused.

Before the first message, a switch only changes `agent`. After it, the new agent can't load the old agent's session, so `code_set_agent_local` hands over the conversation as text:

- `handoff::fold_for_switch` moves the saved transcript into its `earlier` part and ends it with a "Switched from X to Y" divider. The new agent's own turns start empty.
- `handoff::switch_prompt` writes a note with the whole conversation, with a `Tool: …` line for each tool call. It goes in `mesh.handoff.{id}`, the same slot a move between computers uses, and the next prompt carries it once.
- Notes are wrapped in `<patchbay-handoff>…</patchbay-handoff>`. The store drops that wrapper from replayed user messages, so the note doesn't appear in the transcript.
- The UI shows `earlier` dimmed, above the current agent's turns. Those turns can't be edited or rewound.

Switching back to an agent the session had before also starts a fresh ACP session with the note. It does not resume that agent's old session.

## Custom instructions

- Grok: `_meta.rules` on `session/new` / `session/load`.
- Claude Code: `_meta.systemPrompt.append`. The adapter merges it into the `claude_code` preset prompt.
- Antigravity: `_meta.antigravity.systemPrompt.append`, which the server advertises in `initialize` (`agentCapabilities._meta.antigravity.systemPrompt`).
- Codex: `CODEX_CONFIG={"developer_instructions": …}`. The adapter merges `CODEX_CONFIG` into every thread's config. Not yet checked against a real turn.
- Gemini CLI: no append. `GEMINI_SYSTEM_MD` replaces the whole system prompt, so it isn't used.
- OpenCode, Cursor, Kilo: no way in today. Settings says so. ACP has a `systemPrompt` RFD (Claude's adapter advertises `sessionCapabilities.systemPrompt`), but `agent-client-protocol` 0.10.4 has no field for it yet.

## Verified (2026-10-17, this machine)

`cargo test --lib agents_live -- --ignored` runs start, model list, model switch, restart, resume and model restore against the installed agents: Claude Code adapter 0.93.0, OpenCode 2.0.29, Cursor CLI 2026.10.01, Kilo 7.8.12. All four pass. One prompt each went end to end through Claude Code and Cursor, including resume after a turn and usage folding for Claude. OpenCode's prompt failed only because its configured provider server was down (`ECONNREFUSED`); Patchbay showed that error.

Observed quirks:

- Cursor and Claude Code can't `session/load` a session that never got a prompt. Patchbay falls back to a new session and shows a notice.
- Cursor sends no usage. Claude sends `usage_update` with context size and cumulative cost.
- Each agent writes its sessions to its own store (`~/.claude`, `~/.local/share/opencode`, `~/.cursor`, Kilo's data dir). Patchbay keeps only the ACP session id.

## Codex, Gemini CLI and Antigravity (2026-10-19)

Probed from Patchbay's own launch path (`cargo test --lib agents_live -- --ignored`) with codex-acp 2.2.2, Gemini CLI 0.64.0 and the Antigravity ACP server 1.4.1. The machine had no sign-in for any of them, so the runs used placeholder API keys under a throwaway `HOME`. Start, model list, model switch, restart and model restore pass for all three. No prompt has gone end to end yet.

- **Codex** (`@agentclientprotocol/codex-acp`, the adapter the ACP registry lists; it bundles the Codex binary). It reports both legacy `modes` and config options: `mode`, `model`, `reasoning_effort`, `collaboration_mode` (Default / Plan) and `fast-mode`. Its modes are approval and sandbox presets. Patchbay starts it in `read-only` (`INITIAL_AGENT_MODE`, then `set_mode`) and hides `workspace-write`, `agent` ("Auto review", the adapter's default) and `agent-full-access`. `read-only` asks before every edit and network use, but its sandbox runs shell commands without asking; that is listed as a gap. The adapter reads an API key from the environment only when told to sign in with one, so Patchbay sets `DEFAULT_AUTH_REQUEST={"methodId":"api-key"}` when `OPENAI_API_KEY` or `CODEX_API_KEY` is set. `session/load` of a session that never got a prompt fails, and Patchbay falls back to a new session.
- **Gemini CLI** (`gemini --acp`). Legacy `modes` and `models` only. Patchbay selects `default` and hides `autoEdit` and `yolo`; `plan` stays. Its built-in policy allows read-only tools (read, glob, grep, list, web search) without asking, which is listed as a gap. An empty session doesn't `session/load` either. Google moved unpaid and Google One users from Gemini CLI to Antigravity in June 2026; Gemini CLI still serves API-key, Vertex and paid users.
- **Antigravity** (Google's `agy_acp_server`, a ~930 MB Python archive plus `localharness_external`, from `dl.google.com/agy-extensions/…` as listed in the ACP registry). Not on npm, so there is no npx fallback; the user unpacks it and sets the path in Settings, or puts it on PATH. It reports `model`, `mode` and `thought_level` config options plus legacy fields. Patchbay selects `default` and hides `auto_edit` and `yolo`. `session/load` works even for an empty session. It refuses `session/new` until `auth.type` is set in `$GEMINI_HOME/antigravity-acp/settings.json` or a client calls `authenticate`. It keeps a per-workspace trust list (`trusted_workspaces.json`); how an untrusted workspace prompts over ACP is unverified.
- `.codex/` and `.gemini/` joined the protected paths in `permissions.rs`.

## Open items

- **Sign-in for Codex, Gemini CLI and Antigravity.** Patchbay never calls ACP `authenticate`; it shows a hint instead. All three advertise browser OAuth as an auth method, and Antigravity has no terminal login of its own. A "Sign in" button that calls `authenticate` with a chosen method would cover them all.
- **Antigravity install.** A one-click install from the ACP registry (download, unpack under the data dir, record the path) would spare the manual step. The archive is large and proprietary, so it should stay opt-in.
- **Patchbay providers for Codex.** `CODEX_CONFIG` takes `model_provider` and `model_providers` (OpenAI-compatible `base_url`, `env_key`), so Codex could run on Patchbay's OpenAI-kind providers the way OpenCode does. Untested.
- **A real turn on each of the three**, with instructions, approvals and usage, once someone signs in.

- **Patchbay providers for OpenCode, Kilo and Claude Code.** I tried injecting a Patchbay provider into OpenCode 2.0.29 through `OPENCODE_CONFIG_CONTENT`, with and without `enabled_providers`. `opencode acp` ignored it: the model list and default stayed those from `~/.config/opencode/opencode.jsonc`. The next things to try are a private config dir (`OPENCODE_CONFIG_DIR`, or `XDG_CONFIG_HOME` pointed at a Patchbay-owned directory) or OpenCode's `serve` API. A private dir would also hide the user's own OpenCode setup, so it should be opt-in. Claude Code could run on an Anthropic-kind provider through `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`. That is also opt-in and untested.
- **OpenCode retries.** When its provider is unreachable, OpenCode retries for minutes and reports each attempt only in `session_info_update._meta["opencode/retry"]`. Patchbay shows "Working…" the whole time. A generic "agent is retrying" hint would need either that meta key or a timeout.
- **Mesh.** `CodeSession` gained `agent` and `agent_model`. Mesh sync and session migration need to carry them. Migrating a session to a node without that agent installed should fail cleanly. The mesh branch adds DB migrations too, so whichever branch merges second renumbers its migration.
- **Rewind for other agents.** Only Grok has it (`x.ai/rewind/*`). Claude's adapter and OpenCode advertise `sessionCapabilities.fork`, which could support "edit and resend" by forking at a message. Today the UI hides edit-and-resend unless `extensions.rewind` is set.
- **Other ACP agents** in the registry (Goose, Qwen Code, GitHub Copilot CLI and others) need only a new `AgentKind` arm.
- **The npx pin** (`CLAUDE_ACP_PACKAGE`) needs a bump now and then. A global install of `claude-agent-acp` overrides it.
