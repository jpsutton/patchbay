//! The standing allow for team tools: a live team member's call to `send_agent_message` or a team
//! tool on Patchbay's own messaging server is approved once, without a prompt.
//!
//! Also (opt-in per template): a dispatched worker may `git add` / `git commit` (and read-only
//! `git status` / `git diff`) inside its assigned worktree without a prompt. Push, branch delete,
//! force, and history rewrite stay prompted.
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
    cmd.contains(['\n', ';', '&', '|', '`', '>', '<']) || cmd.contains("$(")
}

/// True when `cmd` is a single safe git verb (add/commit/status/diff) with no rewrite/push/force.
fn is_safe_worker_git(cmd: &str) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() || is_compound(cmd) {
        return false;
    }
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    if tokens.first().copied() != Some("git") {
        return false;
    }
    // No directory / git-dir overrides that could aim the command elsewhere.
    if tokens.iter().any(|t| *t == "-C" || t.starts_with("--git-dir") || t.starts_with("--work-tree")) {
        return false;
    }
    // Skip `git -c key=val` pairs to find the subcommand.
    let mut i = 1;
    while i < tokens.len() {
        if tokens[i] == "-c" {
            i += 2;
            continue;
        }
        break;
    }
    let Some(verb) = tokens.get(i).copied() else {
        return false;
    };
    if !matches!(verb, "add" | "commit" | "status" | "diff") {
        return false;
    }
    // Force, amend, and rewrite stay prompted even when the verb looks safe.
    !tokens.iter().any(|t| matches!(*t, "-f" | "--force" | "--amend" | "--soft" | "--hard" | "--mixed"))
}

/// Opt-in worktree git allow: active worker on an assigned item, template opted in, cwd is the
/// assigned worktree, command is a safe git verb.
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
    if team.worktree_root.trim().is_empty() {
        return None;
    }
    let tpl = tdb.template(&team.template_id).ok()?;
    if !tpl.allow_worker_commit {
        return None;
    }
    let worktree = Path::new(&team.worktree_root).join(item_id.to_lowercase());
    let call_cwd = tool_call["rawInput"]["cwd"].as_str().unwrap_or(session_cwd);
    if !paths_equal(Path::new(call_cwd), &worktree) {
        return None;
    }
    let cmd = command_of(tool_call)?;
    if !is_safe_worker_git(&cmd) {
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
        let hit = allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git commit -m work"), &options(), &wt).unwrap();
        assert_eq!(hit.0, "once");
        assert!(hit.1.contains("worker commit"));
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git add LICENSE"), &options(), &wt).is_some());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git status"), &options(), &wt).is_some());
        assert!(allow_team_tool(&d, "w1", "claude-code", &git_exec(&wt, "git diff"), &options(), &wt).is_some());
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
}
