# Capability-aware team roles: design plan

Status: proposal, no code yet. Branch `feat/teams` at bb474c5c.

## Goal

A role can say what it needs ("UI checker needs vision, plus a browser or computer use").
The template editor shows each seat's capabilities, and each agent/model choice's, and warns
when a choice can't meet its role. Warn first; decide on blocking at dispatch later.

## What exists today (facts)

| Layer | What we have | Where | Detected or declared |
|---|---|---|---|
| Model | `vision` bool, default true | `db.rs:328`, `ModelPage.tsx:245` | Declared by the user (default true, so weak) |
| Model | `context_window` | `db.rs:328`, `providers.rs:242` | Detected from the provider's model list |
| Model | `reasoning_effort` param | `ipc.ts:33` | Declared per model |
| Model | tool calling | none | not found |
| Agent | approval `gaps`, hidden modes | `code/agents.rs:197-247` (`ApprovalSetup`) | Static table |
| Agent | system-prompt hook (role survives compaction) | `teams.rs:885` `has_prompt_hook` (grok, claude-code, antigravity, codex) | Static |
| Agent | team-tool auto-approve | `teams/allow.rs` `has_rule` (none for opencode) | Detected from the agent's config |
| Agent | `uses_patchbay_models` (patchbay, grok) | `agents.rs:112` | Static |
| Agent | MCP over http/sse | ACP `mcpCapabilities` at initialize, `mcp.rs:49-61` | Detected, but only at runtime per session |
| Agent | subagents, web search/fetch, computer use, headless stability | none | not found |
| MCP | one global server list, sent to every Code session over ACP | `code/mcp.rs`, `McpPage.tsx:57` | Configured. No `tools/list` probe, so what a session actually got is not known. Agent-native MCP (`~/.claude`) is invisible to sessions. |
| Host | installed | `agents.rs:548` `detect_all` | Detected |
| Host | signed in | `agents.rs:490` (Antigravity only); otherwise ACP `auth_required` at runtime | Mostly runtime |
| Host | macOS Accessibility / Screen Recording | none | not found |
| Teams | `Caps` = workflow powers (writes_items, dispatches, decides, merges) | `teams.rs:86` | Not agent capabilities; keep separate |
| model-registry | models.dev fields: `limit.context`, `reasoning`, `reasoning_options`, `status`, cost | `docs/model-registry/poll_diff.py:119` | Feed; no vision/tool-call fields extracted yet |

## Vocabulary (v1)

Names are snake_case and match models.dev where it has one.

Model layer, a property of the seat's model:
- `vision`: models.dev `modalities.input` contains `image`; locally, the Settings `vision` flag.
- `tool_call`: models.dev `tool_call`.
- `reasoning`: models.dev `reasoning` / `reasoning_options`.
- `context`: a number, not a bool. A role can require `context >= N`.

Harness layer, a property of the agent (static table next to `ApprovalSetup`):
- `prompt_hook`: from `has_prompt_hook`. Moved into the table, with one source of truth.
- `team_tools_auto`: the team tools can be allow-listed. Detected per host via `allow::has_rule`; the table says whether it is possible at all (opencode: no).
- `subagents`, `web_search`, `web_fetch`, `computer_use`: declared per agent.
- `mcp_http`: the agent accepts http/sse MCP servers. Declared in the table, then confirmed at runtime from ACP `mcpCapabilities`.
- Approval gaps stay as they are: shown, never a "requirement".

Session tool layer, provided by MCP servers:
- `browser`, `blender`, and any tag the user adds: a tag on an MCP server in Settings > MCP. Because the list is global, every session of an agent that can reach that server's transport has it.

Host layer, checked, not declared:
- `installed` (detected now), `signed_in` (detected for Antigravity only; others "unknown" until a session hits `auth_required`), and `macos_permissions` for computer use.

### Add / drop vs the original list
- **Add** `mcp_http`. Without it, an http browser MCP silently doesn't reach the agent (it is only logged at `code/mod.rs:250`). That is exactly the "has a browser" question.
- **Add** `context` as a threshold, not a flag.
- **Drop "stable in headless ACP" as a requirement.** It is an observation that changes per agent version, not something a role should demand. Show it as a per-agent note in the warning text instead.
- **Fold** `supports_instructions` into `prompt_hook`. A role that must survive compaction needs the hook; the prepended `<team-instructions>` fallback works for everything else.

