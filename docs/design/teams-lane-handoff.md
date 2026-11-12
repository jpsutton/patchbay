# Teams lane coordinator handoff

From the outgoing coordinator 197cd7ba-f90c-41eb-af03-e48ee9a3ef5b. The director is
4fc0a3d8-fdcc-44bd-afe7-bf576fe62f1b: report to it at each step boundary, with raw lines.

## Branch

- `feat/teams` in `/Users/dfrazer/patchbay-teams`. HEAD: see "State at handoff" below. Never pushed.
- Staging target: `origin/staging` at a09fbe99 (0.24.0, #431).

## Order (director, 2026-11-11)

1. Run 8: worker `team_send` → coordinator `decision_open` → answer in the UI → `member_release`.
2. Unprompted run on a build at 6ea5a497 or later: the coordinator finds `dispatch_item` from the roster hint.
3. Rebase or merge onto origin/staging. `docs/design/teams.md` says merge, and a merge resolves each conflict once. Then cargo, tsc and selftests, then the loop once as a smoke test.
   - Expected conflicts (dry run): `Cargo.toml`, `agent_messaging.rs`, `code/agents.rs`, `mesh_app_tests.rs`.
   - #426 (99745b91) touches the `api.rs` registrations near `team_board`/`team_detail`/`team_decision_answer` (api.rs:226-228), and `web.rs`. Keep both sides' lines.
   - pb-20261110-04 (not on staging) touches `lib.rs` `on_code_event`/`delete_code_session`, `code/mod.rs` `CodeManager`, `web.rs`, `shares.rs` and `bin/patchbayd.rs`. It comes later, in a second small rebase.
4. A Sonnet-default coordinator loop (Claude Code), once. Size S. Only when the Pro 5-hour window has room.
5. A three-worker rehearsal (Codex, Antigravity, Patchbay), automatic, under the same stop rules.
6. A full security re-review of the whole feat/teams diff against staging, including 89ed592a (worktree field) and a0e818f5+6ea5a497 (roster).
Then push and PR are David's decision.

## Scratch-daemon never-rules (ratified by David 2026-11-12, machine-wide)

1. real HOME plus --data-dir, never a HOME override (keyring collision);
2. loopback on a port other than 4420;
3. attach only by PID, binary path, or URL+port, never by app name, Launch Services or open -a;
4. an isolated browser profile if a browser is used;
5. a teardown that leaves no listener and no scoped keyring entries.

### Pool and wake constraints

- **Per-pool cap**: no more than two concurrently active seats per subscription pool: Codex, Antigravity, Claude Teams, Cursor.
- **Wake rule**: never end a turn in a pure wait without a line naming what is waited on and who holds it.

## Standing rules

- No push, no PR. `git add <paths>` only; never stage `skills/`. Don't edit `docs/model-registry/`.
- Sandbox only: `patchbayd --data-dir ~/patchbay-teams-data`, team folder `/tmp/teams-sandbox/proj`, worktrees `/tmp/teams-sandbox/proj/.trees`.
  - Build the daemon with `--no-default-features --features web` into `CARGO_TARGET_DIR=/Users/dfrazer/patchbay/src-tauri/target-teams-live`.
  - Serve it on 127.0.0.1:4531 with `--ui-dir <worktree>/dist`.
- Never touch David's live app (4420, 9797), his test site 4521 (`~/patchbay-teams-ui`, `/tmp/pb-teams-ui`), or the old `/tmp/pb-live` (a Nov 10 run).
- Stop on any Patchbay usage refusal, release the workers and report. Never run on credits or API keys, and nothing on David's Claude Pro account (the "Claude Code login") while its 5-hour window is capped.
- Verify every worker report yourself:
  - Rust: `cargo test --lib` in a throwaway `git worktree add --detach /tmp/pb-verify-x <sha>`, with `CARGO_TARGET_DIR=/Users/dfrazer/patchbay/src-tauri/target`.
  - UI: tsc and `node scripts/selftests.mjs`, with node_modules symlinked from the live tree.
  - Read the diff.
- Security review (`claude-routing:security-reviewer`, diff file plus source) for anything that starts sessions, touches permissions, or feeds text to agents.
- Shell: prefix with `rtk`. Never print bridge tokens (`ps` shows them).

## Sessions

| Id | Role |
|---|---|
| 0e5ebc67-311c-416b-b8ec-2db6562c5615 | Codex: live runs and UI checks (CDP Chrome on a temp profile; screenshots in /tmp/pb-liveN-shots) |
| d2bb0bd5-78f4-4e27-ba51-59a92794026c | Antigravity: code worker. Standing by. |
| dce20f1d-9011-4c60-a9c9-afdde77671a3 | Relays David's decisions; rebuilds 4521 |
| 4fc0a3d8-fdcc-44bd-afe7-bf576fe62f1b | Director |
| 4f7ecee6 | Phase 1-3 author, on hold (questions only) |

Live-run seats: coordinator Codex (UI model "6 Luna"), worker Antigravity ("Gemini 3.8 Flash"). Neither shows a provider or account label; that is a known gap for the capabilities work.

## Evidence so far (raw)

- Run 3 (a14c58f7): B FAIL `no teammate with role or session id Workers; see team_roster`. The roster hid open seats. Fixed in a0e818f5 + 6ea5a497: cargo 346 passed / 23 ignored, security review with no High or Medium.
- Run 4: dispatch FAIL `the team has no worktree folder; set one when creating the team`. The create form had no field. Fixed in 89ed592a.
- Run 2: switching a seat's agent kept the old model ("sonnet" on Codex). Fixed in a14c58f7.
- Run 7 (89ed592a), team live7 7055da0f-2daf-41c5-b36f-e148c0aa6610:
  - A PASS: worktree_root `/private/tmp/teams-sandbox/proj/.trees`.
  - B PASS: `dispatch_item` isError=false, worker session 3e1d5106-ab72-4ec7-8158-1d2c79668520 on antigravity, worktree `.trees/live7-20261111-01`, dispatches row 3, item `dispatched`. Coordinator 8df5bfe6-1ad9-4c79-9eb0-1ec0d5c1e906 (codex).
  - C, guard PASS: the worker's `decision_open` was refused with `the Worker role has no writes_items capability, which is needed to open decisions`. This is by design (teams.md:57/176).
- Titles seen, all auto-approved and matching `teams/allow.rs`:
  - Codex: `mcp.patchbay-agent-messaging.{item_list,item_add,item_set,team_roster,team_send,dispatch_item}`.
  - Antigravity: `patchbay-agent-messaging_{item_get,item_note,decision_open}`.
- Screenshots: /tmp/pb-live4-shots, /tmp/pb-live5-shots, /tmp/pb-live6-shots, /tmp/pb-live7-shots.
- Run 11 (6ea5a497), live7, PASS for the whole loop:
  - Decision: `live7-D1|human|resolved|Hello`.
  - Worker: `3e1d5106|workers|released|1794444285317`, session archived.
  - The worker's commit: `dc5e77c`.
  - Item `awaiting-review`.
  - Screenshots in /tmp/pb-live11-shots.
- Known limit, not a bug: `team_archive` and `team_detach` don't stop the agent (teams.md:308).

## Findings to act on before the three-worker rehearsal

- **F1 (defect): the advisor_first stall.** On a team whose template has no Advisor seat, the Coordinator's triage instructions make it open an `advisor_first` decision. That decision sits in `awaiting_advisor` until a human answers it in the UI. Live9 stalled this way (live9-D1). Fix one of two ways: `decision_open` opens such decisions as `awaiting_user` when no `decides` member exists (teams.md:293 says it should warn), or the template copy keeps the Advisor seat. The rehearsal template must not stall.
- **F2: workers need approval for git.** A worker's commit needs a shell approval. Antigravity in default mode asked for `git add` and `git commit`. In the rehearsal this recurs for every worker whose seat mode asks before running shell commands: Antigravity in default mode, and Codex in read-only mode (its sandbox may also refuse writes to `.git`). Patchbay asks before running shell commands. Decide before the rehearsal: either approve each worker's commits by hand inside its `.trees` worktree only, or add a narrow allow rule. Rule changes need David's OK.
- **F3: release has a guard.** `member_release` refuses unless the item is done, dropped or awaiting-review. Briefs must set the item to awaiting-review first.
- **F4 (gap): no account label.** Codex and Antigravity seats show no provider or account label.

## Open product gap for David

Workers may need a human to approve each `git add` and `git commit` inside their own team worktree. The rehearsal will use per-command approvals scoped to `/tmp/teams-sandbox/proj/.trees/<worker-worktree>` and will not add a global allow rule. A team whose workers cannot commit without a human approving every commit is not yet hands-off. A possible safe default is a team-scoped commit capability limited to the member's assigned worktree, with path validation and an explicit opt-in at team creation; it should permit only staging and committing, never push, branch deletion, or history rewriting. This remains an open product decision; do not implement it without David's decision.

## State at handoff (2026-11-12)

- `feat/teams` HEAD is 6ea5a497 plus this docs commit. The worktree is `/Users/dfrazer/patchbay-teams` (not `~/patchbay`).
- Part 2 (the unprompted run) is NOT done. Team live9 is `c3186f2c-1bb1-4e1b-b7f7-1bb569ea346f`, coordinator `e443d996-083e-45ab-b5c2-0c2d174b9398` (Codex).
  - Its item `live9-20261112-01` is `noticed`.
  - Decision `live9-D1` is `awaiting_advisor` (finding F1), with recommendation "CLEAR: keep normal priority and dispatch".
  - There are 0 dispatch rows. 0e5ebc67 was asked to continue it and has not reported.
- **Your first step: part 2.**
  1. Ask 0e5ebc67 for its result. If it has none, run it yourself or on a fresh Codex session. The setup:
     - `git worktree add --detach /tmp/pb-liveN 6ea5a497`, symlink node_modules, `npx vite build`.
     - Start the binary at `/Users/dfrazer/patchbay/src-tauri/target-teams-live/debug/patchbayd --data-dir /Users/dfrazer/patchbay-teams-data --listen 127.0.0.1:4531 --ui-dir /tmp/pb-liveN/dist`. Already built at 6ea5a497.
  2. Capture the coordinator's tool calls so far, especially `team_roster` and its raw result.
  3. Answer live9-D1 in the UI decision queue with the first option.
  4. Send the coordinator only "The decision is answered. Carry on."
  5. PASS only if it then calls `dispatch_item` with no mention of that tool or of dispatching. Capture the roster call and the dispatches row.
  6. Finish it: the worker commits (allow git add and commit only inside its `.trees` worktree), then the coordinator sets the item to awaiting-review and calls `member_release`.
  7. Report to the director, and tell it the moment 0e5ebc67 is free (it is needed for the test rig, pb-20261111-24).
- Then steps 3-6 of the order above. Don't wait for the director between steps; report each one as it lands.
- Coordinator account: this session runs as Codex on its own login. It must never use Claude Code or the Patchbay agent default, both of which bill to David's capped Claude Pro account. If Codex refuses on usage, tell the director; the fallback is OpenCode.
