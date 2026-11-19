//! The standing allow for team tools: a live team member's call to `send_agent_message` or a team
//! tool on Patchbay's own messaging server is approved once, without a prompt.
//!
//! Also (opt-in per template): a dispatched worker may run a strictly tokenized read-only `ls` /
//! `cat` inside its assigned worktree without a prompt. Tokenization is fail-closed (clean string
//! only: tokens from `[A-Za-z0-9._/:,@+=-]`, single ASCII spaces, no quotes; shell metacharacters
//! rejected; option allow-lists per verb; paths relative, no `..`, symlink-resolved inside the
//! worktree).
//!
//! **#449 C floor (pb-20261113-08):** Patchbay never auto-approves any worker **git** command
//! (`add` / `commit` / `status` / `diff` / `log`), regardless of `allow_worker_commit`, agent
//! kind, scrubbed fact, version, writable-set, or prefix checks. For Codex under workspace-write
//! + on-request, a permission request for a command **is** an escalation (in-sandbox commands
//! never ask); live24 showed the escalated re-ask arrives as the plain template string and was
//! wrongly auto-approved before this floor. Flip [`WORKER_GIT_WRITE_AUTO_APPROVE`] in the A PR
//! to re-enable the gate stack in [`worker_git_auto_approve_gates`]. Until then, add/commit
//! (and any other worker git) prompt the human once per command (allow-once only).
//!
//! The gate stack (kept for the A PR / planted-red tests) still requires the **absolute** path of
//! the git binary Patchbay resolved (never bare `git`), then this exact prefix before the verb,
//! with `core.hooksPath` equal to the Patchbay-created empty read-only hooks dir:
//! `<abs-git> --no-pager -c core.hooksPath=<dir> -c core.fsmonitor=false -c core.sshCommand=
//! -c protocol.ext.allow=never -c commit.gpgSign=false -c core.editor=: -c credential.helper=
//! -c core.askPass=`. `git commit` requires `--no-verify` and `-F`/`--file` (relative path inside
//! the worktree) — never `-m`/`--message`. Pathspecs require a `--` separator first. Repo
//! hooks/fsmonitor/config gates remain as defense in depth. Worktree paths that contain a space
//! or other non-charset byte cannot be expressed in this template and therefore prompt as
//! non-template commands.
//!
//! When the A PR flips the constant, git auto-approve is further limited by agent kind: only
//! Codex with a proven non-persisting tool-shell and repo metadata outside the writable set.
//! Team worker seat spawn strips every `GIT_*` (`scrub_git_env`); mid-shell `export` is not under
//! Patchbay control. Diff/log auto-approve requires `--no-ext-diff` and `--no-textconv`.
//!
//! Residuals (attackers) when auto-approve is on: see `docs/design/teams.md`. Codex `~/.codex`
//! approved prefixes never reach the Patchbay permission path (pb-20261111-83).
//!
//! Not a `permission_rules` row: those match a tool kind and a command prefix, and MCP calls are
//! kind `other`, so a rule would approve every MCP tool of every server. This runs in the
//! `auto_approve` closure before `permissions::decide`. The host-side role check on each call
//! (`Caller::need`) remains the real control; this only spares the prompt.
//!
//! Worker-git permission enrichment (hooks/config + unsandboxed disclosure) applies to commands
//! that **name git directly** or **cannot be classified** as a plain non-git argv — not to a
//! plain argv that only calls git internally (e.g. `python3 tools/release.py`).
//!
//! The title match is by exact string, one form per agent, taken from real transcripts
//! (docs/design/teams.md, "Tool-call titles"). OpenCode has no form here: it reaches MCP tools
//! only through an `execute` tool that runs JavaScript, which can't be allowed by title.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Team, TeamsDb};
use crate::code::agents::{path_key, path_under, temp_writable_roots};
use crate::code::permissions::{allow_option, command_of};

/// Patchbay-owned empty hooks directory for this team (outside worktrees and temp roots).
pub(crate) fn worker_safe_hooks_dir(data_dir: &Path, team_id: &str) -> PathBuf {
    data_dir.join("teams-safe").join(team_id).join("hooks")
}

/// Forward-slash form for `git -c core.hooksPath=` (works on Windows git too; avoids `\\` tokens).
/// Canonicalize when possible so Windows short names (`RUNNER~1`) become long paths — `~` is
/// forbidden in unquoted tokens (tilde expansion). Unknown `\\?\` namespaces are not returned
/// unstripped (prefer the non-canonical input over a Volume GUID path).
pub(crate) fn path_for_git_c_flag(path: &Path) -> String {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        let s = resolved.to_string_lossy();
        let usable = match crate::win_path::strip_windows_verbatim_str(&s) {
            Some(stripped) => stripped,
            None => path.to_string_lossy().into_owned(),
        };
        return usable.replace('\\', "/");
    }
    #[cfg(not(windows))]
    {
        resolved.to_string_lossy().replace('\\', "/")
    }
}

/// True when `path` (canonical) lies under any Codex/F2 temp-writable root.
fn under_temp_writable_root(path: &Path) -> bool {
    let Some(key) = path_key(path) else {
        return true; // unresolvable → treat as unsafe
    };
    let Some(roots) = temp_writable_roots() else {
        return true; // cannot enumerate temps → refuse
    };
    roots.iter().any(|r| path_under(&key, r))
}

/// Refuse auto-approve when hooks dir or data_dir sits under a worker-writable root:
/// team cwd, worktree root, item worktree, or temp (`/tmp`, `/private/tmp`, `$TMPDIR`, `%TEMP%`/`%TMP%`).
fn hooks_location_ok(data_dir: &Path, team: &Team, item_worktree: &Path, hooks_path: &Path) -> bool {
    if under_temp_writable_root(data_dir) || under_temp_writable_root(hooks_path) {
        return false;
    }
    let Some(data_key) = path_key(data_dir) else {
        return false;
    };
    let Some(hooks_key) = path_key(hooks_path) else {
        return false;
    };
    let Ok(cwd) = Path::new(&team.cwd).canonicalize() else {
        return false;
    };
    let Ok(wt_root) = team.effective_worktree_root() else {
        return false;
    };
    let Some(wt_key) = path_key(&wt_root) else {
        return false;
    };
    let Some(item_key) = path_key(item_worktree) else {
        return false;
    };
    let cwd_key = path_key(&cwd).unwrap_or(cwd);
    for root in [&cwd_key, &wt_key, &item_key] {
        if path_under(&hooks_key, root) || path_under(&data_key, root) {
            return false;
        }
    }
    true
}

/// lstat each component from `data_dir` through `hooks_path`; refuse symlinks and divergent resolves.
fn hooks_path_no_symlinks(data_dir: &Path, hooks_path: &Path) -> bool {
    let Ok(rel) = hooks_path.strip_prefix(data_dir) else {
        return false;
    };
    let Ok(mut expected) = data_dir.canonicalize() else {
        return false;
    };
    // data_dir itself must not be a symlink (compare lstat path to canonical).
    {
        let Ok(meta) = std::fs::symlink_metadata(data_dir) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return false;
        }
    }
    for c in rel.components() {
        expected.push(c);
        let Ok(meta) = std::fs::symlink_metadata(&expected) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return false;
        }
        let Ok(canon) = expected.canonicalize() else {
            return false;
        };
        // Walk stays on the expected path (no symlink redirect). Normalize macOS /tmp.
        let left = path_key(&canon).unwrap_or(canon);
        let right = path_key(&expected).unwrap_or_else(|| expected.clone());
        if left != right {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn hooks_unix_owner_and_mode_ok(data_dir: &Path, hooks_path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let uid = unsafe { libc::getuid() };
    let Ok(hooks_meta) = std::fs::metadata(hooks_path) else {
        return false;
    };
    if hooks_meta.uid() != uid {
        return false;
    }
    // No write bits for owner/group/other (0o555-like).
    if hooks_meta.mode() & 0o222 != 0 {
        return false;
    }
    // Parents up through data_dir: not group/world-writable.
    let mut cur = hooks_path.to_path_buf();
    loop {
        let Some(parent) = cur.parent().map(Path::to_path_buf) else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(&parent) else {
            return false;
        };
        if meta.mode() & 0o022 != 0 {
            return false;
        }
        if paths_equal(&parent, data_dir) {
            return true;
        }
        cur = parent;
    }
}

#[cfg(windows)]
fn hooks_unix_owner_and_mode_ok(_data_dir: &Path, hooks_path: &Path) -> bool {
    // Best-effort: skip owner ACL; require the readonly bit ensure_worker_safe_hooks_dir sets.
    let Ok(meta) = std::fs::metadata(hooks_path) else {
        return false;
    };
    meta.is_dir() && meta.permissions().readonly()
}

/// Re-validate the Patchbay hooks dir at every approval (not only at dispatch create time).
pub(crate) fn worker_safe_hooks_dir_ok(data_dir: &Path, team: &Team, item_worktree: &Path, hooks_path: &Path) -> bool {
    if !hooks_location_ok(data_dir, team, item_worktree, hooks_path) {
        return false;
    }
    if !hooks_path_no_symlinks(data_dir, hooks_path) {
        return false;
    }
    let Ok(meta) = std::fs::symlink_metadata(hooks_path) else {
        return false;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }
    // `symlink_metadata` on a non-symlink dir: confirm it's a directory via metadata too.
    let Ok(dir_meta) = std::fs::metadata(hooks_path) else {
        return false;
    };
    if !dir_meta.is_dir() {
        return false;
    }
    if !hooks_unix_owner_and_mode_ok(data_dir, hooks_path) {
        return false;
    }
    let Ok(mut entries) = std::fs::read_dir(hooks_path) else {
        return false;
    };
    if entries.next().is_some() {
        return false;
    }
    true
}

/// `core.hooksPath=…` argument as it should appear in a brief / clean-string command.
/// Never quoted: the template charset cannot express spaces/tabs/`~`, so a hooks dir whose
/// path needs quoting cannot appear in an auto-approvable command (hooks under `data_dir`
/// are clean by construction).
pub(crate) fn hooks_path_c_arg(hooks_path: &Path) -> String {
    format!("core.hooksPath={}", path_for_git_c_flag(hooks_path))
}

/// Absolute path of the `git` binary Patchbay resolved for worker templates / auto-approve.
/// Bare `git` is never accepted by the matcher (login zsh may alias/function-shadow `git`).
pub fn resolved_worker_git() -> Option<PathBuf> {
    find_git_on_path()
}

/// Absolute git path as one clean-string token (forward slashes). `None` when git is missing
/// or the path contains a byte outside `[A-Za-z0-9._/:,@+=-]` (e.g. space) — that install
/// cannot use the auto-approve template and prompts as a non-template command.
pub fn resolved_worker_git_token() -> Option<String> {
    let p = resolved_worker_git()?;
    let s = path_for_git_c_flag(&p);
    if s.is_empty() || !Path::new(&s).is_absolute() {
        return None;
    }
    if !s.chars().all(is_plain_char) {
        return None;
    }
    Some(s)
}

fn find_git_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        #[cfg(windows)]
        let names: &[&str] = &["git.exe", "git.cmd", "git"];
        #[cfg(not(windows))]
        let names: &[&str] = &["git"];
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return std::fs::canonicalize(&candidate).ok().or(Some(candidate));
            }
        }
    }
    None
}

fn is_resolved_git_prog(tok: &str) -> bool {
    let Some(want_tok) = resolved_worker_git_token() else {
        return false;
    };
    if tok == want_tok {
        return true;
    }
    let Some(want) = resolved_worker_git() else {
        return false;
    };
    let got = Path::new(tok);
    if !got.is_absolute() {
        return false;
    }
    match (path_key(got), path_key(&want)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Create/verify the empty read-only hooks dir used in the required `-c core.hooksPath=` prefix.
pub(crate) fn ensure_worker_safe_hooks_dir(data_dir: &Path, team_id: &str) -> Result<PathBuf> {
    let dir = worker_safe_hooks_dir(data_dir, team_id);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    // Ensure we can clear leftovers: temporarily writable if it already exists.
    #[cfg(unix)]
    if dir.is_dir() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
    }
    #[cfg(windows)]
    if dir.is_dir() {
        let mut p = std::fs::metadata(&dir)?.permissions();
        p.set_readonly(false);
        let _ = std::fs::set_permissions(&dir, p);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    #[cfg(windows)]
    {
        let mut p = std::fs::metadata(&dir)?.permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&dir, p).with_context(|| format!("readonly {}", dir.display()))?;
    }
    // Lives under data_dir/teams-safe — outside every team worktree and export tree by construction.
    Ok(dir)
}

/// Exact hardening tokens after the absolute git path and before the verb (unquoted form).
/// Placement: `<abs-git> --no-pager -c …` (`--no-pager` before every `-c`).
fn git_hardening_prefix_tokens(hooks_path: &Path) -> [String; 17] {
    [
        "--no-pager".into(),
        "-c".into(),
        format!("core.hooksPath={}", path_for_git_c_flag(hooks_path)),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.sshCommand=".into(),
        "-c".into(),
        "protocol.ext.allow=never".into(),
        "-c".into(),
        "commit.gpgSign=false".into(),
        "-c".into(),
        "core.editor=:".into(),
        "-c".into(),
        "credential.helper=".into(),
        "-c".into(),
        "core.askPass=".into(),
    ]
}

/// When `tokens` is `<abs-git>` + exact hardening prefix + rest, return the rest (`verb` …).
/// Bare `git` is refused — only the resolved absolute path matches.
fn split_git_hardening<'a>(tokens: &'a [String], hooks_path: &Path) -> Option<&'a [String]> {
    let prog = tokens.first()?;
    if !is_resolved_git_prog(prog) {
        return None;
    }
    let prefix = git_hardening_prefix_tokens(hooks_path);
    if tokens.len() < 1 + prefix.len() + 1 {
        return None;
    }
    for (i, want) in prefix.iter().enumerate() {
        if tokens.get(i + 1) != Some(want) {
            return None;
        }
    }
    Some(&tokens[1 + prefix.len()..])
}

/// Human-visible warning appended when a worker `git` command is not auto-approved.
pub const WORKER_GIT_HOOKS_PROMPT: &str = "runs this repository's git hooks and config";

/// Fixed relative commit-message file in the worktree (never staged; delete after commit).
pub const WORKER_COMMIT_MSG_FILE: &str = ".pb-commit-msg";

/// Warning when an `add` pathspec includes the message file (prompt only — not a refusal).
pub const WORKER_GIT_MSGFILE_IN_ADD_WARN: &str =
    "warning: add pathspec includes the commit message file — stage only work paths; commit with -F .pb-commit-msg; then delete .pb-commit-msg";

/// Disclosure that an approved escalated worker git command leaves the sandbox (C2).
pub const WORKER_GIT_UNSANDBOXED_PROMPT: &str =
    "an approved escalated command runs unsandboxed via a login shell, so your shell profiles run first";

/// ACP `agentInfo.version` values with live evidence that Codex tool-shell `export` does not
/// persist across separate execute calls (live22/live23). The allow decision compares the
/// **session's recorded handshake version for the current launch** to this list — never the
/// build-time package pin. The value is the adapter's self-report (a drift guard for a trusted
/// adapter), not authentication of the binary. Cleared at every spawn before SessionReady.
pub const PROVEN_CODEX_NO_EXPORT_PERSIST: &[&str] = &["2.2.2"];

/// Live evidence: whether each agent's tool-shell persists `export` across commands.
/// `None` = unproven → no git auto-approve. `Some(true)` = persists → no auto-approve.
/// `Some(false)` = does not persist → may auto-approve when other gates pass.
///
/// For Codex, `runtime_version` must be the ACP initialize `agentInfo.version` recorded for the
/// **current** launch (after spawn clear). Absent / unknown / not in
/// [`PROVEN_CODEX_NO_EXPORT_PERSIST`] ⇒ `None`.
pub(crate) fn agent_shell_export_persists(agent: &str, runtime_version: Option<&str>) -> Option<bool> {
    match agent {
        "codex" => {
            let v = runtime_version?;
            if !PROVEN_CODEX_NO_EXPORT_PERSIST.contains(&v) {
                return None;
            }
            Some(false)
        }
        // live22: mark empty after separate export+printf → no persist (still unsandboxed → no git auto-approve).
        "opencode" => Some(false),
        // live22: mark="persists" after separate export+printf → persists → no git auto-approve.
        "cursor" => Some(true),
        // Claude Code / Antigravity / Patchbay / Grok / Kilo: unproven ⇒ no git auto-approve.
        _ => None,
    }
}

/// Only Codex with a recorded, proven non-persisting runtime version may receive git auto-approve.
fn agent_may_git_auto_approve(agent: &str, data_dir: &Path, session: &str) -> bool {
    let recorded = TeamsDb::open(data_dir)
        .ok()
        .and_then(|t| t.acp_runtime_version(session).ok())
        .flatten();
    agent == "codex" && agent_shell_export_persists(agent, recorded.as_deref()) == Some(false)
}

/// #449 C floor (pb-20261113-08): when `false`, Patchbay never auto-approves any worker git
/// command — `add`/`commit` and read-only `status`/`diff`/`log` alike. For Codex under
/// workspace-write + on-request, a permission request **is** an escalation (in-sandbox never
/// asks); live24's escalated re-ask was the plain template string and was wrongly auto-approved
/// while this was effectively true via `allow_worker_commit`. The A PR flips this to `true` and
/// reuses [`worker_git_auto_approve_gates`] (scrubbed, version, writable-set, prefix, …).
pub const WORKER_GIT_WRITE_AUTO_APPROVE: bool = false;

/// Agents whose title form is known (see the module note); the others still prompt.
pub fn has_rule(agent: &str) -> bool {
    matches!(agent, "claude-code" | "codex" | "cursor" | "antigravity" | "patchbay")
}

/// Candidate tools a member may auto-approve (never `create_agent_session` / `share_artifact`).
fn candidate_tools() -> impl Iterator<Item = &'static str> {
    std::iter::once("send_agent_message").chain(super::items::TOOLS.iter().copied())
}

/// Names with an explicit arm in [`tool_cap_gate`]. Must equal `items::TOOLS` ∪
/// `{send_agent_message}` (enforced by `stage6d_team_tool_caps_parity`).
const CAPS_MAPPED_TOOLS: &[&str] = &[
    "send_agent_message",
    "team_roster",
    "team_send",
    "item_list",
    "item_get",
    "item_note",
    "item_add",
    "item_set",
    "decision_open",
    "decision_answer",
    "dispatch_item",
    "member_release",
];

/// Capability gate for a mapped team tool. `None` = no mapping (never auto-allow).
fn tool_cap_gate(tool: &str) -> Option<fn(&super::Caps) -> bool> {
    match tool {
        // No cap gate on the server (any live member).
        "send_agent_message" | "team_roster" | "team_send" | "item_list" | "item_get" => Some(|_| true),
        // Server: writes_items OR assignee; auto-approve for any member (server still enforces).
        "item_note" => Some(|_| true),
        "item_add" | "item_set" | "decision_open" => Some(|c| c.writes_items),
        "decision_answer" => Some(|c| c.decides),
        "dispatch_item" | "member_release" => Some(|c| c.dispatches),
        _ => None,
    }
}

/// Whether `tool` may run for a role with `caps` — same capability gates as `items::run_tool` /
/// `dispatch::{dispatch_item,member_release}` (one source of truth for auto-approve).
fn tool_permitted_by_caps(tool: &str, caps: &super::Caps) -> bool {
    tool_cap_gate(tool).is_some_and(|gate| gate(caps))
}

/// Header prepended to agent-origin prompts for team seats so the ACP first text block can never
/// start with `/` after the adapter's trim (codex-acp `parseCommand`: `prompt[0]` must be text).
pub const AGENT_ORIGIN_TEAM_PREFIX: &str = "Patchbay agent-origin message:\n\n";

/// Sessions whose team-tool floor exemption was revoked after an `/mcp` slash command reached them.
fn team_tool_exemption_revocations() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Mark that `session` must not auto-approve team tools until the next process spawn mints a new nonce.
pub fn revoke_team_tool_exemption(session: &str) {
    if let Ok(mut g) = team_tool_exemption_revocations().lock() {
        g.insert(session.to_owned());
    }
}

/// Cleared when `server_definition` mints a fresh team-seat nonce for this session.
pub fn clear_team_tool_exemption_revocation(session: &str) {
    if let Ok(mut g) = team_tool_exemption_revocations().lock() {
        g.remove(session);
    }
}

pub fn team_tool_exemption_revoked(session: &str) -> bool {
    team_tool_exemption_revocations()
        .lock()
        .ok()
        .map(|g| g.contains(session))
        .unwrap_or(true)
}

#[cfg(test)]
pub fn clear_all_team_tool_exemption_revocations_for_test() {
    if let Ok(mut g) = team_tool_exemption_revocations().lock() {
        g.clear();
    }
}

/// ECMAScript-like trim for slash detection (White_Space + LineTerminator + BOM). When unsure,
/// prefer neutralizing / revoking rather than under-matching the adapter.
pub fn acp_slash_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// Whether the first ACP block looks like a slash command (text starting with `/` after trim).
/// Used by neutralization tests; revocation uses the deliberately broader [`acp_slash_is_mcp`].
pub fn parse_acp_slash_command(blocks: &[Value]) -> Option<&str> {
    let first = blocks.first()?;
    if first.get("type").and_then(|t| t.as_str()) != Some("text") {
        return None;
    }
    let text = acp_slash_trim(first.get("text").and_then(|t| t.as_str())?);
    text.starts_with('/').then_some(text)
}

/// Deliberately over-broad `/mcp` detector for exemption revocation: first block is text and,
/// after trim, starts with `/mcp` case-insensitively. Does **not** tokenize on JS whitespace —
/// so `/mcp<U+FEFF>reconnect`, `/mcp<NBSP>reconnect`, and `/mcpx` all revoke (safe direction).
pub fn acp_slash_is_mcp(blocks: &[Value]) -> bool {
    let Some(text) = parse_acp_slash_command(blocks) else {
        return false;
    };
    text.len() >= 4 && text.as_bytes()[..4].eq_ignore_ascii_case(b"/mcp")
}

/// Agent-origin body for a team seat: always prefix so the text block cannot be a slash command.
pub fn neutralize_agent_origin_for_team_seat(text: String) -> String {
    if text.starts_with(AGENT_ORIGIN_TEAM_PREFIX) {
        return text;
    }
    format!("{AGENT_ORIGIN_TEAM_PREFIX}{text}")
}

