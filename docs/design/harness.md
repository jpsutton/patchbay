# Patchbay's own coding agent

Status: built into the app. Replaces Grok Build as the default agent and as Patchbay's LLM client. This note says how it is put together and why, and what is left out.

## Goals

- Nothing in the release build comes from outside the repository: no vendored agent, no sidecar binary, no patch set to carry.
- Provider-agnostic. The user's own providers and models, over the four wire formats that cover almost every host: OpenAI Chat Completions (and the many servers that imitate it), OpenAI Responses, Anthropic Messages and Google Gemini.
- Stay one agent among several. Patchbay still drives Claude Code, OpenCode, Cursor CLI, Kilo Code, Codex and an installed Grok Build over ACP, and the UI branches on capabilities, never on identity (`harness-agnostic-code-tab`).

## Two crates

### `crates/patchbay-llm`

One neutral conversation model: `Message { role, parts }`, with `Part::{Text, Image, Document, Reasoning, ToolCall, ToolResult}`, `ToolSpec` and system blocks that carry a cache hint. `Client::complete` streams a reply through a callback as `Event::{Text, Reasoning, ToolCall, Retrying}` and returns a `Response` whose assistant message can be appended to the history as it is.

Each wire format is an adapter in `wire/` with a request builder and a stream parser:

| Format | Notes |
|---|---|
| `chat_completions` | tool-call deltas with and without `index`; `reasoning_content`/`reasoning`; usage including cached tokens and a reported cost; `max_completion_tokens` for api.openai.com, `max_tokens` elsewhere (what Ollama, vLLM and the rest take) |
| `responses` | stateless (`store: false`) with `reasoning.encrypted_content` carried back in `Part::Reasoning.opaque`; summaries as reasoning |
| `anthropic` | thinking blocks with signatures round-trip; adaptive thinking plus `output_config.effort`; `cache_control` breakpoints on flagged system blocks and the transcript tip; PDFs as `document` blocks; the Claude-login OAuth identity block |
| `gemini` | `streamGenerateContent?alt=sse`; function calls with `thoughtSignature`; `thinkingLevel` for Gemini 3, `thinkingBudget` before; inline PDFs and images; `parametersJsonSchema` so tool schemas pass as they are |

The client retries before any output has arrived (429, 5xx, connection errors, a stream that ends empty), honors `Retry-After`, enforces an idle timeout and cancels promptly. Chat, title generation and the agent all use it; `providers.rs` maps a model from Settings to its `Config`.

### `crates/patchbay-agent`

An ACP server written against raw JSON-RPC (`rpc.rs`), not the typed ACP crate, so it can carry extension methods and fields freely. It runs inside the app: `acp.rs` connects to it over a pair of in-memory pipes and treats it as one more agent process (`AgentProc::Native`). The same crate builds a `patchbay-agent` binary on stdio for probes. The app side is `code/native.rs`, which implements `Host`: the model list, `model_client` (resolve, refresh a Claude token, build the client), settings and PDF text. Provider code never enters the agent crate, and API keys never travel through ACP frames, which the wire log can record.

Modules: `agent.rs` (ACP methods), `session.rs` (history, mode, files, shells, prompt records), `turn.rs` (the loop), `prompt.rs`, `compact.rs`, `store.rs`, `mcp.rs`, `tools/`.

## The turn