## Shape

Rust:
- `code/agents.rs`: `AgentCaps { prompt_hook, team_tools_allowable, subagents, web_search, web_fetch, computer_use, mcp_http }` per `AgentKind`, beside `ApprovalSetup`. `teams::has_prompt_hook` reads it.
- `teams.rs` `Role`: `requires: Vec<Vec<String>>`. Each inner list is any-of, the outer list is all-of. Example: `[["vision"], ["browser", "computer_use"]]`. Plus `min_context: Option<i64>`. Stored as JSON (one schema column), `#[serde(default)]`, so old roles load unchanged.
- Seat read-fill, the same pattern as `gaps`: `seat.capability_check: Vec<ChoiceCheck { agent, model, has: [cap], missing: [[cap]], unknown: [cap] }>` for the seat agent and each `choice`. Computed on read and never stored, like `gaps` (`teams.rs:170-181`).
- Model capabilities:
  - Agents that use Patchbay models (patchbay, grok) read the `Model` row: `vision`, `context_window`.
  - Other agents choose their own model. For those the seat's `model` string is looked up in a small cached models.dev snapshot (the registry's fields: vision via modalities, tool_call, reasoning, `limit.context`).
  - No match gives `unknown`, never `missing`.
- MCP tags: `McpServer.tags: Vec<String>`. A session tag counts only if the server is enabled and its transport is stdio, or the agent has `mcp_http`.

UI:
- Settings > MCP: a tags field per server, with suggestions `browser`, `blender`.
- Settings > Agents, per agent: a read-only "Capabilities" row from `AgentCaps`. This is where agent facts live.
- Teams > Roles editor: a "Requires" picker (any-of groups, plus a minimum context).
- Teams > Templates editor: per seat and per choice, capability chips. Missing ones show as an amber warning ("Kilo: no vision, no browser"); unknown ones are grey. The template can still be saved.

## Detection vs declaration

| Capability | v1 | Later |
|---|---|---|
| installed, context (Patchbay models), `team_tools_auto` (per host) | detected | |
| `mcp_http` | declared | confirm from ACP initialize; record the last seen value per agent |
| MCP tool tags | declared (user tags) | `tools/list` probe against each configured server, to suggest tags |
| vision / tool_call / reasoning (other agents) | models.dev snapshot | refresh with the model-registry poller |
| vision (Patchbay models) | user flag | prefill from models.dev on model add |
| signed_in | detected for Antigravity, else unknown | per-agent auth probes |
| computer_use permissions | not checked; warning text only | macOS TCC preflight. Open question: TCC attributes the child agent's access to which process? Needs a spike before we rely on it. |
| prompt_hook, subagents, web_*, computer_use (harness) | declared | |

## Teams vs per-agent Settings

Facts about an agent or MCP server live with that agent or server: the Agents and MCP pages, and the Rust table. Teams only states requirements and shows the match. One source, so a new agent version gets updated in one place.

## Increments (each its own commit, verified)

1. Rust: the `AgentCaps` table, `Role.requires` + `min_context`, the seat `capability_check` read-fill, `McpServer.tags`. Unit tests for the any-of/all-of match and the unknown handling.
2. Settings: the MCP tags field, the Agents capability row.
3. Teams editors: the Requires picker and the seat warnings.
4. Later and separate: the models.dev snapshot, the ACP `mcpCapabilities` record, the dispatch-time warning or block.

## Decisions (David, 2026-11-11)

1. Enforcement: warn in the template editor only. Dispatch is not refused.
2. Model capabilities for agents that pick their own model come from a cached models.dev snapshot. No match means "unknown", never "missing".
3. Agent facts (`subagents`, `web_*`, `computer_use`) are declared in the Rust table. Settings displays them, with no per-agent user override.

Sequencing: per the director (4fc0a3d8), this comes after the real-backend end-to-end loop works (dispatch, decision, release).