fn member_caps(data_dir: &Path, session: &str) -> Option<super::Caps> {
    let tdb = TeamsDb::open(data_dir).ok()?;
    let (_team, member) = tdb.membership(session).ok()??;
    Some(tdb.role(&member.role_id).ok()?.caps)
}

/// Active (non-reserved) membership on a non-archived team — used for nonce injection at spawn.
pub fn is_active_team_member(data_dir: &Path, session: &str) -> bool {
    member_caps(data_dir, session).is_some()
}

/// The titles `agent` gives a call of `tool` on MCP server `server`; none for an unknown agent.
/// OpenCode has no title form (reaches MCP only via JS execute) — always prompts.
fn titles(agent: &str, server: &str, tool: &str) -> Vec<String> {
    match agent {
        "claude-code" => vec![format!("mcp__{server}__{tool}")],
        "codex" => vec![format!("mcp.{server}.{tool}")],
        "antigravity" => vec![format!("{server}_{tool}")],
        "cursor" => vec![format!("{server}-{tool}: {tool}"), format!("{server}: {tool}")],
        "patchbay" => vec![format!("{tool} ({server})")],
        _ => vec![],
    }
}

/// Keys that would make a non-execute call more than a message: a path or a command to run.
const BAD_KEYS: &[&str] = &[
    "command", "cmd", "argv", "args", "path", "paths", "file_path", "filePath", "cwd", "files", "uri",
    "url",
];

fn carries_path_or_command(raw: &Value) -> bool {
    let bad = |o: &serde_json::Map<String, Value>| o.keys().any(|k| BAD_KEYS.contains(&k.as_str()));
    match raw {
        Value::Object(o) => bad(o) || o.get("arguments").and_then(Value::as_object).is_some_and(bad),
        _ => false,
    }
}

/// Exact Codex MCP permission rawInput key set (advisor a): only these three keys.
fn is_exact_codex_mcp_raw(raw: &Value) -> bool {
    let Some(obj) = raw.as_object() else {
        return false;
    };
    obj.len() == 3
        && obj.contains_key("server")
        && obj.contains_key("tool")
        && obj.contains_key("arguments")
        && obj["arguments"].is_object()
}

/// The tool a call is, when it is one of ours that this member's role may make unasked.
///
/// - **Exemption server**: only the session's stored team-seat nonce, byte-equal. The fixed
///   `patchbay-agent-messaging` name never matches, even if stored (attach-after-spawn / no
///   relaunch). No stored name ⇒ no match (fail-closed).
/// - **Codex + `kind == "execute"`**: title ignored; rawInput exactly `{server,tool,arguments}`;
///   `server` equals that nonce; `tool` permitted by the seat's role caps.
/// - **Claude Code / Cursor / Antigravity / Patchbay + `kind == "other"`**: title form for the
///   stored nonce; path/command keys refuse; same cap filter.
/// - **OpenCode / unknown**: no form — never matches (always asks).
/// - Non-Codex agent sending the Codex execute shape: no match (asks).
fn matched_tool(data_dir: &Path, agent: &str, session: &str, tool_call: &Value) -> Option<&'static str> {
    let kind = tool_call["kind"].as_str().unwrap_or("");
    let raw = &tool_call["rawInput"];
    // Stage 6d (B): `/mcp` that reached this seat revokes until the next spawn/nonce.
    if team_tool_exemption_revoked(session) {
        return None;
    }
    let injected = crate::agent_messaging::injected_mcp_server_name(session)?;
    // Advisor (a): fixed name never exempts — only a team-seat nonce.
    if !crate::agent_messaging::is_team_seat_nonce(&injected) {
        return None;
    }
    let caps = member_caps(data_dir, session)?;
    let permitted = |tool: &str| tool_permitted_by_caps(tool, &caps);
    match (agent, kind) {
        ("codex", "execute") => {
            if !is_exact_codex_mcp_raw(raw) {
                return None;
            }
            let server = raw["server"].as_str()?;
            let tool = raw["tool"].as_str()?;
            if server != injected {
                return None;
            }
            candidate_tools().find(|t| *t == tool && permitted(t))
        }
        ("claude-code" | "cursor" | "antigravity" | "patchbay", "other") => {
            if carries_path_or_command(raw) {
                return None;
            }
            let title = tool_call["title"].as_str()?;
            candidate_tools().find(|t| permitted(t) && titles(agent, &injected, t).iter().any(|x| x == title))
        }
        _ => None,
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// ---- Worker command allow-list (strict tokenizer) --------------------------------------------

/// Clean-string token charset: `[A-Za-z0-9._/:,@+=-]`.
fn is_plain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._/:,@+=-".contains(c)
}

/// Fail-closed clean-string tokenizer: ASCII space separators only; no quotes/tabs; no token
/// starting with `=`. Bare `git` / `-m` shapes are rejected later by the verb matcher — this
/// layer only admits the charset that would qualify for a future escalated auto-approve.
fn tokenize_worker_command(cmd: &str) -> Option<Vec<String>> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    if cmd.chars().any(|c| c == '"' || c == '\'' || c == '\t') {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for tok in cmd.split(' ').filter(|t| !t.is_empty()) {
        if tok.starts_with('=') {
            return None;
        }
        if !tok.chars().all(is_plain_char) {
            return None;
        }
        out.push(tok.to_owned());
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Option allow-lists (unknown option ⇒ not auto-approvable).
const GIT_ADD_OPTS: &[&str] = &["-A", "--all", "-u", "--update", "--"];
const GIT_STATUS_OPTS: &[&str] = &["-s", "--short", "-b", "--branch", "--porcelain", "--long"];
const GIT_DIFF_OPTS: &[&str] = &[
    "--stat",
    "--name-only",
    "--name-status",
    "--cached",
    "--staged",
    "--no-ext-diff",
    "--no-textconv",
    "--",
];
const GIT_LOG_OPTS: &[&str] = &[
    "--oneline",
    "--decorate",
    "--stat",
    "--reverse",
    "-1",
    "-2",
    "-3",
    "-4",
    "-5",
    "-10",
    "--max-count=1",
    "--max-count=2",
    "--max-count=5",
    "--max-count=10",
    // Required on every auto-approved log (patches / textconv / external diff).
    "--no-ext-diff",
    "--no-textconv",
];

/// Auto-approved `diff` / `log` must carry both flags (textconv + external diff stay off).
fn requires_no_ext_diff_and_no_textconv(rest: &[String]) -> bool {
    let has = |flag: &str| rest.iter().any(|t| t == flag);
    has("--no-ext-diff") && has("--no-textconv")
}
const LS_OPTS: &[&str] = &["-l", "-a", "-A", "-1", "-h", "-R", "-la", "-al", "-lh", "-hl", "-lah", "-alh", "-hla"];

fn resolve_against(root: &Path, p: &str) -> Option<PathBuf> {
    let joined = if Path::new(p).is_absolute() { PathBuf::from(p) } else { root.join(p) };
    let comps: Vec<Component> = joined.components().collect();
    for i in (1..=comps.len()).rev() {
        let Ok(mut real) = comps[..i].iter().collect::<PathBuf>().canonicalize() else {
            continue;
        };
        for c in &comps[i..] {
            match c {
                Component::ParentDir => {
                    real.pop();
                }
                Component::CurDir => {}
                c => real.push(c),
            }
        }
        return Some(real);
    }
    None
}

/// Relative path only; no `..`; symlink-resolved inside worktree; regular file or directory.
fn path_arg_ok(worktree: &Path, p: &str) -> bool {
    if p.is_empty() || p == "-" || p.starts_with('/') || p.starts_with('-') {
        return false;
    }
    let path = Path::new(p);
    if path.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))) {
        return false;
    }
    let Ok(root) = worktree.canonicalize() else {
        return false;
    };
    let Some(real) = resolve_against(&root, p) else {
        return false;
    };
    let Ok(rel) = real.strip_prefix(&root) else {
        return false;
    };
    if rel.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    let Ok(meta) = std::fs::metadata(&real) else {
        return false;
    };
    meta.is_file() || meta.is_dir()
}

fn all_path_args_ok(worktree: &Path, paths: &[String]) -> bool {
    paths.iter().all(|p| path_arg_ok(worktree, p))
}

fn is_opt(tok: &str) -> bool {
    tok.starts_with('-')
}

/// Parse `git` args starting at the verb (hardening prefix already stripped) under per-verb
/// option allow-lists; collect path args. `commit` requires `--no-verify` and `-F`/`--file`
/// (path inside the worktree); `-m`/`--message` never auto-approve. Pathspecs require `--`.
fn parse_git_verb(verb_and_rest: &[String], worktree: &Path) -> bool {
    let verb = verb_and_rest.first().map(String::as_str).unwrap_or("");
    let rest = if verb_and_rest.len() > 1 { &verb_and_rest[1..] } else { &[][..] };
    match verb {
        "add" => {
            let mut paths = Vec::new();
            let mut i = 0;
            let mut seen_dd = false;
            while i < rest.len() {
                let t = &rest[i];
                if !seen_dd && t == "--" {
                    seen_dd = true;
                    i += 1;
                    continue;
                }
                if !seen_dd && is_opt(t) {
                    if !GIT_ADD_OPTS.contains(&t.as_str()) {
                        return false;
                    }
                    i += 1;
                    continue;
                }
                // Pathspec without a preceding `--` → refuse.
                if !seen_dd {
                    return false;
                }
                paths.push(t.clone());
                i += 1;
            }
            // -A/--all/-u may stand alone (no pathspec → `--` not required); otherwise ≥1 path.
            let has_all = rest.iter().any(|t| matches!(t.as_str(), "-A" | "--all" | "-u" | "--update"));
            if paths.is_empty() {
                return has_all;
            }
            all_path_args_ok(worktree, &paths)
        }
        "commit" => {
            // Require --no-verify and exactly one -F/--file. No -m/-q/-v/amend/--template.
            let mut saw_file = false;
            let mut saw_no_verify = false;
            let mut i = 0;
            while i < rest.len() {
                let t = &rest[i];
                if t == "--no-verify" {
                    saw_no_verify = true;
                    i += 1;
                    continue;
                }
                if t == "-F" || t == "--file" {
                    if saw_file {
                        return false;
                    }
                    let Some(path) = rest.get(i + 1) else {
                        return false;
                    };
                    if is_opt(path) || !path_arg_ok(worktree, path) {
                        return false;
                    }
                    // Message file must be a regular file (not a directory).
                    let Ok(root) = worktree.canonicalize() else {
                        return false;
                    };
                    let Some(real) = resolve_against(&root, path) else {
                        return false;
                    };
                    let Ok(meta) = std::fs::metadata(&real) else {
                        return false;
                    };
                    if !meta.is_file() {
                        return false;
                    }
                    saw_file = true;
                    i += 2;
                    continue;
                }
                return false;
            }
            saw_file && saw_no_verify
        }
        "status" => parse_git_pathspec_verb(rest, worktree, GIT_STATUS_OPTS),
        "diff" => {
            parse_git_pathspec_verb(rest, worktree, GIT_DIFF_OPTS)
                && requires_no_ext_diff_and_no_textconv(rest)
        }
        "log" => {
            parse_git_pathspec_verb(rest, worktree, GIT_LOG_OPTS)
                && requires_no_ext_diff_and_no_textconv(rest)
        }
        _ => false,
    }
}

/// Options then optional `--` + pathspecs. Pathspecs without a preceding `--` refuse.
fn parse_git_pathspec_verb(rest: &[String], worktree: &Path, allowed_opts: &[&str]) -> bool {
    let mut paths = Vec::new();
    let mut seen_dd = false;
    let mut i = 0;
    while i < rest.len() {
        let t = &rest[i];
        if !seen_dd && t == "--" {
            seen_dd = true;
            i += 1;
            continue;
        }
        if !seen_dd && is_opt(t) {
            if !allowed_opts.contains(&t.as_str()) {
                return false;
            }
            i += 1;
            continue;
        }
        if !seen_dd {
            return false;
        }
        paths.push(t.clone());
        i += 1;
    }
    all_path_args_ok(worktree, &paths)
}

fn parse_ls_cat(tokens: &[String], worktree: &Path) -> bool {
    let prog = tokens[0].as_str();
    let rest = &tokens[1..];
    let mut paths = Vec::new();
    for t in rest {
        if is_opt(t) {
            if prog != "ls" || !LS_OPTS.contains(&t.as_str()) {
                return false;
            }
        } else {
            paths.push(t.clone());
        }
    }
    match prog {
        "ls" => all_path_args_ok(worktree, &paths),
        "cat" => !paths.is_empty() && all_path_args_ok(worktree, &paths),
        _ => false,
    }
}

fn is_safe_worker_command(cmd: &str, worktree: &Path, hooks_path: &Path) -> bool {
    let Some(tokens) = tokenize_worker_command(cmd) else {
        return false;
    };
    let Some(prog) = tokens.first().map(String::as_str) else {
        return false;
    };
    if is_resolved_git_prog(prog) {
        let Some(verb_rest) = split_git_hardening(&tokens, hooks_path) else {
            return false;
        };
        return !verb_rest.is_empty() && parse_git_verb(verb_rest, worktree);
    }
    match prog {
        "ls" | "cat" => parse_ls_cat(&tokens, worktree),
        // Bare `git` (and every other prog) never auto-approves.
        _ => false,
    }
}

// ---- Repo config / hooks gate -----------------------------------------------------------------

/// Read-only git probe in `cwd`. No `-c` overrides: they would make `config --get` lie about the
/// keys we are inspecting. These subcommands do not run hooks or fsmonitor.
/// Inherits the same scrub as F2 via [`crate::code::agents::apply_hardened_git_env`] so daemon-side
/// `GIT_DIR` / `GIT_CONFIG_*` cannot redirect the probe away from the real repo.
fn hardened_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    crate::code::agents::apply_hardened_git_env(&mut cmd);
    let out = cmd.args(["-C"]).arg(cwd).args(args).output().ok()?;
    if !out.status.success() {
        // `config --get` exits 1 when unset — treat as empty.
        if args.first() == Some(&"config") && out.status.code() == Some(1) {
            return Some(String::new());
        }
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Parse `git config --show-origin --show-scope --list` lines into (scope, file path or None, key=value).
/// With both flags git prints `<scope>\t<origin>\t<key>=<value>` (scope first). File origins look
/// like `file:/path/to/config` or `file:.git/config`; non-file (command/env) → None path.
///
/// Fail closed (R2-4): any non-empty line that cannot be parsed ⇒ `None` (whole list rejected).
fn parse_config_list_origins(raw: &str) -> Option<Vec<(String, Option<PathBuf>, String)>> {
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        let (scope, origin, kv) = match parts.as_slice() {
            [scope, origin, kv] => ((*scope).to_owned(), *origin, (*kv).to_owned()),
            // `--show-origin` alone: `<origin>\t<key>=<value>`
            [origin, kv]
                if origin.starts_with("file:")
                    || origin.starts_with("command:")
                    || origin.starts_with("blob:") =>
            {
                (String::new(), *origin, (*kv).to_owned())
            }
            _ => return None,
        };
        let path = if let Some(p) = origin.strip_prefix("file:") {
            Some(PathBuf::from(p))
        } else {
            None
        };
        out.push((scope, path, kv));
    }
    Some(out)
}

/// Resolve absolute path of a config file origin relative to `worktree` when needed.
fn resolve_config_origin_path(worktree: &Path, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return path_key(path).or_else(|| Some(path.to_path_buf()));
    }
    // Relative origins (e.g. `.git/config`) are relative to the worktree cwd of the probe.
    let joined = worktree.join(path);
    path_key(&joined).or_else(|| Some(joined))
}

/// Effective config contributing files must all lie outside the writable set; unresolvable → refuse.
fn worker_git_config_origins_outside_writable(worktree: &Path, writable: &[PathBuf]) -> bool {
    let Some(raw) = hardened_git(worktree, &["config", "--show-origin", "--show-scope", "--list"]) else {
        return false;
    };
    let Some(entries) = parse_config_list_origins(&raw) else {
        return false; // unparseable record → reject whole parse (R2-4)
    };
    if entries.is_empty() {
        // An empty list with a successful probe is unusual; fail closed.
        return false;
    }
    for (_scope, path, _kv) in &entries {
        let Some(p) = path else {
            // command:/env: origins — no file to place; fail closed (cannot prove outside writable).
            return false;
        };
        let Some(abs) = resolve_config_origin_path(worktree, p) else {
            return false;
        };
        if path_under_any(&abs, writable) {
            return false;
        }
    }
    true
}

/// True when any effective `filter.*.clean|smudge|process` is defined (any origin).
fn worker_git_has_filter_driver(worktree: &Path) -> bool {
    let Some(raw) = hardened_git(worktree, &["config", "--show-origin", "--show-scope", "--list"]) else {
        return true; // fail closed: treat as "has filter" so add/commit prompt
    };
    let Some(entries) = parse_config_list_origins(&raw) else {
        return true; // unparseable → treat as hostile
    };
    for (_scope, _path, kv) in entries {
        let key = kv.split_once('=').map(|(k, _)| k).unwrap_or(kv.as_str());
        let lower = key.to_ascii_lowercase();
        if lower.starts_with("filter.")
            && (lower.ends_with(".clean") || lower.ends_with(".smudge") || lower.ends_with(".process"))
        {
            return true;
        }
    }
    false
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Effective hooks directory for `worktree`, or `None` if it cannot be determined (fail closed).
fn resolve_hooks_dir(worktree: &Path) -> Option<PathBuf> {
    let hooks_path = hardened_git(worktree, &["config", "--get", "core.hooksPath"])?;
    let dir = if hooks_path.is_empty() {
        let rel = hardened_git(worktree, &["rev-parse", "--git-path", "hooks"])?;
        let p = PathBuf::from(rel);
        if p.is_absolute() { p } else { worktree.join(p) }
    } else {
        let p = PathBuf::from(hooks_path);
        if p.is_absolute() { p } else { worktree.join(p) }
    };
    Some(dir)
}

/// Hooks that block worker git auto-approve when executable (githooks(5)).
/// `git commit` checks every name here; `add` / `status` / `diff` / `log` only
/// `post-index-change` (index refresh). Intentionally absent: pre-push, post-checkout,
/// post-merge, pre-rebase, post-rewrite, pre-receive, update, push-to-checkout, etc.
const HOOKS_BLOCKING_COMMIT: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-merge-commit",
    "post-index-change",
    "reference-transaction",
    // git commit may run auto-maintenance, which invokes pre-auto-gc (githooks(5)).
    "pre-auto-gc",
];

fn hooks_dir_has_executable(hooks_dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|name| is_executable_file(&hooks_dir.join(name)))
}

fn blocking_hooks_for_verb(verb: &str) -> &'static [&'static str] {
    match verb {
        "commit" => HOOKS_BLOCKING_COMMIT,
        // Same githooks(5) name as in HOOKS_BLOCKING_COMMIT; only this one runs for these verbs.
        "add" | "status" | "diff" | "log" => &["post-index-change"],
        _ => &[],
    }
}

/// True when auto-approve must fall through to a prompt.
fn worker_git_repo_blocks_auto(worktree: &Path, verb: &str) -> bool {
    let Some(fsmonitor) = hardened_git(worktree, &["config", "--get", "core.fsmonitor"]) else {
        return true; // fail closed if git isn't usable
    };
    if !fsmonitor.is_empty() && fsmonitor != "false" {
        return true;
    }

    let Some(hooks_dir) = resolve_hooks_dir(worktree) else {
        return true; // cannot determine hooks dir
    };
    // Missing hooks directory: nothing to run → allow (subject to fsmonitor above).
    if !hooks_dir.is_dir() {
        return false;
    }

    let names = blocking_hooks_for_verb(verb);
    if names.is_empty() {
        return true;
    }
    hooks_dir_has_executable(&hooks_dir, names)
}

/// Codex workspace-write writable roots: session worktree cwd + temp (`/tmp`, `$TMPDIR`, …).
/// OpenCode and unsandboxed kinds have no known sandbox → `None` (fail closed).
fn worker_git_writable_roots(agent: &str, worktree: &Path) -> Option<Vec<PathBuf>> {
    if agent != "codex" {
        return None;
    }
    let wt = path_key(worktree)?;
    let mut roots = vec![wt];
    let temps = temp_writable_roots()?;
    roots.extend(temps);
    Some(roots)
}

/// True when `path` (or the nearest existing ancestor) lies under any writable root.
/// Unresolvable all the way to filesystem root → treat as inside (fail closed).
fn path_under_any(path: &Path, roots: &[PathBuf]) -> bool {
    let mut cur = path.to_path_buf();
    loop {
        if let Some(key) = path_key(&cur) {
            return roots.iter().any(|r| path_under(&key, r));
        }
        match cur.parent() {
            Some(p) if p != cur.as_path() => cur = p.to_path_buf(),
            _ => return true,
        }
    }
}

