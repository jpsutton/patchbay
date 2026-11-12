# Teams Lane Governance Proposals (F2, F5, F6, F7 & Cursor Gaps)

**Author:** Teams Lane Coordinator (Antigravity)  
**Date:** 2026-11-12  
**Target:** David / Director (`4fc0a3d8-fdcc-44bd-afe7-bf576fe62f1b`)  
**Scope:** Architecture, security, and state machine invariants across `feat/teams`

---

## David's Decisions (2026-11-12)

1. **F5 ("Hard block for team members")**: `create_agent_session` refuses when the caller is an active team member, with a message pointing to `dispatch_item` and to raising the blocker with the user.
2. **F6 ("Role text + dispatch-required guard")**: `builtin-desk` role text specifies that the coordinator never edits project files or implements items; `item_set` refuses `awaiting-review` and `done` unless the item has a dispatch row.
3. **F7 ("Backend check + role text")**: `item_set` rejects `done` when the item's worktree is dirty or its branch has no commit ahead of base, unless a PR URL is set; desk role text requires checking the worktree and git log, never the worker's word.
4. **F2 ("Opt-in, worktree-scoped commit")**: Per team template, off by default: workers may run `git add` and `git commit` without asking only inside their own `.trees` worktree; push, branch deletion and history rewrite stay blocked or prompted. Smallest mechanism fitting existing `teams/allow.rs` allow-list, exposed in the template editor with a plain warning.

---

## 1. Finding F2: Worker Git Commit Approvals Inside Worktree

### Problem Description
Workers operating in dedicated worktrees (`.trees/<item-id>`) require interactive approval for shell commands (`git add`, `git commit`) unless running in `always-approve` mode. Under default permission modes, every worker commit prompts the human user, stalling autonomous workflows. Conversely, granting broad shell approval (`allow_unasked`) risks dangerous commands (`git push`, branch deletion, history rewriting).

### Relevant Code Paths
- `src-tauri/src/teams/allow.rs:20-35`: Defines tool calls a member may invoke without prompting. Currently covers teams-specific IPC tools (`item_*`, `team_*`, `decision_*`), but does not grant scoped git execution.
- `src-tauri/src/teams/dispatch.rs:220-250`: Inspects `ApprovalSetup::for_agent(kind).gaps` when launching worker sessions.

### Options

| Option | What it stops | What it breaks | Size |
|---|---|---|---|
| **Option 1: Manual Interactive Approvals**<br>Keep current behavior. Every `git add` and `git commit` requires human click in UI. | Completely prevents unintended git operations. | Eliminates hands-off automation; teams cannot finish items without human hovering over shell prompts. | XS |
| **Option 2: Scoped Worktree Git Capability (Recommended)**<br>Introduce a scoped worktree git rule in `allow.rs`: automatically permit `git {add, commit, status, diff}` strictly when `cwd` is within `.trees/<item-id>` and target is the item's branch. Never permit `git push`, branch deletion, or operations outside `.trees/`. | Stops unstaged work and eliminates human prompt friction for standard commits. | Requires path validation against `team.worktree_root` in permission evaluator. | S (~30 lines) |
| **Option 3: Full Shell `allow_unasked`**<br>Configure worker seats with `allow_unasked: true` across all commands. | Eliminates prompts entirely. | Violates least-privilege; allows unrestricted shell operations including network and disk modifications outside the tree. | XS |

**Recommendation:** **Option 2**. Scoped capability limited strictly to staging and committing within the assigned worktree.

---

## 2. Finding F5: Coordinator Work Escape via `create_agent_session`

### Problem Description
When `dispatch_item` is refused by guard checks (e.g. unacknowledged seat gaps), an autonomous coordinator can circumvent the failure by invoking `create_agent_session`. In `live10`, OpenCode spawned an ad-hoc session in root `team.cwd` (`/tmp/teams-sandbox/proj`) instead of `.trees/`. That session lacked a `members` row, a `dispatches` row, worktree isolation, and release lifecycle tracking.

