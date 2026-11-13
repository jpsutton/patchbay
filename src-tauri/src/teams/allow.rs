//! The standing allow for team tools: a live team member's call to `send_agent_message` or a team
//! tool on Patchbay's own messaging server is approved once, without a prompt.
//!
//! Also (opt-in per template): a dispatched worker may `git add` / `git commit`, and read-only
//! `ls` / `cat` / `git status` / `git diff` / `git log`, inside its assigned worktree without a
//! prompt. Path args must resolve inside that worktree (no `..`, absolute escape, or symlink out).
//! Push, branch delete, force, history rewrite, and write/exec git options stay prompted.
//!
//! Not a `permission_rules` row: those match a tool kind and a command prefix, and MCP calls are
//! kind `other`, so a rule would approve every MCP tool of every server. This runs in the
//! `auto_approve` closure before `permissions::decide`. The host-side role check on each call
//! (`Caller::need`) remains the real control; this only spares the prompt.
//!
//! The title match is by exact string, one form per agent, taken from real transcripts
//! (docs/design/teams.md, "Tool-call titles"). OpenCode has no form here: it reaches MCP tools
//! only through an `execute` tool that runs JavaScript, which can't be allowed by title.

use std::path::Path;

use serde_json::Value;

use super::TeamsDb;
use crate::code::permissions::{allow_option, command_of};

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

/// Commands that chain, substitute or redirect can't be judged by their verb alone.
fn is_compound(cmd: &str) -> bool {
    cmd.contains(['\n', ';', '&', '|', '`', '>', '<']) || cmd.contains("$(") || cmd.contains("&&") || cmd.contains("||")
}

/// `VAR=value` prefix before the program (e.g. `FOO=bar git …`).
fn has_env_assignment_prefix(tokens: &[&str]) -> bool {
    tokens.first().is_some_and(|t| t.contains('=') && !t.starts_with('-'))
}

/// Git switches that retarget the repo or inject config/hooks/exec — never auto-approve.
fn has_forbidden_git_override(tokens: &[&str]) -> bool {
    tokens.iter().any(|t| {
        matches!(
            *t,
            "-c" | "-C" | "--config-env" | "--exec-path" | "--namespace" | "--git-dir" | "--work-tree"
        ) || t.starts_with("--config-env=")
            || t.starts_with("--exec-path=")
            || t.starts_with("--namespace=")
            || t.starts_with("--git-dir=")
            || t.starts_with("--work-tree=")
    })
}

/// Git log/diff options that write output or enable external filters/commands.
fn has_forbidden_git_write_or_exec(tokens: &[&str]) -> bool {
    tokens.iter().any(|t| {
        matches!(*t, "--output" | "--output-directory" | "--ext-diff" | "--textconv")
            || t.starts_with("--output=")
            || t.starts_with("--output-directory=")
            // `=`-carried command/filter payloads (e.g. `--ext-diff=…`, custom helpers).
            || (t.starts_with("--ext-diff=") || t.starts_with("--textconv="))
    })
}

/// Force / amend / soft-hard reset style flags — never auto-approve.
fn has_forbidden_git_force(tokens: &[&str]) -> bool {
    tokens.iter().any(|t| matches!(*t, "-f" | "--force" | "--amend" | "--soft" | "--hard" | "--mixed"))
}

/// `p` absolute against `root`, resolving symlinks through the longest existing prefix.
fn resolve_against(root: &Path, p: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, PathBuf};
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

fn path_inside_worktree(worktree: &Path, p: &str) -> bool {
    let Ok(root) = worktree.canonicalize() else {
        return false;
    };
    match resolve_against(&root, p) {
        Some(real) => real.strip_prefix(&root).is_ok(),
        None => false,
    }
}

/// Git options whose next token is a value, not a path (so we don't treat `-m msg` as a path).
fn git_opt_takes_value(tok: &str) -> bool {
    matches!(
        tok,
        "-m" | "--message"
            | "-F"
            | "--file"
            | "--author"
            | "--date"
            | "--cleanup"
            | "--fix"
            | "-t"
            | "--template"
            | "-n"
            | "--max-count"
            | "--skip"
            | "--since"
            | "--until"
            | "--after"
            | "--before"
            | "--grep"
            | "-S"
            | "-G"
            | "--diff-filter"
            | "--pretty"
            | "--format"
            | "--relative"
            | "--output"
            | "--output-directory"
    )
}

