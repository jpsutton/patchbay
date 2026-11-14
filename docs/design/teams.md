# Teams: design

Status: draft, local branch `feat/teams` (not proposed upstream yet).

A team is a set of Code sessions that work together on one project, each in a named role: a coordinator
that takes notices and dispatches work, an advisor it consults, a lead that reviews and merges,
and a pool of workers. The roster, the work items and the decisions live in Patchbay and show in
its UI. The user picks the agents, models, counts and role wording.

This generalises a pattern that ran by hand for several days: the `issue-triage-desk` skill, with
its state in `~/.hub/desks/<slug>/` markdown files and its sessions wired together with
`send_agent_message` and `create_agent_session`. That setup is the reference for the built-in
"Triage desk" template (below).

## Goals

- Any agent and model in any seat: Claude Code, Cursor, OpenCode, Antigravity, Codex, Grok,
  Patchbay's own agent.
- Roles are user-editable text plus a few capability flags, not code.
- Team state that agents cannot corrupt: they change it through tools, never by editing tables.
- The user can see who is on the team, what each seat is doing, the items and their status, and
  the decisions waiting on them.
- Patchbay starts worker sessions within limits the user set, without the per-creator cap getting
  in the way.

## Non-goals (for now)

- Mesh sync of team state. Teams are local to the computer that hosts them until this goes
  upstream; seats on other computers are added by hand and messaged over the mesh as today.
- The phone app. Desktop and browser (patchbayd web UI) only.
- Importing an existing Hub desk. A new team starts empty; the old ledger is archived as is.
- Reading the markdown export back in. Export is one-way.
- Scheduling or running work itself. Agents do the work; Patchbay holds state and starts sessions.

## Vocabulary

| Term | Meaning |
|---|---|
| Role | Named instructions plus capabilities, e.g. Advisor. Reusable across teams. |
| Template | A list of seats, each naming a role and an agent/model. Reusable. |
| Seat | One slot in a template: role, agent, model, permission mode, count. |
| Team | A template bound to a project folder, with its own project rules. |
| Member | A Code session filling a seat in a team. |
| Item | One unit of work on the team's board (a ledger row today). |
| Decision | A question routed to the advisor and/or the user, with options and a recommendation. |

## Roles

(The built-in Coordinator role was called Desk until 2026-11-10. Its id is still `builtin-desk`; seeding updates the built-in rows in place and a migration renames the `desk` seat and decision class on teams of the built-in template.)

A role has:

- `name`, and a `standing` flag. Standing seats (coordinator, advisor, lead) stay until the user retires
  them. Task seats (workers) are released when their item closes.
- `instructions`: markdown. The role's job, what it may and may not do, how it reports.
- Capabilities, checked by the team tools (not by trusting the instructions):
  - `writes_items`: may create and change items. Default: coordinator only.
  - `dispatches`: may start worker sessions and assign items. Default: coordinator only.
  - `decides`: may answer advisor-routed decisions (CLEAR or HUMAN). Default: advisor.
  - `merges`: may record a merge on an item. Default: lead.
- `reports_to`: the role its members reply to by default.

Built-in roles, copyable but not editable in place: Coordinator, Advisor, Ship Lead, Worker, Verifier.
Their default instructions come from the reference desk's protocol and DESK.md rules, without the
patchbay-specific parts (those are project rules). A role cannot be deleted while a template uses
it, another role reports to it, or any member (even a released one) has held it; a template
cannot be deleted while a team, even an archived one, uses it.

### How instructions reach an agent

A member's instructions are: role instructions, then the team's project rules, then a short
generated block (team name, its own role, the roster by role, the team tools).

- Agents with a system-prompt hook get it there, through the existing `session_meta` /
  `CODEX_CONFIG` path in `code/agents.rs` (Grok, Claude Code, Antigravity, Codex).
- Agents without one (OpenCode, Cursor, Kilo, and Patchbay's own agent, whose instructions are a
  host-wide setting rather than per session) get the same block as a fixed preamble on every
  prompt, user-typed ones included, not only prompts with an `agent:` origin. Replay and display
  hide the block so the transcript shows only what the user typed. Cost: the block's tokens on
  every turn for those agents. A first-message-only preamble was rejected because members lose it
  on compaction, which happened in the reference desk.
- Hook agents read instructions only when the agent starts. Attaching a running session, or
  editing its role, takes effect on its next agent start; the UI says so.

## Templates and seats