/// Resolve repo metadata paths that must sit outside the worker writable set.
fn worker_git_meta_paths(worktree: &Path) -> Option<[PathBuf; 4]> {
    // Prefer absolute forms so linked-worktree gitfiles are not mistaken for in-tree paths.
    let common = {
        let rel = hardened_git(worktree, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .or_else(|| hardened_git(worktree, &["rev-parse", "--git-common-dir"]))?;
        let p = PathBuf::from(&rel);
        if p.is_absolute() { p } else { worktree.join(p) }
    };
    let gitdir = {
        let rel = hardened_git(worktree, &["rev-parse", "--path-format=absolute", "--git-dir"])
            .or_else(|| hardened_git(worktree, &["rev-parse", "--absolute-git-dir"]))
            .or_else(|| hardened_git(worktree, &["rev-parse", "--git-dir"]))?;
        let p = PathBuf::from(&rel);
        if p.is_absolute() { p } else { worktree.join(p) }
    };
    let config = common.join("config");
    let config_worktree = {
        let rel = hardened_git(worktree, &["rev-parse", "--path-format=absolute", "--git-path", "config.worktree"])
            .or_else(|| hardened_git(worktree, &["rev-parse", "--git-path", "config.worktree"]))?;
        let p = PathBuf::from(&rel);
        if p.is_absolute() { p } else { worktree.join(p) }
    };
    let info_attributes = {
        let rel = hardened_git(worktree, &["rev-parse", "--path-format=absolute", "--git-path", "info/attributes"])
            .or_else(|| hardened_git(worktree, &["rev-parse", "--git-path", "info/attributes"]))?;
        let p = PathBuf::from(&rel);
        if p.is_absolute() { p } else { worktree.join(p) }
    };
    Some([config, config_worktree, gitdir, info_attributes])
}

/// True when common config, config.worktree, gitdir, and info/attributes are all outside writable roots.
fn worker_git_meta_outside_writable(worktree: &Path, writable: &[PathBuf]) -> bool {
    let Some(paths) = worker_git_meta_paths(worktree) else {
        return false;
    };
    for p in &paths {
        if path_under_any(p, writable) {
            return false;
        }
    }
    true
}

/// Non-empty `rawInput.cwd` from the request. Missing or empty is fail-closed: a recorded
/// session cwd only proves the start directory, not the shell's actual cwd (verifier: not
/// push-safe to fall back). Caller still requires this cwd to equal the item worktree.
fn worker_request_cwd(tool_call: &Value) -> Option<&str> {
    tool_call["rawInput"]["cwd"].as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// Gate stack for worker-git auto-approve (scrubbed, version, writable-set, prefix, filters,
/// repo hooks). Kept live for planted-red / A-PR tests even while
/// [`WORKER_GIT_WRITE_AUTO_APPROVE`] is `false`. Caller must already know the command is a
/// resolved-git template whose cwd/hooks/tokenizer checks passed.
pub(crate) fn worker_git_auto_approve_gates(
    data_dir: &Path,
    session: &str,
    agent: &str,
    tool_call: &Value,
    worktree: &Path,
    hooks_path: &Path,
    tdb: &TeamsDb,
) -> bool {
    let Some(cmd) = command_of(tool_call) else {
        return false;
    };
    let Some(tokens) = tokenize_worker_command(&cmd) else {
        return false;
    };
    if !tokens.first().is_some_and(|t| is_resolved_git_prog(t)) {
        return false;
    }
    if !agent_may_git_auto_approve(agent, data_dir, session) {
        return false;
    }
    // F-E: attach/restore of an already-running session was never spawn-scrubbed → prompt.
    if !tdb.git_env_scrubbed(session).ok().unwrap_or(false) {
        return false;
    }
    let Some(writable) = worker_git_writable_roots(agent, worktree) else {
        return false;
    };
    // Meta paths (common config / worktree config / gitdir / info/attributes) + every
    // effective config origin (includes local/worktree/include/includeIf/global/system).
    if !worker_git_meta_outside_writable(worktree, &writable) {
        return false;
    }
    if !worker_git_config_origins_outside_writable(worktree, &writable) {
        return false;
    }
    let Some(verb_rest) = split_git_hardening(&tokens, hooks_path) else {
        return false;
    };
    let verb = verb_rest.first().map(String::as_str).unwrap_or("");
    // Fail closed: any clean/smudge/process filter (any origin) blocks add/commit —
    // worker-writable .gitattributes can select human-defined filters.
    if matches!(verb, "add" | "commit") && worker_git_has_filter_driver(worktree) {
        return false;
    }
    // Defense in depth: still refuse when the repo's own hooks/fsmonitor look hostile.
    if worker_git_repo_blocks_auto(worktree, verb) {
        return false;
    }
    true
}

/// Opt-in worktree allow: active worker on an assigned item, template opted in, cwd is the
/// assigned worktree, command is a safe ls / cat (and, only when
/// [`WORKER_GIT_WRITE_AUTO_APPROVE`] is true, a safe git form that passes
/// [`worker_git_auto_approve_gates`]).
fn allow_worker_commit(
    data_dir: &Path,
    session: &str,
    agent: &str,
    tool_call: &Value,
    options: &Value,
    _session_cwd: &str,
) -> Option<(String, String)> {
    if tool_call["kind"].as_str() != Some("execute") {
        return None;
    }
    if !data_dir.join("teams.db").is_file() {
        return None;
    }
    let tdb = TeamsDb::open(data_dir).ok()?;
    let (team, member) = tdb.membership(session).ok()??;
    // Role.standing decides worker status — seat keys are user-named (S1).
    if !membership_role_is_worker(&tdb, &member) {
        return None;
    }
    let item_id = member.item_id.as_deref()?;
    let tpl = tdb.template(&team.template_id).ok()?;
    if !tpl.allow_worker_commit {
        return None;
    }
    let worktree_root = team.effective_worktree_root().ok()?;
    let worktree = worktree_root.join(item_id.to_lowercase());
    // Fail closed: no request cwd ⇒ no auto-approve (do not use session cwd).
    let call_cwd = worker_request_cwd(tool_call)?;
    if !paths_equal(Path::new(call_cwd), &worktree) {
        return None;
    }
    let hooks_path = worker_safe_hooks_dir(data_dir, &team.id);
    // Re-validate at every approval: location, no symlinks, owner/mode, empty.
    if !worker_safe_hooks_dir_ok(data_dir, &team, &worktree, &hooks_path) {
        return None;
    }
    let cmd = command_of(tool_call)?;
    if !is_safe_worker_command(&cmd, &worktree, &hooks_path) {
        return None;
    }
    let Some(tokens) = tokenize_worker_command(&cmd) else {
        return None;
    };
    let is_git = tokens.first().is_some_and(|t| is_resolved_git_prog(t));
    if is_git {
        // #449 prompt floor: never auto-approve worker git (pb-20261113-08).
        if !WORKER_GIT_WRITE_AUTO_APPROVE {
            return None;
        }
        if !worker_git_auto_approve_gates(data_dir, session, agent, tool_call, &worktree, &hooks_path, &tdb)
        {
            return None;
        }
    }
    Some((allow_option(options)?, "Auto-approved: worker commit in worktree".into()))
}

/// Shell/meta forms the fail-closed tokenizer cannot fully classify as a plain argv (R2-3).
/// Over-warning is acceptable: these always get the hooks/config phrase on a worker execute.
fn command_not_plain_classified_argv(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    if tokenize_worker_command(cmd).is_none() {
        return true;
    }
    // Expansions / substitution / chaining the tokenizer rejects or ignores.
    if cmd.contains('$')
        || cmd.contains('`')
        || cmd.contains(';')
        || cmd.contains('|')
        || cmd.contains('&')
        || cmd.contains('\n')
    {
        return true;
    }
    // Wrappers that can invoke git without a plain `git` basename as argv0.
    let first = cmd.split_whitespace().next().unwrap_or("");
    let base = Path::new(first)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first)
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "sh" | "bash" | "zsh" | "dash" | "fish" | "env" | "xargs" | "make" | "eval" | "command" | "busybox"
    ) || lower.contains("sh -c")
        || lower.contains("bash -c")
        || lower.contains("zsh -c")
        || lower.contains("xargs git")
        || lower.contains("eval ")
}

/// True when `cmd` looks like it may invoke git, **or** cannot be fully classified as a plain
/// non-git argv (R2-3 conservative). Wrappers, absolute paths, `g=git; $g status`, `$(which git)`,
/// `sh -c`, `xargs git`, aliases — when unsure, true.
pub fn command_may_invoke_git(cmd: &str) -> bool {
    let trimmed = cmd.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if command_not_plain_classified_argv(cmd) {
        return true;
    }
    // Fast path: tokenizer accepted and first prog is exactly `git`.
    if let Some(tokens) = tokenize_worker_command(cmd) {
        if tokens.first().map(String::as_str) == Some("git") {
            return true;
        }
        // Absolute / wrapped git basename in argv0.
        if let Some(prog) = tokens.first() {
            let base = Path::new(prog)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(prog);
            if base == "git" || base == "git.exe" {
                return true;
            }
        }
    }
    // Basename `git` on any path-like token.
    for tok in cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '\'' | '"')) {
        if tok.is_empty() {
            continue;
        }
        let base = Path::new(tok).file_name().and_then(|s| s.to_str()).unwrap_or(tok);
        if base == "git" || base == "git.exe" {
            return true;
        }
    }
    let lower = cmd.to_ascii_lowercase();
    lower.contains(" git ")
        || lower.contains("\tgit ")
        || lower.contains("`git")
        || lower.contains("$(git")
        || lower.starts_with("git ")
        || lower.contains("/git ")
        || lower.ends_with("/git")
        || lower.contains("/git\"")
        || lower.contains("/git'")
}

/// True when an ACP permission option is persistent ("don't ask again" / prefix amendment / always).
pub fn is_persistent_permission_kind(kind: &str) -> bool {
    matches!(kind, "allow_always" | "reject_always")
}

/// Drop persistent options so the UI only offers allow-once and reject for worker git (C2).
pub fn strip_persistent_permission_options(options: &mut Value) {
    let Some(arr) = options.as_array_mut() else {
        return;
    };
    arr.retain(|o| {
        let kind = o.get("kind").and_then(Value::as_str).unwrap_or("");
        !is_persistent_permission_kind(kind)
    });
}

/// G3: worker-git offered set is exactly allow_once + reject_once kinds (unknown kinds dropped).
pub fn restrict_worker_git_options(options: &mut Value) {
    let Some(arr) = options.as_array_mut() else {
        return;
    };
    arr.retain(|o| {
        matches!(
            o.get("kind").and_then(Value::as_str).unwrap_or(""),
            "allow_once" | "reject_once"
        )
    });
}

/// True when membership's role is non-standing (a worker). Fail closed: if the role cannot be
/// resolved, treat as worker so the floor / always-approve refusal still apply (S1).
fn membership_role_is_worker(tdb: &TeamsDb, member: &super::Member) -> bool {
    match tdb.role(&member.role_id) {
        Ok(role) => !role.standing,
        Err(_) => true,
    }
}

/// Active-membership worker check only (no launch fact, no DB-absent short-circuit).
/// `Ok(None)` = no active membership; `Ok(Some(bool))` = worker or standing; `Err` = DB/query error.
fn active_membership_is_worker(tdb: &TeamsDb, session: &str) -> anyhow::Result<Option<bool>> {
    match tdb.membership(session)? {
        None => Ok(None),
        Some((_team, member)) => Ok(Some(membership_role_is_worker(tdb, &member))),
    }
}

/// Daemon-lifetime set of session ids this process attached or dispatched as workers (U1/X1).
/// OR'd into every [`worker_floor_applies`] branch so an attach-only worker whose `teams.db` is
/// replaced by a valid empty DB still floors. Cleared for a session only on a **non-worker
/// relaunch** of that session id ([`forget_worker_session`] from spawn); not on release/stop/detach.
/// Otherwise the entry lives until daemon exit.
fn known_worker_sessions() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Record that this daemon made `session` a worker (attach / dispatch / worker-seat spawn).
pub fn remember_worker_session(session: &str) {
    if let Ok(mut g) = known_worker_sessions().lock() {
        g.insert(session.to_owned());
    }
}

/// Drop the daemon-memory worker mark. Call only on a non-worker relaunch of that session id
/// (see [`crate::code::agents::maybe_scrub_worker_git_env`]) — not on release, stop, or detach.
pub fn forget_worker_session(session: &str) {
    if let Ok(mut g) = known_worker_sessions().lock() {
        g.remove(session);
    }
}

/// True when this daemon has attached/dispatched `session` as a worker since start.
pub fn daemon_knows_worker_session(session: &str) -> bool {
    known_worker_sessions()
        .lock()
        .map(|g| g.contains(session))
        .unwrap_or(false)
}

pub fn daemon_known_worker_sessions() -> Vec<String> {
    known_worker_sessions().lock().map(|g| g.iter().cloned().collect()).unwrap_or_default()
}

#[cfg(test)]
pub fn clear_known_worker_sessions_for_test() {
    if let Ok(mut g) = known_worker_sessions().lock() {
        g.clear();
    }
}

/// Whether the worker-git floor / always-approve refusal applies to `session` (X1/U1/T1/T2/S1).
///
/// `process_launched_as_worker`: immutable per ACP connection — true when this process was
/// spawned while filling a worker seat. Needs no DB; cannot be cleared by release/stop/unlink.
///
/// Combined rule (any one ⇒ floor):
/// - process flag
/// - [`daemon_knows_worker_session`] (attach/dispatch memory — every branch, not only NotFound)
/// - DB metadata/open/query error (fail closed)
/// - active non-standing membership
/// - persisted `launched_as_worker`
///
/// DB NotFound with none of the above ⇒ non-team (ordinary rules).
/// Worker status comes from [`Role::standing`] (non-standing = worker), not the seat key string.
pub fn worker_floor_applies(data_dir: &Path, session: &str, process_launched_as_worker: bool) -> bool {
    if process_launched_as_worker || daemon_knows_worker_session(session) {
        return true;
    }
    let path = data_dir.join("teams.db");
    match std::fs::metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return false; // absent + not process/daemon-known ⇒ non-team
        }
        Err(_) => return true, // metadata error — fail closed
        Ok(meta) if !meta.is_file() => return true,
        Ok(_) => {}
    }
    let tdb = match TeamsDb::open(data_dir) {
        Ok(t) => t,
        Err(_) => return true, // present but unreadable/corrupt
    };
    match active_membership_is_worker(&tdb, session) {
        Err(_) => true,
        Ok(Some(is_worker)) => is_worker,
        Ok(None) => match tdb.launched_as_worker(session) {
            Ok(launched) => launched,
            Err(_) => true,
        },
    }
}

/// DB / daemon-memory worker check with no process-launch flag (mode setter, attach gates).
pub fn is_worker_seat_member(data_dir: &Path, session: &str) -> bool {
    worker_floor_applies(data_dir, session, false)
}

/// Prompt-floor classifier (G1/U1): a team worker execute that names git or cannot be classified
/// must always ask — before saved rules and before mode policy. A command-less execute is exempt
/// only when [`matched_tool`] positively matches a Patchbay team tool for `agent` (same matcher
/// [`allow_team_tool`] uses); the floor still runs before `allow_team_tool`.
pub fn worker_git_floor_must_ask(
    data_dir: &Path,
    session: &str,
    tool_call: &Value,
    process_launched_as_worker: bool,
    agent: &str,
) -> bool {
    if tool_call["kind"].as_str() != Some("execute") {
        return false;
    }
    if !worker_floor_applies(data_dir, session, process_launched_as_worker) {
        return false;
    }
    // Positively matched team tool (exact Codex MCP shape + injected server) ⇒ exempt.
    if matched_tool(data_dir, agent, session, tool_call).is_some() {
        return false;
    }
    match command_of(tool_call) {
        None => true,
        Some(cmd) => {
            let raw = &tool_call["rawInput"];
            // Advisor a: MCP-shaped attempts that failed the exact match still ask — even when
            // the extra command/cmd is non-git (otherwise a saved execute rule could approve).
            if raw.get("server").is_some() || raw.get("tool").is_some() || raw.get("arguments").is_some()
            {
                return true;
            }
            command_may_invoke_git(&cmd)
        }
    }
}

/// Remove every agent-supplied Patchbay-reserved field from one object (tool call or request).
/// Clears top-level `patchbay*` / `workerGitPrompt` and `_meta.patchbay` (any casing of the meta key).
fn strip_patchbay_keys(obj: &mut serde_json::Map<String, Value>) {
    let keys: Vec<String> = obj
        .keys()
        .filter(|k| {
            k.starts_with("patchbay")
                || k.eq_ignore_ascii_case("workerGitPrompt")
                || *k == "patchbayCommand"
                || *k == "patchbayCwd"
        })
        .cloned()
        .collect();
    for k in keys {
        obj.remove(&k);
    }
    if let Some(meta) = obj.get_mut("_meta").and_then(|m| m.as_object_mut()) {
        let meta_keys: Vec<String> = meta
            .keys()
            .filter(|k| k.eq_ignore_ascii_case("patchbay"))
            .cloned()
            .collect();
        for k in meta_keys {
            meta.remove(&k);
        }
    }
}

/// Remove every agent-supplied Patchbay-reserved field (G2 / S2).
/// Clears top-level `patchbay*` / `workerGitPrompt` and `_meta.patchbay` on the value itself and,
/// when present, on nested `toolCall` (permission-request shape). Call on every incoming agent
/// `tool_call` / `tool_call_update` and on every permission request before cache / fan-out / enrich.
pub fn strip_agent_patchbay_fields(raw: &mut Value) {
    if let Some(obj) = raw.as_object_mut() {
        strip_patchbay_keys(obj);
    }
    if let Some(tc) = raw.get_mut("toolCall").and_then(|t| t.as_object_mut()) {
        strip_patchbay_keys(tc);
    }
}

/// First failing reason why `cmd` is not an exact worker add/commit template (G4).
fn diagnose_worker_git_template(
    cmd: &str,
    cwd: &str,
    worktree: Option<&Path>,
    hooks_path: Option<&Path>,
) -> (bool, String, Option<String>, Option<&'static str>) {
    let Some(tokens) = tokenize_worker_command(cmd) else {
        return (
            false,
            "NOT the worker template".into(),
            Some("charset or unclassifiable argv".into()),
            None,
        );
    };
    let Some(prog) = tokens.first().map(String::as_str) else {
        return (
            false,
            "NOT the worker template".into(),
            Some("unclassifiable argv".into()),
            None,
        );
    };
    if prog == "git" || prog == "git.exe" {
        return (
            false,
            "NOT the worker template".into(),
            Some("bare git".into()),
            None,
        );
    }
    if !is_resolved_git_prog(prog) {
        return (
            false,
            "NOT the worker template".into(),
            Some("not the resolved git binary".into()),
            None,
        );
    }
    let Some(hooks) = hooks_path else {
        return (
            false,
            "NOT the worker template".into(),
            Some("no team hooks path".into()),
            None,
        );
    };
    let Some(verb_rest) = split_git_hardening(&tokens, hooks) else {
        return (
            false,
            "NOT the worker template".into(),
            Some("extra -c or missing hardening prefix".into()),
            None,
        );
    };
    let verb = verb_rest.first().map(String::as_str).unwrap_or("");
    if !matches!(verb, "add" | "commit") {
        return (
            false,
            "NOT the worker template".into(),
            Some(format!("verb {verb:?} (not add|commit)")),
            None,
        );
    }
    let Some(wt) = worktree else {
        return (
            false,
            "NOT the worker template".into(),
            Some("no assigned worktree".into()),
            None,
        );
    };
    if cwd.is_empty() || !paths_equal(Path::new(cwd), wt) {
        return (
            false,
            "NOT the worker template".into(),
            Some("cwd not the worktree".into()),
            None,
        );
    }
    if !parse_git_verb(verb_rest, wt) {
        let reason = if verb == "add" && !verb_rest.iter().any(|t| t == "--") {
            "missing --"
        } else {
            "template shape (options/pathspecs)"
        };
        return (
            false,
            "NOT the worker template".into(),
            Some(reason.into()),
            None,
        );
    }
    let v = if verb == "add" { "add" } else { "commit" };
    (
        true,
        format!("exact worker template ({v})"),
        None,
        Some(v),
    )
}

/// When a live worker's execute may invoke git and is about to prompt: host-controlled exact
/// command + hooks/config warning; strip persistent/unknown options (C2/G3); templateMatch (G4).
pub fn enrich_worker_git_permission(data_dir: &Path, session: &str, raw: &mut Value) {
    // G2: never trust agent-supplied Patchbay fields on any permission request reaching enrich.
    strip_agent_patchbay_fields(raw);
    let tool_call = &raw["toolCall"];
    if tool_call["kind"].as_str() != Some("execute") {
        return;
    }
    if !is_worker_seat_member(data_dir, session) {
        return;
    }
    let raw_cmd = &tool_call["rawInput"]["command"];
    // S3: an array command is never "exact worker template" — space-joining can make a spaced
    // element look like several tokens. Label arrays non-template / unclassifiable for the badge.
    let (string_cmd, command_argv, unclassifiable, array_command) = match raw_cmd {
        Value::Array(parts) => {
            let argv: Vec<String> = parts.iter().filter_map(Value::as_str).map(str::to_owned).collect();
            if argv.is_empty() {
                (None, None, true, true)
            } else {
                (None, Some(argv), false, true)
            }
        }
        Value::String(s) => (Some(s.clone()), None, false, false),
        _ => match command_of(tool_call) {
            Some(s) => (Some(s), None, false, false),
            None => (None, None, true, false),
        },
    };
    // String commands classify as themselves; arrays join only for the may-invoke-git floor gate
    // (never for the exact-template badge — see array_command / S3).
    let classify_cmd = match (&string_cmd, &command_argv) {
        (_, Some(argv)) => argv.join(" "),
        (Some(s), None) => s.clone(),
        (None, None) => String::new(),
    };
    if !unclassifiable && classify_cmd.is_empty() {
        return;
    }
    if !unclassifiable && !command_may_invoke_git(&classify_cmd) {
        return;
    }
    let Ok(tdb) = TeamsDb::open(data_dir) else {
        return;
    };
    let Ok(Some((team, member))) = tdb.membership(session) else {
        return;
    };
    let display_cwd = tool_call["rawInput"]["cwd"].as_str().unwrap_or("").to_owned();
    let worktree = member.item_id.as_ref().and_then(|id| {
        team.effective_worktree_root()
            .ok()
            .map(|root| root.join(id.to_lowercase()))
    });
    let hooks_path = worker_safe_hooks_dir(data_dir, &team.id);
    let (tmpl_ok, tmpl_label, tmpl_reason, _verb) = if unclassifiable {
        (
            false,
            "NOT the worker template".into(),
            Some("unclassifiable argv".into()),
            None,
        )
    } else if array_command {
        // S3: simpler choice — never claim exact template for argv arrays.
        (
            false,
            "NOT the worker template".into(),
            Some("argv array (not classified as exact template)".into()),
            None,
        )
    } else {
        diagnose_worker_git_template(
            &classify_cmd,
            &display_cwd,
            worktree.as_deref(),
            Some(hooks_path.as_path()),
        )
    };
    let host_command = if let Some(argv) = &command_argv {
        // Never silent space-join for display: show argv list form.
        format!("argv: [{}]", argv.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(", "))
    } else if unclassifiable {
        "unclassifiable command".to_owned()
    } else {
        string_cmd.clone().unwrap_or_else(|| classify_cmd.clone())
    };
    if let Some(o) = raw.get_mut("toolCall").and_then(|t| t.as_object_mut()) {
        let meta = o.entry("_meta").or_insert_with(|| json!({}));
        if let Some(m) = meta.as_object_mut() {
            let pb = m.entry("patchbay").or_insert_with(|| json!({}));
            if let Some(p) = pb.as_object_mut() {
                p.insert("workerGitPrompt".into(), json!(true));
                p.insert("command".into(), json!(host_command.clone()));
                p.insert("cwd".into(), json!(display_cwd.clone()));
                if let Some(argv) = &command_argv {
                    p.insert("commandArgv".into(), json!(argv));
                }
                let mut tm = serde_json::Map::new();
                tm.insert("ok".into(), json!(tmpl_ok));
                tm.insert("label".into(), json!(tmpl_label));
                if let Some(r) = &tmpl_reason {
                    tm.insert("reason".into(), json!(r));
                }
                p.insert("templateMatch".into(), Value::Object(tm));
            }
        }
    }
    strip_persistent_permission_options(&mut raw["options"]);
    restrict_worker_git_options(&mut raw["options"]);
    append_hooks_prompt(raw, &host_command, &display_cwd);
    if !tmpl_ok {
        // Non-template: say hooks/config/filters run with the user's rights if approved.
        if let Some(ar) = raw.get_mut("toolCall").and_then(|t| t.get_mut("askReason")) {
            if let Some(s) = ar.as_str() {
                let extra = "NOT the worker template — if approved, repository hooks/config/filters run with your rights";
                if !s.contains("NOT the worker template") {
                    *ar = json!(format!("{s}\n{extra}"));
                }
            }
        }
    }
}

