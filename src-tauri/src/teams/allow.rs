//! The standing allow for team tools: a live team member's call to `send_agent_message` or a team
//! tool on Patchbay's own messaging server is approved once, without a prompt.
//!
//! Also (opt-in per template): a dispatched worker may run a strictly tokenized `git add` /
//! `git commit`, and read-only `ls` / `cat` / `git status` / `git diff` / `git log`, inside its
//! assigned worktree without a prompt. Tokenization is fail-closed (shell metacharacters rejected;
//! option allow-lists per verb; paths relative, no `..`, symlink-resolved inside the worktree).
//!
//! Auto-approved **git** commands must carry this exact prefix (fixed order) after `git` and before
//! the verb, with `core.hooksPath` equal to the Patchbay-created empty read-only hooks dir:
//! `git --no-pager -c core.hooksPath=<dir> -c core.fsmonitor=false -c core.sshCommand=
//! -c protocol.ext.allow=never -c commit.gpgSign=false -c core.editor=: -c credential.helper=
//! -c core.askPass=`. (`--no-pager` sits immediately after `git`, before every `-c`.)
//! `git commit` also requires `--no-verify` and either `-m`/`--message` or `-F`/`--file` (path
//! inside the worktree). Any other `-c`, missing piece, or different value falls through to a
//! normal prompt. Repo hooks/fsmonitor/config gates remain as defense in depth.
//!
//! Git auto-approve is further limited by agent kind: only Codex with a proven non-persisting
//! tool-shell (`agent_shell_export_persists` = `Some(false)`, version-pinned) and whose repo
//! common config, `config.worktree`, gitdir, `info/attributes`, and every effective config
//! origin (includes) all lie **outside** the workspace-write writable set (worktree cwd + temp;
//! see sbx-probe-home). Any effective `filter.*.clean|smudge|process` refuses `add`/`commit`.
//! OpenCode, Cursor, Claude Code, and other unsandboxed / unproven kinds always prompt for git.
//! Team worker seat spawn strips every `GIT_*` (`scrub_git_env`); mid-shell `export` is not under
//! Patchbay control. Diff/log auto-approve requires `--no-ext-diff` and `--no-textconv`.
//!
//! The `-c` prefix stops repo-config hook/fsmonitor changes made after the approval check from
//! executing, as long as the Patchbay hooks dir stays protected (`worker_safe_hooks_dir_ok` at
//! every approval). Residuals (attackers):
//! - the worker itself if `data_dir` is temp/worktree-writable (refused by location checks);
//! - an outside same-user process for the hooks dir under normal placement;
//! - mid-shell `export GIT_*` when the agent's tool-shell persists env across commands (kinds
//!   where that is proven or unproven never get git auto-approve);
//! - human-configured clean/smudge filters when config/attributes are writable (writable-set gate
//!   refuses auto-approve; when outside writable set the worker cannot plant them unasked);
//! - human-approved bare git stays check-then-exec racy;
//! - Codex `~/.codex` approved prefixes never reach the Patchbay permission path (pb-20261111-83).
//!
//! Not a `permission_rules` row: those match a tool kind and a command prefix, and MCP calls are
//! kind `other`, so a rule would approve every MCP tool of every server. This runs in the
//! `auto_approve` closure before `permissions::decide`. The host-side role check on each call
//! (`Caller::need`) remains the real control; this only spares the prompt.
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

