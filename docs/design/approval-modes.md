# Approval modes for every coding agent

Status: plan. Nothing here is built yet. It replaces the policy in PR #16 (`feat/client-approval-policy`) and keeps that PR's plumbing: the `code_sessions.permission_mode` column, its mesh sync, the `code.set_permission_mode` RPC and the header control.

## Goal

Each Code session has one approval mode. The mode decides which tool calls run without asking. It should mean the same thing whichever agent runs the session, and where an agent can't honor it, the UI should say so instead of showing a setting that does nothing.

## What's wrong with the first attempt (#16)

- **Auto approves almost everything.** Only `execute` calls and calls without a kind still ask. `delete`, `move`, `fetch`, `other` (MCP tools, Claude's NotebookEdit and Skill) and `switch_mode` all go through. No path check is made, so edits outside the project go through too.
- **It answers questions that aren't permission prompts.** Claude's ExitPlanMode ("Approve Plan") arrives as `switch_mode`. Cursor's AskQuestion arrives as a permission request whose answers are `allow_once` options. Auto and Always would approve the plan and pick an answer to the question.
- **It can pick `allow_always`.** `allow_option` falls back to `allow_always` when there is no `allow_once` option. On Claude that option can switch the session to bypass mode (the `exit-plan-*` options), and on Claude and Cursor it writes rules into the user's own settings files.
- **The agent's own policy runs first.** Grok reads `[ui] permission_mode` from the shared `config.toml`, which comes from the global preference. A session set to Ask under a global Auto never asks. OpenCode allows everything by default, so with OpenCode Ask means nothing. Cursor edits files in the workspace without asking at all.

## Principle: agents ask, Patchbay decides

At launch, each agent is configured to ask over ACP for everything it can. Patchbay then answers each `session/request_permission` from the session's mode. Three things follow:

- Changing the mode never needs a restart, because the agent's configuration doesn't depend on it.
- Every agent gets the same meaning for each mode, wherever Patchbay sees the request.
- Deny rules in the agent's own config still apply. Claude Code, Cursor, OpenCode and Kilo enforce them before asking, so Always can't override them. Grok is unverified.

What Patchbay never sees, it can't gate. Each agent lists those gaps (see "Per-agent setup"), and the header shows them.

## Classifying a request

`classify(tool_call, options, cwd)` returns one class. It uses the merged tool call that `UiClient::merged_tool_call` already builds, so `locations` from earlier updates count.

| Class | Matches |
|---|---|
| Decision | kind `switch_mode`; any option id starting `exit-plan-`; Cursor's AskQuestion (an option id `__ask_question_skip__`, or no reject option at all). Not a safety prompt: the user is being asked to choose. |
| Read | kind `read`, `search` or `think`, with every path inside the project. A `search` with no path but a query or URL counts as Network (Cursor's WebSearch). |
| Project edit | kind `edit` or `move`, with every path inside the project and none protected. |
| Delete | kind `delete`, or an `edit` that the agent titles as a delete (Cursor sends deletes as `edit`). |
| Shell | kind `execute`. |
| Network | kind `fetch`, and web searches as above. |
| Outside | any path outside the project, or a protected path. |
| Unknown | kind `other`, a missing kind, or paths that can't be read. |

Paths come from `locations[].path` and the usual `rawInput` keys (`path`, `file_path`, `target_file`, `filePath`, `source`, `destination`). Each one is resolved against the cwd and canonicalized, following symlinks. A path whose parent doesn't exist yet resolves through its nearest existing ancestor. An edit that names no path is Unknown, never Project edit.

Protected paths count as Outside even inside the project: `.git/`, `.env` and `.env.*` (but not `.env.example`), and the agents' own config directories (`.claude/`, `.cursor/`, `.grok/`, `.opencode/`, `.kilocode/`).

## Modes

| Class | Ask | Auto | Always |
|---|---|---|---|
| Decision | ask | ask | ask |
| Read | ask | allow | allow |
| Project edit | ask | allow | allow |
| Delete | ask | ask | allow |
| Shell | ask | ask (later: read-only commands allowed, see phase 3) | allow |
| Network | ask | ask | allow |
| Outside | ask | ask | allow |
| Unknown | ask | ask | allow |

Precedence, first match wins:

1. Decision: ask the user.
2. A Patchbay deny rule matches (phase 3): pick `reject_once`.
3. A saved always-allow rule matches: allow.
4. The mode allows the class: allow.
5. Otherwise ask.

"Allow" always means the `allow_once` option. If the request offers no `allow_once`, Patchbay asks the user; it never picks `allow_always` on its own. Saved rules change to the same behavior. Today they can fall back to `allow_always`.

Every automatic answer emits `AutoApproved` with the class and mode (for example "Auto: edit inside the project"). The tool row already shows the "auto" badge.

Labels: Cursor's ACP modes include one called "Ask" (read-only Q&A), shown right next to this control. To avoid two different "Ask" buttons, the approval control reads **Ask first**, **Auto edits** and **Approve all**. The stored values stay `default`, `auto` and `always-approve`, matching Grok's names and the preference.

## Per-agent setup

`agents::prepare()` gains an `approvals` part of `Launch`. The UI reads only what it advertises, never the agent id.

```rust
pub struct ApprovalSetup {
    /// Mode to select right after session/new or session/load, so the agent asks.
    pub select_mode: Option<String>,
    /// Agent modes that are really approval policies; hidden from the agent-mode control.
    pub hide_modes: Vec<String>,
    /// What this agent does without asking, whatever the mode. Shown in the header.
    pub gaps: Vec<&'static str>,
}
```

Environment and config changes go in the existing `env` and config-writing steps.

### Patchbay (the built-in agent)

- It never decides on its own: every call that changes something (Write, Edit, NotebookEdit, Bash, KillShell, WebFetch, MCP tools) goes to the client as a `session/request_permission` with ACP tool kinds (`edit`, `execute`, `fetch`, `other`), so `classify` and `decide` judge it like any other agent's. Plan approval (`switch_mode`, option ids `exit-plan-*`) and questions (`__ask_question_skip__`) are decisions and always ask.
- Gaps: reads and searches inside the project (Read, Glob, Grep, BashOutput) run without asking. A read or search outside the project asks.
- Bash edits are not tracked as edits: the call is `execute`, so it asks like any shell command, but a rewind can't restore files it changed.

### Grok Build

- Write `[ui] permission_mode = "default"` into the rendered `config.toml`, whatever the preference says. Patchbay decides from there.
- `GROK_HOME` is shared by every session (`data_dir/grok`), so this config can't hold a per-session mode. Pinning it to `default` avoids the problem.
- Unverified: whether Grok's own permission rules in that config apply before it asks.
- Gaps: none known.
- Later, maybe: Grok's Auto is a background classifier, not a fixed list. It could be offered as an extra Grok-only mode through the `x.ai/yolo_mode_changed` notification (`{"permission_mode": "auto"}`). That notification affects the whole process, and Patchbay runs one Grok process per session, so it would work per session. It's out of scope here.

### Claude Code (`claude-agent-acp` 0.93.0)

- After `session/new` or `session/load`, `session/set_mode` to `default` (its "Manual" mode). The starting mode comes only from the user's settings files; `_meta.claudeCode.options.permissionMode` is overwritten by the adapter.
- `hide_modes`: `acceptEdits`, `auto`, `bypassPermissions`, `dontAsk`. Keep `default` and `plan` in the agent-mode control. Leaving plan mode re-selects `default`, not the mode ExitPlanMode's options would pick.
- Gaps: rules in the user's `permissions.allow` (in their Claude settings) run without asking. Patchbay could pass `_meta.claudeCode.options.settings` to override them, but that block is SDK-side and unverified, and overriding the user's own allow rules is a choice to make deliberately. Start by documenting the gap.

### Cursor CLI (2026.10.01)

- No per-session control exists. ACP modes `agent`, `plan` and `ask` are task modes. `--force` (per process) makes Cursor ask about nothing, which is the opposite of what's wanted.
- Launch as today. Cursor asks for: shell commands not on its allowlist, deletes, writes outside the workspace, MCP tools and web access.
- Gaps, shown in the header:
  - "Cursor edits files in this project without asking, in every mode."
  - "Shell commands on Cursor's own allowlist run without asking." That list is `permissions.allow` in `~/.cursor/cli-config.json`.
- Approve all works by answering everything Cursor asks, so `--force` isn't needed. Ask first and Auto edits differ only for what Cursor does ask about.
- Later: because silent edits can't be gated, a cheap undo would help, such as a git stash-style checkpoint per turn. That's a separate design.

### OpenCode (2.0.29) and Kilo Code (7.8.12)

- Both read inline config from an env var: `OPENCODE_CONFIG_CONTENT` for OpenCode, `KILO_CONFIG_CONTENT` for Kilo. Inject `{"permission": {"*": "ask"}}` so every tool asks.
- `deny` entries in the user's config should survive this. **Unverified:** how an inline `"*"` merges with the user's pattern rules. Verify first. If the inline config replaces the user's `permission` block, read their deny entries and carry them over.
- **Unverified:** `docs/design/agents.md` records that `opencode acp` ignored a provider injected through `OPENCODE_CONFIG_CONTENT`, while the research for this plan found the variable read and loaded. Settle that with a live test before relying on it. The fallback is a Patchbay-defined agent (`patchbay`, with `permission: {"*": "ask"}`) selected with `set_mode`. That still needs the config to load, so if neither works, list "OpenCode allows everything by default" as a gap.
- `hide_modes`: none. OpenCode's and Kilo's modes are agents (`build`, `plan`, `code`, `debug`, `ask`).
- Kilo asks only `once` / `reject` for skill-shell and sandbox-escalation prompts. That's fine: without `allow_always` on offer, Patchbay picks `once` anyway.

## UI

- **Header control:** the three-way control from #16, relabeled. On the phone it doesn't fit next to the agent-mode control, so it moves into the session menu with the current mode shown as a chip.
- **Gaps:** if the agent advertises any, a small "i" next to the control lists them. They also appear once, as an info line, when a session starts in Auto edits or Ask first.
- **Agent-mode control:** modes in `hide_modes` disappear from it.
- **Preference:** Settings → Default permission mode stays as the default for new sessions. Its hint stops mentioning deny rules, because only the agents' own deny rules exist until phase 3.
- **Setting the mode** no longer bumps `updated_at`, like `set_code_session_activity`. Changing a setting shouldn't move the session to the top of the list.

## Mesh and phone

The decision runs on the host, inside the agent's ACP client, which is where it runs today. The mode syncs with the session row, as in #16. The phone changes it through `code.set_permission_mode`, which goes through the gateway to the host. #16 has a compile error in the phone command (`json!({ "id": id, "mode" })`); fix that in phase 1.

## Phases

1. **Classifier and safe answers** (Patchbay only; fixes #16):
   - `classify()`, the mode table, `allow_once` only, decisions never answered.
   - Relabel the control, fix the phone command, stop bumping `updated_at`.
   - Make saved rules use `allow_once` only.
2. **Make agents ask:**
   - `ApprovalSetup` in `prepare()`.
   - Grok config pinned to `default`; Claude `set_mode` plus hidden modes.
   - OpenCode and Kilo env injection, after verifying it.
   - Gaps shown in the header.
3. **Deny rules and read-only shell:**
   - Patchbay deny rules: the same table as allow rules plus an `effect` column, answering `reject_once`.
   - In Auto edits, built-in read-only commands run without asking when they aren't compound (`is_compound` already exists): `ls`, `cat`, `head`, `tail`, `wc`, `rg`, `grep`, `find` without `-exec` or `-delete`, `git status`, `git diff`, `git log`, `git show`.
4. **Undo for silent edits:** a per-turn checkpoint so Cursor's ungated edits can be rolled back. Separate design.

## Tests

- **`classify()` table tests:** fixtures captured from each agent's real payloads. Record them with `agents_live` and save them under `src-tauri/tests/fixtures/permissions/`. Include:
  - Claude's ExitPlanMode with its `exit-plan-*` options, and NotebookEdit as `other`.
  - Cursor's AskQuestion, a delete sent as `edit`, and WebSearch as `search`.
  - Kilo's once/reject-only prompt.
  - Edits through a symlink that leaves the project, edits to `.git/config` and `.env`, and an edit with no path.
- **Option choice:** never `allow_always`, and no `allow_once` means the user is asked.
- **Live tests** (`agents_live`, ignored by default), per agent and mode:
  - A write inside the project produces a request in Ask first (except Cursor, which is a listed gap) and is answered automatically in Auto edits.
  - A shell command asks in Auto edits and is answered automatically in Approve all.
  - A plan approval always reaches the user.
  - Changing the mode mid-session applies to the next request.
- **Mesh:** set the mode from a phone against a session hosted on another node, and check the next request there.

## Open questions

- Whether overriding a user's own Claude `permissions.allow` rules is wanted, or the documented gap is enough.
- Whether Auto edits should allow deletes inside the project. This plan asks, because Cursor already asks for every delete and a delete is harder to undo than an edit.
- Whether Grok's classifier Auto should appear as an extra Grok-only mode later.
