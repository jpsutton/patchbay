# Fork a session from a message

Status: proposal, not built.

## What the user gets

On a user message in the Code view, a **Fork from here** action. It makes a new Code session that has the conversation up to that message (the message itself included, so it can be edited before it is sent again), on the same agent, model and folder. The original session stays as it was. It is "edit and resend" without losing the branch you came from, and it lets you try a second approach from a known-good point.

This is the idea mainplane calls fork and revert: one operation, copy the first N records of a session into a new one (`Sessions.Copy(from, id, n)` in mainplane's `pkg/statefile/session.go`). Rewind is the same operation in place.

## What each agent can do

| Agent | Fork at message N | Fork at the end | How |
|---|---|---|---|
| Patchbay | yes | yes | new `patchbay/fork` method, below |
| Grok Build | not known | not known | has `x.ai/rewind/*`; check whether it has a fork |
| Claude Code, OpenCode | **no** | yes, if `sessionCapabilities.fork` | ACP `session/fork` |
| Others | no | no | hide the action |

The limit for Claude and OpenCode is in the protocol. ACP's `session/fork` (unstable, behind `unstable_session_fork`; `ForkSessionRequest` in `agent-client-protocol-schema` 0.11.4, `src/agent.rs:1251`) takes `sessionId`, `cwd`, `mcpServers` and `_meta`, and **no message position**: it copies the whole session. Fork at message N would also need a rewind of the copy, and those agents have none (`docs/design/agents.md`, "Rewind for other agents"). So for them the most this design can offer is **Fork session** (at the end), not **Fork from here**. That is still useful: it gives a second branch to try something in while the first keeps its state. The `agents.md` note that `session/fork` "could support edit and resend by forking at a message" is wrong on this point and should be corrected when this is built.

The UI branches on what the agent advertises (`agents.md`, "Shape"), so:

- **Fork from here** on a message: shown when `extensions.fork` is set (Patchbay's agent).
- **Fork session** in the session menu: shown when `extensions.fork` is set or the agent advertises `sessionCapabilities.fork`.

## Patchbay's agent

A new ACP extension method, beside `patchbay/rewind/*` in `crates/patchbay-agent/src/agent.rs`:

```
patchbay/fork  { sessionId, targetPromptIndex? }  ->  { sessionId: <new id> }
```

- `targetPromptIndex` uses the same numbering as `patchbay/rewind/points`: prompts since the last compaction. Absent means the end.
- Refused while a turn runs, like rewind.
- It reads the source's `<id>.jsonl`, keeps `store::effective(entries)`, cuts the message entries the way `Entry::Truncate { len }` would (to just before the target prompt), and writes the result as `<new id>.jsonl` with a fresh `Meta` entry. `Req` entries are not copied: they describe calls the new session did not make.
- A compaction in the source stays in the fork as its `Compacted` entry. The summary's pointer to the source's `.jsonl` (added with this change, `compact_transcript`) still points at the source file, which is correct: the history before the summary is there.
- Subagent transcripts (`agent/subagents/<id>/`) are not copied. Reports that name them keep pointing at the source session's folder, which still exists.
- The client then opens the new session with `session/load`, which replays it as for any reload.

Writing the file under the agent, not in the app, keeps the store format owned by one crate.

## The app

- `code_fork(id, ordinal?)` in `src-tauri/src/lib.rs`, routed to the session's host over the mesh like `code_rewind` (`remote_host` then `state.mesh.rpc(&host, "code.fork", …)`).
- `CodeManager` sends `patchbay/fork` or ACP `session/fork` (`AgentCmd::Fork`, refused unless the agent supports it, as `AgentCmd::Rewind` is), creates the `code_sessions` row (same agent, `agent_model`, folder, permission mode; title "<source title> (fork)"), and opens it.
- For Fork from here, the composer of the new session is prefilled with the target message's text and is not sent. That is the edit-and-resend case.

## Files

A fork shares the folder with its source, and the files are as they are now, not as they were at message N. Two sessions editing one checkout will collide. Options, cheapest first:

1. Say so in the confirm dialog, and offer nothing else. Recommended for the first version.
2. A **Fork into a worktree** checkbox, reusing `subagent::create_worktree` (a new branch from `HEAD`). Uncommitted changes don't carry over.
3. Put the files back to message N in the fork's worktree from rewind's per-prompt snapshots. They live in memory only and don't track Bash, so this would work only sometimes. Not worth it.

## The prompt cache

A fork from the end has the same history as its source. For Anthropic the cache is keyed on content, not session, so the fork's first request reads the source's cached prefix if it is still warm (`cache.rs` knows when it was last used). A fork from message N shares only the prefix up to N. Nothing to build; the usage bar's cache clock should start from the source's `lastAt` for a fork from the end, else from cold.

## Tests

- Agent (`tests/e2e.rs`, mock LLM): three prompts, fork at index 1, load the fork, check it replays two prompts' worth and the source is unchanged; fork at the end; fork refused while busy; a fork after a compaction keeps the summary.
- App smoke (`cargo test --lib smoke -- --ignored`): fork through `CodeManager`, the new row's agent, model and folder.

## Open questions

- Does Grok Build have a fork? Check its `x.ai/*` methods.
- Should the fork carry the source's to-do list? Probably not: a fork is a new direction.
- Mesh: is a fork always hosted where its source is? Yes for the first version, since the agent writes the file locally.