/// `core.hooksPath=…` argument as it should appear in a brief / command.
/// Quoted when the path has spaces, tabs, or `~` (Windows short names) so the tokenizer accepts it.
pub(crate) fn hooks_path_c_arg(hooks_path: &Path) -> String {
    let inner = format!("core.hooksPath={}", path_for_git_c_flag(hooks_path));
    if inner.chars().any(|c| c == ' ' || c == '\t' || c == '~') {
        format!("\"{inner}\"")
    } else {
        inner
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

/// Exact hardening tokens after `git` and before the verb (unquoted form).
/// Placement: `git --no-pager -c …` (`--no-pager` before every `-c`).
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

/// When `tokens` is `git` + exact hardening prefix + rest, return the rest (`verb` …).
fn split_git_hardening<'a>(tokens: &'a [String], hooks_path: &Path) -> Option<&'a [String]> {
    if tokens.first().map(String::as_str) != Some("git") {
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

/// live22 RESULT observed adapter pin (`CODEX_ACP_PACKAGE` / docs). Any other version ⇒ unproven.
pub const LIVE22_CODEX_ACP_VERSION: &str = "2.2.2";

/// True when the in-tree Codex ACP package pin still matches the live22-proven version.
pub(crate) fn codex_acp_version_matches_live22() -> bool {
    crate::code::agents::CODEX_ACP_PACKAGE
        .rsplit_once('@')
        .is_some_and(|(_, v)| v == LIVE22_CODEX_ACP_VERSION)
}

/// Live22+ evidence: whether each agent's tool-shell persists `export` across commands.
/// `None` = unproven → no git auto-approve. `Some(true)` = persists → no auto-approve.
/// `Some(false)` = does not persist → may auto-approve when other gates pass (Codex only, and only
/// when [`codex_acp_version_matches_live22`] — a package bump without a new live proof ⇒ prompt).
pub(crate) fn agent_shell_export_persists(agent: &str) -> Option<bool> {
    match agent {
        // live22 export-persist (sandbox 4531) on codex-acp 2.2.2: mark empty → no persist.
        "codex" => {
            if !codex_acp_version_matches_live22() {
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

/// Only Codex with proven non-persisting shell may receive git auto-approve.
fn agent_may_git_auto_approve(agent: &str) -> bool {
    agent == "codex" && agent_shell_export_persists(agent) == Some(false)
}

const SERVER: &str = "patchbay-agent-messaging";

/// Agents whose title form is known (see the module note); the others still prompt.
pub fn has_rule(agent: &str) -> bool {
    matches!(agent, "claude-code" | "codex" | "cursor" | "antigravity" | "patchbay")
}

/// Tools a member may call without asking. Never `create_agent_session` or `share_artifact`.
fn allowed_tools() -> impl Iterator<Item = &'static str> {
    std::iter::once("send_agent_message").chain(super::items::TOOLS.iter().copied())
}

/// The titles `agent` gives a call of `tool` on our server; none for an agent without a known form.
fn titles(agent: &str, tool: &str) -> Vec<String> {
    match agent {
        "claude-code" => vec![format!("mcp__{SERVER}__{tool}")],
        "codex" => vec![format!("mcp.{SERVER}.{tool}")],
        "antigravity" => vec![format!("{SERVER}_{tool}")],
        "cursor" => vec![format!("{SERVER}-{tool}: {tool}"), format!("{SERVER}: {tool}")],
        "patchbay" => vec![format!("{tool} ({SERVER})")],
        _ => vec![],
    }
}

/// Keys that would make a call more than a message: a path or a command to run.
const BAD_KEYS: &[&str] = &["command", "cmd", "path", "paths", "file_path", "filePath", "cwd", "files", "uri", "url"];

fn carries_path_or_command(raw: &Value) -> bool {
    let bad = |o: &serde_json::Map<String, Value>| o.keys().any(|k| BAD_KEYS.contains(&k.as_str()));
    match raw {
        Value::Object(o) => bad(o) || o.get("arguments").and_then(Value::as_object).is_some_and(bad),
        _ => false,
    }
}

/// The tool a call is, when it is one of ours that a member may make unasked.
fn matched_tool(agent: &str, tool_call: &Value) -> Option<&'static str> {
    let title = tool_call["title"].as_str()?;
    let kind = tool_call["kind"].as_str().unwrap_or("");
    let raw = &tool_call["rawInput"];
    if carries_path_or_command(raw) {
        return None;
    }
    allowed_tools().find(|t| {
        titles(agent, t).iter().any(|x| x == title)
            && match kind {
                "other" => true,
                // Codex files its MCP calls under execute; require the MCP shape, not a shell command.
                "execute" => title.starts_with("mcp.") && raw["arguments"].is_object(),
                _ => false,
            }
    })
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// ---- Worker command allow-list (strict tokenizer) --------------------------------------------

/// Plain tokens after tokenization: `[A-Za-z0-9._/@:+,=-]+`.
fn is_plain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._/@:+,=-".contains(c)
}

fn is_forbidden_outside_message(c: char) -> bool {
    matches!(
        c,
        '\\' | '$' | '`' | '~' | '*' | '?' | '[' | ']' | '{' | '}' | '<' | '>' | '|' | '&' | ';' | '(' | ')' | '!' | '#' | '\n' | '\r' | '\0'
    ) || !c.is_ascii()
        || c.is_control()
}

fn is_forbidden_inside_message_quote(c: char) -> bool {
    matches!(c, '$' | '`' | '\\' | '!' | '\n' | '\r' | '\0') || !c.is_ascii() || c.is_control()
}

/// Quotes for `-c` values (may contain spaces in `core.hooksPath=…`); still no shell expansion.
fn is_forbidden_inside_c_quote(c: char) -> bool {
    matches!(c, '$' | '`' | '!' | '\n' | '\r' | '\0') || !c.is_ascii() || c.is_control()
}

/// Fail-closed tokenizer. Quotes for `git commit -m` / `--message` values and for `-c` values.
fn tokenize_worker_command(cmd: &str) -> Option<Vec<String>> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    let mut chars = cmd.chars().peekable();
    let mut expect_message = false;
    let mut expect_c_value = false;

    while let Some(&c) = chars.peek() {
        if c == ' ' || c == '\t' {
            chars.next();
            continue;
        }
        if expect_message && (c == '"' || c == '\'') {
            let quote = c;
            chars.next();
            let mut msg = String::new();
            loop {
                let ch = chars.next()?;
                if ch == quote {
                    break;
                }
                if is_forbidden_inside_message_quote(ch) {
                    return None;
                }
                msg.push(ch);
            }
            if msg.is_empty() {
                return None;
            }
            out.push(msg);
            expect_message = false;
            continue;
        }
        if expect_c_value && (c == '"' || c == '\'') {
            let quote = c;
            chars.next();
            let mut val = String::new();
            loop {
                let ch = chars.next()?;
                if ch == quote {
                    break;
                }
                if is_forbidden_inside_c_quote(ch) {
                    return None;
                }
                val.push(ch);
            }
            if val.is_empty() {
                return None;
            }
            out.push(val);
            expect_c_value = false;
            continue;
        }

        let mut tok = String::new();
        while let Some(&ch) = chars.peek() {
            if ch == ' ' || ch == '\t' {
                break;
            }
            if is_forbidden_outside_message(ch) || !is_plain_char(ch) {
                return None;
            }
            tok.push(ch);
            chars.next();
        }
        if tok.is_empty() {
            return None;
        }
        let is_msg_flag = tok == "-m" || tok == "--message";
        let is_c_flag = tok == "-c";
        out.push(tok);
        if expect_message {
            expect_message = false;
        } else if expect_c_value {
            expect_c_value = false;
        } else if is_msg_flag {
            expect_message = true;
        } else if is_c_flag {
            expect_c_value = true;
        }
    }
    if expect_message || expect_c_value {
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
/// option allow-lists; collect path args. `commit` requires `--no-verify` and `-m`/`--message`
/// or `-F`/`--file` (path inside the worktree).
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
                paths.push(t.clone());
                i += 1;
            }
            // -A/--all/-u may stand alone; otherwise at least one path.
            let has_all = rest.iter().any(|t| matches!(t.as_str(), "-A" | "--all" | "-u" | "--update"));
            if paths.is_empty() {
                return has_all;
            }
            all_path_args_ok(worktree, &paths)
        }
        "commit" => {
            // Require --no-verify and exactly one of -m/--message or -F/--file. No -q/-v/amend/--template.
            let mut saw_message = false;
            let mut saw_no_verify = false;
            let mut i = 0;
            while i < rest.len() {
                let t = &rest[i];
                if t == "--no-verify" {
                    saw_no_verify = true;
                    i += 1;
                    continue;
                }
                if t == "-m" || t == "--message" {
                    if saw_message {
                        return false;
                    }
                    let Some(msg) = rest.get(i + 1) else {
                        return false;
                    };
                    if msg.is_empty() || is_opt(msg) {
                        return false;
                    }
                    saw_message = true;
                    i += 2;
                    continue;
                }
                if t == "-F" || t == "--file" {
                    if saw_message {
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
                    saw_message = true;
                    i += 2;
                    continue;
                }
                return false;
            }
            saw_message && saw_no_verify
        }
        "status" => {
            let mut paths = Vec::new();
            for t in rest {
                if is_opt(t) {
                    if !GIT_STATUS_OPTS.contains(&t.as_str()) {
                        return false;
                    }
                } else {
                    paths.push(t.clone());
                }
            }
            all_path_args_ok(worktree, &paths)
        }
        "diff" => {
            let mut paths = Vec::new();
            for t in rest {
                if is_opt(t) {
                    if !GIT_DIFF_OPTS.contains(&t.as_str()) {
                        return false;
                    }
                } else {
                    paths.push(t.clone());
                }
            }
            requires_no_ext_diff_and_no_textconv(rest) && all_path_args_ok(worktree, &paths)
        }
        "log" => {
            let mut paths = Vec::new();
            for t in rest {
                if is_opt(t) {
                    if !GIT_LOG_OPTS.contains(&t.as_str()) {
                        return false;
                    }
                } else {
                    paths.push(t.clone());
                }
            }
            requires_no_ext_diff_and_no_textconv(rest) && all_path_args_ok(worktree, &paths)
        }
        _ => false,
    }
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
    match prog {
        "git" => {
            let Some(verb_rest) = split_git_hardening(&tokens, hooks_path) else {
                return false;
            };
            !verb_rest.is_empty() && parse_git_verb(verb_rest, worktree)
        }
        "ls" | "cat" => parse_ls_cat(&tokens, worktree),
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
fn parse_config_list_origins(raw: &str) -> Vec<(String, Option<PathBuf>, String)> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        let (scope, origin, kv) = match parts.as_slice() {
            [scope, origin, kv] => ((*scope).to_owned(), *origin, (*kv).to_owned()),
            // `--show-origin` alone: `<origin>\t<key>=<value>`
            [origin, kv] if origin.starts_with("file:") || origin.starts_with("command:") || origin.starts_with("blob:") => {
                (String::new(), *origin, (*kv).to_owned())
            }
            _ => continue,
        };
        let path = if let Some(p) = origin.strip_prefix("file:") {
            Some(PathBuf::from(p))
        } else {
            None
        };
        out.push((scope, path, kv));
    }
    out
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
    let entries = parse_config_list_origins(&raw);
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
    for (_scope, _path, kv) in parse_config_list_origins(&raw) {
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

/// Opt-in worktree allow: active worker on an assigned item, template opted in, cwd is the
/// assigned worktree, command is a safe git / ls / cat form. Git additionally requires a Codex
/// non-persisting shell and repo metadata outside the writable set.
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
    if member.seat != "workers" {
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
    if tokens.first().map(String::as_str) == Some("git") {
        if !agent_may_git_auto_approve(agent) {
            return None;
        }
        // F-E: attach/restore of an already-running session was never spawn-scrubbed → prompt.
        if !tdb.git_env_scrubbed(session).ok().unwrap_or(false) {
            return None;
        }
        let Some(writable) = worker_git_writable_roots(agent, &worktree) else {
            return None;
        };
        // Meta paths (common config / worktree config / gitdir / info/attributes) + every
        // effective config origin (includes local/worktree/include/includeIf/global/system).
        if !worker_git_meta_outside_writable(&worktree, &writable) {
            return None;
        }
        if !worker_git_config_origins_outside_writable(&worktree, &writable) {
            return None;
        }
        let Some(verb_rest) = split_git_hardening(&tokens, &hooks_path) else {
            return None;
        };
        let verb = verb_rest.first().map(String::as_str).unwrap_or("");
        // Fail closed: any clean/smudge/process filter (any origin) blocks add/commit —
        // worker-writable .gitattributes can select human-defined filters.
        if matches!(verb, "add" | "commit") && worker_git_has_filter_driver(&worktree) {
            return None;
        }
        // Defense in depth: still refuse when the repo's own hooks/fsmonitor look hostile.
        if worker_git_repo_blocks_auto(&worktree, verb) {
            return None;
        }
    }
    Some((allow_option(options)?, "Auto-approved: worker commit in worktree".into()))
}

/// True when `cmd` looks like it may invoke git: any whitespace/shell token whose basename is
/// `git` (wrappers, absolute paths, `command git`, `env git`, `sh -c '…git…'`). When unsure, true.
pub fn command_may_invoke_git(cmd: &str) -> bool {
    let trimmed = cmd.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    // Fast path: tokenizer accepted and first prog is exactly `git`.
    if let Some(tokens) = tokenize_worker_command(cmd) {
        if tokens.first().map(String::as_str) == Some("git") {
            return true;
        }
    }
    // Basename `git` on any path-like token (handles /usr/bin/git, rtk, env prefixes when split).
    for tok in cmd.split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '\'' | '"')) {
        if tok.is_empty() {
            continue;
        }
        let base = Path::new(tok).file_name().and_then(|s| s.to_str()).unwrap_or(tok);
        if base == "git" || base == "git.exe" {
            return true;
        }
    }
    // `sh -c` / shell strings: substring heuristic when parsing is ambiguous.
    let lower = cmd.to_ascii_lowercase();
    lower.contains(" git ") || lower.contains("\tgit ") || lower.contains("`git") || lower.contains("$(git")
        || lower.starts_with("git ") || lower.contains("/git ") || lower.ends_with("/git")
        || lower.contains("/git\"") || lower.contains("/git'")
}

/// When a live worker's execute may invoke git and is about to prompt, enrich `askReason` / allow
/// option names with [`WORKER_GIT_HOOKS_PROMPT`]. Host-controlled (ACP permission path) so the UI
/// still shows the sentence when the agent supplies its own title.
pub fn enrich_worker_git_permission(data_dir: &Path, session: &str, raw: &mut Value) {
    let tool_call = &raw["toolCall"];
    if tool_call["kind"].as_str() != Some("execute") {
        return;
    }
    let Some(cmd) = command_of(tool_call) else {
        return;
    };
    if !command_may_invoke_git(&cmd) {
        return;
    }
    if !data_dir.join("teams.db").is_file() {
        return;
    }
    let Ok(tdb) = TeamsDb::open(data_dir) else {
        return;
    };
    let Ok(Some((_, member))) = tdb.membership(session) else {
        return;
    };
    if member.seat != "workers" {
        return;
    }
    append_hooks_prompt(raw);
}

fn append_hooks_prompt(raw: &mut Value) {
    let phrase = WORKER_GIT_HOOKS_PROMPT;
    if let Some(o) = raw.get_mut("toolCall").and_then(|t| t.as_object_mut()) {
        let existing = o.get("askReason").and_then(Value::as_str).unwrap_or("").to_owned();
        if !existing.contains(phrase) {
            let next = if existing.is_empty() {
                phrase.to_owned()
            } else {
                format!("{existing}\n{phrase}")
            };
            o.insert("askReason".into(), json!(next));
        }
    }
    if let Some(opts) = raw.get_mut("options").and_then(Value::as_array_mut) {
        for opt in opts {
            let kind = opt.get("kind").and_then(Value::as_str).unwrap_or("");
            if !kind.starts_with("allow") {
                continue;
            }
            let name = opt.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
            if name.contains(phrase) {
                continue;
            }
            let next = if name.is_empty() {
                phrase.to_owned()
            } else {
                format!("{name} ({phrase})")
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
    if let Some(tool) = matched_tool(agent, tool_call) {
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

    fn call(title: &str, kind: &str, raw: Value) -> Value {
        json!({ "title": title, "kind": kind, "rawInput": raw })
    }

    fn options() -> Value {
        json!([{ "kind": "allow_always", "optionId": "always" }, { "kind": "allow_once", "optionId": "once" }])
    }

    #[test]
    fn each_agents_title_form_matches_and_nothing_else_does() {
        // Titles as the agents sent them (main Patchbay DB, 2026-11-09).
        for (agent, title, kind, raw) in [
            ("claude-code", "mcp__patchbay-agent-messaging__send_agent_message", "other", json!({ "message": "hi" })),
            ("codex", "mcp.patchbay-agent-messaging.send_agent_message", "execute", json!({ "arguments": { "message": "hi" } })),
            ("antigravity", "patchbay-agent-messaging_item_set", "other", json!({ "arguments": { "itemId": "x" } })),
            ("cursor", "patchbay-agent-messaging-send_agent_message: send_agent_message", "other", Value::Null),
            ("cursor", "patchbay-agent-messaging: dispatch_item", "other", Value::Null),
            ("patchbay", "send_agent_message (patchbay-agent-messaging)", "other", json!({})),
            ("claude-code", "mcp__patchbay-agent-messaging__member_release", "other", json!({})),
        ] {
            assert!(matched_tool(agent, &call(title, kind, raw)).is_some(), "{title}");
        }
        let claude = call("mcp__patchbay-agent-messaging__send_agent_message", "other", json!({}));
        for other in ["codex", "cursor", "antigravity", "patchbay", "opencode", "grok", "kilo"] {
            assert!(matched_tool(other, &claude).is_none(), "{other}");
        }
        assert!(matched_tool("claude-code", &call("patchbay-agent-messaging: send_agent_message", "other", json!({}))).is_none());
        for (title, kind, raw) in [
            ("mcp__patchbay-agent-messaging__create_agent_session", "other", json!({})),
            ("mcp__patchbay-agent-messaging__share_artifact", "other", json!({})),
            ("mcp__patchbay-agent-messaging__list_agent_sessions", "other", json!({})),
            ("mcp__other-server__send_agent_message", "other", json!({})),
            ("mcp__patchbay-agent-messaging__send_agent_message", "execute", json!({})),
            ("mcp.patchbay-agent-messaging.send_agent_message", "execute", json!({ "command": "x" })),
            ("mcp.patchbay-agent-messaging.send_agent_message", "execute", Value::Null),
            ("mcp__patchbay-agent-messaging__send_agent_message", "other", json!({ "cwd": "/" })),
            ("mcp__patchbay-agent-messaging__item_set", "other", json!({ "arguments": { "path": "/etc" } })),
            ("cargo test", "execute", json!({ "command": "cargo test" })),
        ] {
            assert!(matched_tool("claude-code", &call(title, kind, raw)).is_none(), "{title}");
        }
    }

    #[test]
    fn only_live_team_members_get_it_and_only_once() {
        let d = std::env::temp_dir().join(format!("pb-allow-{}", crate::db::new_id()));
        std::fs::create_dir_all(d.join("proj")).unwrap();
        let tc = call("mcp__patchbay-agent-messaging__team_roster", "other", json!({}));
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
        // Tests model a dispatch-spawned worker (F-E launched-scrubbed fact).
        t.mark_git_env_scrubbed("w1").unwrap();
        (d, worktree.to_string_lossy().into_owned(), proj.to_string_lossy().into_owned(), item_id.to_owned(), hooks)
    }

    /// Exact auto-approve prefix for git verbs (hooks path from fixture).
    fn git_pfx(hooks: &Path) -> String {
        format!(
            "git --no-pager -c {} -c core.fsmonitor=false -c core.sshCommand= -c protocol.ext.allow=never -c commit.gpgSign=false -c core.editor=: -c credential.helper= -c core.askPass=",
            hooks_path_c_arg(hooks)
        )
    }

    /// Agent eligible for git auto-approve in tests (Codex + proven non-persist).
    const GIT_AGENT: &str = "codex";

    fn git_cmd(hooks: &Path, rest: &str) -> String {
        format!("{} {rest}", git_pfx(hooks))
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
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        let hit = allow_team_tool(
            &d,
            "w1",
            GIT_AGENT,
            &git_exec(&wt, &git_cmd(&safe, "commit --no-verify -m work")),
            &options(),
            &wt,
        )
        .unwrap();
        assert_eq!(hit.0, "once");
        assert!(hit.1.contains("worker commit"));
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &git_cmd(&safe, "add LICENSE")), &options(), &wt).is_some());
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &git_cmd(&safe, "status")), &options(), &wt).is_some());
        assert!(allow_team_tool(
            &d,
            "w1",
            GIT_AGENT,
            &git_exec(&wt, &git_cmd(&safe, "diff --no-ext-diff --no-textconv")),
            &options(),
            &wt
        )
        .is_some());
        assert!(allow_team_tool(
            &d,
            "w1",
            GIT_AGENT,
            &git_exec(&wt, &git_cmd(&safe, "commit --no-verify -m \"hello world\"")),
            &options(),
            &wt
        )
        .is_some());
        // Bare git without the required -c prefix prompts.
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, "git commit --no-verify -m work"), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &git_cmd(&safe, "commit -m work")), &options(), &wt).is_none());
    }

    #[test]
    fn worker_readonly_ls_cat_git_log_accepted_when_paths_stay_inside() {
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
        for rest in [
            "log --no-ext-diff --no-textconv",
            "log -1 --no-ext-diff --no-textconv",
            "status",
            "diff --no-ext-diff --no-textconv",
            "status -s -b",
        ] {
            let cmd = git_cmd(&safe, rest);
            assert!(
                allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &cmd), &options(), &wt).is_some(),
                "should allow: {cmd}"
            );
        }
        // Missing either flag → prompt.
        for rest in [
            "diff",
            "diff --no-ext-diff",
            "diff --no-textconv",
            "log",
            "log --no-ext-diff",
            "log --no-textconv",
        ] {
            let cmd = git_cmd(&safe, rest);
            assert!(
                allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &cmd), &options(), &wt).is_none(),
                "should refuse without both flags: {cmd}"
            );
        }
    }

    #[test]
    fn worker_shell_metachar_and_message_file_bypasses_rejected() {
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
            "git --no-pager -c core.hooksPath=/tmp/evil -c core.fsmonitor=false -c core.sshCommand= -c protocol.ext.allow=never -c commit.gpgSign=false -c core.editor=: -c credential.helper= -c core.askPass= commit --no-verify -m x"
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
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        let commit = git_cmd(&safe, "commit --no-verify -m work");
        let add = git_cmd(&safe, "add LICENSE");
        let status = git_cmd(&safe, "status");
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some());

        let repo_hooks = resolve_hooks_dir(std::path::Path::new(&wt)).expect("hooks dir");
        // pre-push alone (David's templateDir) does not block commit/add.
        write_hook(&repo_hooks, "pre-push", true);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some(),
            "executable pre-push alone still auto-approves commit"
        );
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &add), &options(), &wt).is_some());

        // Non-executable commit-msg ignored on unix; on Windows any hook file fails closed
        // (`is_executable_file` treats existing files as executable — no reliable exec bit).
        write_hook(&repo_hooks, "commit-msg", false);
        #[cfg(unix)]
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some());
        #[cfg(not(unix))]
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_none(),
            "Windows: hook file present fails closed"
        );
        write_hook(&repo_hooks, "commit-msg", true);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_none(),
            "executable commit-msg refuses"
        );
        // add is not blocked by commit-msg.
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &add), &options(), &wt).is_some());
        std::fs::remove_file(repo_hooks.join("commit-msg")).unwrap();
        write_hook(&repo_hooks, "pre-auto-gc", true);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_none(),
            "executable pre-auto-gc refuses (commit may run auto-maintenance)"
        );
        std::fs::remove_file(repo_hooks.join("pre-auto-gc")).unwrap();
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some());

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
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some(),
            "pre-push in .git/hooks does not block"
        );
        write_hook(&main_hooks, "pre-commit", true);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_none(),
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
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some());
        write_hook(&external, "commit-msg", true);
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_none(),
            "external hooksPath commit-msg"
        );

        // post-index-change blocks add and status.
        std::fs::remove_file(external.join("commit-msg")).unwrap();
        write_hook(&external, "post-index-change", true);
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &add), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &status), &options(), &wt).is_none());

        // Empty / missing hooks dir → allow.
        let empty2 = std::path::Path::new(&wt).join("empty-hooks-2");
        std::fs::create_dir_all(&empty2).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.hooksPath", empty2.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some());
        let missing = std::path::Path::new(&wt).join("missing-hooks-dir");
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.hooksPath", missing.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some());
    }

    #[test]
    fn worker_commit_prompts_when_opt_in_is_off_by_default() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(false);
        assert!(allow_team_tool(
            &d,
            "w1",
            GIT_AGENT,
            &git_exec(&wt, &git_cmd(&safe, "commit --no-verify -m work")),
            &options(),
            &wt
        )
        .is_none());
    }

    #[test]
    fn worker_commit_outside_worktree_or_forbidden_git_prompts() {
        let (d, wt, proj, _item, safe) = worker_commit_fixture(true);
        let commit = git_cmd(&safe, "commit --no-verify -m work");
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
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
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
            "commit --no-verify -m work",
            "add LICENSE",
            "status",
            "diff --no-ext-diff --no-textconv",
        ] {
            let cmd = git_cmd(&safe, rest);
            assert!(
                allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &cmd), &options(), &wt).is_some(),
                "should allow: {cmd}"
            );
        }
    }

    /// Missing/empty request cwd is fail-closed even when the session cwd is the worktree.
    #[test]
    fn worker_commit_missing_cwd_refuses_even_with_session_cwd() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        let commit = git_cmd(&safe, "commit --no-verify -m work");
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec_no_cwd(&commit), &options(), &wt).is_none(),
            "omitted cwd must refuse"
        );
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec_empty_cwd(&commit), &options(), &wt).is_none(),
            "empty-string cwd must refuse (Cursor)"
        );
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec_empty_cwd(&git_cmd(&safe, "add LICENSE")), &options(), &wt).is_none(),
            "empty cwd git add must refuse"
        );
    }

    #[test]
    fn worker_commit_missing_cwd_without_session_record_refuses() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let commit = git_cmd(&safe, "commit --no-verify -m work");
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
        let (d, wt, proj, _item, safe) = worker_commit_fixture(true);
        let commit = git_cmd(&safe, "commit --no-verify -m work");
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
        let approved = git_cmd(&safe, "commit --no-verify -m planted-red");
        // Approval check while repo hooks are still empty / clean.
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &approved), &options(), &wt).is_some(),
            "must auto-approve before planting"
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
        let tdb = TeamsDb::open(&d).unwrap();
        let (team, _) = tdb.membership("w1").unwrap().unwrap();
        let wt_path = Path::new(&wt);
        let commit = git_cmd(&safe, "commit --no-verify -m x");
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &commit), &options(), &wt).is_some(),
            "baseline must approve"
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
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &git_cmd(&hooked, "commit --no-verify -m x")), &options(), &wt).is_none());

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
        let cmd = git_cmd(&hooks_temp, "commit --no-verify -m x");
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
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        let cmd = git_cmd(&safe, "status");
        for agent in ["claude-code", "opencode", "cursor", "antigravity", "patchbay", "grok"] {
            assert!(
                allow_team_tool(&d, "w1", agent, &git_exec(&wt, &cmd), &options(), &wt).is_none(),
                "{agent} must prompt for git"
            );
        }
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &cmd), &options(), &wt).is_some());
    }

    #[test]
    fn worker_git_auto_approve_requires_launched_scrubbed_fact() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let t = TeamsDb::open(&d).unwrap();
        t.0.execute("UPDATE members SET git_env_scrubbed = 0 WHERE session_id = 'w1'", [])
            .unwrap();
        let cmd = git_cmd(&safe, "status");
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &cmd), &options(), &wt).is_none(),
            "attach/restore without spawn scrub must prompt"
        );
        t.mark_git_env_scrubbed("w1").unwrap();
        assert!(allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &cmd), &options(), &wt).is_some());
    }

    #[test]
    fn codex_export_persist_tied_to_live22_acp_version() {
        assert_eq!(LIVE22_CODEX_ACP_VERSION, "2.2.2");
        assert!(
            crate::code::agents::CODEX_ACP_PACKAGE.ends_with("@2.2.2"),
            "bump requires new live proof; package={}",
            crate::code::agents::CODEX_ACP_PACKAGE
        );
        assert!(codex_acp_version_matches_live22());
        assert_eq!(agent_shell_export_persists("codex"), Some(false));
        assert!(agent_may_git_auto_approve("codex"));
    }

    #[test]
    fn worker_commit_message_file_inside_worktree_accepted() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let root = Path::new(&wt);
        std::fs::write(root.join("LICENSE"), "x").unwrap();
        std::fs::write(root.join("msg.txt"), "from-file").unwrap();
        assert!(
            allow_team_tool(
                &d,
                "w1",
                GIT_AGENT,
                &git_exec(&wt, &git_cmd(&safe, "commit --no-verify -F msg.txt")),
                &options(),
                &wt
            )
            .is_some()
        );
        assert!(
            allow_team_tool(
                &d,
                "w1",
                GIT_AGENT,
                &git_exec(&wt, &git_cmd(&safe, "commit --no-verify -F /etc/passwd")),
                &options(),
                &wt
            )
            .is_none()
        );
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
        let cmd = git_cmd(&safe, "add LICENSE");
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
        let cmd = git_cmd(&safe, "add LICENSE");
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
        assert!(
            allow_team_tool(&d, "w1", GIT_AGENT, &git_exec(&wt, &with_flags), &options(), &wt).is_some(),
            "both flags required for auto-approve"
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
        assert!(reason.contains(WORKER_GIT_HOOKS_PROMPT), "{reason}");
        assert!(raw["options"][0]["name"].as_str().unwrap().contains(WORKER_GIT_HOOKS_PROMPT));
        assert_eq!(raw["options"][1]["name"].as_str().unwrap(), "Deny");
        let _ = safe;
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
                raw["options"][0]["name"]
                    .as_str()
                    .unwrap_or("")
                    .contains(WORKER_GIT_HOOKS_PROMPT),
                "option name must carry hooks phrase for {cmd:?}"
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
}