/// Positional path args after the program (and git verb), skipping options and their values.
fn path_args_after<'a>(tokens: &'a [&str], skip: usize) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = skip;
    let mut seen_double_dash = false;
    while i < tokens.len() {
        let t = tokens[i];
        if !seen_double_dash && t == "--" {
            seen_double_dash = true;
            i += 1;
            continue;
        }
        if !seen_double_dash && t.starts_with('-') && t.contains('=') {
            // `--opt=value` — value attached; not a path arg.
            i += 1;
            continue;
        }
        if !seen_double_dash && t.starts_with('-') {
            if git_opt_takes_value(t) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        out.push(t);
        i += 1;
    }
    out
}

/// True when every path argument resolves inside `worktree` (symlinks followed).
fn paths_stay_in_worktree(worktree: &Path, paths: &[&str]) -> bool {
    paths.iter().all(|p| path_inside_worktree(worktree, p))
}

/// Single safe worker command: git add/commit/status/diff/log, or ls/cat, with path containment.
fn is_safe_worker_command(cmd: &str, worktree: &Path) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() || is_compound(cmd) {
        return false;
    }
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    if has_env_assignment_prefix(&tokens) {
        return false;
    }
    let Some(prog) = tokens.first().copied() else {
        return false;
    };
    match prog {
        "git" => {
            if has_forbidden_git_override(&tokens)
                || has_forbidden_git_write_or_exec(&tokens)
                || has_forbidden_git_force(&tokens)
            {
                return false;
            }
            let Some(verb) = tokens.get(1).copied() else {
                return false;
            };
            if !matches!(verb, "add" | "commit" | "status" | "diff" | "log") {
                return false;
            }
            let paths = path_args_after(&tokens, 2);
            paths_stay_in_worktree(worktree, &paths)
        }
        "ls" | "cat" => {
            let paths = path_args_after(&tokens, 1);
            paths_stay_in_worktree(worktree, &paths)
        }
        _ => false,
    }
}

