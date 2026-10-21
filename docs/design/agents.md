# Pluggable coding agents

Status: first version on `feat/agent-backends`. This note records how the Code tab drives more than one coding agent, what each agent supports, and what is still open.

## Goal

A Code session runs one coding agent. Grok Build ships with Patchbay and stays the default. A session can instead run an agent the user already has installed and signed in to:

| Agent | id | Launch | Sign-in |
|---|---|---|---|
| Grok Build | `grok` | bundled sidecar: `grok agent --no-leader [-m pb-<model>] stdio` | Patchbay's providers (rendered `config.toml`) |
| Claude Code | `claude-code` | `claude-agent-acp`, or `npx -y @agentclientprotocol/claude-agent-acp@<pinned>` | the user's Claude Code login, or its terminal sign-in from the Code tab |
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
  - Handles ACP `auth_required` (-32000) with a sign-in flow (see "Sign-in" below).
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

## Sign-in

An agent that isn't signed in refuses `session/new` (or `session/load`) with `auth_required` (-32000). Instead of failing, `acp::run` then:

1. Picks the auth methods from `initialize` that Patchbay can run (`auth_choices`):
   - Methods the agent carries out itself: no `type`, or `type: "agent"`. A method whose `_meta` has an `api-key` entry is marked `apiKey`, and the UI asks for a key.
   - Terminal methods: `type: "terminal"`, or the older convention of a `_meta["terminal-auth"]` entry. These are marked `terminal` (see "Terminal sign-in" below).
   - Methods of type `env_var` are left out, and so are custom gateways (`_meta.gateway`), which need a URL.
2. Sends `authRequired { message, methods }`. The message is the old hint text: Patchbay's `login_hint`, or the methods' descriptions.
3. Waits for `AgentCmd::Authenticate { method_id, api_key }` (the `code_authenticate` command). It also answers `AgentCmd::TerminalAuth`. Other commands get "the agent isn't signed in yet". `Shutdown` or a restart ends the wait cleanly.
4. Calls ACP `authenticate`. A browser method can block for minutes while the user signs in, and the loop keeps serving `Shutdown` meanwhile. An API key goes in `_meta["api-key"]`. Gemini CLI reads that key as a string, and codex-acp reads it as `{ "apiKey": … }`. So Patchbay sends the string first and the object if that is refused.
5. On success it retries opening the session, and the command resolves once the session is up. If the agent accepts the sign-in but still refuses the session, the command fails with the agent's message, and a new `authRequired` follows. With no method Patchbay can run, the hint becomes the start error, as before.

Nothing here depends on which agent runs; it follows only the advertised methods. The agent stores whatever credentials it gets. Patchbay never saves the key and never logs it.

### Signed out mid-session

An agent can also lose its sign-in after the session started: a logout, or an expired Codex or Gemini token. Then `session/prompt` fails with `auth_required`. The prompt task runs beside the command loop, so it reports through events:

- With methods Patchbay can run, it sends `authRequired` with "… needs you to sign in again", then `promptDone` with stop reason `error`. No error entry goes in the transcript.
- With none, it sends an error with the `login_hint` ("… needs you to sign in again. Run `claude` …") in place of the bare "Authentication required".

The main loop then accepts `AgentCmd::Authenticate` and runs ACP `authenticate` beside the loop, because a browser sign-in can take minutes. The session already exists, so nothing is retried. On success the agent thread sends `authResolved` and a notice, "Signed in. Send your message again.", and the user resends. A refused sign-in fails the command and leaves the card up. Outside such a loss, `Authenticate` still gets "the agent is already signed in", and a second sign-in while one runs gets "a sign-in is already in progress".

### Terminal sign-in

ACP's unstable `AuthMethodTerminal` (agent-client-protocol-schema 0.11.4) is a method the client carries out: it runs the agent's binary with the method's extra `args` and `env` in an interactive terminal, so the user signs in through the agent's own TUI. Agents offer such methods only to clients that say they can run them. Patchbay sends both forms of that capability in `initialize`: `clientCapabilities.auth.terminal = true` (ACP) and `clientCapabilities._meta["terminal-auth"] = true` (the older convention). Claude's adapter (claude-agent-acp 0.93) then offers `claude-ai-login` and `console-login`, `type: "terminal"`, with `args` such as `["--cli", "auth", "login", "--claudeai"]`. Before this, Patchbay sent neither form, so Claude Code advertised no methods and Patchbay fell back to its "Run `claude` … /login" hint.

How Patchbay runs a terminal method:

- The card's button opens a terminal inside the card. `term_open` (and the `term.open` mesh RPC, for a session on another computer) takes an `authMethod`. The UI never sends a command line. The host asks the agent thread (`AgentCmd::TerminalAuth`), and `terminal_auth_command` builds the command from the advertised method and from how Patchbay launched the agent:
  - When the method has `_meta["terminal-auth"].command`, that command and its `args` run.
  - Otherwise, ACP's form runs: the agent's own launch program and arguments, then the method's `args`. For the npx launch, that is `npx -y @agentclientprotocol/claude-agent-acp@<pinned> --cli auth login --claudeai`.
  - The agent's launch environment applies, plus the method's `env`. A method id that isn't an advertised terminal method is refused.
