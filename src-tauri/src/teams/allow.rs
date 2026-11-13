//! The standing allow for team tools: a live team member's call to `send_agent_message` or a team
//! tool on Patchbay's own messaging server is approved once, without a prompt.
//!
//! Also (opt-in per template): a dispatched worker may run a strictly tokenized `git add` /
//! `git commit`, and read-only `ls` / `cat` / `git status` / `git diff` / `git log`, inside its
//! assigned worktree without a prompt. Tokenization is fail-closed (shell metacharacters rejected;
//! option allow-lists per verb; paths relative, no `..`, symlink-resolved inside the worktree).
//!
//! Auto-approved **git** commands must carry this exact `-c` prefix (fixed order) before the verb,
//! with `core.hooksPath` equal to the Patchbay-created empty read-only hooks dir for the team:
//! `-c core.hooksPath=<dir> -c core.fsmonitor=false -c core.sshCommand= -c protocol.ext.allow=never`.
//! `git commit` also requires `--no-verify`. Any other `-c`, missing piece, or different value falls
//! through to a normal prompt. Repo hooks/fsmonitor/config gates remain as defense in depth.
//!
//! Residual: config-defined filters / textconv / external diff; human-approved bare git stays
//! check-then-exec racy; scrubbed `GIT_*` / `GIT_CONFIG_NOSYSTEM` cannot be forced into the agent
//! shell (env-beats-config). This does not cover Codex workers under the user's `~/.codex` prefix
//! rules (pb-20261111-83).
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
use serde_json::Value;

use super::TeamsDb;
use crate::code::permissions::{allow_option, command_of};

/// Patchbay-owned empty hooks directory for this team (outside worktrees and temp roots).
pub(crate) fn worker_safe_hooks_dir(data_dir: &Path, team_id: &str) -> PathBuf {
    data_dir.join("teams-safe").join(team_id).join("hooks")
}