/// Opt-in worktree allow: active worker on an assigned item, template opted in, cwd is the
/// assigned worktree, command is a safe git / ls / cat form.
fn allow_worker_commit(data_dir: &Path, session: &str, tool_call: &Value, options: &Value, session_cwd: &str) -> Option<(String, String)> {
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
    let worktree = team.effective_worktree_root().ok()?.join(item_id.to_lowercase());
    let call_cwd = tool_call["rawInput"]["cwd"].as_str().unwrap_or(session_cwd);
    if !paths_equal(Path::new(call_cwd), &worktree) {
        return None;
    }
    let cmd = command_of(tool_call)?;
    if !is_safe_worker_command(&cmd, &worktree) {
        return None;
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
        // Another agent's form doesn't match, and an agent with no known form matches nothing.
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

    fn worker_commit_fixture(opt_in: bool) -> (std::path::PathBuf, String, String, String) {
        let d = std::env::temp_dir().join(format!("pb-allow-git-{}", crate::db::new_id()));
        let proj = d.join("proj");
        let trees = proj.join("trees");
        std::fs::create_dir_all(&trees).unwrap();
        let t = TeamsDb::open(&d).unwrap();
        // Copy built-in to a user template so opt-in can be saved.
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
        let item_id = "G-20261112-01";
        let worktree = trees.join(item_id.to_lowercase());
        std::fs::create_dir_all(&worktree).unwrap();
        t.attach(&team.id, "w1", "workers").unwrap();
        t.0.execute("UPDATE members SET item_id = ?1 WHERE session_id = 'w1'", [item_id]).unwrap();
        (d, worktree.to_string_lossy().into_owned(), proj.to_string_lossy().into_owned(), item_id.to_owned())
    }

    fn git_exec(cwd: &str, command: &str) -> Value {
        json!({ "title": command, "kind": "execute", "rawInput": { "command": command, "cwd": cwd } })
    }

    #[test]
    fn worker_commit_inside_own_worktree_when_template_opts_in() {
        let (d, wt, _proj, _item) = worker_commit_fixture(true);
        std::fs::write(std::path::Path::new(&wt).join("LICENSE"), "x").unwrap();
        let hit = allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git commit -m work"), &options(), &wt).unwrap();
        assert_eq!(hit.0, "once");
        assert!(hit.1.contains("worker commit"));
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git add LICENSE"), &options(), &wt).is_some());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git status"), &options(), &wt).is_some());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git diff"), &options(), &wt).is_some());
    }

    #[test]
    fn worker_readonly_ls_cat_git_log_accepted_when_paths_stay_inside() {
        let (d, wt, _proj, _item) = worker_commit_fixture(true);
        let root = std::path::Path::new(&wt);
        std::fs::write(root.join("README"), "hi").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let accept = ["ls", "ls src", "cat README", "git log", "git log -1", "git status", "git diff"];
        for cmd in accept {
            assert!(
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, cmd), &options(), &wt).is_some(),
                "should allow: {cmd}"
            );
        }
    }

    #[test]
    fn worker_readonly_path_escapes_and_write_exec_opts_rejected() {
        let (d, wt, proj, _item) = worker_commit_fixture(true);
        let root = std::path::Path::new(&wt);
        std::fs::write(root.join("README"), "hi").unwrap();
        let outside = std::path::Path::new(&proj).join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let link = root.join("leak");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let refuse = [
            "cat /etc/passwd",
            "cat ../../outside.txt",
            "cat leak",
            "ls /tmp",
            "ls ..",
            "git log --output=/tmp/out",
            "git log --output /tmp/out",
            "git log --ext-diff",
            "git log --textconv",
            "git log --ext-diff=/bin/true",
            "git log --textconv=/bin/true",
        ];
        for cmd in refuse {
            assert!(
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, cmd), &options(), &wt).is_none(),
                "should refuse: {cmd}"
            );
        }
    }

    #[test]
    fn worker_commit_prompts_when_opt_in_is_off_by_default() {
        let (d, wt, _proj, _item) = worker_commit_fixture(false);
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git commit -m work"), &options(), &wt).is_none());
    }

    #[test]
    fn worker_commit_outside_worktree_or_forbidden_git_prompts() {
        let (d, wt, proj, _item) = worker_commit_fixture(true);
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&proj, "git commit -m work"), &options(), &proj).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec("/", "git commit -m work"), &options(), "/").is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git push origin HEAD"), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git branch -d topic"), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git branch -D topic"), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git rebase main"), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git reset --hard HEAD~1"), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git commit --amend -m x"), &options(), &wt).is_none());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git add -f secret"), &options(), &wt).is_none());
    }

    #[test]
    fn worker_git_config_and_path_overrides_are_never_auto_approved() {
        let (d, wt, _proj, _item) = worker_commit_fixture(true);
        let refuse = [
            "git -c core.hooksPath=/tmp/hooks commit -m x",
            "git -c core.fsmonitor=/tmp/fsmon status",
            "git --config-env=http.extraHeader=TOKEN commit -m x",
            "git --config-env=core.hooksPath=HOOKS status",
            "git --exec-path=/tmp/evil commit -m x",
            "git --exec-path=/tmp/evil status",
            "git --git-dir=/tmp/other.git commit -m x",
            "git --git-dir=/tmp/other.git status",
            "git --work-tree=/tmp/elsewhere commit -m x",
            "git -C /tmp/elsewhere commit -m x",
            "git --namespace=evil commit -m x",
            "git --namespace=evil status",
            "FOO=bar git commit -m x",
            "HOOKS=/tmp/h git status",
            "git commit -m x && git status",
            "git status || true",
        ];
        for cmd in refuse {
            assert!(
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, cmd), &options(), &wt).is_none(),
                "should refuse: {cmd}"
            );
        }
        // Existing allowed forms still pass.
        for cmd in ["git commit -m work", "git add LICENSE", "git status", "git diff"] {
            assert!(
                allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, cmd), &options(), &wt).is_some(),
                "should allow: {cmd}"
            );
        }
    }
}