- The command runs in a PTY in the session's folder, on the computer that runs the agent, through `TerminalManager::open_program`.
- When it exits 0, the card restarts the agent (`startAgent`), as its Cancel button does. A fresh process reads the new login, and the restart resumes the session (`session/load`) when the sign-in was mid-session. Patchbay doesn't call ACP `authenticate` after a terminal sign-in, because the spec doesn't ask for it and Claude's adapter rejects it for these methods. A non-zero exit shows the exit code on the card, and Back returns to the method list.

### UI

`CodeState.auth` holds the event. It is cleared by `sessionReady`, by `authResolved` and by the agent exiting. `SignInCard` shows one button per method: a password field for API-key methods, and an embedded terminal for terminal methods. Its title is "Sign in to start the agent" before the session starts and "Sign in to continue" mid-session. While an `authenticate` sign-in runs, it says to finish in the browser, and offers Cancel, which restarts the agent. The status light shows "needs you", and the agent picker stays usable, so the user can pick another agent instead. `LiveAgent.auth` keeps the event for windows and phones that attach later, and `authResolved` clears it. For a session on another computer, `code.authenticate` goes over the mesh with the long RPC timeout, and the browser opens on the computer that runs the agent. The phone app doesn't render the card yet.

What each agent advertises (signed out, 2026-10-19):

| Agent | Methods |
|---|---|
| Codex | `api-key` (key), `chat-gpt` (browser; left out when `NO_BROWSER` is set). `chat-gpt-device-code` only for clients with URL elicitation, which Patchbay lacks. |
| Gemini CLI | `oauth-personal` (browser), `gemini-api-key` (key), `vertex-ai` (environment), `gateway` (left out) |
| Antigravity | `oauth-personal`, `oauth-business`, `gemini-api-key` (from `GEMINI_API_KEY`, no key field), `agent-platform` |

Tests in `acp.rs` drive a fake Python ACP agent:

- `sign_in_then_start_the_session`: a refused key, the second key shape, and the session start. The agent lists its terminal method only because Patchbay advertised the capability.
- `sign_in_again_when_a_prompt_is_refused`: the prompt `expire` signs the agent out. Then come the `authRequired` card, a refused and an accepted sign-in in the main loop, `authResolved`, and a prompt that works again.
- `refused_prompt_without_methods_shows_the_login_hint`: the same loss with no methods shows the hint as an error.
- `terminal_sign_in_then_restart`: runs the command `TerminalAuth` returns, and checks that a restarted agent starts its session without asking.

`fakeBackend.ts` fakes the flow for Codex and Gemini CLI: the key `bad` is refused, and browser methods take 2.5 s. Claude Code gets the two terminal methods, whose fake login prints a few lines and exits 0 after 3 s. The prompt `/logout` signs the session's agent out, to show the mid-session card.

## Custom instructions

