# Fork a session from a message

Status: built for Patchbay's own agent. Other agents: see the table below.

## What the user gets

- **Fork from here**, an action on each user message in the Code view. It makes a new Code session with the conversation *before* that message, on the same computer, agent, model, folder and approval mode, titled "<source title> (fork)". The message itself goes into the new session's composer, unsent, so it can be edited and sent. The original session stays as it was. It is "edit and resend" without losing the branch you came from.
- **Fork session**, in the session's menu: the same, with the whole conversation and an empty composer. A second branch to try something in while the first keeps its state.

Both show only while the agent is ready and idle, and only when it advertises `extensions.fork`.

This is the idea mainplane calls fork and revert: one operation, copy the first N records of a session into a new one (`Sessions.Copy(from, id, n)` in mainplane's `pkg/statefile/session.go`). Rewind is the same cut made in place.

## What each agent can do

| Agent | Fork at message N | Fork at the end | How |
|---|---|---|---|
| Patchbay | yes | yes | `patchbay/fork`, below |
| Grok Build | not known | not known | has `x.ai/rewind/*`; check whether it has a fork |
| Claude Code, OpenCode | **no** | possible, not built | ACP `session/fork` |
| Others | no | no | the actions are hidden |

The limit for Claude and OpenCode is in the protocol. ACP's `session/fork` (unstable, behind `unstable_session_fork`; `ForkSessionRequest` in `agent-client-protocol-schema` 0.11.4, `src/agent.rs:1251`) takes `sessionId`, `cwd`, `mcpServers` and `_meta`, and **no message position**: it copies the whole session. Fork at message N would also need a rewind of the copy, and those agents have none (`agents.md`, "Rewind for other agents"). So the most they can get is **Fork session**, through `session/fork` when they advertise `sessionCapabilities.fork`. That isn't built: it needs the unstable ACP feature in the client and a live test against each adapter.

## Patchbay's agent

An ACP extension method beside `patchbay/rewind/*` (`crates/patchbay-agent/src/agent.rs`, `Agent::fork`):

```
patchbay/fork  { sessionId, targetPromptIndex? }  ->  { sessionId: <new id> }
```

- `targetPromptIndex` uses the numbering of `patchbay/rewind/points`: prompts since the last compaction. The copy stops just before that prompt, where a rewind to it would cut. Absent means the whole conversation.
- Refused while a turn runs, like rewind, and for an index with no prompt ("no fork point").
- `store::fork_entries` takes the source's `<id>.jsonl`, drops `Req` entries (they describe calls the new session did not make), appends the cut as a `Truncate` and keeps `store::effective` of that. `Store::create_with` writes the result as `<new id>.jsonl` in one go and reports a failed write, so a half-written fork never looks like a session. The source's `Meta` comes along, so the fork resumes with its model, mode and effort.
- A compaction in the source stays in the fork as its `Compacted` entry. The summary's pointer to the source's `.jsonl` (`compact_transcript`) still points at the source file, which is right: the history before the summary is there.
- Subagent transcripts (`agent/subagents/<id>/`) are not copied. Reports that name them keep pointing at the source session's folder, which still exists.
- The new session opens with `session/load`, which replays it as for any reload.

Writing the file in the agent, not the app, keeps the store format owned by one crate.

### The cut and compaction

In memory, a compaction leaves the summary as message 0, and `PromptRec::msg_index` counts it. `store::effective` used to give the summary no position, so `Truncate { len }` after a compaction kept one message more than the session had in memory: after a compaction and a rewind, a reload brought the rewound prompt back. The summary now takes position 0 (`a_rewind_after_a_compaction_counts_the_summary`). Fork uses the same cut, so it inherits the fix.

## The app

- `code_fork(id, ordinal?, preview)` in `src-tauri/src/lib.rs`. A session hosted on another computer is forked there (mesh RPC `code.fork`, also open to phones through the gateway), and the caller waits for the new row to arrive by sync, as `create_code_session` does.
- `code_fork_local` sends `AgentCmd::Fork` (refused unless `extensions.fork`, like `AgentCmd::Rewind`). In `acp.rs`, the transcript ordinal maps to a prompt index through `patchbay/rewind/points` and `pick_rewind_point`, as for rewind. It then creates the `code_sessions` row (agent, `model_ref`, `agent_model`, folder and approval mode from the source) with `acp_session_id` set to the copy, so starting it loads the copy.
- The UI (`CodeView.tsx`, `forkSession`) seeds the new session's composer draft (`seedDraft` in `Composer.tsx`) with the message and its attachments, then opens it.

## Files

A fork shares the folder with its source, and the files are as they are now, not as they were at the message. Two sessions editing one checkout can collide. The first version says so in a toast after the fork and offers nothing else. Later, cheapest first:

1. A **Fork into a worktree** option, reusing `subagent::create_worktree` (a new branch from `HEAD`). Uncommitted changes don't carry over.
2. Putting the files back to the message in that worktree from rewind's per-prompt snapshots. They live in memory only and miss Bash changes, so this would work only sometimes. Not worth it.

## The prompt cache

A fork from the end has the same history as its source. Anthropic keys the cache on content, not session, so the fork's first request reads the source's cached prefix while it is warm. A fork at a message shares the prefix up to it. The fork's usage bar starts cold; it doesn't inherit the source's cache clock.

## Tests

- Agent (`tests/e2e.rs`, mock LLM, `a_session_forks_at_a_prompt_or_at_the_end`): three prompts; fork at index 1 and at the end; an index with no prompt is refused; each fork replays what it should; the fork at a message takes a new prompt and the source never sees it.
- Store (`store.rs`): `a_fork_cuts_like_a_rewind_and_leaves_request_records_behind`, `a_rewind_after_a_compaction_counts_the_summary`.
- App smoke (`cargo test --lib smoke_code_fork -- --ignored`): through `CodeManager`, the copy loads in a new Code session with the first exchange and without the second.
- Not automated: the UI. Fork from here and Fork session were type-checked, not clicked through.

## Open questions

- Does Grok Build have a fork? Check its `x.ai/*` methods.
- Should the fork carry the source's to-do list? It doesn't now: a fork is a new direction.
