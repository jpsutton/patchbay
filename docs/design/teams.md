# Teams: design

Status: draft, local branch `feat/teams` (not proposed upstream yet).

A team is a set of Code sessions that work together on one project, each in a named role: a desk
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

A role has:

- `name`, and a `standing` flag. Standing seats (desk, advisor, lead) stay until the user retires
  them. Task seats (workers) are released when their item closes.
- `instructions`: markdown. The role's job, what it may and may not do, how it reports.
- Capabilities, checked by the team tools (not by trusting the instructions):
  - `writes_items`: may create and change items. Default: desk only.
  - `dispatches`: may start worker sessions and assign items. Default: desk only.
  - `decides`: may answer advisor-routed decisions (CLEAR or HUMAN). Default: advisor.
  - `merges`: may record a merge on an item. Default: lead.
- `reports_to`: the role its members reply to by default.

Built-in roles, copyable but not editable in place: Desk, Advisor, Ship Lead, Worker, Verifier.
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
  prompt with an `agent:` origin: the first brief, every peer message and every dispatch
  (`dispatch_item` must send with that origin). Prompts the user types get no preamble. Cost: the
  block's tokens on each such message. A first-message-only preamble was rejected because members
  lose it on compaction, which happened in the reference desk.
- Hook agents read instructions only when the agent starts. Attaching a running session, or
  editing its role, takes effect on its next agent start; the UI says so.

## Templates and seats

A seat has a role, an agent, an optional model, a permission mode, and:

- standing seats: `count` (usually 1);
- worker seats: `pool`, the most workers of this seat alive at once, and an optional list of
  agent/model choices the desk picks from (e.g. Claude Code Sonnet, Cursor Auto, Antigravity
  Gemini).

The template also holds the decision routing:

| Class | Goes to | Reference desk example |
|---|---|---|
| `human` | The user, always | permissions, releases, app builds, rule changes |
| `advisor_first` | Advisor, which answers CLEAR or HUMAN | scope, priority, whether a proof gap blocks |
| `desk` | No gate | dispatch, merge verdicts on reviewed PRs, item mechanics |

The class list and the examples are editable text; Patchbay only routes by class.

## Team

A team binds a template to a folder (the creator's folder rules for `create_agent_session` still
apply to workers: the folder or inside it) and adds:

- `slug`, `id_prefix` (items are `<prefix>-YYYYMMDD-NN`), `pr_base`, `worktree_root`;
- `project_rules`: markdown, e.g. "set `CARGO_TARGET_DIR`", "the mock LLM port 8765 is taken".
- `export_dir`: default `~/.hub/desks/<slug>/`. Creating a team, and every export, refuses a
  folder whose `LEDGER.md` or `ROSTER.md` lacks the generated header, so a team never overwrites
  a hand-run Hub desk with the same slug.

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
`CHANGELOG.md` in `export_dir`, so agents outside Patchbay can read the state. The files carry a
header saying they are generated. Nothing reads them back.

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

Members report progress with `item_note`; the desk keeps sole ownership of status by default
(single writer, as in the reference desk), though a template may grant `writes_items` wider.

## Starting sessions

- Standing seats are started by the user from the team panel.
- In the default permission mode, a member's call to `send_agent_message` (and later the team
  tools) waits on a permission prompt. Unattended teams need those tools allowed for members
  without asking; phase 4 decides how (a seat permission mode, or a standing allow rule for the
  team tools only).
- `dispatch_item` starts workers. It is exempt from `MAX_CREATED_SESSIONS` (5) and
  `MAX_CREATE_DEPTH` (2) in `lib.rs`; instead it refuses when the seat's `pool` is full.
  Released members stop counting.
- The approval-gap rule (a non-always-approve creator may not start Cursor, Claude Code, Codex)
  does not apply to seats the user configured in the template: choosing the agent in the template
  is the approval (decided 2026-11-09). The worker still gets the seat's permission mode, never
  looser than the team's. This loosens an existing safety check, so phase 4 gets a security
  review before it is used.
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
| desk | Desk | OpenCode | 1 |
| advisor | Advisor | Claude Code, Opus | 1 |
| lead | Ship Lead | Claude Code, Sonnet | 1 |
| workers | Worker | Claude Code Sonnet, Cursor Auto, Antigravity Gemini, OpenCode | pool 4 |

Routing: the three classes in the table above, with the reference desk's examples.

## Development setup

- Branch `feat/teams` in the worktree `~/patchbay-teams`, tracking `origin/staging`. Sync with
  `git fetch origin staging && git merge origin/staging`. Never pushed.
- Run the branch as `patchbayd --data-dir ~/patchbay-teams-data` (empty folder). Mesh keychain
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
3. **Items and decisions.** The item and decision tools with capability checks, the board, the
   decision queue. Done when: a desk member can only change items through the tools, a worker's
   `item_set` is refused, and a decision goes desk → advisor → user with the verdict shown.
4. **Dispatch.** `dispatch_item`, pool limits, worktree creation, `member_release`. Done when: a
   desk fills a 2-worker pool from mixed agents, a third dispatch is refused until one is released,
   and the 5-per-creator cap no longer blocks it.

The Teams sidebar page is deferred until after phase 4; it touches `CodeSidebar.tsx` and
`App.tsx`, which change daily upstream.

Each phase has its own tests in the existing style (`cargo test` for `teams.db` and capability
checks; the fake ACP agent for preamble delivery).