A seat has a role, an agent, an optional model, a permission mode, and:

- standing seats: `count` (usually 1);
- worker seats: `pool`, the most workers of this seat alive at once, and an optional list of
  agent/model choices the coordinator picks from (e.g. Claude Code Sonnet, Cursor Auto, Antigravity
  Gemini). Worker seats may not be saved or Started in `always-approve` (same rule as dispatch);
  a human may still switch a live session's mode in the session UI — that override is deliberate.

The template also holds the decision routing:

| Class | Goes to | Reference desk example |
|---|---|---|
| `human` | The user, always | permissions, releases, app builds, rule changes |
| `advisor_first` | Advisor, which answers CLEAR or HUMAN | scope, priority, whether a proof gap blocks |
| `coordinator` | No gate | dispatch, merge verdicts on reviewed PRs, item mechanics |

The class list and the examples are editable text; Patchbay only routes by class.

Templates may also set `allow_worker_commit` (off by default; built-ins never carry it). When on,
a dispatched worker whose **request** `rawInput.cwd` is its assigned item worktree may run, without
a prompt, a single command that passes a fail-closed tokenizer (no shell metacharacters; plain tokens
match `[A-Za-z0-9._/@:+,=-]+`; quotes for `git commit -m` / `--message` values and for `-c` values
that need spaces) and a per-verb option allow-list. Missing or empty `cwd` on the request is
fail-closed (no fallback to the session's recorded start cwd — that only proves launch directory,
not the shell's actual cwd). Agents that do not report cwd on execute requests (Cursor today)
therefore always prompt for worker git commands under this rule.

**Git auto-approve requires an exact command prefix** (David, #449): at dispatch Patchbay creates a
team-scoped empty read-only hooks directory under `data_dir/teams-safe/<team_id>/hooks` (outside
worktrees), and the worker brief / role text give the real path. Every auto-approved `git` command
must carry, in this fixed order after `git` and before the verb (`--no-pager` before every `-c`):

`git --no-pager -c core.hooksPath=<that dir> -c core.fsmonitor=false -c core.sshCommand=
-c protocol.ext.allow=never -c commit.gpgSign=false -c core.editor=: -c credential.helper=
-c core.askPass=`

Any other `-c`, missing piece, or different value → normal prompt (no auto-approve). `git commit`
also requires `--no-verify` plus `-m` / `--message` or `-F` / `--file` (path inside the worktree).
`ls` / `cat` do not need the prefix. Git auto-approve is Codex-only (proven non-persisting shell +
writable-set gate); other agents always prompt for git, and the permission prompt includes
`runs this repository's git hooks and config`.

- `git commit`: `--no-verify` and `-m` / `--message` or `-F` / `--file` (required; no `-q`/`-v`/`--template`/amend).
- `git add`: paths and/or `-A` / `--all` / `-u` / `--update` / `--` (no `-f`, no `--pathspec-from-file`).
- `git status`: `-s` / `--short` / `-b` / `--branch` / `--porcelain` / `--long`, plus relative paths.
- `git diff`: `--stat` / `--name-only` / `--name-status` / `--cached` / `--staged` / `--no-ext-diff` /
  `--no-textconv`, plus relative paths.
- `git log`: `--oneline` / `--decorate` / `--stat` / `--reverse` / `-1`…`-5` / `-10` /
  `--max-count=1|2|5|10`, plus relative paths.
- `ls`: `-l` / `-a` / `-A` / `-1` / `-h` / `-R` / `-la` / `-al` / `-lh` / `-hl` / `-lah` / `-alh` /
  `-hla`, plus relative paths.
- `cat`: one or more relative paths required (no bare `cat`, no `-`).

Path args must be relative (no `..` component), resolve inside the worktree after following
symlinks, and name a regular file or directory. As defense in depth, before any `git` verb
auto-approves Patchbay still reads `core.fsmonitor` and the effective hooks directory
(`core.hooksPath` if set, else `rev-parse --git-path hooks`). Auto-approve falls to a prompt if
`fsmonitor` is set to anything but false/empty; if the hooks dir cannot be determined; or if an
**executable** hook that git would run for that verb is present (githooks(5)): for `commit` —
`pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`, `pre-merge-commit`,
`post-index-change`, `reference-transaction`, `pre-auto-gc`; for `add` / `status` / `diff` / `log` —
`post-index-change` only. Hooks not run by these verbs (`pre-push`, `post-checkout`, …) do not
block. Non-executable hook files are ignored; a missing hooks directory is allowed.

The required `-c` prefix stops repo-config hook/fsmonitor changes made after the approval check from
executing, as long as the Patchbay hooks dir stays protected (re-validated at every approval:
canonical location outside team cwd / worktree root / item worktree / temp roots; no symlinks in
the path from `data_dir`; owned by the current user; no write bits on the hooks dir; parents not
group/world-writable; empty). Planted-red: hook+fsmonitor planted after the check do not run under
the forced flags, and do run without them.

Residuals (attackers):
- the worker itself if `data_dir` is temp or under a worktree-writable root (now refused);
- an outside same-UID process can race the safe hooks directory (validation and exec are not
  atomic) under normal placement (a non-temp, non-worktree `data_dir`); ancestors above `data_dir` are not
  lstat-checked; Windows owner/ACL enforcement is best-effort readonly metadata and Windows
  behaviour is unexecuted reasoning;
- mid-shell `export GIT_*` after spawn: Patchbay scrubs every `GIT_*` from the child env at
  Launch/spawn (`scrub_git_env` + `GIT_CONFIG_NOSYSTEM=1`). Mid-shell set is not under Patchbay
  control; kinds where the tool-shell persists `export` across commands (or is unproven) never get
  git auto-approve. Codex with proven non-persist may auto-approve only when repo common config,
  `config.worktree`, gitdir, and `info/attributes` are outside the workspace-write writable set
  (worktree cwd + temp; sbx-probe-home). OpenCode and unsandboxed kinds always prompt for git;
- clean/smudge filters via `.gitattributes` when config/attributes are writable (writable-set gate
  refuses auto-approve); textconv/external diff residuals remain for human-approved bare git;
- human-approved bare git stays check-then-exec racy;
- Codex `~/.codex` approved prefixes never reach the Patchbay permission path (pb-20261111-83).
Unbounded `ls -R` is not capped here.

## Team

A team binds a template to a folder (the creator's folder rules for `create_agent_session` still
apply to workers: the folder or inside it) and adds:

- `slug`, `id_prefix` (items are `<prefix>-YYYYMMDD-NN`), `pr_base`, `worktree_root`;
- `project_rules`: markdown, e.g. "set `CARGO_TARGET_DIR`", "the mock LLM port 8765 is taken".
- `export_dir`: optional folder for markdown export (empty means export is off). When set, creating
  a team, and every export, refuses a folder whose `LEDGER.md` or `ROSTER.md` lacks the generated
  header, so a team never overwrites a hand-run Hub desk with the same slug.

The template editor shows, for a seat whose agents can act without asking, what each does unasked
(`ApprovalSetup.gaps`) beside an `allowUnasked` tick, and a line for agents whose team tools still
prompt (OpenCode). Capability edits to a role apply to live members on their next tool call.

A session fills at most one seat across all teams. Seat counts (standing `count`, worker `pool`)
are enforced on every attach, by hand or by dispatch.

## Storage

Team state goes in its own SQLite file, `teams.db`, in the data folder, with its own
`user_version` and migration list. Not in `MIGRATIONS` in `db.rs`: that list's index is the schema
version, so a branch-only migration would collide with the next upstream one and a database that
ran ours would skip theirs. Members link to sessions by session id; `code_sessions` gets no new
columns.

Tables (sketch):

```
roles(id, name, standing, instructions, caps_json, reports_to, builtin, updated_at)
templates(id, name, seats_json, routing_json, updated_at)
teams(id, slug, template_id, cwd, id_prefix, pr_base, worktree_root, project_rules,
      export_dir, created_at, archived_at)
members(team_id, session_id, seat, role_id, status, item_id, joined_at, left_at)
items(team_id, id, title, type, severity, status, member_session, plan, pr_url,
      created_at, updated_at)
item_events(team_id, item_id, at, actor_session, kind, text)   -- the changelog
decisions(team_id, id, item_id, question, options_json, recommendation, class,
          advisor_verdict, advisor_text, user_answer, status, created_at, resolved_at)
```

Item `status` uses the reference desk's values: noticed, triaged, blocked, dispatched,
in-progress, awaiting-review, done, dropped. `item_events` is append-only.

### Markdown export

After every change Patchbay rewrites `LEDGER.md` (index), `ROSTER.md` and appends to
`CHANGELOG.md` in `export_dir`, so agents outside Patchbay can read the state. When `export_dir` is
empty or absent, export is off and Patchbay writes no files. The files carry a header saying they are
generated. Nothing reads them back.

The export folder must be a real folder outside the team's folder and worktree root. Patchbay
refuses it, or any of the three files, when it is a symlink, and strips control characters from
event, title and question text before writing.

Known limit: `decision_open` lets a `writes_items` role choose the class. Class `coordinator` resolves with
no gate, so a role that can write items can pre-resolve its own question. That is acceptable while
nothing acts on a decision; dispatch must not wait on a decision without a host-side check of who
opened it and what class it took.

## Team tools

Added to the messaging MCP (`agent_messaging.rs`, `tools_list_for`) only for sessions that are
team members, and checked against the caller's role capabilities on the host:

| Tool | Who | Does |
|---|---|---|
| `team_roster` | all | Members with role, seat, agent, model, activity, current item |
| `team_send` | all | `send_agent_message` addressed by role or session id |
| `item_add` / `item_set` / `item_note` | `writes_items` | Create an item, change named fields, append an event |
| `item_list` / `item_get` | all | Read the board or one item with its events |
| `dispatch_item` | `dispatches` | Create the worktree from `pr_base`, start a worker in a free seat, send the brief with the role preamble, set the item `dispatched` |
| `decision_open` | `writes_items` | Open a decision; routing decides who sees it |
| `decision_answer` | `decides`, or the user | Advisor CLEAR/HUMAN, or the user's answer |
| `member_release` | `dispatches` | Release a task worker after its cleanup report |

Members report progress with `item_note`; the coordinator keeps sole ownership of status by default
(single writer, as in the reference desk), though a template may grant `writes_items` wider.

## Starting sessions

- Standing seats are started by the user from the team panel.
- In the default permission mode, a member's call to `send_agent_message` (and later the team
  tools) waits on a permission prompt. Unattended teams need those tools allowed for members
  without asking. Decided 2026-11-09: members get `send_agent_message` and the team tools
  allowed without a prompt (a standing allow for those tools only, for live team members);
  every other tool still asks according to the seat's permission mode. Phase 4's design gets a
  security review before it is built.
- **Tool-call titles (verified 2026-11-10 from transcripts in the main Patchbay database; no new
  live run).** The allow rule (`teams/allow.rs`) matches the title each agent gives a call to a
  tool on `patchbay-agent-messaging`, exactly, per tool name:

  | Agent | Title | Kind |
  |---|---|---|
  | Claude Code | `mcp__patchbay-agent-messaging__send_agent_message` | other |
  | Codex | `mcp.patchbay-agent-messaging.send_agent_message` | execute (MCP shape: `rawInput.arguments`) |
  | Cursor | `patchbay-agent-messaging: send_agent_message`, or `patchbay-agent-messaging-send_agent_message: send_agent_message` | other |
  | Antigravity | `patchbay-agent-messaging_send_agent_message` | other |
  | Patchbay | `send_agent_message (patchbay-agent-messaging)` | other |

  The team tools were not in those transcripts (they pre-date the team tools); the forms are the
  same with the tool name swapped, which is an inference. **OpenCode has no rule.** It reaches MCP
  tools only through an `execute` tool that runs JavaScript (`tools["patchbay-agent-messaging"].send_agent_message(...)`),
  so allowing it would allow arbitrary code. An OpenCode member still waits on a prompt for every
  team tool call; the built-in coordinator seat is OpenCode, so an unattended coordinator needs a
  different agent or a person at the prompt. The user-chosen server name `patchbay-agent-messaging`
  is reserved in `mcp::acp_servers`.
- `dispatch_item` (coordinator only, `dispatches` capability) starts workers from a worker seat
  (a seat whose role isn't standing), on this computer only. As built:
  - **Agent.** The seat's own agent or one of its `choices`, never anything from item text. Agents
    that can act without asking whatever the mode (`ApprovalSetup.gaps`: Patchbay, Claude Code,
    Codex, Antigravity, Cursor) also need the seat's `allowUnasked` tick, set by the user in the
    template editor after it shows the gaps text. The tick stores the seat's agent, model, choices,
    mode and the joined gaps text shown for that seat, and lapses if any of those change (including
    a gaps-text change alone, so a new consent line re-requires the tick); built-in templates never
    carry one. For a Codex worker seat the editor and header use the dispatched-worker gaps: under
    Auto, "edits files and runs commands inside its worktree sandbox without asking, and also writes
    under /tmp and $TMPDIR without asking; network stays off; anything else outside still asks."
  - **Mode.** The stricter of the seat's mode and the coordinator's current mode; always-approve is
    refused. Workers have `created_by` = the coordinator, so lowering the coordinator's mode lowers theirs.
    A **current** active dispatched Codex worker whose effective mode is Auto launches in Codex
    `workspace-write` (network off; worktree plus `/tmp` and `$TMPDIR` writable) only when `cwd` is
    set and `codex_worker_workspace_write_blocked` is clear: a linked item worktree whose common git
    dir is outside the worktree and, while F1 temp excludes are not confirmed, not under `/tmp` /
    `$TMPDIR`. Omitted cwd maps to `read-only` for the mode (consent gaps may still describe the
    allowed case). On Windows Auto→workspace-write always refuses until codex-acp sandboxing there
    is proven (`CODEX_WORKER_WINDOWS_READONLY_HINT`). **F1 (upstream limit):**
    `@agentclientprotocol/codex-acp@2.2.2` hardcodes `excludeSlashTmp` / `excludeTmpdirEnvVar`
    (`exclude_slash_tmp` / `exclude_tmpdir_env_var`) to false on every turn, so a `CODEX_CONFIG` /
    `[sandbox_workspace_write]` exclude cannot stick; Patchbay keeps
    `CODEX_WORKER_TEMP_EXCLUDES_CONFIRMED = false` until an adapter (or Patchbay) can confirm those
    excludes in effect. Otherwise F2 keeps the worker `read-only` (temp teams, main-checkout cwd,
    unprobeable git, Windows) and `dispatch_item`'s `note` includes the matching hint. Default stays
    `read-only`. The `dispatches` row must match the membership's `team_id` and `item_id`.
    Released, restored-after-release, swapped-out, or item-`done`/`dropped` sessions stay
    `read-only` on the next launch even if a `dispatches` history row remains. Standing seats and
    non-team Codex Auto stay `read-only`. **Lifecycle (known):** a *running* Codex worker keeps the
    sandbox it launched with between item `done` and `member_release`; mode is fixed at launch.
    Release stops/archives the session; any later restore+start is read-only. Coordinators release
    right after the item is done.
  - **Pool.** The seat's `count` workers at once, enforced with the rate limits (10 dispatches per
    hour and 40 per day per team) in one `BEGIN IMMEDIATE` transaction that reserves the place
    before the worktree or session exists; a failed start gives the place back, though the
    attempt still counts toward the rate. Released members stop counting. Manual `attach` takes
    the same lock.
  - **Cap.** Dispatched workers are exempt from `MAX_CREATED_SESSIONS` (the coordinator's count
    subtracts them) and a dispatched worker's own `create_agent_session` is refused.
  - **Worktree.** `git worktree add -b <item id lowercased> -- <worktree_root>/<item id> <pr_base>`,
    from the local or `origin` branch; removed again if the start fails.
  - **Brief.** The item's title and plan go inside an `untrusted-item-content` fence, with forged
    `team-instructions`, fence and handoff tags defanged. The same defanging applies to `team_send`
    text and to decision notifications.
  - `member_release` stops the agent, revokes its bridge token and frees the place, and is allowed
    only when the item is done, dropped or awaiting-review.
  - **Limit.** `teams.db` is a file in the data folder. An agent with an unrestricted shell as the
    same user can edit it (including the tick), so the tick and the role check bind agents that
    use their tools, not ones that are free to run any command.
- Same-computer only, as today.

## UI

A Teams page in the sidebar:

- Teams list, and per team: roster (role badge, agent, model, live `activity` from
  `list_agent_sessions`, current item), board grouped by status, decisions waiting on the user
  (with the advisor's verdict and any disagreement shown), and recent events.
- Role and template editors in Settings, in the style of the Prompts page.
- Member sessions show a role badge in the session list.

## Built-in template: Triage desk

From the reference desk:

| Seat | Role | Agent / model (default, editable) | Count |
|---|---|---|---|
| coordinator | Coordinator | Claude Code, Sonnet | 1 |
| advisor | Advisor | Claude Code, Opus | 1 |
| lead | Ship Lead | Claude Code, Sonnet | 1 |
| workers | Worker | Claude Code Sonnet, Cursor Auto, Antigravity Gemini, OpenCode | pool 4 |

Routing: the three classes in the table above, with the reference desk's examples.

## Development setup

- Branch `feat/teams` in the worktree `~/patchbay-teams`, tracking `origin/staging`. Sync with
  `git fetch origin staging && git merge origin/staging`. Never pushed.
- Run the branch as `patchbayd --data-dir <data-dir>` (empty folder). Mesh keychain
  entries are scoped by data folder (`mesh/keys.rs` `keyring_scope`), so it gets its own mesh
  identity. Test teams work in a throwaway clone, never the real checkout.
- Container (Colima) isolation is deferred to the unattended-dispatch tests in phase 4.

## Phases

1. **Read-only.** Done 2026-11-09 (`4a1824ea`, `41d53b74`). `teams.db`, built-in roles and the
   triage template, create a team and attach sessions to seats by hand, instruction delivery
   (hook or preamble), markdown export, and a read-only Teams page under Settings (not yet the
   sidebar). Not verified: a real agent turn reading the instructions.
2. **Editors and live check.** Done 2026-11-09 (`ad0e6ba8`). Role and template editors under
   Settings > Teams (built-ins copyable only; routing has no editor yet), delete guards, and a
   role badge in the session list. Live: a real OpenCode member (preamble path) and a real Claude
   Code member (system-prompt path) each named their role and quoted the team's project rule.
   Not verified: a member holding a copied role in a live turn (the delivery path doesn't
   distinguish copies), and the editors and badge seen in a browser.
3. **Items and decisions.** Done 2026-11-09 (`3f265eeb`). Team tools on the messaging MCP for
   live members only (listed when the agent's MCP starts, checked against teams.db on every
   call, after the bridge's token and host checks), items with append-only events (enforced by
   triggers), IDs from a per-day counter (UTC date, never reused), decisions, the board and the
   decision queue under Settings > Teams. Live: real Claude Code coordinator, advisor (CLEAR) and worker
   (`item_set` refused). Decision rules as built:
   - Status: `awaiting_advisor`, `awaiting_user`, `cleared` (advisor decided), `resolved` (user
     answered, or class `coordinator`). The user may answer a decision still awaiting the advisor.
   - CLEAR needs a `choice` (one of the options when there are options). The advisor disagrees
     when its choice differs from the coordinator's recommendation; HUMAN is never a disagreement.
   - With no `decides` member, `decision_open` records the decision with a warning and the user
     answers it.
   Not verified: a real advisor HUMAN verdict and the disagreement flag in a live turn (tests
   only), and the board and queue in a browser.
4. **Dispatch.** Built 2026-11-10 (`2de3c364`, `2b882c1c`): `dispatch_item`, pool and rate
   limits, worktree creation, `member_release`, `item_note` for the item's assignee
   (`items.member_session` is the only authority), the allow rule for team tools (above, "Tool-call
   titles"), the editor tick, prompt fencing. Tests: `the_coordinator_fills_a_pool_and_releases_workers`
   fills a 2-worker pool from claude-code and cursor, refuses the third until a release, shows the
   5-per-creator cap isn't consumed, and a worker notes its own item but not another's.
   Review (2026-11-10) fixes are in the later commits; known limits, not fixed:
   - The `worktree_root` symlink swap needs shell access as the same user; the same-user limit
     covers it. Dispatch re-resolves the folder and refuses one that left the team folder.
   - The acknowledgement snapshot holds the seat's role id, agent, model, choices, mode and gaps
     text, not the role's capabilities (editing a role's capabilities does not lapse it). Changing
     the gaps text alone re-requires the tick (`unasked_ack` PartialEq vs `snapshot`).
   - `team_detach` and `team_archive` don't stop the agent or revoke its bridge token; only
     `member_release` does.
   - The bridge token sits in the MCP server's environment, readable by the agent's own shell.
   - The messaging bridge token reaches Claude Code inside `--mcp-config {...PATCHBAY_MESSAGE_TOKEN...}` on its command line, so plain `ps` shows it, and on macOS any local user can read process arguments (pre-existing in agent messaging).
   Not verified: a real agent started by `dispatch_item` (tests use a stand-in agent); the allow
   rule against the team tools' real titles (inferred from `send_agent_message`); a sandbox live
   run; the editor tick in a browser; container isolation for unattended dispatch.

The Teams sidebar page is deferred until after phase 4; it touches `CodeSidebar.tsx` and
`App.tsx`, which change daily upstream.

Each phase has its own tests in the existing style (`cargo test` for `teams.db` and capability
checks; the fake ACP agent for preamble delivery).