/// Forward-slash form for `git -c core.hooksPath=` (works on Windows git too; avoids `\\` tokens).
pub(crate) fn path_for_git_c_flag(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// `core.hooksPath=…` argument as it should appear in a brief / command (quoted if it has spaces).
pub(crate) fn hooks_path_c_arg(hooks_path: &Path) -> String {
    let inner = format!("core.hooksPath={}", path_for_git_c_flag(hooks_path));
    if inner.contains(' ') || inner.contains('\t') {
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

/// Exact `-c` sequence tokens after `git` and before the verb (unquoted form).
fn git_hardening_prefix_tokens(hooks_path: &Path) -> [String; 8] {
    [
        "-c".into(),
        format!("core.hooksPath={}", path_for_git_c_flag(hooks_path)),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.sshCommand=".into(),
        "-c".into(),
        "protocol.ext.allow=never".into(),
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
const GIT_DIFF_OPTS: &[&str] =
    &["--stat", "--name-only", "--name-status", "--cached", "--staged", "--no-ext-diff", "--no-textconv"];
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
];
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

/// Parse `git` args starting at the verb (hardening `-c` prefix already stripped) under per-verb
/// option allow-lists; collect path args. `commit` requires `--no-verify` and `-m`/`--message`.
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
            // Require -m/--message and --no-verify. No -q/-v/amend/-F/--template.
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
            all_path_args_ok(worktree, &paths)
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
            all_path_args_ok(worktree, &paths)
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
fn hardened_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .ok()?;
    if !out.status.success() {
        // `config --get` exits 1 when unset — treat as empty.
        if args.first() == Some(&"config") && out.status.code() == Some(1) {
            return Some(String::new());
        }
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
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

/// Non-empty `rawInput.cwd` from the request. Missing or empty is fail-closed: a recorded
/// session cwd only proves the start directory, not the shell's actual cwd (verifier: not
/// push-safe to fall back). Caller still requires this cwd to equal the item worktree.
fn worker_request_cwd(tool_call: &Value) -> Option<&str> {
    tool_call["rawInput"]["cwd"].as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// Opt-in worktree allow: active worker on an assigned item, template opted in, cwd is the
/// assigned worktree, command is a safe git / ls / cat form.
fn allow_worker_commit(data_dir: &Path, session: &str, tool_call: &Value, options: &Value, _session_cwd: &str) -> Option<(String, String)> {
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
    // Fail closed if the Patchbay hooks dir was never created/verified for this team.
    if !hooks_path.is_dir() {
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
        let Some(verb_rest) = split_git_hardening(&tokens, &hooks_path) else {
            return None;
        };
        let verb = verb_rest.first().map(String::as_str).unwrap_or("");
        // Defense in depth: still refuse when the repo's own hooks/fsmonitor look hostile.
        if worker_git_repo_blocks_auto(&worktree, verb) {
            return None;
        }
    }
    Some((allow_option(options)?, "Auto-approved: worker commit in worktree".into()))
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
    allow_worker_commit(data_dir, session, tool_call, options, session_cwd)
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

    fn worker_commit_fixture(opt_in: bool) -> (std::path::PathBuf, String, String, String, PathBuf) {
        let d = std::env::temp_dir().join(format!("pb-allow-git-{}", crate::db::new_id()));
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
        std::fs::create_dir_all(&worktree).unwrap();
        // Real git repo so config probes succeed. Host `init.templateDir` may install executable
        // hooks (e.g. pre-push); point hooksPath at an empty dir so defaults stay allowable.
        assert!(
            std::process::Command::new("git")
                .args(["-c", "init.defaultBranch=main", "init", "-q"])
                .current_dir(&worktree)
                .status()
                .unwrap()
                .success()
        );
        let empty_hooks = worktree.join(".pb-test-hooks-empty");
        std::fs::create_dir_all(&empty_hooks).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", worktree.to_str().unwrap(), "config", "core.hooksPath", empty_hooks.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        t.attach(&team.id, "w1", "workers").unwrap();
        t.0.execute("UPDATE members SET item_id = ?1 WHERE session_id = 'w1'", [item_id]).unwrap();
        (d, worktree.to_string_lossy().into_owned(), proj.to_string_lossy().into_owned(), item_id.to_owned(), hooks)
    }

    /// Exact auto-approve prefix for git verbs (hooks path from fixture).
    fn git_pfx(hooks: &Path) -> String {
        format!(
            "git -c {} -c core.fsmonitor=false -c core.sshCommand= -c protocol.ext.allow=never",
            hooks_path_c_arg(hooks)
        )
    }

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
            "claude-code",
            &git_exec(&wt, &git_cmd(&safe, "commit --no-verify -m work")),
            &options(),
            &wt,
        )
        .unwrap();
        assert_eq!(hit.0, "once");
        assert!(hit.1.contains("worker commit"));
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &git_cmd(&safe, "add LICENSE")), &options(), &wt).is_some());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &git_cmd(&safe, "status")), &options(), &wt).is_some());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &git_cmd(&safe, "diff")), &options(), &wt).is_some());
        assert!(allow_team_tool(
            &d,
            "w1",
            "claude-code",
            &git_exec(&wt, &git_cmd(&safe, "commit --no-verify -m \"hello world\"")),
            &options(),
            &wt
        )
        .is_some());
        // Bare git without the required -c prefix prompts.
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git commit --no-verify -m work"), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &git_cmd(&safe, "commit -m work")), &options(), &wt).is_none());
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
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, cmd), &options(), &wt).is_some(),
                "should allow: {cmd}"
            );
        }
        for rest in ["log", "log -1", "status", "diff", "status -s -b"] {
            let cmd = git_cmd(&safe, rest);
            assert!(
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &cmd), &options(), &wt).is_some(),
                "should allow: {cmd}"
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
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, cmd), &options(), &wt).is_none(),
                "should refuse: {cmd}"
            );
        }
        // Wrong hooksPath value with otherwise-valid sequence still refuses.
        let wrong = format!(
            "git -c core.hooksPath=/tmp/evil -c core.fsmonitor=false -c core.sshCommand= -c protocol.ext.allow=never commit --no-verify -m x"
        );
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &wrong), &options(), &wt).is_none());
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
            assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "cat leak"), &options(), &wt).is_none());
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
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &git_cmd(&safe, "status")), &options(), &wt).is_none(),
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
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_some());

        let repo_hooks = resolve_hooks_dir(std::path::Path::new(&wt)).expect("hooks dir");
        // pre-push alone (David's templateDir) does not block commit/add.
        write_hook(&repo_hooks, "pre-push", true);
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_some(),
            "executable pre-push alone still auto-approves commit"
        );
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &add), &options(), &wt).is_some());

        // Non-executable commit-msg ignored on unix; on Windows any hook file fails closed
        // (`is_executable_file` treats existing files as executable — no reliable exec bit).
        write_hook(&repo_hooks, "commit-msg", false);
        #[cfg(unix)]
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_some());
        #[cfg(not(unix))]
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_none(),
            "Windows: hook file present fails closed"
        );
        write_hook(&repo_hooks, "commit-msg", true);
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_none(),
            "executable commit-msg refuses"
        );
        // add is not blocked by commit-msg.
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &add), &options(), &wt).is_some());
        std::fs::remove_file(repo_hooks.join("commit-msg")).unwrap();
        write_hook(&repo_hooks, "pre-auto-gc", true);
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_none(),
            "executable pre-auto-gc refuses (commit may run auto-maintenance)"
        );
        std::fs::remove_file(repo_hooks.join("pre-auto-gc")).unwrap();
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_some());

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
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_some(),
            "pre-push in .git/hooks does not block"
        );
        write_hook(&main_hooks, "pre-commit", true);
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_none(),
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
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_some());
        write_hook(&external, "commit-msg", true);
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_none(),
            "external hooksPath commit-msg"
        );

        // post-index-change blocks add and status.
        std::fs::remove_file(external.join("commit-msg")).unwrap();
        write_hook(&external, "post-index-change", true);
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &add), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &status), &options(), &wt).is_none());

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
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_some());
        let missing = std::path::Path::new(&wt).join("missing-hooks-dir");
        assert!(
            std::process::Command::new("git")
                .args(["-C", &wt, "config", "core.hooksPath", missing.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &commit), &options(), &wt).is_some());
    }

    #[test]
    fn worker_commit_prompts_when_opt_in_is_off_by_default() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(false);
        assert!(allow_team_tool(
            &d,
            "w1",
            "claude-code",
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
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&proj, &commit), &options(), &proj).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec("/", &commit), &options(), "/").is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &git_cmd(&safe, "push origin HEAD")), &options(), &wt).is_none());
        assert!(allow_team_tool(
            &d,
            "w1",
            "claude-code",
            &git_exec(&wt, &git_cmd(&safe, "commit --amend --no-verify -m x")),
            &options(),
            &wt
        )
        .is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &git_cmd(&safe, "add -f secret")), &options(), &wt).is_none());
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
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, cmd), &options(), &wt).is_none(),
                "should refuse: {cmd}"
            );
        }
        for rest in ["commit --no-verify -m work", "add LICENSE", "status", "diff"] {
            let cmd = git_cmd(&safe, rest);
            assert!(
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &cmd), &options(), &wt).is_some(),
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
            allow_team_tool(&d, "w1", "claude-code", &git_exec_no_cwd(&commit), &options(), &wt).is_none(),
            "omitted cwd must refuse"
        );
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec_empty_cwd(&commit), &options(), &wt).is_none(),
            "empty-string cwd must refuse (Cursor)"
        );
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec_empty_cwd(&git_cmd(&safe, "add LICENSE")), &options(), &wt).is_none(),
            "empty cwd git add must refuse"
        );
    }

    #[test]
    fn worker_commit_missing_cwd_without_session_record_refuses() {
        let (d, wt, _proj, _item, safe) = worker_commit_fixture(true);
        let commit = git_cmd(&safe, "commit --no-verify -m work");
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec_no_cwd(&commit), &options(), "").is_none(),
            "no request cwd and empty session cwd"
        );
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec_empty_cwd(&commit), &options(), "   ").is_none(),
            "whitespace-only session cwd"
        );
        let _ = wt;
    }

    #[test]
    fn worker_commit_supplied_cwd_mismatch_refuses() {
        let (d, wt, proj, _item, safe) = worker_commit_fixture(true);
        let commit = git_cmd(&safe, "commit --no-verify -m work");
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&proj, &commit), &options(), &wt).is_none(),
            "request cwd is project root, not worktree — even when session_cwd is worktree"
        );
        assert!(
            allow_team_tool(&d, "w1", "claude-code", &git_exec("/tmp", &commit), &options(), &wt).is_none(),
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
            allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, &approved), &options(), &wt).is_some(),
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
        let doc_path = std::path::PathBuf::from("/tmp/pb-planted-red-raws.txt");
        std::fs::write(&doc_path, &doc).unwrap();
        eprintln!("{doc}");
        assert!(
            marker_after_bare.contains("planted-red"),
            "marker must appear without forced -c; marker={marker_after_bare:?}; raw={without_c_raw}"
        );
        assert!(doc_path.is_file(), "planted-red raws written to {}", doc_path.display());
    }
}