/// True when `git … add -- …` pathspecs include the fixed (or legacy) commit-message file name.
fn add_pathspec_includes_commit_msg_file(cmd: &str) -> bool {
    let Some((_, rest)) = cmd.split_once(" add -- ") else {
        return false;
    };
    rest.split_ascii_whitespace().any(|t| {
        t == WORKER_COMMIT_MSG_FILE
            || t == "PB_COMMIT_MSG"
            || t.ends_with("/.pb-commit-msg")
            || t.ends_with("/PB_COMMIT_MSG")
    })
}

fn append_hooks_prompt(raw: &mut Value, cmd: &str, cwd: &str) {
    let phrase = WORKER_GIT_HOOKS_PROMPT;
    let unsandboxed = WORKER_GIT_UNSANDBOXED_PROMPT;
    let msgfile_warn = add_pathspec_includes_commit_msg_file(cmd).then_some(WORKER_GIT_MSGFILE_IN_ADD_WARN);
    // Host-controlled askReason: exact command + cwd, hooks/config warning, escalated disclosure.
    let host = if cwd.is_empty() {
        format!("Command: {cmd}\n{phrase}\n{unsandboxed}")
    } else {
        format!("Command: {cmd}\ncwd: {cwd}\n{phrase}\n{unsandboxed}")
    };
    let host = match msgfile_warn {
        Some(w) => format!("{host}\n{w}"),
        None => host,
    };
    if let Some(o) = raw.get_mut("toolCall").and_then(|t| t.as_object_mut()) {
        let existing = o.get("askReason").and_then(Value::as_str).unwrap_or("").to_owned();
        let next = if existing.is_empty() || existing == host {
            host
        } else if existing.contains(phrase) && existing.contains(unsandboxed) && existing.contains(cmd) {
            match msgfile_warn {
                Some(w) if !existing.contains(w) => format!("{existing}\n{w}"),
                _ => existing,
            }
        } else {
            format!("{host}\n{existing}")
        };
        o.insert("askReason".into(), json!(next));
    }
    if let Some(opts) = raw.get_mut("options").and_then(Value::as_array_mut) {
        for opt in opts {
            let kind = opt.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind != "allow_once" {
                continue;
            }
            let name = opt.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
            if name.contains(phrase) && name.contains(unsandboxed) {
                continue;
            }
            let tag = format!("{phrase}; {unsandboxed}");
            let next = if name.is_empty() {
                tag
            } else if name.contains(phrase) {
                format!("{name}; {unsandboxed}")
            } else {
                format!("{name} ({tag})")
            };
            if let Some(o) = opt.as_object_mut() {
                o.insert("name".into(), json!(next));
            }
        }
    }
}