### Relevant Code Paths
- `src-tauri/src/agent_messaging.rs:32, 399-410`: Exposes `create_agent_session` loopback unconditionally to all agents.
- `src-tauri/src/lib.rs:2840-2915` (`create_agent_session`): Checks session depth and caller caps, but does not check if caller is bound to a team role.
- `src-tauri/src/teams/allow.rs:27`: Notes `create_agent_session` is disallowed for team auto-approval, but `always-approve` mode bypasses this.

### Options

| Option | What it stops | What it breaks | Size |
|---|---|---|---|
| **Option 1: Prompt Guidance Only**<br>Add negative constraints to `builtin-desk` instructions advising against ad-hoc sessions. | Provides behavioral guidance to well-behaved LLMs. | Easily bypassed by prompt drift, model fallbacks, or aggressive plan execution. | XS |
| **Option 2: Hard Capability Gate (Recommended)**<br>In `src-tauri/src/lib.rs:create_agent_session` (or `agent_messaging.rs`), check if `caller_id` belongs to an active team (`teams.db: members`). If caller is a team member, refuse with: *"Team members must dispatch workers through `dispatch_item`, not ad-hoc sessions."* | Mechanically prevents any team member from escaping team worktree and membership controls. | Prevents non-worker auxiliary sessions created by a team (an anti-pattern in teams architecture anyway). | S (~15 lines) |
| **Option 3: Interactive Approval Gate**<br>Force interactive prompt for `create_agent_session` even when session is in `always-approve`. | Alerts human to out-of-band creation. | Adds UI friction; allows escape if human accidentally approves. | M (~40 lines) |

**Recommendation:** **Option 2**. Team coordinators must only provision child agents via `dispatch_item` into isolated worktrees.

---

## 3. Finding F6: Coordinator Self-Implementation in `team.cwd`

### Problem Description
In `live10` pass 2, after user decision `live10-D1` was resolved, the coordinator received `"The decision is answered. Carry on."` With no worker dispatched yet, the coordinator edited `team.cwd/README.md` directly, committed it, and marked the item done within 6 seconds. No worker was dispatched and no worktree was created.

### Root Cause
1. `builtin-desk` role text lacks an explicit negative prohibition against self-implementation.
2. The coordinator runs with `cwd = team.cwd` and retains general filesystem edit capabilities.
3. `item_set` permits transitioning an item to `in-progress` and `done` even if no dispatch row exists.

### Relevant Code Paths
- `src-tauri/src/teams.rs:245-255`: `builtin-desk` role instructions define coordinator duties but do not explicitly forbid writing code.
- `src-tauri/src/teams/items.rs:230-260`: `item_set` status transitions do not enforce that `status == "done"` requires a completed dispatch.

### Options

| Option | What it stops | What it breaks | Size |
|---|---|---|---|
| **Option A: Role Instruction Clarification**<br>Add explicit negative rule to `builtin-desk`: *"You do not write code, edit project files, or implement items yourself. Every implementation change must be performed by a worker dispatched to a worktree."* | Stops LLMs that follow negative role constraints. | Relies solely on prompt adherence; not enforced if LLM chooses to edit. | XS |
| **Option B: Read-Only Filesystem Sandbox for Coordinator**<br>Enforce read-only file access on `team.cwd` for coordinator sessions via ACP permission filters. | Hard stop against coordinator writes in project root. | Complex ACP filter plumbing; prevents coordinator from creating scratch plans or local notes if needed. | M (~80 lines) |
| **Option C: Combined Role Instruction + Item State Machine Guard (Recommended)**<br>Combine Option A with a guard in `item_set`: refuse transition to `awaiting-review` or `done` unless the item has an active or completed entry in `dispatches`. | Prevents marking items done without a worker dispatch, while clearly guiding coordinator behavior in prompts. | None; fits existing state machine invariants. | S (~25 lines) |

