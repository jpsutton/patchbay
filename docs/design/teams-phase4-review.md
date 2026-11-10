# Teams phase 4: security review and handoff

Design review of phase 4 (dispatch) before it is built, 2026-11-09, by a read-only
`security-reviewer` against `docs/design/teams.md` at `572488b2` and the phase 1-3 code. One
finding (export symlinks) was re-checked by hand. Line numbers are from that commit.

## Decisions already made (David)

- Members call `send_agent_message` and the team tools without a prompt; every other tool
  follows the seat's permission mode.
- Choosing a gap agent (one that can act without asking) for a seat in a template counts as
  approval. The review below tightens how that approval is recorded; it does not reverse it.
- Phase 4 is built by a fresh worker under a fresh coordinator, from this file and the design.

## Step 0: fix before phase 4 (existing phase 1-3 code)

1. **Export follows symlinks** (Medium, confirmed). `check_export_dir` (`teams.rs:678`) passes a
   dangling symlink because the read fails; `write_export` writes through symlinks and never
   checks `CHANGELOG.md`. Event text carries raw item titles with newlines. Fix: refuse the
   folder or any of the three files if `symlink_metadata` says symlink; strip control characters
   from event text; refuse an `export_dir` inside a team's cwd or worktree root.
2. **Team notifications skip rate limits** (Medium). Messages sent by `decision_open` /
   `decision_answer` bypass `check_send_limits` (`items.rs:465`). Apply the same limits.
3. **Unvalidated inputs** (Medium). `worktree_root`, `id_prefix`, `pr_base` are stored as given
   (`teams.rs:399`). Canonicalize `worktree_root` and require it inside the team cwd; restrict
   `id_prefix` to `[A-Za-z0-9_-]`; check `pr_base` with `git check-ref-format`, refuse a leading
   `-`, and pass `--` to git.
4. **Coordinator picks the decision class** (Medium). A `writes_items` role can open a question as class
   `coordinator` (then `desk`), which resolves with no gate (`items.rs:319`). Acceptable for now; record it in the
   doc. Do not gate dispatch on decisions without a host-side check.

## Phase 4 requirements

### Approval-gap exemption (High)

- The gap agents are Patchbay, Claude Code, Codex, Antigravity and Cursor
  (`agents.rs:210-245`), not just three; the doc must list them.
- `dispatch_item` starts only the seat's `agent` or one of its `choices`.
- A seat needs an explicit `allowUnasked` acknowledgement, set by the user in the template
  editor after it shows the agent's `ApprovalSetup.gaps` text. Built-in templates never carry
  it. The acknowledgement stores the seat's agent, choices and mode and lapses if any changes.
- Worker permission mode = the stricter of the seat's mode and the coordinator's current mode.
- Workers get `created_by = coordinator`, so the existing mode ceiling cascades.
- The doc states the limit: `teams.db` is a file in the data folder; an agent with unrestricted
  shell as the same user can edit it. Refuse, or loudly warn on, always-approve seats.

### Pool exemption (High)

- Workers keep `created_by = coordinator`; the 5-per-creator check skips team-dispatched sessions (a
  marker, or a teams.db lookup). A worker's own `create_agent_session` is refused.
- Seat reservation under a per-team lock, count + insert in one `BEGIN IMMEDIATE` transaction,
  reserved before the session is created and rolled back if the start fails (today `attach`
  counts then inserts with no lock, `teams.rs:418`).
- A per-team dispatch rate and a daily total, not only a concurrent pool.
- `member_release` stops the agent and revokes its bridge token (`agent_messaging.rs:52`), and
  is allowed only when the item is done, dropped or awaiting-review.

### Team tool allow rule (Medium)

- Not a `permission_rules` row: those match only kind + command prefix, and MCP calls are kind
  `other`, so a rule would approve every MCP tool from every server (`permissions.rs:33`, test at
  `:421`).
- Put it in the `auto_approve` closure (`code/mod.rs:222`) before `permissions::decide`. Allow
  only when `teams::is_member` is true, the call is from the Patchbay messaging server and names
  a tool in a fixed list (`send_agent_message`, `teams::TOOLS`, `dispatch_item`,
  `member_release`), and it carries no paths or command. Allow-once only. Never
  `create_agent_session` or `share_artifact`.
- Reserve the server name `patchbay-agent-messaging` in `mcp::acp_servers` so a user MCP server
  can't take it. First check what tool-call title each agent actually sends (not yet verified).
- The host-side capability check (`Caller::need`, `items.rs:479`) stays the real control.

### Prompt injection (High once dispatch runs unattended)

- Item `title`/`plan`, decision text and `team_send` text reach other agents raw, and a peer can
  forge `<team-instructions>` tags. Dispatch briefs wrap item text in an "untrusted content"
  fence and strip forged tags. Never take agent, mode, cwd or paths from item text.

### Smaller

- `item_note` on "own item": pick one of `items.member_session` / `members.item_id` as the
  authority and check only that.
- Role capability edits apply to live members immediately; the UI says so.

## Handoff

- Branch `feat/teams`, worktree `~/patchbay-teams`, tracking `origin/staging`, never pushed.
- Phases 1-3 built and verified (`cargo test --lib` 317 passed at `3f265eeb`).
- Phase 3 worker `4f7ecee6-3616-43f0-b6d4-4bde3caa1ed3` is on hold; it can answer questions
  about the code but should not take phase 4.
- Rules for every worker: never push; run only `patchbayd --data-dir ~/patchbay-teams-data`;
  test teams only in `/tmp/teams-sandbox`; `CARGO_TARGET_DIR=/Users/dfrazer/patchbay/src-tauri/target`;
  small hooks in the busy shared files; every claim backed by raw output.
- Order: step 0 fixes (one commit), then phase 4 as in `teams.md` with the requirements above,
  then a security review of the diff before any real (non-sandbox) use.
- Still owed to David: a look at the Settings > Teams screens in a browser (editors, board,
  decision queue, badge); nobody has seen them rendered.
