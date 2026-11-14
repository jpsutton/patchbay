# Teams lane coordinator handoff

Lane process notes for continuing `feat/teams`. Report to the director session at each
step boundary. Session ids and host paths are omitted here (see live evidence folders
under the sandbox data dir when needed).

## Branch

- Worktree: `<repo>` on branch `feat/teams`. Staging target: `origin/staging`.
- Do not push or open a PR unless the product owner decides.

## Order (high level)

1. Worker `team_send` → coordinator `decision_open` → UI answer → `member_release`.
2. Unprompted coordinator finds `dispatch_item` from the roster hint.
3. Rebase or merge onto `origin/staging` (after the security cut); then cargo, tsc, selftests, smoke loop.
4. Optional Sonnet-default coordinator loop; three-worker rehearsal under the same stop rules.
5. Full security re-review of `feat/teams` against staging before merge.

## Scratch-daemon never-rules

1. Real `HOME` plus `--data-dir`, never a `HOME` override (keyring collision).
2. Loopback on a port other than the live app port (`4420`).
3. Attach only by PID, binary path, or URL+port — never by app name / Launch Services / `open -a`.
4. Isolated browser profile if a browser is used.
5. Teardown leaves no listener and no scoped keyring entries.

### Pool and wake constraints

- **Per-pool cap:** at most two concurrently active seats per subscription pool (Codex, Antigravity, Claude Teams, Cursor).
- **Wake rule:** never end a turn in a pure wait without naming what is waited on and who holds it.
- **Permission responses as human:** `code_permission_respond` over WebSocket is allowed only inside the Teams sandbox on loopback (not the live app port), only for commands entirely inside the item worktree, and only when the run criteria allow approvals. List every approval in the run report. Never grant seat acknowledgements this way.
- **Zero-approval runs:** nobody approves anything; a prompt fails that criterion.
- **Credential files:** never read credential or account files.
- **Database read-only:** sandbox `teams.db` / `patchbay.sqlite` are read-only to lane seats; state changes only via product UI/tools.

## Standing rules

- No push/PR from workers unless ordered. Stage explicit paths only; never `skills/` or `target-review/`. Do not edit `docs/model-registry/` for ordinary lane work (public-exposure scrub commits may touch it if ordered).
- Sandbox only: `patchbayd --data-dir <data-dir>`, team folder under a temp sandbox project, worktrees under that project's `.trees`.
  - Build daemon with `--no-default-features --features web` into `CARGO_TARGET_DIR=<repo>/src-tauri/target-teams-live` (or the seat's reused target).
  - Serve on loopback with `--ui-dir` pointing at a built UI dir.
- **`lsregister -u` after any local `dev.patchbay.app` bundle.**
- Never touch the live desktop app ports or non-sandbox data folders.
- Stop on usage refusal; never burn credits/API keys against a capped personal login.
- Verify: both locked feature legs (`ipc-test` and `web`), plus tsc/selftests when UI changes. Prefer a throwaway detached worktree; `CARGO_TARGET_DIR=<main-checkout>/src-tauri/target` is fine for speed but correctness must not depend on CTD (hook runs without it).
- Shell: prefix with `rtk`. Never print bridge tokens.

## Findings retained for design

- **F1:** `advisor_first` must not stall forever when no `decides` member exists (`awaiting_user` + warn).
- **F2:** workers historically needed shell approval for git; opt-in `allow_worker_commit` + forced `-c` prefix is the merge-gate mechanism (see `teams.md` / `approval-modes.md`).
- **F3:** `member_release` only when item is done, dropped, or awaiting-review.
- **F4:** seats may lack a provider/account label in the UI (capabilities gap).

## Build rules (SSD wear)

1. One `CARGO_TARGET_DIR` per seat; reuse it. No fresh targets under `/private/tmp`. No `cargo clean` unless a cold build is required and stated.
2. Verifiers run targeted `--lib` legs; full local builds only where CI cannot cover (desktop shell, hardware rigs, sandbox daemon binary).
3. On merge close-out, delete that worktree's `src-tauri/target` when the row merges.

## State note

Product state, HEAD, and live-run evidence live in the sandbox evidence tree and git — not duplicated here with host paths or session UUIDs.