1. Turn the ACP prompt into parts, add the harness's reminders and record a rewind point.
2. Compact first when the last request filled the window past the threshold (the model's own auto-compact percent, else the preference, else 85).
3. Build the request: the static system block (persona, tool guidance, honesty about outcomes) with a cache breakpoint (see the cache clock below), then a dynamic block (environment, plan-mode note, the user's instructions, `AGENTS.md`/`CLAUDE.md` from the working directory up to the repository root), then the history and the tools for the mode.
4. Stream the reply as `agent_message_chunk` and `agent_thought_chunk`.
5. Announce every tool call as a pending card, then run them: consecutive read-only calls that need no approval together, the rest one after another. A call that changes anything asks first with `session/request_permission` (with a diff preview for edits). Output of long commands streams on the card's terminal channel.
6. Append the results, with reminders, and go round again until the model stops asking for tools (at most 100 requests per prompt).

Usage goes out as `usage_update` (context fill, `_meta.autoCompactPercent`) and in the prompt response (`usage`), which `code/mod.rs` folds into the same `Usage` as every other agent's.

### Tools

`Read` (text with line numbers, images, PDFs as text, notebooks as cells), `Write`, `Edit`, `NotebookEdit` (all need a prior read and stop when the file changed since), `Glob`, `Grep` (the `ignore` and `regex` crates, no `rg` binary), `Bash` (a process group, persistent working directory, timeout, background mode with `BashOutput` and `KillShell`), `TodoWrite` (shown as the plan), `WebFetch` (the page's main content as Markdown, see below; cross-host redirects are returned, not followed), `WebSearch` (Brave Search; offered only when an API key is set, see below), `Task`, `SendMessage` and `TaskStop` (subagents, see below), `AskUserQuestion`, `ExitPlanMode` and the tools of the user's MCP servers (`mcp__server__tool`). The MCP client is small and local: stdio and streamable HTTP, `tools/list` and `tools/call`.

### Fetching pages

`WebFetch` keeps what a model needs from a page (`extract.rs`). It picks the main content: `main`, `[role=main]`, a single `article` or a known content container such as `.entry-content`, else the block with the most prose and the fewest links. Inside it, it drops scripts, styles, forms, images and other non-text tags, hidden elements (`hidden`, `aria-hidden`, `display:none`), `nav`, `aside`, `footer`, page headers (an article's own header with a title stays), elements with a furniture role (`navigation`, `banner`, `contentinfo`, `complementary`, `search`) or class (cookie and consent banners, ads, sidebars, share bars, related lists, comments, popups), and short runs of nothing but links. A drop that would take half the page is ignored, so a wrong guess can't empty it. The rest becomes Markdown with relative links made absolute and a `Title:`/`URL:` header. With a `prompt` the session model answers it from that text; without one the cleaned page itself comes back, up to 40,000 characters.

### Web search

`WebSearch` calls the Brave Search API (`/res/v1/web/search`). The key comes from the `BRAVE_API_KEY` environment variable, else the OS keychain, else the database when there is no keychain; Settings → Coding agents → Patchbay sets and removes it. Without a key the tool is not offered. Domain filters become `site:` and `-site:` operators; results come back as numbered plain text with a reminder to end the answer with a Sources list of Markdown links. Each search asks first (kind `fetch`), like WebFetch. `PATCHBAY_BRAVE_URL` points the tool at another host, for the mock server in the tests.

The Chat tab uses the same key as a fallback. Preferences → Chat → Web search turns search on (off by default). Where the provider has a search of its own, chat offers that instead: Anthropic's `web_search` server tool (up to 5 searches per reply), OpenAI Responses' `web_search` and Gemini's `google_search`. Only the vendors' own endpoints count (`providers::native_web_search`), not other servers that speak the same wire format. The provider runs those inside the reply, and the wire adapters report each query as `Event::Search`. Every other provider gets the `WebSearch` function tool when a Brave key is set; `chat::send` runs the calls itself, with no approval prompt, and asks again for up to 5 rounds. The last round sets `tool_choice` to none so the model has to answer. Both paths show "Searching the web: …" as the reply's status line. Chat history keeps the reply text only, not the search calls.

### Subagents

A subagent is a child `Session` (`session.rs`): same client, model and MCP servers, but its own history, files, shells, working directory and tool-call id prefix (`agent_1:`), so its calls never collide with the main agent's cards. Its tool calls show in the transcript with a "↳" prefix and ask for permission like any other. It lives in memory only.

- **Types.** `explore` is read-only (Read, Glob, Grep, WebFetch, WebSearch). `general-purpose` starts fresh with every tool except the ones that talk to the user or start subagents. `fork` starts with the main agent's conversation (up to, but not including, the message holding the call) and its system prompt, and does one directive. In plan mode only `explore` starts. A subagent can't start subagents, so there is one level.
- **Parallel.** Several `explore` or background calls in one message run side by side. Ones that may edit files take turns.
- **Background.** `run_in_background` returns at once; the report comes back as a `<task-notification>` reminder on the main agent's next step. When the main agent has nothing left to do, the agent keeps `session/prompt` open until the background subagents finish and then runs another step with their reports; this is the `_meta.subagents` client capability `acp.rs` already advertises. A client without it gets the reports on the next prompt instead. Stop on the user's cancel stops them too, and so does Send now.
- **Named and resumable.** A subagent gets an id (`agent_N`) and optionally a name. `SendMessage` queues a message for one that runs, or continues a finished one from its whole history (in the turn, or in the background). `TaskStop` stops a background one.
- **Isolation.** `isolation: "worktree"` runs it in a new git worktree under `<data>/agent/worktrees/`, on a branch `patchbay/agent_N-…`. When it finishes untouched the worktree and branch are removed; otherwise they stay, and the report says where, so the changes can be reviewed and merged. Paths there are outside the project, so Auto edits does not pre-approve them.
- Their tokens count toward the turn's usage.

### Plan mode, compaction, rewind, reminders

- Plan mode is an ACP mode. The model is offered only read-only tools plus `ExitPlanMode`, whose approval is a decision the client always asks about.
- Compaction (`/compact [focus]` or automatic) asks the model for a structured summary and replaces the history with it (`Entry::Compacted`). The summarizer sees the history without images and with shortened tool output.
- Stopping one call. Each running tool call has its own cancel token, a child of the turn's, kept in `Session.calls` by tool call id. `patchbay/cancel_tool` (`sessionId`, `toolCallId`) cancels it: the call ends as failed, the model reads that the user stopped it (`call_stopped`, not `interrupted`, which is for a stopped turn) and the turn goes on. `patchbay/kill_shell` (`sessionId`, `shellId`) kills one background Bash job. A background Bash call's final update carries `_meta.background_shell {id}` (the id BashOutput and KillShell take); when the job ends a `tool_call_update` with `_meta.background_shell {id, done: true, exit_code}` follows (`-1` when it was killed). The app offers both as a Stop button on the card and in the Jobs panel, only when `extensions.cancelTool` is set (Patchbay's own agent, hosts that have this change). Test: `stopping_a_tool_call_or_job_that_is_not_running_does_nothing`, `registry_kills_one_job_by_id`.
- Rewind (`patchbay/rewind/points` and `.../execute`) drops a prompt and everything after it (`Entry::Truncate`), and with `mode: "all"` puts back the files the later prompts changed from per-prompt snapshots taken before the first change. Snapshots live in memory: after a restart the conversation can still rewind, the files can't. Files changed through Bash are not tracked.
- Reminders are `<system-reminder>` parts the harness adds to the next user message: a file the agent read changed under it, the model was switched, the task list went stale.

### Prompt cache clock and `/keepwarm`

Anthropic keeps a cached prompt for a fixed time after its last use, and a prompt that arrives later re-writes the whole context at the cache-write price. The TTL is chosen per request (`cache_control.ttl`): the API default is 5 minutes, `"1h"` asks for an hour. A Claude subscription login asks for `"1h"` for the agent (`providers::agent_cache_ttl`, as Claude Code does; the API accepts it on the OAuth token without an extra beta header). A stored API key keeps the 5-minute default, because the 1-hour write costs more per token. Chat keeps the default. Every breakpoint of one request carries the same TTL, since the API rejects a 5-minute entry ahead of a 1-hour one.

The session (`cache.rs`) records when a request last read or wrote the cache, and sends it in `usage_update`'s `_meta.cache` (`lastAt`, `ttlSecs`, `auto`). Only Anthropic requests have a clock; for the other formats the endpoint decides and nothing is sent. `code/usage.rs` keeps it in the session's usage state and the usage bar counts down from `lastAt` (amber in the last 5 minutes of a context of 30k tokens or more, "cold" after). A model change, an effort change or a compaction clears `lastAt`, because the cached prefix no longer matches.

- `/keepwarm` sends the history again with the one-line question "Reply with just: ok" and drops the answer. The request comes from the same `turn::build_request` as the real one, so the provider reads the cached prefix and restarts its clock for the price of a cache read. The history is not changed.
- `/keepwarm auto` toggles auto mode. Every 30 seconds an idle session in auto mode checks the clock and refreshes when the cache is warm with under 5 minutes left and the context is at least 30k tokens, up to 3 times until the next real request.
- A prompt sent after the cache expired on a context of 30k tokens or more gets a one-line note saying what it will re-cache.

### Persistence

One JSON line per entry in `<data>/agent/sessions/<id>.jsonl`: settings, messages (the neutral form, so reasoning signatures and encrypted reasoning survive), the final card of each tool call, compactions and rewinds, and a `req` entry per model call (outcome, stop reason, the output cap sent, usage, time to first text, total time, retries, error), so a stalled or failed turn can be explained afterwards (`jq 'select(.t=="req")' <id>.jsonl`). Replay and restore skip `req` entries, and a build that predates them skips the lines. `session/load` replays the effective entries as updates, so the UI shows what it showed before. The file moves with the session between computers (`code/agent_state.rs`).

## What is left out

The reference this was built from (`docs/HARNESS-SPEC-generic.md`) describes more than a desktop app needs. Not built: the coordinator/worker harness, `Workflow`, team messaging, `ScheduleWakeup`, `Monitor`, deferred tools with `ToolSearch`, `Skill`, artifacts, lean and classic tool descriptions and the persona variants for other surfaces. Subagents are one level deep, live in memory (they don't survive a restart) and have no `model` override.

## Prompts

All prompt text is in `crates/patchbay-agent/prompts/`, loaded by `prompts.rs`:

| File | Holds |
|---|---|
| `system.md`, `environment.md`, `compact.md`, `subagent.md` | the big prompts, each with a minified twin `*.min.md` |
| `messages.json` | the short fragments (reminders, refusals, notes to subagents, the compaction wrapper, the WebFetch answering prompt) as `{full, min}` pairs with `{placeholders}` |
| `tools.min.json` | the minified description of each tool; the full one sits with the tool's schema in `tools/` |
| `terse.md` | the reply-style instruction used only when minifying (it has no full form) |

**Minified prompts** are optional. They shrink what the agent sends and ask the model to answer tersely. The minified texts follow the `caveman-compress` rules of the caveman plugin: no articles, filler, hedging or pleasantries; fragments are fine; code spans, paths, tool names, numbers and `{placeholders}` stay exact. They cut the prompt files and tool descriptions by about a fifth, and a whole first request by about 11%, because the tools' JSON schemas don't shrink. Two more pieces go with them, from the caveman plugin's reply rules: `terse.md`, a short reply-style instruction added to the static system block, and a one-line `TERSE MODE ACTIVE` reminder added to each user prompt (a system prompt alone is often forgotten; the reminder is what makes it stick). Replies come back in fragments without articles or filler, with code, commands, paths and errors exact, and normal prose for security warnings and irreversible-action confirmations. In a live test on a Claude login the same question got a reply about 45% shorter. The instruction costs about as much as the prompt texts save, so the saving on a request is small (about 6% on a first request) and the real gain is in the replies. Tests keep each pair honest: the minified form must be shorter, keep every code span, placeholder and heading, name only real tools, and every message the code asks for must exist.

Who decides:
1. The session, if it chose: the ACP config option `minify_prompts` (`session/set_config_option`, values `on` and `off`, listed in `configOptions` of `session/new` and `session/load`), or `_meta.minifyPrompts` on `session/new` and `session/load`. The choice is saved with the session.
2. Otherwise the host's `Settings::minify_prompts`: in the app, Settings → Coding agents → Patchbay → "Compact prompts" (it applies to the next request, with no restart); for the `patchbay-agent` stdio binary, `PATCHBAY_AGENT_MINIFY=1`.

A subagent uses its parent's form from the moment it starts. Adding a prompt means adding both forms; adding a message means a `{full, min}` pair in `messages.json`.

## Tests

- `cargo test` in each crate: adapter fixtures, SSE decoding, tools, store, compaction, MCP against a tiny stdio server.
- `cargo test -- --ignored` against `scripts/mock-llm.py`, which speaks all four formats: tool round trips per format (`patchbay-llm`), and the agent over ACP per format, plan mode, compaction and reload, rewind, change reminders, questions and subagents (`patchbay-agent`), plus web search against the mock's Brave endpoint, parallel, background, fork, named and worktree subagents.
- `cargo test --lib smoke -- --ignored` in `src-tauri` drives the agent through `CodeManager`: edit with permission, usage, resume, rewind, rules and MCP.