**Recommendation:** **Option C**. Role prompt clarity backed by an invariant check in `item_set`.

---

## 4. Finding F7: Review Accepts Unverified, Uncommitted Work as Done

### Problem Description
In `live11`, worker `310f7d8f` created the MIT `LICENSE` and modified `README.md` in its `.trees/live11-20261112-01` worktree. However, the worker never executed `git commit` or `git add`. It messaged the coordinator:
> *"Files changed: README.md (modified), LICENSE (added/untracked)."*

The coordinator (`5822176e`) **did not read or inspect any worktree files** (proven by session transcript: 0 file read or git tools called during review). It accepted the worker's natural-language note at face value, posted:
> *"Coordinator review: LICENSE reported as standard MIT... files changed README.md (modified), LICENSE (added) in worktree... No PR URL provided."*
and immediately called `item_set(status: "done")` and `member_release`. Work was left uncommitted in an ephemeral worktree.

### Root Cause
1. `item_set(status: "done")` does not verify git branch progression or working tree cleanliness.
2. The coordinator role instructions do not require automated verification (reading git diff/log or checking tests) before accepting an item.

### Relevant Code Paths
- `src-tauri/src/teams/items.rs:240-275` (`item_set`): Validates status enum transitions, but does not check worktree git status.
- `src-tauri/src/teams.rs:245-255` (`builtin-desk`): Instructions say *"workers report to you... you own item status"*, but lack verification standards.

### Definition of Done Invariant
An item cannot transition to `status = "done"` unless:
1. **Durable Branch Commit:** The item's git branch in `worktree_root` has at least one commit ahead of `team.pr_base` (`git rev-parse <branch> != git rev-parse <base>`), AND `git status --porcelain` is clean (no uncommitted dirty files); OR
2. **Recorded PR:** A validated `pr_url` is attached to the item.

### Options

| Option | What it stops | What it breaks | Size |
|---|---|---|---|
| **Option A: Desk Prompt Only**<br>Add instruction to `builtin-desk` requiring coordinators to check `git log` and inspect worktrees before marking done. | Encourages verification. | Soft guidance; coordinators can still accept self-reports and close items prematurely. | XS |
| **Option B: Backend Pre-Done Invariant (Recommended)**<br>In `src-tauri/src/teams/items.rs:item_set`, before updating status to `done`, check git status of the item's worktree. If uncommitted changes exist or branch has not advanced ahead of base branch (and `pr_url` is absent), return an error: *"Cannot mark item done: branch '<item_branch>' has no new commits or has uncommitted changes. Worker must commit changes."* | Mechanically prevents work loss and ensures every completed item represents durable committed history. | Incomplete worker implementations fail the `done` transition, requiring coordinator to instruct worker to commit. | S (~35 lines) |
| **Option C: Auto-Commit on Release**<br>When `member_release` or `item_set(status: "done")` is called, automatically stage and commit any dirty changes with a generic message. | Prevents work loss automatically. | Masking mechanism; hides bad worker discipline and generates poorly documented commits. Anti-pattern. | M (~60 lines) |

**Recommendation:** **Option B** (backed by Option A prompt guidance). Enforce durable commits at the API boundary.

---

## 5. Policy Note: Cursor Seat Acknowledgement & Approval Gaps

### Context
`src-tauri/src/teams/dispatch.rs:231-237` checks `ApprovalSetup::for_agent(kind).gaps`. Because Cursor has known approval gaps, `dispatch_item` bails unless:
```rust
seat.allow_unasked && seat.acknowledged() && !template_id.starts_with("builtin-")
```
### Ruling
Director ruled: **Coding agents must NOT pre-acknowledge Cursor seats on templates.** Pre-acknowledgement bypasses human awareness of security risks. If Cursor seats are used in a team, the human user must explicitly acknowledge the seat gap in the UI before dispatch will succeed. For automated runs and headless testing, use OpenCode, Antigravity, or Codex seats which do not suffer from unacknowledged gap refusals.
