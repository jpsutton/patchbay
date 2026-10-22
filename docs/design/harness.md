# Patchbay's own coding agent

Status: built into the app. Replaces Grok Build as the default agent and as Patchbay's LLM client. This note says how it is put together and why, and what is left out.

## Goals

- Nothing in the release build comes from outside the repository: no vendored agent, no sidecar binary, no patch set to carry.
- Provider-agnostic. The user's own providers and models, over the four wire formats that cover almost every host: OpenAI Chat Completions (and the many servers that imitate it), OpenAI Responses, Anthropic Messages and Google Gemini.
- Stay one agent among several. Patchbay still drives Claude Code, OpenCode, Cursor CLI, Kilo Code, Codex, Gemini CLI and an installed Grok Build over ACP, and the UI branches on capabilities, never on identity (`harness-agnostic-code-tab`).

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
3. Build the request: the static system block (persona, tool guidance, honesty about outcomes) with a cache breakpoint, then a dynamic block (environment, plan-mode note, the user's instructions, `AGENTS.md`/`CLAUDE.md` from the working directory up to the repository root), then the history and the tools for the mode.
4. Stream the reply as `agent_message_chunk` and `agent_thought_chunk`.
5. Announce every tool call as a pending card, then run them: consecutive read-only calls that need no approval together, the rest one after another. A call that changes anything asks first with `session/request_permission` (with a diff preview for edits). Output of long commands streams on the card's terminal channel.
6. Append the results, with reminders, and go round again until the model stops asking for tools (at most 100 requests per prompt).

Usage goes out as `usage_update` (context fill, `_meta.autoCompactPercent`) and in the prompt response (`usage`), which `code/mod.rs` folds into the same `Usage` as every other agent's.

### Tools

`Read` (text with line numbers, images, PDFs as text, notebooks as cells), `Write`, `Edit`, `NotebookEdit` (all need a prior read and stop when the file changed since), `Glob`, `Grep` (the `ignore` and `regex` crates, no `rg` binary), `Bash` (a process group, persistent working directory, timeout, background mode with `BashOutput` and `KillShell`), `TodoWrite` (shown as the plan), `WebFetch` (HTML to Markdown, answered by the session model; cross-host redirects are returned, not followed), `WebSearch` (Brave Search; offered only when an API key is set, see below), `Task`, `SendMessage` and `TaskStop` (subagents, see below), `AskUserQuestion`, `ExitPlanMode` and the tools of the user's MCP servers (`mcp__server__tool`). The MCP client is small and local: stdio and streamable HTTP, `tools/list` and `tools/call`.

### Web search

`WebSearch` calls the Brave Search API (`/res/v1/web/search`). The key comes from the `BRAVE_API_KEY` environment variable, else the OS keychain, else the database when there is no keychain; Settings → Coding agents → Patchbay sets and removes it. Without a key the tool is not offered. Domain filters become `site:` and `-site:` operators; results come back as numbered plain text with a reminder to end the answer with a Sources list of Markdown links. Each search asks first (kind `fetch`), like WebFetch. `PATCHBAY_BRAVE_URL` points the tool at another host, for the mock server in the tests.

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
- Rewind (`patchbay/rewind/points` and `.../execute`) drops a prompt and everything after it (`Entry::Truncate`), and with `mode: "all"` puts back the files the later prompts changed from per-prompt snapshots taken before the first change. Snapshots live in memory: after a restart the conversation can still rewind, the files can't. Files changed through Bash are not tracked.
- Reminders are `<system-reminder>` parts the harness adds to the next user message: a file the agent read changed under it, the model was switched, the task list went stale.

### Persistence

One JSON line per entry in `<data>/agent/sessions/<id>.jsonl`: settings, messages (the neutral form, so reasoning signatures and encrypted reasoning survive), the final card of each tool call, compactions and rewinds. `session/load` replays the effective entries as updates, so the UI shows what it showed before. The file moves with the session between computers (`code/agent_state.rs`).

## What is left out

The reference this was built from (`docs/HARNESS-SPEC-generic.md`) describes more than a desktop app needs. Not built: the coordinator/worker harness, `Workflow`, team messaging, `ScheduleWakeup`, `Monitor`, deferred tools with `ToolSearch`, `Skill`, artifacts, lean and classic tool descriptions and the persona variants for other surfaces. Subagents are one level deep, live in memory (they don't survive a restart) and have no `model` override.

## Tests

- `cargo test` in each crate: adapter fixtures, SSE decoding, tools, store, compaction, MCP against a tiny stdio server.
- `cargo test -- --ignored` against `scripts/mock-llm.py`, which speaks all four formats: tool round trips per format (`patchbay-llm`), and the agent over ACP per format, plan mode, compaction and reload, rewind, change reminders, questions and subagents (`patchbay-agent`), plus web search against the mock's Brave endpoint, parallel, background, fork, named and worktree subagents.
- `cargo test --lib smoke -- --ignored` in `src-tauri` drives the agent through `CodeManager`: edit with permission, usage, resume, rewind, rules and MCP.