- Grok: `_meta.rules` on `session/new` / `session/load`.
- Claude Code: `_meta.systemPrompt.append`. The adapter merges it into the `claude_code` preset prompt.
- Antigravity: `_meta.antigravity.systemPrompt.append`, which the server advertises in `initialize` (`agentCapabilities._meta.antigravity.systemPrompt`).
- Codex: `CODEX_CONFIG={"developer_instructions": …}`. The adapter merges `CODEX_CONFIG` into every thread's config. Not yet checked against a real turn.
- Gemini CLI: no append. `GEMINI_SYSTEM_MD` replaces the whole system prompt, so it isn't used.
- OpenCode, Cursor, Kilo: no way in today. Settings says so. ACP has a `systemPrompt` RFD (Claude's adapter advertises `sessionCapabilities.systemPrompt`), but `agent-client-protocol` 0.10.4 has no field for it yet.

## Messages sent mid-turn (2026-10-20)

The composer stays open while the agent works. `acp::run` keeps one turn at a time per agent and never sends a second `session/prompt` while one is running. Most agents mishandle one:

| Agent | Overlapping `session/prompt` |
|---|---|
| Claude Code (claude-agent-acp 0.93.0) | queued until the turn ends |
| Codex (codex-acp 2.2.2) | joins the running turn, but the first request likely never gets an answer |
| Grok Build | queued by default |
| OpenCode 2.1.1 | refused (-32603) |
| Kilo 7.8.15 | ends the running turn early, after its current step |
| Gemini CLI 0.64.0, Cursor 2026.10 | cancels the running turn |
| Antigravity 1.4.1 | races; output can be lost |

So a message sent mid-turn takes one of two paths:

- **Steered** into the running turn when the agent supports it:
  - Claude Code and Codex advertise `initialize` `_meta.steering.supported` and take `_session/steering` (`{sessionId, prompt}`). Patchbay sends `_meta.steering.idleBehavior: "promptRequired"`, so Claude hands back a message that arrives just after its turn ended. Patchbay then sends it as a new turn. Codex ignores this field. In that window it starts a turn of its own, and no `session/prompt` reports when that turn ends.
  - Grok Build takes `x.ai/interject` (`{sessionId, text, content}`) and merges the message at its next safe point. Grok doesn't advertise this, so it is `Extensions.interject`. Its interject carries only text and images, so a message with files or @-mentions waits for the next turn instead.
  - Patchbay checks steering support only from what the agent advertises, never from whether a call returns. Antigravity answers any `_` method with `null`.
- **Queued** otherwise, or when a steer fails. Patchbay sends queued messages one at a time when the running turn ends normally. A stop, or a turn that ends in an error, drops the queue. The window that sent the dropped messages gets their text back in its composer.

Every prompt carries a `promptId`, which the UI generates and the backend fills in when it is missing. The agent thread reports each step as a `userPrompt` event with a `delivery`:
- `sent`: a new turn. This marks the session busy.
- `steered`: emitted before the agent answers, so the message shows ahead of the reply.
- `queued`: the UI moves it from the transcript to the queue list above the composer. When its turn starts, it comes again as `sent`.

`unqueued` removes messages from the queue: the user clicked ✕ (`code_unqueue`), or the turn was stopped or failed (`restore: true`). The store keys all of this by prompt id, so the sending window, other windows and a catch-up replay all agree. A `userPrompt` without a `promptId` comes from an older host and keeps the old origin check.

Tests: `steering_joins_the_running_turn`, `a_refused_steer_waits_for_the_next_turn`, `prompts_mid_turn_wait_without_steering`, `stopping_hands_back_the_queue` and `grok_interjects_text_and_queues_files` drive the fake agent in `acp.rs`. `fakeBackend.ts` steers for Grok, Claude Code and Codex, and queues for the other agents.

Not verified against the real agents yet: a live `_session/steering` turn on Claude Code or Codex, and `x.ai/interject` on the Grok sidecar. The queue isn't in the attach snapshot (`LiveAgent`): a window that opens a session mid-turn sees a queued message only once it is sent. Mesh catch-up replays include it. The composer sends mid-turn only to hosts that report `extensions.steering`; an older host gets the old behavior.

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

- **Sign-in, not yet covered.** The phone app shows no sign-in card. `env_var` auth methods (ACP unstable), custom gateways, and URL elicitation (codex-acp's device-code sign-in) aren't supported. No real browser or terminal sign-in has been tried end to end, since nobody is signed in on this machine. A direct probe of claude-agent-acp 0.93 with Patchbay's capabilities does list `claude-ai-login` and `console-login`.
- **Antigravity install.** A one-click install from the ACP registry (download, unpack under the data dir, record the path) would spare the manual step. The archive is large and proprietary, so it should stay opt-in.
- **Patchbay providers for Codex.** `CODEX_CONFIG` takes `model_provider` and `model_providers` (OpenAI-compatible `base_url`, `env_key`), so Codex could run on Patchbay's OpenAI-kind providers the way OpenCode does. Untested.
- **A real turn on each of the three**, with instructions, approvals and usage, once someone signs in.

- **Patchbay providers for OpenCode, Kilo and Claude Code.** I tried injecting a Patchbay provider into OpenCode 2.0.29 through `OPENCODE_CONFIG_CONTENT`, with and without `enabled_providers`. `opencode acp` ignored it: the model list and default stayed those from `~/.config/opencode/opencode.jsonc`. The next things to try are a private config dir (`OPENCODE_CONFIG_DIR`, or `XDG_CONFIG_HOME` pointed at a Patchbay-owned directory) or OpenCode's `serve` API. A private dir would also hide the user's own OpenCode setup, so it should be opt-in. Claude Code could run on an Anthropic-kind provider through `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`. That is also opt-in and untested.
- **OpenCode retries.** When its provider is unreachable, OpenCode retries for minutes and reports each attempt only in `session_info_update._meta["opencode/retry"]`. Patchbay shows "Working…" the whole time. A generic "agent is retrying" hint would need either that meta key or a timeout.
- **Mesh.** `CodeSession` gained `agent` and `agent_model`. Mesh sync and session migration need to carry them. Migrating a session to a node without that agent installed should fail cleanly. The mesh branch adds DB migrations too, so whichever branch merges second renumbers its migration.
- **Rewind for other agents.** Only Grok has it (`x.ai/rewind/*`). Claude's adapter and OpenCode advertise `sessionCapabilities.fork`, which could support "edit and resend" by forking at a message. Today the UI hides edit-and-resend unless `extensions.rewind` is set.
- **Other ACP agents** in the registry (Goose, Qwen Code, GitHub Copilot CLI and others) need only a new `AgentKind` arm.
- **The npx pin** (`CLAUDE_ACP_PACKAGE`) needs a bump now and then. A global install of `claude-agent-acp` overrides it.