/// The option to pick and what to tell the UI, when `tool_call` is a team member's call to one of
/// our tools, in the form `agent` (the session's own) uses — or a worktree-scoped worker git
/// command when the template opts in. `session_cwd` is the Code session's working directory.
pub fn allow_team_tool(
    data_dir: &Path,
    session: &str,
    agent: &str,
    tool_call: &Value,
    options: &Value,
    session_cwd: &str,
) -> Option<(String, String)> {
    if let Some(tool) = matched_tool(data_dir, agent, session, tool_call) {
        // No teams.db means no teams; don't create one just to look.
        if !data_dir.join("teams.db").is_file() {
            return None;
        }
        TeamsDb::open(data_dir).ok()?.membership(session).ok()??;
        return Some((allow_option(options)?, format!("Auto-approved: team tool {tool}")));
    }
    allow_worker_commit(data_dir, session, agent, tool_call, options, session_cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Stable injected MCP name for unit tests (stands in for per-session nonce).
    const INJ: &str = "patchbay_team_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn inject(session: &str) {
        clear_all_team_tool_exemption_revocations_for_test();
        clear_team_tool_exemption_revocation(session);
        crate::agent_messaging::set_injected_mcp_server_name_for_test(session, INJ);
    }

    fn call(title: &str, kind: &str, raw: Value) -> Value {
        json!({ "title": title, "kind": kind, "rawInput": raw })
    }

    fn codex_mcp(tool: &str, args: Value) -> Value {
        json!({
            "title": format!("mcp.{INJ}.{tool}"),
            "kind": "execute",
            "rawInput": { "server": INJ, "tool": tool, "arguments": args }
        })
    }

    fn claude_mcp(tool: &str, raw: Value) -> Value {
        json!({
            "title": format!("mcp__{INJ}__{tool}"),
            "kind": "other",
            "rawInput": raw
        })
    }

    fn options() -> Value {
        json!([{ "kind": "allow_always", "optionId": "always" }, { "kind": "allow_once", "optionId": "once" }])
    }

    /// Tiny teams.db + one attached member on `seat` (session id `sid`).
    fn member_fixture(seat: &str, sid: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-allow-mem-{}", crate::db::new_id()));
        std::fs::create_dir_all(d.join("proj")).unwrap();
        let t = TeamsDb::open(&d).unwrap();
        let team = t
            .create_team(super::super::NewTeam {
                template_id: "builtin-triage-desk".into(),
                slug: format!("m{}", &crate::db::new_id()[..8]),
                cwd: d.join("proj").to_string_lossy().into_owned(),
                id_prefix: None,
                pr_base: None,
                worktree_root: None,
                project_rules: None,
                export_dir: Some(d.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        t.attach(&team.id, sid, seat).unwrap();
        inject(sid);
        d
    }

    #[test]
    fn each_agents_title_form_matches_and_nothing_else_does() {
        let d = member_fixture("coordinator", "t");
        for (agent, title, kind, raw) in [
            ("claude-code", format!("mcp__{INJ}__send_agent_message"), "other", json!({ "message": "hi" })),
            ("codex", format!("mcp.{INJ}.send_agent_message"), "execute", json!({ "server": INJ, "tool": "send_agent_message", "arguments": { "message": "hi" } })),
            ("antigravity", format!("{INJ}_item_set"), "other", json!({ "arguments": { "itemId": "x" } })),
            ("cursor", format!("{INJ}-send_agent_message: send_agent_message"), "other", Value::Null),
            ("cursor", format!("{INJ}: dispatch_item"), "other", Value::Null),
            ("patchbay", format!("send_agent_message ({INJ})"), "other", json!({})),
            ("claude-code", format!("mcp__{INJ}__member_release"), "other", json!({})),
        ] {
            assert!(matched_tool(&d, agent, "t", &call(&title, kind, raw)).is_some(), "{title}");
        }
        let claude = claude_mcp("send_agent_message", json!({}));
        for other in ["codex", "cursor", "antigravity", "patchbay", "opencode", "grok", "kilo"] {
            assert!(matched_tool(&d, other, "t", &claude).is_none(), "{other}");
        }
        assert!(matched_tool(&d, "claude-code", "t", &call(&format!("{INJ}: send_agent_message"), "other", json!({}))).is_none());
        for (title, kind, raw) in [
            (format!("mcp__{INJ}__create_agent_session"), "other", json!({})),
            (format!("mcp__{INJ}__share_artifact"), "other", json!({})),
            (format!("mcp__{INJ}__list_agent_sessions"), "other", json!({})),
            ("mcp__other-server__send_agent_message".into(), "other", json!({})),
            (format!("mcp__{INJ}__send_agent_message"), "execute", json!({})),
            (format!("mcp.{INJ}.send_agent_message"), "execute", json!({ "command": "x" })),
            (format!("mcp.{INJ}.send_agent_message"), "execute", Value::Null),
            (format!("mcp__{INJ}__send_agent_message"), "other", json!({ "cwd": "/" })),
            (format!("mcp__{INJ}__item_set"), "other", json!({ "arguments": { "path": "/etc" } })),
            ("cargo test".into(), "execute", json!({ "command": "cargo test" })),
        ] {
            assert!(matched_tool(&d, "claude-code", "t", &call(&title, kind, raw)).is_none(), "{title}");
        }
        // Non-Codex agent sending Codex execute shape ⇒ no match.
        assert!(
            matched_tool(&d, "claude-code", "t", &codex_mcp("item_note", json!({ "itemId": "i", "text": "x" }))).is_none(),
            "Claude must not match Codex execute shape"
        );
        // OpenCode never matches.
        assert!(matched_tool(&d, "opencode", "t", &codex_mcp("item_note", json!({ "itemId": "i", "text": "x" }))).is_none());
    }

    #[test]
    fn native_worker_team_tool_exemption_uses_only_its_nonce_and_role_caps() {
        let session = format!("native-worker-{}", crate::db::new_id());
        let d = member_fixture("workers", &session);
        inject(&session);
        let roster = |server: &str| call(&format!("team_roster ({server})"), "other", json!({}));
        assert_eq!(matched_tool(&d, "patchbay", &session, &roster(INJ)), Some("team_roster"));
        let other = "patchbay_team_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert!(matched_tool(&d, "patchbay", &session, &roster(other)).is_none());
        crate::agent_messaging::set_injected_mcp_server_name_for_test(&session, "patchbay-agent-messaging");
        assert!(matched_tool(&d, "patchbay", &session, &roster("patchbay-agent-messaging")).is_none());
        inject(&session);
        assert!(matched_tool(&d, "patchbay", &session, &call(&format!("dispatch_item ({INJ})"), "other", json!({}))).is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn only_live_team_members_get_it_and_only_once() {
        let d = std::env::temp_dir().join(format!("pb-allow-{}", crate::db::new_id()));
        std::fs::create_dir_all(d.join("proj")).unwrap();
        inject("s1");
        let tc = claude_mcp("team_roster", json!({}));
        let options = options();
        assert!(allow_team_tool(&d, "s1", "claude-code", &tc, &options, "/tmp").is_none(), "no teams.db yet");
        let t = TeamsDb::open(&d).unwrap();
        let team = t
            .create_team(super::super::NewTeam {
                template_id: "builtin-triage-desk".into(),
                slug: "t".into(),
                cwd: d.join("proj").to_string_lossy().into_owned(),
                id_prefix: None,
                pr_base: None,
                worktree_root: None,
                project_rules: None,
                export_dir: Some(d.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        assert!(allow_team_tool(&d, "s1", "claude-code", &tc, &options, "/tmp").is_none(), "not a member");
        t.attach(&team.id, "s1", "coordinator").unwrap();
        assert_eq!(allow_team_tool(&d, "s1", "claude-code", &tc, &options, "/tmp").unwrap().0, "once");
        assert!(allow_team_tool(&d, "s1", "claude-code", &tc, &json!([{ "kind": "allow_always", "optionId": "a" }]), "/tmp").is_none(), "allow-once only");
        assert!(allow_team_tool(&d, "s1", "cursor", &tc, &options, "/tmp").is_none(), "wrong agent's form");
        t.detach(&team.id, "s1").unwrap();
        assert!(allow_team_tool(&d, "s1", "claude-code", &tc, &options, "/tmp").is_none(), "released");
    }

    /// Fixture data_dir outside `/tmp` / `$TMPDIR` (R1a refuses temp data_dir). Prefer
    /// `CARGO_TARGET_DIR` like F2's `f2_non_temp_fixture`.
    fn allow_non_temp_data_dir(tag: &str) -> PathBuf {
        let base = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
        let d = base.join("allow-fixtures").join(format!("{tag}-{}", crate::db::new_id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        if let (Some(key), Some(roots)) = (path_key(&d), temp_writable_roots()) {
            for r in &roots {
                assert!(
                    !path_under(&key, r),
                    "allow fixture must not sit under temp root {r:?} (got {key:?})"
                );
            }
        }
        d
    }

    fn worker_commit_fixture(opt_in: bool) -> (std::path::PathBuf, String, String, String, PathBuf) {
        let d = allow_non_temp_data_dir("git");
        let proj = d.join("proj");
        let trees = proj.join("trees");
        std::fs::create_dir_all(&trees).unwrap();
        let t = TeamsDb::open(&d).unwrap();
        let builtin = t.template("builtin-triage-desk").unwrap();
        let tpl = t
            .save_template(super::super::TemplateInput {
                id: None,
                name: "commit-opt".into(),
                seats: builtin.seats,
                routing: builtin.routing,
                allow_worker_commit: opt_in,
            })
            .unwrap();
        let team = t
            .create_team(super::super::NewTeam {
                template_id: tpl.id,
                slug: "g".into(),
                cwd: proj.to_string_lossy().into_owned(),
                id_prefix: Some("G".into()),
                pr_base: None,
                worktree_root: Some(trees.to_string_lossy().into_owned()),
                project_rules: None,
                export_dir: Some(d.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        let hooks = ensure_worker_safe_hooks_dir(&d, &team.id).unwrap();
        let item_id = "G-20261112-01";
        let worktree = trees.join(item_id.to_lowercase());
        // Linked worktree: main repo at proj so .git/config, gitdir, info/attributes sit outside
        // the Codex workspace-write set (worktree cwd + temp) — required for git auto-approve.
        assert!(
            std::process::Command::new("git")
                .args(["-c", "init.defaultBranch=main", "init", "-q"])
                .current_dir(&proj)
                .status()
                .unwrap()
                .success()
        );
        for args in [
            ["-C", proj.to_str().unwrap(), "config", "user.name", "t"] as [&str; 5],
            ["-C", proj.to_str().unwrap(), "config", "user.email", "t@t"],
        ] {
            assert!(std::process::Command::new("git").args(args).status().unwrap().success());
        }
        let empty_hooks = proj.join(".pb-test-hooks-empty");
        std::fs::create_dir_all(&empty_hooks).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", proj.to_str().unwrap(), "config", "core.hooksPath", empty_hooks.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(proj.join("README"), "base").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", proj.to_str().unwrap(), "add", "README"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["-C", proj.to_str().unwrap(), "commit", "-q", "--no-verify", "-m", "base"])
                .status()
                .unwrap()
                .success()
        );
        let branch = format!("worker/{}", item_id.to_lowercase());
        assert!(
            std::process::Command::new("git")
                .args([
                    "-C",
                    proj.to_str().unwrap(),
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    worktree.to_str().unwrap(),
                    "HEAD",
                ])
                .status()
                .unwrap()
                .success()
        );
        t.attach(&team.id, "w1", "workers").unwrap();
        t.0.execute("UPDATE members SET item_id = ?1 WHERE session_id = 'w1'", [item_id]).unwrap();
        // Tests model a dispatch-spawned worker (F-E launched-scrubbed fact + R2-1 handshake version).
        t.mark_git_env_scrubbed("w1").unwrap();
        t.mark_acp_runtime_version("w1", "2.2.2").unwrap();
        (d, worktree.to_string_lossy().into_owned(), proj.to_string_lossy().into_owned(), item_id.to_owned(), hooks)
    }

    /// Absolute git binary token used by the matcher / brief (never bare `git`).
    /// Call [`skip_without_clean_git`] first on hosts where PATH git is unclean (e.g. Windows
    /// `Program Files`) — product is fail-closed there; those tests return early.
    fn git_abs() -> String {
        resolved_worker_git_token().expect("git on PATH with clean-string path for tests")
    }

    /// Skip when the host git path is not a clean-string absolute token (P3 / Windows).
    fn skip_without_clean_git() -> bool {
        if resolved_worker_git_token().is_none() {
            eprintln!("skip: no clean-string absolute git on PATH (fail-closed product path)");
            true
        } else {
            false
        }
    }

    /// Exact auto-approve prefix for git verbs (absolute git + hooks path from fixture).
    fn git_pfx(hooks: &Path) -> String {
        format!(
            "{} --no-pager -c {} -c core.fsmonitor=false -c core.sshCommand= -c protocol.ext.allow=never -c commit.gpgSign=false -c core.editor=: -c credential.helper= -c core.askPass=",
            git_abs(),
            hooks_path_c_arg(hooks)
        )
    }

    /// Agent eligible for git auto-approve in tests (Codex + proven non-persist).
    const GIT_AGENT: &str = "codex";

    fn git_cmd(hooks: &Path, rest: &str) -> String {
        format!("{} {rest}", git_pfx(hooks))
    }

    /// Write the relative `-F` message file required by the template shape.
    fn write_commit_msg(wt: &str, body: &str) {
        std::fs::write(Path::new(wt).join("PB_COMMIT_MSG"), body).unwrap();
    }

    fn commit_f(hooks: &Path) -> String {
        git_cmd(hooks, "commit --no-verify -F PB_COMMIT_MSG")
    }

    /// Direct gate-stack check (must stay live while [`WORKER_GIT_WRITE_AUTO_APPROVE`] is false).
    fn gates_pass(d: &Path, session: &str, agent: &str, tc: &Value, wt: &str, hooks: &Path) -> bool {
        let tdb = TeamsDb::open(d).unwrap();
        worker_git_auto_approve_gates(d, session, agent, tc, Path::new(wt), hooks, &tdb)
    }

    /// #449 floor: worker git never auto-approves via [`allow_team_tool`].
    fn assert_git_floor_prompts(d: &Path, session: &str, agent: &str, tc: &Value, opts: &Value, cwd: &str) {
        assert!(
            allow_team_tool(d, session, agent, tc, opts, cwd).is_none(),
            "C floor: worker git must prompt (WORKER_GIT_WRITE_AUTO_APPROVE=false); cmd={tc}"
        );
    }

    fn git_exec(cwd: &str, command: &str) -> Value {
        json!({ "title": command, "kind": "execute", "rawInput": { "command": command, "cwd": cwd } })
    }

    fn git_exec_no_cwd(command: &str) -> Value {
        json!({ "title": command, "kind": "execute", "rawInput": { "command": command } })
    }

    fn git_exec_empty_cwd(command: &str) -> Value {
        json!({ "title": command, "kind": "execute", "rawInput": { "command": command, "cwd": "" } })
    }

    #[test]
    fn worker_commit_inside_own_worktree_when_template_opts_in() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        write_commit_msg(&wt, "work");
        let opts = options();
        // #449 floor: allow_team_tool never auto-approves git; gates still pass for A PR.
        let commit_tc = git_exec(&wt, &commit_f(&safe));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &commit_tc, &opts, &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe), "gates must pass for valid commit");
        let add_tc = git_exec(&wt, &git_cmd(&safe, "add -- LICENSE"));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &add_tc, &opts, &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &add_tc, &wt, &safe));
        let status_tc = git_exec(&wt, &git_cmd(&safe, "status"));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &status_tc, &opts, &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &status_tc, &wt, &safe));
        let diff_tc = git_exec(&wt, &git_cmd(&safe, "diff --no-ext-diff --no-textconv"));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &diff_tc, &opts, &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &diff_tc, &wt, &safe));
        write_commit_msg(&wt, "hello world");
        let commit2 = git_exec(&wt, &commit_f(&safe));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &commit2, &opts, &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &commit2, &wt, &safe));
        // Bare `git` (even with full hardening) and -m shapes never pass gates.
        let bare_full = format!(
            "git --no-pager -c {} -c core.fsmonitor=false -c core.sshCommand= -c protocol.ext.allow=never -c commit.gpgSign=false -c core.editor=: -c credential.helper= -c core.askPass= commit --no-verify -F PB_COMMIT_MSG",
            hooks_path_c_arg(&safe)
        );
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &git_exec(&wt, &bare_full), &opts, &wt);
        assert!(!gates_pass(&d, "w1", GIT_AGENT, &git_exec(&wt, &bare_full), &wt, &safe));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &git_exec(&wt, "git commit --no-verify -m work"), &opts, &wt);
        let dash_m = git_cmd(&safe, "commit --no-verify -m work");
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &git_exec(&wt, &dash_m), &opts, &wt);
        // `-m` / missing `--` are refused by is_safe_worker_command (before the gate stack).
        assert!(
            !is_safe_worker_command(&dash_m, Path::new(&wt), &safe),
            "-m must fail the safe-command matcher"
        );
        let no_ddash = git_cmd(&safe, "add LICENSE");
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &git_exec(&wt, &no_ddash), &opts, &wt);
        assert!(
            !is_safe_worker_command(&no_ddash, Path::new(&wt), &safe),
            "pathspec without -- must fail the safe-command matcher"
        );
    }

    #[test]
    fn worker_readonly_ls_cat_git_log_accepted_when_paths_stay_inside() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let root = std::path::Path::new(&wt);
        std::fs::write(root.join("README"), "hi").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let accept_plain = ["ls", "ls src", "ls -la src", "cat README"];
        for cmd in accept_plain {
            assert!(
                allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, cmd), &options(), &wt).is_some(),
                "should allow: {cmd}"
            );
        }
        let opts = options();
        for rest in [
            "log --no-ext-diff --no-textconv",
            "log -1 --no-ext-diff --no-textconv",
            "status",
            "diff --no-ext-diff --no-textconv",
            "status -s -b",
        ] {
            let cmd = git_cmd(&safe, rest);
            let tc = git_exec(&wt, &cmd);
            // #449: read-only git also prompts (Codex permission ask = escalation).
            assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &opts, &wt);
            assert!(gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe), "gates must pass: {cmd}");
        }
        // Missing either flag → safe-command matcher refuses; floor still prompts.
        for rest in [
            "diff",
            "diff --no-ext-diff",
            "diff --no-textconv",
            "log",
            "log --no-ext-diff",
            "log --no-textconv",
        ] {
            let cmd = git_cmd(&safe, rest);
            let tc = git_exec(&wt, &cmd);
            assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &opts, &wt);
            assert!(
                !is_safe_worker_command(&cmd, Path::new(&wt), &safe),
                "safe-command must refuse without both flags: {cmd}"
            );
        }
    }

    #[test]
    fn worker_shell_metachar_and_message_file_bypasses_rejected() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, proj, _item, safe) = worker_commit_fixture(true);
        let root = std::path::Path::new(&wt);
        std::fs::write(root.join("README"), "hi").unwrap();
        std::fs::write(std::path::Path::new(&proj).join("outside.txt"), "secret").unwrap();
        let refuse = [
            // Verifier-listed bypasses (whitespace-split was not what the shell sees).
            r#"cat "../outside.txt""#,
            r#"cat "$HOME/.ssh/id_rsa""#,
            r#"cat ${HOME}/.ssh/id_rsa"#,
            r#"cat ~/.ssh/id_rsa"#,
            "git commit -F /etc/passwd",
            "git commit --template=/etc/passwd",
            // Related escapes / options.
            "cat ../../outside.txt",
            "cat /etc/passwd",
            "cat -",
            "cat",
            "ls ..",
            "ls /tmp",
            "git commit -m x --no-verify",
            "git commit -m x -q",
            "git add -f README",
            "git add --pathspec-from-file=x",
            "git log --output=/tmp/out",
            "git log --ext-diff",
            "FOO=bar git status",
            "git -c core.hooksPath=/tmp commit -m x",
            "git commit -m \"hi$USER\"",
            "git commit -m 'hi`id`'",
        ];
        for cmd in refuse {
            assert!(
                allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, cmd), &options(), &wt).is_none(),
                "should refuse: {cmd}"
            );
        }
        // Wrong hooksPath value with otherwise-valid sequence still refuses.
        let wrong = format!(
            "{} --no-pager -c core.hooksPath=/tmp/evil -c core.fsmonitor=false -c core.sshCommand= -c protocol.ext.allow=never -c commit.gpgSign=false -c core.editor=: -c credential.helper= -c core.askPass= commit --no-verify -F PB_COMMIT_MSG",
            git_abs()
        );
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &wrong), &options(), &wt).is_none());
        let _ = safe;
    }

    fn write_hook(dir: &std::path::Path, name: &str, executable: bool) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = executable;
    }

    #[test]
    fn worker_symlink_escape_and_fsmonitor_block_auto() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, proj, _item, safe) = worker_commit_fixture(true);
        let root = std::path::Path::new(&wt);
        std::fs::write(root.join("README"), "hi").unwrap();
        // Symlink escape needs unix create; product path resolve refuses escapes on all OSes.
        #[cfg(unix)]
        {
            let outside = std::path::Path::new(&proj).join("outside.txt");
            std::fs::write(&outside, "secret").unwrap();
            let link = root.join("leak");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, "cat leak"), &options(), &wt).is_none());
        }
        #[cfg(not(unix))]
        let _ = proj;

        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.fsmonitor", "/tmp/fsmon"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &git_cmd(&safe, "status")), &options(), &wt).is_none(),
            "fsmonitor set"
        );
    }

        #[test]
    fn worker_verb_relevant_executable_hooks_block_not_pre_push() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        write_commit_msg(&wt, "work");
        let opts = options();
        let commit_tc = git_exec(&wt, &commit_f(&safe));
        let add_tc = git_exec(&wt, &git_cmd(&safe, "add -- LICENSE"));
        let status_tc = git_exec(&wt, &git_cmd(&safe, "status"));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &commit_tc, &opts, &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe));

        let repo_hooks = resolve_hooks_dir(std::path::Path::new(&wt)).expect("hooks dir");
        // pre-push alone (David's templateDir) does not block commit/add.
        write_hook(&repo_hooks, "pre-push", true);
        assert!(
            gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe),
            "executable pre-push alone still passes gates for commit"
        );
        assert!(gates_pass(&d, "w1", GIT_AGENT, &add_tc, &wt, &safe));

        // Non-executable commit-msg ignored on unix; on Windows any hook file fails closed
        // (`is_executable_file` treats existing files as executable — no reliable exec bit).
        write_hook(&repo_hooks, "commit-msg", false);
        #[cfg(unix)]
        assert!(gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe));
        #[cfg(not(unix))]
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe),
            "Windows: hook file present fails closed"
        );
        write_hook(&repo_hooks, "commit-msg", true);
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe),
            "executable commit-msg refuses gates"
        );
        // add is not blocked by commit-msg.
        assert!(gates_pass(&d, "w1", GIT_AGENT, &add_tc, &wt, &safe));
        std::fs::remove_file(repo_hooks.join("commit-msg")).unwrap();
        write_hook(&repo_hooks, "pre-auto-gc", true);
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe),
            "executable pre-auto-gc refuses (commit may run auto-maintenance)"
        );
        std::fs::remove_file(repo_hooks.join("pre-auto-gc")).unwrap();
        assert!(gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe));

        // Main .git/hooks with pre-commit after unsetting hooksPath.
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "--unset", "core.hooksPath"])
                .status()
                .unwrap()
                .success()
        );
        let main_hooks = {
            let rel = hardened_git(std::path::Path::new(&wt), &["rev-parse", "--git-path", "hooks"]).unwrap();
            let p = std::path::PathBuf::from(&rel);
            if p.is_absolute() { p } else { std::path::Path::new(&wt).join(p) }
        };
        if main_hooks.is_dir() {
            for e in std::fs::read_dir(&main_hooks).unwrap().flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        write_hook(&main_hooks, "pre-push", true);
        assert!(
            gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe),
            "pre-push in .git/hooks does not block"
        );
        write_hook(&main_hooks, "pre-commit", true);
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe),
            "executable pre-commit in .git/hooks"
        );

        // External hooksPath with commit-msg → refuse; with only pre-push → allow.
        let external = std::path::Path::new(&wt).join("external-hooks");
        write_hook(&external, "pre-push", true);
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.hooksPath", external.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe));
        write_hook(&external, "commit-msg", true);
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe),
            "external hooksPath commit-msg"
        );

        // post-index-change blocks add and status.
        std::fs::remove_file(external.join("commit-msg")).unwrap();
        write_hook(&external, "post-index-change", true);
        assert!(!gates_pass(&d, "w1", GIT_AGENT, &add_tc, &wt, &safe));
        assert!(!gates_pass(&d, "w1", GIT_AGENT, &status_tc, &wt, &safe));

        // Empty / missing hooks dir → gates pass (floor still prompts).
        let empty2 = std::path::Path::new(&wt).join("empty-hooks-2");
        std::fs::create_dir_all(&empty2).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.hooksPath", empty2.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe));
        let missing = std::path::Path::new(&wt).join("missing-hooks-dir");
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.hooksPath", missing.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &commit_tc, &opts, &wt);
    }

    #[test]
    fn worker_commit_prompts_when_opt_in_is_off_by_default() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(false);
        assert!(allow_team_tool(
            &d,
            "w1",
            GIT_AGENT,
            &git_exec(&wt, &commit_f(&safe)),
            &options(),
            &wt
        )
        .is_none());
    }

    #[test]
    fn worker_commit_outside_worktree_or_forbidden_git_prompts() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, proj, _item, safe) = worker_commit_fixture(true);
        let commit = commit_f(&safe);
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&proj, &commit), &options(), &proj).is_none());
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec("/", &commit), &options(), "/").is_none());
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &git_cmd(&safe, "push origin HEAD")), &options(), &wt).is_none());
        assert!(allow_team_tool(
            &d,
            "w1",
            GIT_AGENT,
            &git_exec(&wt, &git_cmd(&safe, "commit --amend --no-verify -m x")),
            &options(),
            &wt
        )
        .is_none());
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &git_cmd(&safe, "add -f secret")), &options(), &wt).is_none());
    }

    #[test]
    fn worker_git_config_and_path_overrides_are_never_auto_approved() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        write_commit_msg(&wt, "work");
        let refuse = [
            "git -c core.hooksPath=/tmp/hooks commit -m x",
            "git -c core.fsmonitor=/tmp/fsmon status",
            "git --config-env=http.extraHeader=TOKEN commit -m x",
            "git --exec-path=/tmp/evil status",
            "git --git-dir=/tmp/other.git status",
            "git --work-tree=/tmp/elsewhere commit -m x",
            "git -C /tmp/elsewhere commit -m x",
            "git --namespace=evil status",
            "FOO=bar git commit -m x",
            "git commit -m x && git status",
            "git status || true",
        ];
        for cmd in refuse {
            assert!(
                allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, cmd), &options(), &wt).is_none(),
                "should refuse: {cmd}"
            );
        }
        for rest in [
            "commit --no-verify -F PB_COMMIT_MSG",
            "add -- LICENSE",
            "status",
            "diff --no-ext-diff --no-textconv",
        ] {
            let cmd = git_cmd(&safe, rest);
            let tc = git_exec(&wt, &cmd);
            assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &options(), &wt);
            assert!(gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe), "gates must pass: {cmd}");
        }
    }

    /// Missing/empty request cwd is fail-closed even when the session cwd is the worktree.
    #[test]
    fn worker_commit_missing_cwd_refuses_even_with_session_cwd() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        let commit = commit_f(&safe);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec_no_cwd(&commit), &options(), &wt).is_none(),
            "omitted cwd must refuse"
        );
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec_empty_cwd(&commit), &options(), &wt).is_none(),
            "empty-string cwd must refuse (Cursor)"
        );
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec_empty_cwd(&git_cmd(&safe, "add -- LICENSE")), &options(), &wt).is_none(),
            "empty cwd git add must refuse"
        );
    }

    #[test]
    fn worker_commit_missing_cwd_without_session_record_refuses() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let commit = commit_f(&safe);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec_no_cwd(&commit), &options(), "").is_none(),
            "no request cwd and empty session cwd"
        );
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec_empty_cwd(&commit), &options(), "   ").is_none(),
            "whitespace-only session cwd"
        );
        let _ = wt;
    }

    #[test]
    fn worker_commit_supplied_cwd_mismatch_refuses() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, proj, _item, safe) = worker_commit_fixture(true);
        let commit = commit_f(&safe);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&proj, &commit), &options(), &wt).is_none(),
            "request cwd is project root, not worktree — even when session_cwd is worktree"
        );
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec("/tmp", &commit), &options(), &wt).is_none(),
            "request cwd elsewhere"
        );
    }

    /// PLANTED-RED: hooks + fsmonitor planted after the approval check must not run when the
    /// approved command carries the forced `-c` prefix; without those flags the marker appears.
    #[cfg(unix)]
    #[test]
    fn worker_git_planted_red_hooks_after_approve_do_not_run_with_forced_c() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let root = std::path::Path::new(&wt);
        std::fs::write(root.join("tracked.txt"), "body").unwrap();
        write_commit_msg(&wt, "planted-red");
        // Identity + initial commit so later commits can succeed.
        for args in [
            ["-C", wt.as_str(), "config", "user.name", "t"] as [&str; 5],
            ["-C", wt.as_str(), "config", "user.email", "t@t"],
        ] {
            assert!(std::process::Command::new("git").args(args).status().unwrap().success());
        }
        assert!(
            std::process::Command::new("git")
                .args([
                    "-C",
                    &wt,
                    "-c",
                    &format!("core.hooksPath={}", path_for_git_c_flag(&safe)),
                    "-c",
                    "core.fsmonitor=false",
                    "add",
                    "tracked.txt",
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args([
                    "-C",
                    &wt,
                    "-c",
                    &format!("core.hooksPath={}", path_for_git_c_flag(&safe)),
                    "-c",
                    "core.fsmonitor=false",
                    "commit",
                    "--no-verify",
                    "-m",
                    "base",
                ])
                .status()
                .unwrap()
                .success()
        );

        let marker = d.join("PLANTED_RED_MARKER");
        let _ = std::fs::remove_file(&marker);
        let approved = commit_f(&safe);
        let approved_tc = git_exec(&wt, &approved);
        // Gate check while repo hooks are still empty / clean (floor still prompts).
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &approved_tc, &options(), &wt);
        assert!(
            gates_pass(&d, "w1", GIT_AGENT, &approved_tc, &wt, &safe),
            "gates must pass before planting"
        );

        // AFTER approval: plant executable pre-commit + fsmonitor that write the marker.
        let planted_hooks = root.join(".planted-hooks");
        std::fs::create_dir_all(&planted_hooks).unwrap();
        let hook = planted_hooks.join("pre-commit");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho planted-red-hook >> {}\nexit 0\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fsmon = d.join("planted-fsmon.sh");
        std::fs::write(
            &fsmon,
            format!("#!/bin/sh\necho planted-red-fsmon >> {}\nexit 0\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&fsmon, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.hooksPath", planted_hooks.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.fsmonitor", fsmon.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );

        // Touch a file so commit has something to do.
        std::fs::write(root.join("tracked.txt"), "changed").unwrap();
        assert!(
            std::process::Command::new("git")
                .args([
                    "-C",
                    &wt,
                    "-c",
                    &format!("core.hooksPath={}", path_for_git_c_flag(&safe)),
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.sshCommand=",
                    "-c",
                    "protocol.ext.allow=never",
                    "add",
                    "tracked.txt",
                ])
                .status()
                .unwrap()
                .success()
        );

        // Run the approved hardened command — marker must NOT appear.
        let with_c = std::process::Command::new("sh")
            .arg("-c")
            .arg(&approved)
            .current_dir(&wt)
            .output()
            .unwrap();
        let with_c_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            with_c.status.code(),
            String::from_utf8_lossy(&with_c.stdout),
            String::from_utf8_lossy(&with_c.stderr)
        );
        let marker_after_c = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(
            marker_after_c.is_empty(),
            "marker must not appear with forced -c; marker={marker_after_c:?}; raw={with_c_raw}"
        );
        assert!(with_c.status.success(), "hardened commit should succeed: {with_c_raw}");

        // Same commit without forced -c — marker MUST appear (hooks/fsmonitor from config).
        let _ = std::fs::remove_file(&marker);
        std::fs::write(root.join("tracked.txt"), "changed-again").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "add", "tracked.txt"])
                .status()
                .unwrap()
                .success()
        );
        let without_c = std::process::Command::new("git")
            .args(["-C", &wt, "commit", "-m", "no-hardening"])
            .output()
            .unwrap();
        let without_c_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            without_c.status.code(),
            String::from_utf8_lossy(&without_c.stdout),
            String::from_utf8_lossy(&without_c.stderr)
        );
        let marker_after_bare = std::fs::read_to_string(&marker).unwrap_or_default();
        let doc = format!(
            "PLANTED-RED with forced -c flags raw output:\n{with_c_raw}\n\n\
             PLANTED-RED without forced -c flags raw output:\n{without_c_raw}\n\n\
             marker_after_c={marker_after_c:?}\nmarker_after_bare={marker_after_bare:?}\n"
        );
        let doc_path = d.join("pb-planted-red-raws.txt");
        std::fs::write(&doc_path, &doc).unwrap();
        eprintln!("planted-red raws path: {}\n{doc}", doc_path.display());
        assert!(
            marker_after_bare.contains("planted-red"),
            "marker must appear without forced -c; marker={marker_after_bare:?}; raw={without_c_raw}"
        );
        assert!(doc_path.is_file(), "planted-red raws written to {}", doc_path.display());

        // R1: after planting, a loosened Patchbay hooks dir must refuse auto-approval.
        std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &approved), &options(), &wt).is_none(),
            "loosened hooks mode must refuse auto-approval"
        );
        // Restore empty read-only, then replace hooks dir with a symlink → refuse.
        std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&safe);
        let decoy = d.join("hooks-decoy");
        std::fs::create_dir_all(&decoy).unwrap();
        std::os::unix::fs::symlink(&decoy, &safe).unwrap();
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &approved), &options(), &wt).is_none(),
            "symlinked hooks dir must refuse auto-approval"
        );
    }

    #[cfg(unix)]
    #[test]
    fn worker_safe_hooks_dir_ok_refuses_symlink_planted_file_mode_and_temp_data() {
        use std::os::unix::fs::PermissionsExt;
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        write_commit_msg(&wt, "x");
        let tdb = TeamsDb::open(&d).unwrap();
        let (team, _) = tdb.membership("w1").unwrap().unwrap();
        let wt_path = Path::new(&wt);
        let commit = commit_f(&safe);
        let commit_tc = git_exec(&wt, &commit);
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &commit_tc, &options(), &wt);
        assert!(
            gates_pass(&d, "w1", GIT_AGENT, &commit_tc, &wt, &safe),
            "baseline gates must pass"
        );
        assert!(worker_safe_hooks_dir_ok(&d, &team, wt_path, &safe));

        // File planted inside hooks dir.
        std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(safe.join("pre-commit"), "#!/bin/sh\n").unwrap();
        assert!(!worker_safe_hooks_dir_ok(&d, &team, wt_path, &safe));
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_none());
        std::fs::remove_file(safe.join("pre-commit")).unwrap();
        std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(worker_safe_hooks_dir_ok(&d, &team, wt_path, &safe));

        // Mode loosened (owner write).
        std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!worker_safe_hooks_dir_ok(&d, &team, wt_path, &safe));
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_none());
        std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Hooks dir replaced by symlink.
        let parent = safe.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir(&safe);
        let decoy = d.join("decoy-hooks");
        std::fs::create_dir_all(&decoy).unwrap();
        std::os::unix::fs::symlink(&decoy, &safe).unwrap();
        assert!(!worker_safe_hooks_dir_ok(&d, &team, wt_path, &safe));
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_none());
        let _ = std::fs::remove_file(&safe);
        // Restore a real safe dir for the parent-symlink case.
        let hooks = ensure_worker_safe_hooks_dir(&d, &team.id).unwrap();

        // Parent (team_id dir) replaced by symlink.
        let team_dir = hooks.parent().unwrap().to_path_buf();
        let teams_safe = team_dir.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&teams_safe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let backup = d.join("team-dir-backup");
        // Move contents aside: remove hooks first (readonly), then rename team_dir.
        std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&hooks);
        std::fs::rename(&team_dir, &backup).unwrap();
        std::os::unix::fs::symlink(&backup, &team_dir).unwrap();
        let hooked = worker_safe_hooks_dir(&d, &team.id);
        // Recreate hooks under the symlinked parent — path walk must still refuse.
        std::fs::create_dir_all(&hooked).unwrap();
        std::fs::set_permissions(&hooked, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(
            !worker_safe_hooks_dir_ok(&d, &team, wt_path, &hooked),
            "parent symlink must refuse"
        );
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit_f(&hooked)), &options(), &wt).is_none());

        // data_dir under temp root → location refuse (separate fixture).
        let temp_d = std::env::temp_dir().join(format!("pb-allow-temp-{}", crate::db::new_id()));
        std::fs::create_dir_all(temp_d.join("proj/trees")).unwrap();
        let t = TeamsDb::open(&temp_d).unwrap();
        let builtin = t.template("builtin-triage-desk").unwrap();
        let tpl = t
            .save_template(super::super::TemplateInput {
                id: None,
                name: "temp-data".into(),
                seats: builtin.seats,
                routing: builtin.routing,
                allow_worker_commit: true,
            })
            .unwrap();
        let team_temp = t
            .create_team(super::super::NewTeam {
                template_id: tpl.id,
                slug: "tmpd".into(),
                cwd: temp_d.join("proj").to_string_lossy().into_owned(),
                id_prefix: Some("T".into()),
                pr_base: None,
                worktree_root: Some(temp_d.join("proj/trees").to_string_lossy().into_owned()),
                project_rules: None,
                export_dir: Some(temp_d.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        let hooks_temp = ensure_worker_safe_hooks_dir(&temp_d, &team_temp.id).unwrap();
        let item = "T-20261113-01";
        let wt_temp = temp_d.join("proj/trees").join(item.to_lowercase());
        std::fs::create_dir_all(&wt_temp).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-c", "init.defaultBranch=main", "init", "-q"])
                .current_dir(&wt_temp)
                .status()
                .unwrap()
                .success()
        );
        let empty = wt_temp.join(".pb-test-hooks-empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", wt_temp.to_str().unwrap(), "config", "core.hooksPath", empty.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        t.attach(&team_temp.id, "wtmp", "workers").unwrap();
        t.0.execute("UPDATE members SET item_id = ?1 WHERE session_id = 'wtmp'", [item]).unwrap();
        t.mark_git_env_scrubbed("wtmp").unwrap();
        assert!(
            !worker_safe_hooks_dir_ok(&temp_d, &team_temp, &wt_temp, &hooks_temp),
            "temp data_dir must refuse"
        );
        let cmd = commit_f(&hooks_temp);
        assert!(
            allow_team_tool(
                &temp_d,
                "wtmp",
                GIT_AGENT,
                &git_exec(wt_temp.to_str().unwrap(), &cmd),
                &options(),
                wt_temp.to_str().unwrap()
            )
            .is_none(),
            "temp data_dir auto-approve must refuse"
        );
        let _ = std::fs::remove_dir_all(&temp_d);
    }

    #[test]
    fn worker_git_auto_approve_refuses_unsandboxed_agents() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        let cmd = git_cmd(&safe, "status");
        for agent in ["claude-code", "opencode", "cursor", "antigravity", "patchbay", "grok"] {
            assert!(
                allow_team_tool(&d, "w1", agent, &git_exec(&wt, &cmd), &options(), &wt).is_none(),
                "{agent} must prompt for git"
            );
        }
        let tc = git_exec(&wt, &cmd);
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &options(), &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe), "codex gates must still pass");
    }

    #[test]
    fn worker_git_auto_approve_requires_launched_scrubbed_fact() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let t = TeamsDb::open(&d).unwrap();
        t.0.execute("UPDATE members SET git_env_scrubbed = 0 WHERE session_id = 'w1'", [])
            .unwrap();
        let cmd = git_cmd(&safe, "status");
        let tc = git_exec(&wt, &cmd);
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &options(), &wt);
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe),
            "attach/restore without spawn scrub must fail gates"
        );
        t.mark_git_env_scrubbed("w1").unwrap();
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &options(), &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe), "scrubbed fact restores gates");
    }

    #[test]
    fn codex_export_persist_keys_on_recorded_runtime_version() {
        if skip_without_clean_git() {
            return;
        }
        assert!(PROVEN_CODEX_NO_EXPORT_PERSIST.contains(&"2.2.2"));
        assert_eq!(agent_shell_export_persists("codex", Some("2.2.2")), Some(false));
        assert_eq!(agent_shell_export_persists("codex", None), None);
        assert_eq!(agent_shell_export_persists("codex", Some("9.9.9")), None);
        assert_eq!(agent_shell_export_persists("codex", Some("")), None);

        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let t = TeamsDb::open(&d).unwrap();
        let cmd = git_cmd(&safe, "status");
        let tc = git_exec(&wt, &cmd);
        // Fixture records 2.2.2 → gates pass; floor still prompts.
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &options(), &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe), "2.2.2 gates pass");
        // Absent runtime version ⇒ gates refuse.
        t.0.execute("UPDATE members SET acp_runtime_version = NULL WHERE session_id = 'w1'", [])
            .unwrap();
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe),
            "absent runtime version must fail gates"
        );
        // Mismatched runtime version ⇒ gates refuse.
        t.mark_acp_runtime_version("w1", "0.0.0").unwrap();
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe),
            "mismatched runtime version must fail gates"
        );
        // Build-time package pin must not appear in the decision path (R2-1): wrong recorded
        // version still prompts even when CODEX_ACP_PACKAGE says 2.2.2.
        assert!(
            crate::code::agents::CODEX_ACP_PACKAGE.contains("2.2.2"),
            "package pin is informational only; decision uses recorded version"
        );
    }

    /// V1 / R2-1: relaunch clears the previous process's handshake version; gate sees None until
    /// the new SessionReady records a version for this launch.
    #[test]
    fn relaunch_clears_acp_runtime_version_until_session_ready() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let t = TeamsDb::open(&d).unwrap();
        let cmd = git_cmd(&safe, "status");
        assert_eq!(t.acp_runtime_version("w1").unwrap().as_deref(), Some("2.2.2"));
        assert!(t.git_env_scrubbed("w1").unwrap());
        let tc = git_exec(&wt, &cmd);
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &options(), &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe));

        // Simulate prepare/spawn: clear launch-bound facts (as maybe_scrub_worker_git_env does).
        t.clear_launch_bound_facts("w1").unwrap();
        assert_eq!(t.acp_runtime_version("w1").unwrap(), None, "relaunch => None until SessionReady");
        assert!(!t.git_env_scrubbed("w1").unwrap(), "scrub fact also cleared until this launch records it");
        assert!(
            !gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe),
            "stale prior-process version must fail gates after clear"
        );

        // New launch records scrub + SessionReady version.
        t.mark_git_env_scrubbed("w1").unwrap();
        assert!(!gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe), "scrub alone without version");
        t.mark_acp_runtime_version("w1", "2.2.2").unwrap();
        assert!(gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe), "version+scrub restores gates");
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &options(), &wt);
    }

    #[test]
    fn parse_config_list_origins_rejects_unparseable_record() {
        let ok = "global\tfile:/home/u/.gitconfig\tuser.name=Ada\nlocal\tfile:.git/config\tcore.bare=false\n";
        assert!(parse_config_list_origins(ok).is_some());
        let bad = "global\tfile:/home/u/.gitconfig\tuser.name=Ada\nthis-line-has-no-tabs-or-origin\n";
        assert!(
            parse_config_list_origins(bad).is_none(),
            "any unparseable non-empty line must fail the whole parse"
        );
        assert!(parse_config_list_origins("").unwrap().is_empty());
    }

    #[test]
    fn scrub_reinjects_global_commit_identity() {
        use crate::code::agents::{read_human_commit_identity, scrub_git_env};
        let Some((name, email)) = read_human_commit_identity() else {
            // CI / sandboxes without global identity: scrub still leaves the four unset.
            let mut env = vec![("GIT_SSH_COMMAND".into(), "evil".into())];
            let mut env_remove = Vec::new();
            scrub_git_env(&mut env, &mut env_remove);
            assert!(!env.iter().any(|(k, _)| k.starts_with("GIT_AUTHOR") || k.starts_with("GIT_COMMITTER")));
            return;
        };
        let mut env = vec![
            ("GIT_SSH_COMMAND".into(), "evil".into()),
            ("GIT_AUTHOR_NAME".into(), "worker-forged".into()),
        ];
        let mut env_remove = Vec::new();
        scrub_git_env(&mut env, &mut env_remove);
        assert!(!env.iter().any(|(k, _)| k == "GIT_SSH_COMMAND"));
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("GIT_AUTHOR_NAME"), Some(name.as_str()));
        assert_eq!(get("GIT_AUTHOR_EMAIL"), Some(email.as_str()));
        assert_eq!(get("GIT_COMMITTER_NAME"), Some(name.as_str()));
        assert_eq!(get("GIT_COMMITTER_EMAIL"), Some(email.as_str()));
        assert_ne!(get("GIT_AUTHOR_NAME"), Some("worker-forged"));
    }

    #[test]
    fn worker_commit_message_file_inside_worktree_accepted() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let root = Path::new(&wt);
        std::fs::write(root.join("LICENSE"), "x").unwrap();
        std::fs::write(root.join("msg.txt"), "from-file").unwrap();
        let ok_tc = git_exec(&wt, &git_cmd(&safe, "commit --no-verify -F msg.txt"));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &ok_tc, &options(), &wt);
        assert!(gates_pass(&d, "w1", GIT_AGENT, &ok_tc, &wt, &safe));
        let bad_tc = git_exec(&wt, &git_cmd(&safe, "commit --no-verify -F /etc/passwd"));
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &bad_tc, &options(), &wt);
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &bad_tc, &options(), &wt).is_none());
    }

    /// PLANTED-RED (b / F-B i): in-tree filter.clean — gate prompts; with guard (filter unset) add
    /// runs marker-absent; without guard the clean filter RUNS and marker is present.
    #[cfg(unix)]
    #[test]
    fn planted_red_filter_in_tree_runs_without_guard() {
        let d = allow_non_temp_data_dir("planted-b-intree");
        let proj = d.join("proj");
        let trees = proj.join("trees");
        let item_id = "G-20261112-01";
        let wt = trees.join(item_id.to_lowercase());
        std::fs::create_dir_all(&wt).unwrap();
        let t = TeamsDb::open(&d).unwrap();
        let builtin = t.template("builtin-triage-desk").unwrap();
        let tpl = t
            .save_template(super::super::TemplateInput {
                id: None,
                name: "planted-b".into(),
                seats: builtin.seats,
                routing: builtin.routing,
                allow_worker_commit: true,
            })
            .unwrap();
        let team = t
            .create_team(super::super::NewTeam {
                template_id: tpl.id,
                slug: "pb".into(),
                cwd: proj.to_string_lossy().into_owned(),
                id_prefix: Some("G".into()),
                pr_base: None,
                worktree_root: Some(trees.to_string_lossy().into_owned()),
                project_rules: None,
                export_dir: Some(d.join("out").to_string_lossy().into_owned()),
            })
            .unwrap();
        let safe = ensure_worker_safe_hooks_dir(&d, &team.id).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-c", "init.defaultBranch=main", "init", "-q"])
                .current_dir(&wt)
                .status()
                .unwrap()
                .success()
        );
        let empty = wt.join(".pb-hooks");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", wt.to_str().unwrap(), "config", "core.hooksPath", empty.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let marker = d.join("FILTER_MARKER");
        let filter = d.join("evil-clean.sh");
        std::fs::write(
            &filter,
            format!("#!/bin/sh\necho planted-red-filter >> {}\ncat\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(wt.join(".gitattributes"), "* filter=evil\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", wt.to_str().unwrap(), "config", "filter.evil.clean", filter.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        t.attach(&team.id, "w1", "workers").unwrap();
        t.0.execute("UPDATE members SET item_id = ?1 WHERE session_id = 'w1'", [item_id]).unwrap();
        t.mark_git_env_scrubbed("w1").unwrap();
        std::fs::write(wt.join("LICENSE"), "x").unwrap();
        let wt_s = wt.to_string_lossy().into_owned();
        let cmd = git_cmd(&safe, "add -- LICENSE");
        assert!(is_safe_worker_command(&cmd, &wt, &safe), "tokenizer alone accepts");
        let with_gate = allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt_s, &cmd), &options(), &wt_s);
        assert!(with_gate.is_none(), "in-tree filter/config must prompt");

        // With guard: unset filter — add must not write marker.
        let _ = std::fs::remove_file(&marker);
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt_s, "config", "--unset", "filter.evil.clean"])
                .status()
                .unwrap()
                .success()
        );
        let with_guard = std::process::Command::new("git")
            .args(["-C", &wt_s, "add", "LICENSE"])
            .output()
            .unwrap();
        let with_guard_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            with_guard.status.code(),
            String::from_utf8_lossy(&with_guard.stdout),
            String::from_utf8_lossy(&with_guard.stderr)
        );
        let marker_with = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(marker_with.is_empty(), "marker ABSENT with filter unset: {with_guard_raw}");

        // Without guard: restore filter — add RUNS clean filter → marker PRESENT.
        // Clear any inherited GIT_DIR so -C is honored (parallel tests must not leak process env).
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt_s, "config", "filter.evil.clean", filter.to_str().unwrap()])
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .status()
                .unwrap()
                .success()
        );
        let _ = std::fs::remove_file(&marker);
        // Need a change so clean filter runs again.
        std::fs::write(wt.join("LICENSE"), "y").unwrap();
        let without = std::process::Command::new("git")
            .args(["-C", &wt_s, "add", "LICENSE"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        let without_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            without.status.code(),
            String::from_utf8_lossy(&without.stdout),
            String::from_utf8_lossy(&without.stderr)
        );
        let marker_without = std::fs::read_to_string(&marker).unwrap_or_default();
        let doc = format!(
            "PLANTED-RED (F-B i) filter in-tree\nallow={with_gate:?}\nwith_guard:\n{with_guard_raw}\n\
             marker_with={marker_with:?}\n\nwithout_guard:\n{without_raw}\nmarker_without={marker_without:?}\n"
        );
        let raw_path = d.join("pb-planted-red-b-intree-raws.txt");
        std::fs::write(&raw_path, &doc).unwrap();
        eprintln!("{doc}");
        assert!(
            marker_without.contains("planted-red-filter"),
            "marker PRESENT without guard; raws={}",
            raw_path.display()
        );
    }

    /// PLANTED-RED (b / F-B ii): `include.path` into the worktree — origin gate prompts; filter
    /// via include RUNS without guard.
    #[cfg(unix)]
    #[test]
    fn planted_red_filter_via_include_into_worktree() {
        let (d, wt, proj, _item, safe) = worker_commit_fixture(true);
        let marker = d.join("FILTER_INCLUDE_MARKER");
        let filter = d.join("evil-include-clean.sh");
        std::fs::write(
            &filter,
            format!("#!/bin/sh\necho planted-red-include-filter >> {}\ncat\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Include file lives inside the worktree (writable set); main config stays outside.
        let include = Path::new(&wt).join("evil-gitconfig");
        std::fs::write(
            &include,
            format!("[filter \"evil\"]\n\tclean = {}\n", filter.display()),
        )
        .unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &proj, "config", "include.path", include.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(Path::new(&wt).join(".gitattributes"), "* filter=evil\n").unwrap();
        std::fs::write(Path::new(&wt).join("LICENSE"), "x").unwrap();
        let cmd = git_cmd(&safe, "add -- LICENSE");
        assert!(is_safe_worker_command(&cmd, Path::new(&wt), &safe));
        let with_gate = allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &cmd), &options(), &wt);
        assert!(
            with_gate.is_none(),
            "include.path into worktree must refuse auto-approve (origin or filter)"
        );

        // With guard: remove include — add marker-absent.
        let _ = std::fs::remove_file(&marker);
        assert!(
            std::process::Command::new("git")
                .args(["-C", &proj, "config", "--unset", "include.path"])
                .status()
                .unwrap()
                .success()
        );
        let with_guard = std::process::Command::new("git")
            .args(["-C", &wt, "add", "LICENSE"])
            .output()
            .unwrap();
        let with_guard_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            with_guard.status.code(),
            String::from_utf8_lossy(&with_guard.stdout),
            String::from_utf8_lossy(&with_guard.stderr)
        );
        let marker_with = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(marker_with.is_empty(), "ABSENT with include removed: {with_guard_raw}");

        // Without guard: restore include — filter RUNS.
        assert!(
            std::process::Command::new("git")
                .args(["-C", &proj, "config", "include.path", include.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        let _ = std::fs::remove_file(&marker);
        std::fs::write(Path::new(&wt).join("LICENSE"), "z").unwrap();
        let without = std::process::Command::new("git")
            .args(["-C", &wt, "add", "LICENSE"])
            .output()
            .unwrap();
        let without_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            without.status.code(),
            String::from_utf8_lossy(&without.stdout),
            String::from_utf8_lossy(&without.stderr)
        );
        let marker_without = std::fs::read_to_string(&marker).unwrap_or_default();
        let doc = format!(
            "PLANTED-RED (F-B ii) filter via include into worktree\nallow={with_gate:?}\n\
             with_guard:\n{with_guard_raw}\nmarker_with={marker_with:?}\n\n\
             without_guard:\n{without_raw}\nmarker_without={marker_without:?}\n"
        );
        let raw_path = d.join("pb-planted-red-b-include-raws.txt");
        std::fs::write(&raw_path, &doc).unwrap();
        eprintln!("{doc}");
        assert!(
            marker_without.contains("planted-red-include-filter"),
            "marker PRESENT without guard; raws={}",
            raw_path.display()
        );
    }

    /// PLANTED-RED (F-A): configured textconv marker ABSENT with `--no-ext-diff --no-textconv`,
    /// PRESENT without those flags. Raws under the fixture dir (not /tmp).
    #[cfg(unix)]
    #[test]
    fn planted_red_diff_textconv_absent_with_flags() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let root = Path::new(&wt);
        let marker = d.join("TEXTCONV_MARKER");
        let _ = std::fs::remove_file(&marker);
        let driver = d.join("evil-textconv.sh");
        std::fs::write(
            &driver,
            format!("#!/bin/sh\necho planted-red-textconv >> {}\ncat \"$1\"\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "diff.evil.textconv", driver.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join(".gitattributes"), "*.bin diff=evil\n").unwrap();
        std::fs::write(root.join("blob.bin"), "v1").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "add", "blob.bin", ".gitattributes"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args([
                    "-C",
                    &wt,
                    "-c",
                    &format!("core.hooksPath={}", path_for_git_c_flag(&safe)),
                    "-c",
                    "core.fsmonitor=false",
                    "commit",
                    "--no-verify",
                    "-m",
                    "bin",
                ])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join("blob.bin"), "v2").unwrap();

        let with_flags = format!(
            "{} diff --no-ext-diff --no-textconv -- blob.bin",
            git_pfx(&safe)
        );
        let with_tc = git_exec(&wt, &with_flags);
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &with_tc, &options(), &wt);
        assert!(
            gates_pass(&d, "w1", GIT_AGENT, &with_tc, &wt, &safe),
            "both flags required for gates to pass"
        );
        let _ = std::fs::remove_file(&marker);
        let out_with = std::process::Command::new("sh")
            .arg("-c")
            .arg(&with_flags)
            .current_dir(&wt)
            .output()
            .unwrap();
        let with_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            out_with.status.code(),
            String::from_utf8_lossy(&out_with.stdout),
            String::from_utf8_lossy(&out_with.stderr)
        );
        let marker_with = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(
            marker_with.is_empty(),
            "marker must be ABSENT with --no-ext-diff --no-textconv; marker={marker_with:?}; raw={with_raw}"
        );

        let _ = std::fs::remove_file(&marker);
        let without = std::process::Command::new("git")
            .args(["-C", &wt, "diff", "--", "blob.bin"])
            .output()
            .unwrap();
        let without_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            without.status.code(),
            String::from_utf8_lossy(&without.stdout),
            String::from_utf8_lossy(&without.stderr)
        );
        let marker_without = std::fs::read_to_string(&marker).unwrap_or_default();
        let doc = format!(
            "PLANTED-RED (F-A) diff textconv\nwith_flags:\n{with_raw}\nmarker_with={marker_with:?}\n\n\
             without_flags:\n{without_raw}\nmarker_without={marker_without:?}\n"
        );
        let doc_path = d.join("pb-planted-red-textconv-raws.txt");
        std::fs::write(&doc_path, &doc).unwrap();
        eprintln!("{doc}");
        assert!(
            marker_without.contains("planted-red-textconv"),
            "marker must be PRESENT without flags; raws={}",
            doc_path.display()
        );
        assert!(doc_path.is_file());
    }

    /// PLANTED-RED (c): spawn scrub removes inherited GIT_*; with guard removed the marker appears.
    #[cfg(unix)]
    #[test]
    fn planted_red_export_scrub_guard() {
        use crate::code::agents::scrub_git_env;
        let d = allow_non_temp_data_dir("export-scrub");
        let marker = d.join("EXPORT_MARKER");
        let wrapper = d.join("evil-ssh.sh");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\necho planted-red-export >> {}\nexit 0\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

        // With guard: scrub strips GIT_SSH_COMMAND (known redirector) then child sees nothing.
        let mut env = vec![("GIT_SSH_COMMAND".into(), wrapper.to_string_lossy().into_owned())];
        let mut env_remove = Vec::new();
        scrub_git_env(&mut env, &mut env_remove);
        assert!(env_remove.iter().any(|k| k == "GIT_SSH_COMMAND"));
        assert!(!env.iter().any(|(k, _)| k == "GIT_SSH_COMMAND"));
        assert_eq!(
            env.iter().find(|(k, _)| k == "GIT_CONFIG_NOSYSTEM").map(|(_, v)| v.as_str()),
            Some("1")
        );
        assert_eq!(
            env.iter().find(|(k, _)| k == "GIT_CONFIG_GLOBAL").map(|(_, v)| v.as_str()),
            Some("/dev/null")
        );
        let _ = std::fs::remove_file(&marker);
        let mut child = std::process::Command::new("sh");
        child.arg("-c").arg("printf '%s' \"$GIT_SSH_COMMAND\"");
        // Start from a clean env for the var, then apply scrub removes + forced env.
        child.env_remove("GIT_SSH_COMMAND");
        for k in &env_remove {
            child.env_remove(k);
        }
        for (k, v) in &env {
            child.env(k, v);
        }
        let with_guard = child.output().unwrap();
        let with_guard_raw = format!(
            "status={:?}\nstdout={:?}\nstderr={}",
            with_guard.status.code(),
            String::from_utf8_lossy(&with_guard.stdout),
            String::from_utf8_lossy(&with_guard.stderr)
        );
        let marker_with = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(
            String::from_utf8_lossy(&with_guard.stdout).is_empty(),
            "GIT_SSH_COMMAND must be absent after scrub: {with_guard_raw}"
        );
        assert!(marker_with.is_empty(), "marker must be absent with guard");

        // Without guard: inherit GIT_SSH_COMMAND and invoke it.
        let _ = std::fs::remove_file(&marker);
        let without = std::process::Command::new("sh")
            .arg("-c")
            .arg("\"$GIT_SSH_COMMAND\"")
            .env("GIT_SSH_COMMAND", wrapper.to_string_lossy().as_ref())
            .output()
            .unwrap();
        let without_raw = format!(
            "status={:?}\nstdout={}\nstderr={}",
            without.status.code(),
            String::from_utf8_lossy(&without.stdout),
            String::from_utf8_lossy(&without.stderr)
        );
        let marker_without = std::fs::read_to_string(&marker).unwrap_or_default();
        let doc = format!(
            "PLANTED-RED (c) export scrub\nwith_guard:\n{with_guard_raw}\n\nwithout_guard:\n{without_raw}\n\n\
             marker_with={marker_with:?}\nmarker_without={marker_without:?}\n"
        );
        let doc_path = d.join("pb-planted-red-c-raws.txt");
        std::fs::write(&doc_path, &doc).unwrap();
        eprintln!("{doc}");
        assert!(
            marker_without.contains("planted-red-export"),
            "marker must appear without guard; raws={}",
            doc_path.display()
        );
    }

    #[test]
    fn enrich_worker_git_permission_adds_hooks_phrase() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let mut raw = json!({
            "toolCall": {
                "kind": "execute",
                "rawInput": { "command": "git status", "cwd": wt },
                "askReason": "needs shell",
                "title": "agent-supplied title"
            },
            "options": [
                { "kind": "allow_once", "optionId": "once", "name": "Allow" },
                { "kind": "reject_once", "optionId": "no", "name": "Deny" }
            ]
        });
        enrich_worker_git_permission(&d, "w1", &mut raw);
        let reason = raw["toolCall"]["askReason"].as_str().unwrap();
        assert!(reason.contains("Command: git status"), "{reason}");
        assert!(reason.contains(&format!("cwd: {wt}")), "{reason}");
        assert!(reason.contains(WORKER_GIT_HOOKS_PROMPT), "{reason}");
        assert!(reason.contains(WORKER_GIT_UNSANDBOXED_PROMPT), "{reason}");
        assert_eq!(raw["toolCall"]["_meta"]["patchbay"]["command"], "git status");
        assert_eq!(raw["toolCall"]["_meta"]["patchbay"]["cwd"], wt);
        assert_eq!(raw["toolCall"]["_meta"]["patchbay"]["workerGitPrompt"], true);
        let once_name = raw["options"][0]["name"].as_str().unwrap();
        assert!(once_name.contains(WORKER_GIT_HOOKS_PROMPT), "{once_name}");
        assert!(once_name.contains(WORKER_GIT_UNSANDBOXED_PROMPT), "{once_name}");
        assert_eq!(raw["options"][1]["name"].as_str().unwrap(), "Deny");
        let _ = safe;
    }

    #[test]
    fn enrich_warns_when_add_pathspec_includes_commit_msg_file() {
        // Windows: PATH git often has a space (`Program Files`) — product fail-closed; skip like
        // the other absolute-git matcher tests (6e96da56 / P3).
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let cmd = git_cmd(&safe, "add -- hello.txt PB_COMMIT_MSG");
        let mut raw = json!({
            "toolCall": {
                "kind": "execute",
                "rawInput": { "command": cmd, "cwd": wt },
            },
            "options": [
                { "kind": "allow_once", "optionId": "allow_once", "name": "Yes, proceed" },
                { "kind": "reject_once", "optionId": "cancel", "name": "No" }
            ]
        });
        enrich_worker_git_permission(&d, "w1", &mut raw);
        let reason = raw["toolCall"]["askReason"].as_str().unwrap();
        assert!(reason.contains(WORKER_GIT_MSGFILE_IN_ADD_WARN), "{reason}");
        // Still a prompt, not a refusal: options remain.
        assert_eq!(raw["options"].as_array().unwrap().len(), 2);
        assert!(add_pathspec_includes_commit_msg_file(&git_cmd(&safe, "add -- a .pb-commit-msg")));
        assert!(!add_pathspec_includes_commit_msg_file(&git_cmd(&safe, "add -- hello.txt")));
    }

    #[test]
    fn enrich_worker_git_permission_strips_persistent_options() {
        let (d, wt, _proj, _item, _safe) = worker_commit_fixture(true);
        let mut raw = json!({
            "toolCall": {
                "kind": "execute",
                "rawInput": { "command": "git add f", "cwd": wt },
            },
            "options": [
                { "kind": "allow_once", "optionId": "allow_once", "name": "Yes, proceed" },
                { "kind": "allow_always", "optionId": "accept_execpolicy_amendment",
                  "name": "Yes, and don't ask again for commands that start with `git add`" },
                { "kind": "reject_once", "optionId": "cancel", "name": "No" }
            ]
        });
        enrich_worker_git_permission(&d, "w1", &mut raw);
        let opts = raw["options"].as_array().unwrap();
        assert_eq!(opts.len(), 2, "{opts:?}");
        assert_eq!(opts[0]["kind"], "allow_once");
        assert_eq!(opts[0]["optionId"], "allow_once");
        assert_eq!(opts[1]["kind"], "reject_once");
        assert!(opts.iter().all(|o| o["kind"] != "allow_always"));
        // Persistent id must not remain selectable after strip (C2 server refuse input).
        assert!(!opts.iter().any(|o| o["optionId"] == "accept_execpolicy_amendment"));
    }

    #[test]
    fn allow_team_tool_picks_allow_once_only_and_refuses_persistent_only() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let cmd = git_cmd(&safe, "status");
        let tc = git_exec(&wt, &cmd);
        // Planted red: only persistent options → not auto-approved (C3).
        let only_always = json!([
            { "kind": "allow_always", "optionId": "accept_execpolicy_amendment", "name": "Always" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &tc, &only_always, &wt).is_none(),
            "persistent-only options must not auto-approve"
        );
        // Team tool path also uses allow_option (permissions.rs:57-62 allow_once only).
        inject("w1");
        let team_tc = claude_mcp("team_roster", json!({}));
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &team_tc, &only_always, &wt).is_none(),
            "team tools: persistent-only must not auto-approve"
        );
        let with_once = json!([
            { "kind": "allow_always", "optionId": "always", "name": "Always" },
            { "kind": "allow_once", "optionId": "once", "name": "Once" },
        ]);
        // #449: worker git never auto-approves even with allow_once offered.
        assert_git_floor_prompts(&d, "w1", GIT_AGENT, &tc, &with_once, &wt);
        // ls still auto-approves under allow_worker_commit (non-git).
        let ls_hit = allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, "ls"), &with_once, &wt)
            .expect("ls allow_once offered");
        assert_eq!(ls_hit.0, "once");
        let team_hit = allow_team_tool(&d, "w1", "claude-code", &team_tc, &with_once, &wt).expect("team allow_once");
        assert_eq!(team_hit.0, "once");
    }

    #[test]
    fn zsh_lc_wrapped_git_is_never_auto_approved() {
        if skip_without_clean_git() {
            return;
        }
        // HOLD: escalated Codex form is zsh -lc; shell string Patchbay must parse → allow=None.
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let inner = format!("{} add -- f", git_pfx(&safe));
        let argv = json!(["/bin/zsh", "-lc", inner]);
        let joined = format!("/bin/zsh -lc {inner}");
        let opts = options();
        for cmd in [
            json!({ "kind": "execute", "rawInput": { "command": argv, "cwd": wt } }),
            json!({ "kind": "execute", "rawInput": { "command": joined, "cwd": wt } }),
        ] {
            assert!(
                allow_team_tool(&d, "w1", GIT_AGENT, &cmd, &opts, &wt).is_none(),
                "zsh -lc wrapped git must not auto-approve; cmd={cmd}"
            );
        }
    }

    /// LIVE24 request shape: escalated Codex re-ask arrives as the **plain** template string
    /// (not zsh -lc). Options include allow_once + accept_execpolicy_amendment + cancel.
    /// #449 floor ⇒ None; enrichment carries command/cwd/hooks/unsandboxed; persistent stripped.
    #[test]
    fn live24_plain_template_escalation_shape_prompts_with_enrichment() {
        if skip_without_clean_git() {
            return;
        }
        assert!(
            !WORKER_GIT_WRITE_AUTO_APPROVE,
            "C floor constant must stay false until the A PR"
        );
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(Path::new(&wt).join("hello-live24.txt"), "live24\n").unwrap();
        write_commit_msg(&wt, "Add hello-live24 file");
        // Exact live24 shape (plain template; cwd = worktree).
        let add_cmd = git_cmd(&safe, "add -- hello-live24.txt");
        let commit_cmd = commit_f(&safe);
        let live24_options = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes, proceed" },
            { "kind": "allow_always", "optionId": "accept_execpolicy_amendment",
              "name": "Yes, and don't ask again for commands that start with this prefix" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        for cmd in [&add_cmd, &commit_cmd] {
            let tc = git_exec(&wt, cmd);
            // Decision: never auto-approve (even with opt-in, scrubbed, 2.2.2, gates would pass).
            assert!(
                allow_team_tool(&d, "w1", GIT_AGENT, &tc, &live24_options, &wt).is_none(),
                "live24 plain template must prompt; cmd={cmd}"
            );
            assert!(
                gates_pass(&d, "w1", GIT_AGENT, &tc, &wt, &safe),
                "gates still pass for A PR planted-red; cmd={cmd}"
            );
            let mut raw = json!({
                "toolCall": tc,
                "options": live24_options.clone(),
            });
            enrich_worker_git_permission(&d, "w1", &mut raw);
            let reason = raw["toolCall"]["askReason"].as_str().unwrap_or("");
            assert!(reason.contains(cmd), "askReason must carry command verbatim; got {reason}");
            assert!(reason.contains(&format!("cwd: {wt}")), "{reason}");
            assert!(reason.contains(WORKER_GIT_HOOKS_PROMPT), "{reason}");
            assert!(reason.contains(WORKER_GIT_UNSANDBOXED_PROMPT), "{reason}");
            assert_eq!(raw["toolCall"]["_meta"]["patchbay"]["command"], *cmd);
            assert_eq!(raw["toolCall"]["_meta"]["patchbay"]["cwd"], wt);
            let opts = raw["options"].as_array().unwrap();
            assert!(opts.iter().all(|o| o["kind"] != "allow_always"), "{opts:?}");
            assert!(!opts.iter().any(|o| o["optionId"] == "accept_execpolicy_amendment"));
            assert!(opts.iter().any(|o| o["kind"] == "allow_once"));
            let once_name = opts
                .iter()
                .find(|o| o["kind"] == "allow_once")
                .and_then(|o| o["name"].as_str())
                .unwrap_or("");
            assert!(once_name.contains(WORKER_GIT_HOOKS_PROMPT), "{once_name}");
            assert!(once_name.contains(WORKER_GIT_UNSANDBOXED_PROMPT), "{once_name}");
        }
    }

    #[test]
    fn enrich_worker_git_permission_covers_wrapped_and_absolute_shapes() {
        let (d, wt, _proj, _item, _safe) = worker_commit_fixture(true);
        let shapes = [
            "rtk git status",
            "/usr/bin/git status",
            "command git status",
            "env git status",
            "env -i HOME=/tmp git status",
            "sh -c 'git status'",
            "sh -c \"git status\"",
            "git status",
            // R2-3 conservative: expansions / wrappers the tokenizer cannot fully classify.
            "g=git; $g status",
            "$(which git) status",
            "xargs git",
            "eval git status",
        ];
        for cmd in shapes {
            let mut raw = json!({
                "toolCall": {
                    "kind": "execute",
                    "title": "agent own title",
                    "rawInput": { "command": cmd, "cwd": wt },
                    "askReason": ""
                },
                "options": [{ "kind": "allow_once", "optionId": "once", "name": "Allow" }]
            });
            assert!(command_may_invoke_git(cmd), "classify: {cmd}");
            enrich_worker_git_permission(&d, "w1", &mut raw);
            let reason = raw["toolCall"]["askReason"].as_str().unwrap_or("");
            assert!(
                reason.contains(WORKER_GIT_HOOKS_PROMPT),
                "askReason must carry hooks phrase for {cmd:?}; got {reason:?}"
            );
            assert!(
                reason.contains(WORKER_GIT_UNSANDBOXED_PROMPT),
                "askReason must disclose unsandboxed escalated path for {cmd:?}; got {reason:?}"
            );
            let once_name = raw["options"][0]["name"].as_str().unwrap_or("");
            assert!(
                once_name.contains(WORKER_GIT_HOOKS_PROMPT),
                "option name must carry hooks phrase for {cmd:?}"
            );
            assert!(
                once_name.contains(WORKER_GIT_UNSANDBOXED_PROMPT),
                "option name must disclose unsandboxed escalated path for {cmd:?}"
            );
        }
        // Non-git must not get the phrase.
        let mut raw = json!({
            "toolCall": {
                "kind": "execute",
                "rawInput": { "command": "ls -la", "cwd": wt },
                "askReason": "list"
            },
            "options": [{ "kind": "allow_once", "optionId": "once", "name": "Allow" }]
        });
        enrich_worker_git_permission(&d, "w1", &mut raw);
        assert_eq!(raw["toolCall"]["askReason"].as_str().unwrap(), "list");
        assert_eq!(raw["options"][0]["name"].as_str().unwrap(), "Allow");
    }

    #[test]
    fn hardened_git_ignores_daemon_git_dir_and_config_parameters() {
        let (d, wt, _proj, _item, _safe) = worker_commit_fixture(true);
        let decoy = d.join("decoy.git");
        std::fs::create_dir_all(&decoy).unwrap();

        // Unhardened child with daemon-like redirectors: GIT_DIR wins over -C.
        let poisoned = std::process::Command::new("git")
            .args(["-C", &wt, "rev-parse", "--is-inside-work-tree"])
            .env("GIT_DIR", &decoy)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_PARAMETERS", "'core.hooksPath=/tmp/evil'")
            .output()
            .unwrap();
        let poisoned_out = String::from_utf8_lossy(&poisoned.stdout);
        assert!(
            !poisoned.status.success() || poisoned_out.trim() != "true",
            "unhardened probe must not see the real worktree under GIT_DIR; out={poisoned_out:?}"
        );

        // Same redirectors on the Command, then apply_hardened_git_env clears them (F-C).
        let mut cmd = std::process::Command::new("git");
        cmd.env("GIT_DIR", &decoy)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_PARAMETERS", "'core.hooksPath=/tmp/evil'");
        crate::code::agents::apply_hardened_git_env(&mut cmd);
        let cleared = cmd
            .args(["-C", &wt, "rev-parse", "--is-inside-work-tree"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&cleared.stdout).trim(),
            "true",
            "hardened env must inspect the real repo; stderr={}",
            String::from_utf8_lossy(&cleared.stderr)
        );
        assert_eq!(
            hardened_git(Path::new(&wt), &["rev-parse", "--is-inside-work-tree"]).as_deref(),
            Some("true")
        );
    }

    /// Rename the fixture's `"workers"` seat key (and membership) to a custom pool name (S1).
    fn rekey_worker_seat(d: &Path, new_seat: &str) {
        let t = TeamsDb::open(d).unwrap();
        let (team, _) = t.membership("w1").unwrap().expect("fixture membership");
        let tpl = t.template(&team.template_id).unwrap();
        let seats: Vec<super::super::Seat> = tpl
            .seats
            .into_iter()
            .map(|mut s| {
                if s.seat == "workers" {
                    s.seat = new_seat.into();
                }
                s
            })
            .collect();
        t.save_template(super::super::TemplateInput {
            id: Some(tpl.id),
            name: tpl.name,
            seats,
            routing: tpl.routing,
            allow_worker_commit: tpl.allow_worker_commit,
        })
        .unwrap();
        t.0.execute("UPDATE members SET seat = ?1 WHERE session_id = 'w1'", [new_seat])
            .unwrap();
        assert_eq!(t.membership("w1").unwrap().unwrap().1.seat, new_seat);
        assert!(
            is_worker_seat_member(d, "w1"),
            "custom seat {new_seat} must still count as worker via Role.standing"
        );
    }

    /// G1(a)+(b)+(c)+(d): decide_session_auto_approve floor — not allow.rs alone.
    #[test]
    fn g1_decide_session_auto_approve_worker_git_floor() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let worker = db.create_code_session(&wt, None, "codex", "always-approve").unwrap();
        let outsider = db.create_code_session(&wt, None, "codex", "default").unwrap();
        // Point membership at the real code-session id (fixture used literal "w1").
        let tdb = TeamsDb::open(&d).unwrap();
        tdb.0
            .execute(
                "UPDATE members SET session_id = ?1 WHERE session_id = 'w1'",
                [&worker.id],
            )
            .unwrap();
        db.add_permission_rule("*", "execute", "").unwrap();
        let opts = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        let template = git_cmd(&safe, "add -- hello.txt");
        let tc = |cmd: &str| {
            json!({
                "kind": "execute",
                "rawInput": { "command": cmd, "cwd": wt },
            })
        };
        // (a) wildcard rule + worker template => ask
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc(&template), &opts, false).is_none(),
            "wildcard rule must not approve worker git"
        );
        // (b) always-approve mode on worker => still ask
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc(&template), &opts, false).is_none(),
            "always-approve mode must not approve worker git"
        );
        // (c) non-template + wrapped git => ask
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc("git add -- x"), &opts, false).is_none()
        );
        assert!(
            crate::code::decide_session_auto_approve(
                &d,
                &db,
                &worker.id,
                &wt,
                &tc("bash -c 'git status'"),
                &opts
            , false)
            .is_none()
        );
        // (d) non-worker with same rule still auto-approves
        let hit = crate::code::decide_session_auto_approve(&d, &db, &outsider.id, &wt, &tc("echo hi"), &opts, false);
        assert!(hit.is_some(), "non-worker must still match saved execute rule: {hit:?}");
    }

    #[test]
    fn native_worker_outer_classifier_preserves_prompts_even_with_auto_rules() {
        let d = std::env::temp_dir().join(format!("pb-native-outer-{}", crate::db::new_id()));
        let project = d.join("proj");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::create_dir_all(project.join(".git/hooks")).unwrap();
        let outside = d.join("outside");
        let home = d.join("home");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        #[cfg(unix)] std::os::unix::fs::symlink(&outside, project.join("escape")).unwrap();
        let db = crate::db::Db::open(&d.join("patchbay.sqlite")).unwrap();
        let worker = db.create_code_session(&project.to_string_lossy(), None, "patchbay", "auto").unwrap();
        let tdb = TeamsDb::open(&d).unwrap();
        let team = tdb.create_team(crate::teams::NewTeam {
            template_id: "builtin-triage-desk".into(), slug: format!("nw{}", &crate::db::new_id()[..8]),
            cwd: project.to_string_lossy().into_owned(), id_prefix: None, pr_base: None,
            worktree_root: Some(project.join("trees").to_string_lossy().into_owned()), project_rules: None,
            export_dir: Some(d.join("out").to_string_lossy().into_owned()),
        }).unwrap();
        tdb.attach(&team.id, &worker.id, "workers").unwrap();
        db.add_permission_rule("*", "execute", "").unwrap();
        db.add_permission_rule("*", "edit", "").unwrap();
        db.add_permission_rule("*", "read", "").unwrap();
        let options = json!([{"kind":"allow_once","optionId":"allow"},{"kind":"reject_once","optionId":"reject"}]);
        let mut calls = vec![
            json!({"kind":"execute","rawInput":{"command":"echo worker"}}),
            json!({"kind":"edit","locations":[{"path":project.join(".git/hooks/x")}],"rawInput":{"file_path":project.join(".git/hooks/x")}}),
            json!({"kind":"edit","locations":[{"path":project.join(".claude/settings.json")}],"rawInput":{"file_path":project.join(".claude/settings.json")}}),
            json!({"kind":"read","locations":[{"path":home.join(".ssh/id_ed25519")}],"rawInput":{"file_path":home.join(".ssh/id_ed25519")}}),
        ];
        #[cfg(unix)] calls.push(json!({"kind":"edit","locations":[{"path":project.join("escape/new")}],"rawInput":{"file_path":project.join("escape/new")}}));
        for call in calls {
            assert!(crate::code::decide_session_auto_approve(&d, &db, &worker.id, &project.to_string_lossy(), &call, &options, true).is_none(), "native worker call auto-approved: {call}");
        }
        let _ = std::fs::remove_dir_all(d);
    }

    /// Advisor a–c + role caps + Codex pin + planted reds for the worker team-tool floor exemption.
    #[test]
    fn g1_worker_matched_team_tool_execute_exempt_from_floor() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let worker = db.create_code_session(&wt, None, "codex", "always-approve").unwrap();
        let tdb = TeamsDb::open(&d).unwrap();
        tdb.0
            .execute(
                "UPDATE members SET session_id = ?1 WHERE session_id = 'w1'",
                [&worker.id],
            )
            .unwrap();
        inject(&worker.id);
        let coord = db.create_code_session(&wt, None, "codex", "auto").unwrap();
        let team_id: String = tdb
            .0
            .query_row("SELECT team_id FROM members WHERE session_id = ?1", [&worker.id], |r| r.get(0))
            .unwrap();
        tdb.attach(&team_id, &coord.id, "coordinator").unwrap();
        inject(&coord.id);
        db.add_permission_rule("*", "execute", "").unwrap();
        let opts = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        let item_note = codex_mcp("item_note", json!({ "itemId": "i1", "text": "progress" }));
        let not_allowed = codex_mcp("create_agent_session", json!({ "title": "x" }));
        let dispatch = codex_mcp("dispatch_item", json!({ "itemId": "i1" }));
        let commandless_non_mcp = json!({
            "kind": "execute",
            "title": "Run something",
            "rawInput": {}
        });
        let mcp_plus_command = json!({
            "kind": "execute",
            "title": format!("mcp.{INJ}.item_note"),
            "rawInput": {
                "server": INJ,
                "tool": "item_note",
                "arguments": { "itemId": "i1", "text": "progress" },
                "command": "echo hi"
            }
        });
        let lookalike_prefix = json!({
            "kind": "execute",
            "rawInput": {
                "server": "patchbay-team",
                "tool": "item_note",
                "arguments": { "itemId": "i1", "text": "x" }
            }
        });
        let lookalike_case = json!({
            "kind": "execute",
            "rawInput": {
                "server": INJ.to_ascii_uppercase(),
                "tool": "item_note",
                "arguments": { "itemId": "i1", "text": "x" }
            }
        });
        let lookalike_homo = json!({
            "kind": "execute",
            "rawInput": {
                "server": "patchbay_team_\u{1d00}aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "tool": "item_note",
                "arguments": { "itemId": "i1", "text": "x" }
            }
        });
        // Advisor (b): different valid-shaped nonce must not match the stored one.
        let other_nonce = "patchbay_team_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let lookalike_other_nonce = json!({
            "kind": "execute",
            "rawInput": {
                "server": other_nonce,
                "tool": "item_note",
                "arguments": { "itemId": "i1", "text": "x" }
            }
        });
        // Advisor (a): fixed name never exempts for a team seat that holds a nonce.
        let fixed_name_call = json!({
            "kind": "execute",
            "rawInput": {
                "server": "patchbay-agent-messaging",
                "tool": "item_note",
                "arguments": { "itemId": "i1", "text": "x" }
            }
        });
        let template = git_cmd(&safe, "add -- hello.txt");
        let git_tc = json!({
            "kind": "execute",
            "rawInput": { "command": template, "cwd": wt },
        });
        // Non-Codex worker + Codex shape ⇒ asks.
        let claude_worker = db.create_code_session(&wt, None, "claude-code", "auto").unwrap();
        // Re-key a second worker seat is heavy; reuse worker id agent by deciding with claude agent string via matched_tool.
        assert!(
            matched_tool(&d, "claude-code", &worker.id, &item_note).is_none(),
            "non-Codex agent + Codex execute shape must not match"
        );

        assert!(
            matched_tool(&d, "codex", &worker.id, &mcp_plus_command).is_none(),
            "extra command key must not match as team tool"
        );
        assert!(
            matched_tool(&d, "codex", &worker.id, &dispatch).is_none(),
            "worker must not auto-match dispatch_item (no dispatches cap)"
        );
        assert!(
            matched_tool(&d, "codex", &coord.id, &dispatch).is_some(),
            "coordinator must match dispatch_item"
        );

        let hit = crate::code::decide_session_auto_approve(
            &d, &db, &worker.id, &wt, &item_note, &opts, false,
        );
        assert!(
            hit.as_ref().is_some_and(|(id, desc)| id == "allow_once" && desc.contains("item_note")),
            "worker item_note must auto-allow via team tool: {hit:?}"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &not_allowed, &opts, false)
                .is_none(),
            "create_agent_session must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &dispatch, &opts, false)
                .is_none(),
            "worker dispatch_item must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &coord.id, &wt, &dispatch, &opts, false)
                .is_some(),
            "coordinator dispatch_item must allow"
        );
        assert!(
            crate::code::decide_session_auto_approve(
                &d, &db, &worker.id, &wt, &commandless_non_mcp, &opts, false
            )
            .is_none(),
            "command-less non-MCP execute must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &mcp_plus_command, &opts, false)
                .is_none(),
            "extra command key must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &lookalike_prefix, &opts, false)
                .is_none(),
            "lookalike prefix server must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &lookalike_case, &opts, false)
                .is_none(),
            "case-variant server must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(
                &d, &db, &worker.id, &wt, &lookalike_other_nonce, &opts, false
            )
            .is_none(),
            "advisor (b) different nonce lookalike must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &lookalike_homo, &opts, false)
                .is_none(),
            "Unicode homoglyph server must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(
                &d, &db, &worker.id, &wt, &fixed_name_call, &opts, false
            )
            .is_none(),
            "advisor (a) fixed name on team seat must ask"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &git_tc, &opts, false)
                .is_none(),
            "worker git add must still ask under Always + wildcard"
        );
        let coord_hit = crate::code::decide_session_auto_approve(
            &d, &db, &coord.id, &wt, &item_note, &opts, false,
        );
        assert!(
            coord_hit
                .as_ref()
                .is_some_and(|(id, desc)| id == "allow_once" && desc.contains("item_note")),
            "coordinator item_note must still auto-allow: {coord_hit:?}"
        );

        // Advisor (a) note: non-Codex seats use title forms with the stored nonce.
        let claude_worker_note = claude_mcp("item_note", json!({ "itemId": "i1", "text": "progress" }));
        assert!(
            matched_tool(&d, "claude-code", &worker.id, &claude_worker_note).is_some(),
            "Claude worker must match title form with stored nonce"
        );
        let claude_coord_dispatch = claude_mcp(
            "dispatch_item",
            json!({ "itemId": "i1" }),
        );
        assert!(
            matched_tool(&d, "claude-code", &coord.id, &claude_coord_dispatch).is_some(),
            "Claude coordinator must match dispatch_item title form with stored nonce"
        );
        // Advisor (d): no stored nonce ⇒ fail closed; fixed name stored on a member ⇒ still no match.
        crate::agent_messaging::clear_injected_mcp_server_name_for_test(&worker.id);
        assert!(
            matched_tool(&d, "codex", &worker.id, &item_note).is_none(),
            "advisor (d) no nonce ⇒ no match"
        );
        crate::agent_messaging::set_injected_mcp_server_name_for_test(
            &worker.id,
            "patchbay-agent-messaging",
        );
        assert!(
            matched_tool(&d, "codex", &worker.id, &fixed_name_call).is_none(),
            "advisor (a)/(d) fixed name stored on member never exempts"
        );
        // Restore nonce for any later assertions in this fixture's drop path.
        inject(&worker.id);
        inject(&coord.id);
        let _ = claude_worker;
    }

    /// Advisor (c): non-team sessions keep the fixed MCP server name as their tool identifier.
    #[test]
    fn non_team_session_keeps_fixed_mcp_server_name() {
        let sid = format!("nonteam-fixed-{}", crate::db::new_id());
        let (bridge, listener) =
            crate::agent_messaging::AgentMessagingBridge::bind(None).unwrap();
        drop(listener);
        let def = bridge
            .server_definition(&sid, "codex", true, false)
            .unwrap();
        assert_eq!(
            def["name"].as_str().unwrap(),
            "patchbay-agent-messaging",
            "non-team tool identifier must stay the fixed name"
        );
        assert_eq!(
            crate::agent_messaging::injected_mcp_server_name(&sid).as_deref(),
            Some("patchbay-agent-messaging")
        );
        bridge.revoke(&sid);
    }

    /// Stage 6d (A): agent-origin briefs that would be slash commands become plain text.
    #[test]
    fn stage6d_agent_origin_slash_briefs_are_neutralized() {
        let cases = [
            "/mcp reconnect",
            "  \t/mcp reconnect",
            "/plan",
            "\u{FEFF}/mcp reconnect",
        ];
        for raw in cases {
            let body = neutralize_agent_origin_for_team_seat(raw.to_owned());
            let blocks = vec![json!({ "type": "text", "text": body })];
            assert!(
                parse_acp_slash_command(&blocks).is_none(),
                "neutralized {raw:?} must not be a slash command: {blocks:?}"
            );
            assert!(
                body.contains(raw.trim()) || body.contains(acp_slash_trim(raw)),
                "body still carries the original text as plain content"
            );
        }
        // Multi-block: resource first, slash text second — adapter only looks at prompt[0].
        let multi = vec![
            json!({ "type": "resource_link", "uri": "file:///x", "name": "x" }),
            json!({ "type": "text", "text": "/mcp reconnect" }),
        ];
        assert!(parse_acp_slash_command(&multi).is_none());
    }

    /// Stage 6d (B): `/mcp` that reaches a team seat revokes exemption until spawn.
    #[test]
    fn stage6d_mcp_slash_revokes_team_tool_exemption() {
        if skip_without_clean_git() {
            return;
        }
        clear_all_team_tool_exemption_revocations_for_test();
        let (d, wt, _proj, _item, _safe) = worker_commit_fixture(true);
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let worker = db.create_code_session(&wt, None, "codex", "always-approve").unwrap();
        TeamsDb::open(&d)
            .unwrap()
            .0
            .execute(
                "UPDATE members SET session_id = ?1 WHERE session_id = 'w1'",
                [&worker.id],
            )
            .unwrap();
        inject(&worker.id);
        let opts = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        let item_note = codex_mcp("item_note", json!({ "itemId": "i1", "text": "progress" }));
        assert!(
            crate::code::decide_session_auto_approve(
                &d, &db, &worker.id, &wt, &item_note, &opts, false
            )
            .is_some(),
            "before revoke, exact-nonce team tool allows"
        );
        // Over-broad detector: FEFF/NBSP/tab between mcp and args, and bare /MCP, all revoke.
        // `/mcpx` also revokes (acceptable over-revocation).
        for raw in [
            "/mcp\u{FEFF}reconnect",
            "/mcp\u{00A0}reconnect",
            "/MCP",
            "/mcp\treconnect",
            "/mcpx",
        ] {
            let blocks = vec![json!({ "type": "text", "text": raw })];
            assert!(acp_slash_is_mcp(&blocks), "must revoke for {raw:?}");
        }
        let human_mcp = vec![json!({ "type": "text", "text": "  /MCP reconnect" })];
        assert!(acp_slash_is_mcp(&human_mcp));
        revoke_team_tool_exemption(&worker.id);
        assert!(
            crate::code::decide_session_auto_approve(
                &d, &db, &worker.id, &wt, &item_note, &opts, false
            )
            .is_none(),
            "advisor (B) after /mcp: exact-nonce team tool must ask"
        );
        // Spawn mints a new nonce and clears revocation.
        let (bridge, listener) =
            crate::agent_messaging::AgentMessagingBridge::bind(None).unwrap();
        drop(listener);
        let _ = bridge
            .server_definition(&worker.id, "codex", true, true)
            .unwrap();
        assert!(!team_tool_exemption_revoked(&worker.id));
        // Re-inject the test nonce after spawn replaced it.
        inject(&worker.id);
        assert!(
            crate::code::decide_session_auto_approve(
                &d, &db, &worker.id, &wt, &item_note, &opts, false
            )
            .is_some(),
            "after new spawn, exemption returns"
        );
        bridge.revoke(&worker.id);
        clear_all_team_tool_exemption_revocations_for_test();
    }

    /// Caps mapping arms must equal `items::TOOLS` ∪ `{send_agent_message}` exactly.
    #[test]
    fn stage6d_team_tool_caps_parity() {
        use std::collections::BTreeSet;
        let mapped: BTreeSet<&str> = CAPS_MAPPED_TOOLS.iter().copied().collect();
        let required: BTreeSet<&str> = std::iter::once("send_agent_message")
            .chain(super::super::items::TOOLS.iter().copied())
            .collect();
        assert_eq!(
            mapped, required,
            "CAPS_MAPPED_TOOLS must equal items::TOOLS ∪ {{send_agent_message}}"
        );
        let all = super::super::Caps {
            writes_items: true,
            dispatches: true,
            decides: true,
            merges: true,
        };
        let none = super::super::Caps::default();
        for tool in CAPS_MAPPED_TOOLS {
            assert!(
                tool_cap_gate(tool).is_some(),
                "mapped name {tool} must have a tool_cap_gate arm"
            );
            let gate = tool_cap_gate(tool).unwrap();
            assert_eq!(tool_permitted_by_caps(tool, &all), gate(&all));
            assert_eq!(tool_permitted_by_caps(tool, &none), gate(&none));
        }
        assert!(
            tool_cap_gate("brand_new_unmapped_tool").is_none(),
            "unmapped tool must not auto-allow"
        );
        assert!(!tool_permitted_by_caps("brand_new_unmapped_tool", &all));
    }

    /// S1: G1(a–d) with a custom-named pool seat (`backend`) — floor keys off Role.standing.
    #[test]
    fn s1_custom_seat_name_still_hits_worker_git_floor() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        rekey_worker_seat(&d, "backend");
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let worker = db.create_code_session(&wt, None, "codex", "always-approve").unwrap();
        let outsider = db.create_code_session(&wt, None, "codex", "default").unwrap();
        let tdb = TeamsDb::open(&d).unwrap();
        tdb.0
            .execute(
                "UPDATE members SET session_id = ?1 WHERE session_id = 'w1'",
                [&worker.id],
            )
            .unwrap();
        assert!(is_worker_seat_member(&d, &worker.id), "backend seat is a worker");
        assert!(!is_worker_seat_member(&d, &outsider.id));
        db.add_permission_rule("*", "execute", "").unwrap();
        let opts = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        let template = git_cmd(&safe, "add -- hello.txt");
        let tc = |cmd: &str| {
            json!({
                "kind": "execute",
                "rawInput": { "command": cmd, "cwd": wt },
            })
        };
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc(&template), &opts, false).is_none(),
            "S1(a) custom seat + wildcard"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc(&template), &opts, false).is_none(),
            "S1(b) custom seat + always-approve mode"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc("git add -- x"), &opts, false).is_none(),
            "S1(c) custom seat + non-template"
        );
        let hit = crate::code::decide_session_auto_approve(&d, &db, &outsider.id, &wt, &tc("echo hi"), &opts, false);
        assert!(hit.is_some(), "S1(d) non-member still auto-approves: {hit:?}");
    }

    /// S1: mode setter refuses always-approve for a custom-named worker seat.
    #[test]
    fn s1_mode_setter_refuses_always_approve_for_custom_worker_seat() {
        let (d, wt, _proj, _item, _safe) = worker_commit_fixture(true);
        rekey_worker_seat(&d, "backend");
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let worker = db.create_code_session(&wt, None, "codex", "default").unwrap();
        TeamsDb::open(&d)
            .unwrap()
            .0
            .execute(
                "UPDATE members SET session_id = ?1 WHERE session_id = 'w1'",
                [&worker.id],
            )
            .unwrap();
        // Mirror lib.rs:3469 set_permission_mode_local gate (same predicate).
        assert!(
            is_worker_seat_member(&d, &worker.id),
            "mode setter must see backend seat as worker and refuse always-approve"
        );
    }

    /// T1: teams.db present but unreadable/corrupt ⇒ floor must ask; absent ⇒ ordinary.
    #[test]
    fn t1_corrupt_teams_db_floor_asks_absent_unchanged() {
        if skip_without_clean_git() {
            return;
        }
        let d = allow_non_temp_data_dir("t1-db");
        let cwd = d.to_string_lossy().into_owned();
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let sess = db.create_code_session(&cwd, None, "codex", "default").unwrap();
        db.add_permission_rule("*", "execute", "").unwrap();
        let opts = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        let tc = json!({
            "kind": "execute",
            "rawInput": { "command": "git status", "cwd": cwd },
        });
        // Absent teams.db: ordinary — wildcard rule may auto-approve.
        assert!(!d.join("teams.db").is_file());
        assert!(!is_worker_seat_member(&d, &sess.id), "absent DB ⇒ not a worker");
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &sess.id, &cwd, &tc, &opts, false).is_some(),
            "absent DB: ordinary auto-approve still works"
        );
        // Present but corrupt (not a SQLite DB): fail closed ⇒ floor asks.
        std::fs::write(d.join("teams.db"), b"not-a-sqlite-database").unwrap();
        assert!(
            is_worker_seat_member(&d, &sess.id),
            "corrupt DB ⇒ treat as worker"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &sess.id, &cwd, &tc, &opts, false).is_none(),
            "corrupt DB: floor must ask for git"
        );
    }

    /// T2: released membership + launched_as_worker still set ⇒ floor asks / Always refused;
    /// after clear (fresh non-worker spawn) ⇒ generic behaviour.
    #[test]
    fn t2_launched_as_worker_survives_release_until_clear() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let worker = db.create_code_session(&wt, None, "codex", "always-approve").unwrap();
        let tdb = TeamsDb::open(&d).unwrap();
        tdb.0
            .execute(
                "UPDATE members SET session_id = ?1 WHERE session_id = 'w1'",
                [&worker.id],
            )
            .unwrap();
        tdb.mark_launched_as_worker(&worker.id).unwrap();
        assert!(tdb.launched_as_worker(&worker.id).unwrap());
        // Release membership (left_at) — same as member_release detach.
        let team_id = tdb.membership(&worker.id).unwrap().unwrap().0.id;
        tdb.detach(&team_id, &worker.id).unwrap();
        assert!(
            tdb.membership(&worker.id).unwrap().is_none(),
            "membership gone"
        );
        assert!(
            tdb.launched_as_worker(&worker.id).unwrap(),
            "launch fact survives release"
        );
        assert!(
            is_worker_seat_member(&d, &worker.id),
            "floor still binds via launch fact"
        );
        db.add_permission_rule("*", "execute", "").unwrap();
        let opts = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        let template = git_cmd(&safe, "add -- hello.txt");
        let tc = json!({
            "kind": "execute",
            "rawInput": { "command": template, "cwd": wt },
        });
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc, &opts, false).is_none(),
            "released + launch fact ⇒ floor asks"
        );
        // Fresh non-worker spawn clears the fact.
        tdb.clear_launch_bound_facts(&worker.id).unwrap();
        assert!(!tdb.launched_as_worker(&worker.id).unwrap());
        assert!(!is_worker_seat_member(&d, &worker.id));
        let hit = crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc, &opts, false);
        assert!(hit.is_some(), "after clear: generic auto-approve: {hit:?}");
    }

    /// U1(a–f): process-launch flag survives DB unlink/replace/release; Always refused under floor.
    #[test]
    fn u1_process_launch_flag_survives_db_unlink_rename_release() {
        if skip_without_clean_git() {
            return;
        }
        clear_known_worker_sessions_for_test();
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let worker = db.create_code_session(&wt, None, "codex", "auto").unwrap();
        let outsider = db.create_code_session(&wt, None, "codex", "default").unwrap();
        let tdb = TeamsDb::open(&d).unwrap();
        tdb.0
            .execute(
                "UPDATE members SET session_id = ?1 WHERE session_id = 'w1'",
                [&worker.id],
            )
            .unwrap();
        tdb.mark_launched_as_worker(&worker.id).unwrap();
        remember_worker_session(&worker.id);
        let process = true;
        db.add_permission_rule("*", "execute", "").unwrap();
        let opts = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        let template = git_cmd(&safe, "add -- hello.txt");
        let tc = json!({
            "kind": "execute",
            "rawInput": { "command": template, "cwd": wt },
        });

        // (a) teams.db unlinked ⇒ still ask (process flag).
        let gone = d.join("teams.db.unlinked");
        std::fs::rename(d.join("teams.db"), &gone).unwrap();
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc, &opts, process)
                .is_none(),
            "U1(a) unlink + process flag ⇒ ask"
        );
        assert!(
            worker_floor_applies(&d, &worker.id, process),
            "U1(f) Always refused after unlink"
        );

        // (b) replaced by empty valid DB ⇒ still ask via process flag.
        let empty = allow_non_temp_data_dir("u1-empty-teams");
        let _empty_db = TeamsDb::open(&empty).unwrap();
        std::fs::copy(empty.join("teams.db"), d.join("teams.db")).unwrap();
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc, &opts, process)
                .is_none(),
            "U1(b) empty replacement DB + process flag ⇒ ask"
        );
        assert!(
            worker_floor_applies(&d, &worker.id, process),
            "U1(f) Always refused after replace"
        );

        // Restore original DB for (c).
        let _ = std::fs::remove_file(d.join("teams.db"));
        std::fs::rename(&gone, d.join("teams.db")).unwrap();
        let tdb = TeamsDb::open(&d).unwrap();
        let team_id = tdb.membership(&worker.id).unwrap().unwrap().0.id;
        tdb.detach(&team_id, &worker.id).unwrap();
        // stop must NOT clear persisted fact; even if cleared, process flag holds.
        tdb.clear_launched_as_worker(&worker.id).unwrap();
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc, &opts, process)
                .is_none(),
            "U1(c) released + cleared fact + process flag ⇒ ask"
        );
        assert!(
            worker_floor_applies(&d, &worker.id, process),
            "U1(f) Always refused after release"
        );

        // (d) non-worker, no DB ⇒ ordinary.
        clear_known_worker_sessions_for_test();
        let d2 = allow_non_temp_data_dir("u1-d");
        let cwd2 = d2.to_string_lossy().into_owned();
        let db2 = crate::db::Db::open(&d2.join("pb.sqlite")).unwrap();
        let plain = db2.create_code_session(&cwd2, None, "codex", "default").unwrap();
        db2.add_permission_rule("*", "execute", "").unwrap();
        let tc2 = json!({
            "kind": "execute",
            "rawInput": { "command": "git status", "cwd": cwd2 },
        });
        assert!(
            crate::code::decide_session_auto_approve(&d2, &db2, &plain.id, &cwd2, &tc2, &opts, false)
                .is_some(),
            "U1(d) non-worker absent DB ⇒ ordinary auto-approve"
        );

        // (e) next spawn as non-worker ⇒ generic.
        forget_worker_session(&worker.id);
        tdb.clear_launch_bound_facts(&worker.id).unwrap();
        let hit =
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc, &opts, false);
        assert!(hit.is_some(), "U1(e) non-worker relaunch ⇒ generic: {hit:?}");
        assert!(
            !worker_floor_applies(&d, &outsider.id, false),
            "outsider never under floor"
        );
        clear_known_worker_sessions_for_test();
    }

    /// X1: attach-only worker (daemon memory, no process-launch flag); teams.db replaced by a
    /// valid empty DB ⇒ floor still asks; always-approve refused via is_worker_seat_member.
    #[test]
    fn x1_attach_only_worker_empty_db_replacement_still_floors() {
        if skip_without_clean_git() {
            return;
        }
        clear_known_worker_sessions_for_test();
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let db = crate::db::Db::open(&d.join("pb.sqlite")).unwrap();
        let worker = db.create_code_session(&wt, None, "codex", "auto").unwrap();
        // Attach-only: remember without process flag / without requiring active membership after
        // the DB swap (team_attach path).
        remember_worker_session(&worker.id);
        assert!(daemon_knows_worker_session(&worker.id));
        db.add_permission_rule("*", "execute", "").unwrap();
        let opts = json!([
            { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
            { "kind": "reject_once", "optionId": "cancel", "name": "No" }
        ]);
        let template = git_cmd(&safe, "add -- hello.txt");
        let tc = json!({
            "kind": "execute",
            "rawInput": { "command": template, "cwd": wt },
        });
        // Replace teams.db with a valid empty DB (opens, no membership, no launch fact).
        let _ = std::fs::remove_file(d.join("teams.db"));
        let empty = allow_non_temp_data_dir("x1-empty-teams");
        let _ = TeamsDb::open(&empty).unwrap();
        std::fs::copy(empty.join("teams.db"), d.join("teams.db")).unwrap();
        // Attach-only: no process-launch flag (false).
        assert!(
            worker_floor_applies(&d, &worker.id, false),
            "X1: daemon-known after empty DB replace ⇒ floor"
        );
        assert!(
            is_worker_seat_member(&d, &worker.id),
            "X1: always-approve setter refuses (is_worker_seat_member)"
        );
        assert!(
            crate::code::decide_session_auto_approve(&d, &db, &worker.id, &wt, &tc, &opts, false)
                .is_none(),
            "X1: decide asks for worker git"
        );
        clear_known_worker_sessions_for_test();
    }

    /// G2: agent-supplied patchbayCommand is stripped; host meta wins.
    #[test]
    fn g2_strip_spoofed_patchbay_fields_before_enrich() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(Path::new(&wt).join("hello.txt"), "x").unwrap();
        let real = git_cmd(&safe, "add -- hello.txt");
        let mut raw = json!({
            "toolCall": {
                "kind": "execute",
                "patchbayCommand": "echo harmless",
                "patchbayCwd": "/tmp/spoof",
                "workerGitPrompt": false,
                "_meta": { "patchbay": { "command": "echo from meta", "cwd": "/spoof", "workerGitPrompt": false } },
                "rawInput": { "command": real, "cwd": wt },
            },
            "options": [
                { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
                { "kind": "allow_always", "optionId": "always", "name": "Always" },
                { "kind": "reject_once", "optionId": "cancel", "name": "No" },
                { "kind": "other_kind", "optionId": "weird", "name": "Weird" }
            ]
        });
        enrich_worker_git_permission(&d, "w1", &mut raw);
        let pb = &raw["toolCall"]["_meta"]["patchbay"];
        assert_eq!(pb["command"].as_str(), Some(real.as_str()), "host command, not spoof");
        assert_eq!(pb["cwd"].as_str(), Some(wt.as_str()));
        assert_eq!(pb["workerGitPrompt"], json!(true));
        assert!(raw["toolCall"].get("patchbayCommand").is_none());
        // G3: only allow_once + reject_once remain
        let kinds: Vec<&str> = raw["options"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| o["kind"].as_str())
            .collect();
        assert_eq!(kinds, vec!["allow_once", "reject_once"]);
        assert_eq!(pb["templateMatch"]["ok"], json!(true), "{pb:?}");
    }

    #[test]
    fn g4_template_match_reports_bare_git_and_missing_cwd() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, _safe) = worker_commit_fixture(true);
        let mut raw = json!({
            "toolCall": {
                "kind": "execute",
                "rawInput": { "command": "git add -- hello.txt", "cwd": wt },
            },
            "options": [
                { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
                { "kind": "reject_once", "optionId": "cancel", "name": "No" }
            ]
        });
        enrich_worker_git_permission(&d, "w1", &mut raw);
        let tm = &raw["toolCall"]["_meta"]["patchbay"]["templateMatch"];
        assert_eq!(tm["ok"], json!(false));
        assert!(tm["reason"].as_str().unwrap().contains("bare git"));
        assert!(
            raw["toolCall"]["askReason"]
                .as_str()
                .unwrap()
                .contains("NOT the worker template")
        );
    }

    /// S3: argv array whose element contains a space is never "exact worker template".
    #[test]
    fn s3_argv_array_with_space_element_never_exact_template() {
        if skip_without_clean_git() {
            return;
        }
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        // Space-joining this argv would look like a valid template; element-wise it is not.
        let argv = json!([
            git_abs(),
            "--no-pager",
            format!("-c"),
            format!("core.hooksPath={}", safe.display()),
            "-c", "core.fsmonitor=false",
            "-c", "core.sshCommand=",
            "-c", "protocol.ext.allow=never",
            "-c", "commit.gpgSign=false",
            "-c", "core.editor=:",
            "-c", "credential.helper=",
            "-c", "core.askPass=",
            "add", "--", "file with spaces.txt"
        ]);
        let mut raw = json!({
            "toolCall": {
                "kind": "execute",
                "rawInput": { "command": argv, "cwd": wt },
            },
            "options": [
                { "kind": "allow_once", "optionId": "allow_once", "name": "Yes" },
                { "kind": "reject_once", "optionId": "cancel", "name": "No" },
                { "kind": "allow_always", "optionId": "always", "name": "Always" }
            ]
        });
        enrich_worker_git_permission(&d, "w1", &mut raw);
        let tm = &raw["toolCall"]["_meta"]["patchbay"]["templateMatch"];
        assert_eq!(tm["ok"], json!(false), "{tm}");
        assert!(
            tm["reason"].as_str().unwrap().contains("argv array"),
            "S3 reason: {tm}"
        );
        assert_eq!(tm["label"], json!("NOT the worker template"));
        assert!(raw["toolCall"]["_meta"]["patchbay"]["commandArgv"].is_array());
    }
}
