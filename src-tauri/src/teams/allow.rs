//! The standing allow for team tools: a live team member's call to `send_agent_message` or a team
//! tool on Patchbay's own messaging server is approved once, without a prompt.
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
use crate::code::permissions::allow_option;

const SERVER: &str = "patchbay-agent-messaging";

/// Tools a member may call without asking. Never `create_agent_session` or `share_artifact`.
fn allowed_tools() -> impl Iterator<Item = &'static str> {
    std::iter::once("send_agent_message").chain(super::items::TOOLS.iter().copied())
}

/// The titles each agent gives a call of `tool` on our server.
fn titles(tool: &str) -> [String; 6] {
    [
        format!("mcp__{SERVER}__{tool}"),   // Claude Code
        format!("mcp.{SERVER}.{tool}"),     // Codex (kind execute)
        format!("{SERVER}_{tool}"),         // Antigravity
        format!("{SERVER}-{tool}: {tool}"), // Cursor
        format!("{SERVER}: {tool}"),        // Cursor, other build
        format!("{tool} ({SERVER})"),       // Patchbay's agent
    ]
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
fn matched_tool(tool_call: &Value) -> Option<&'static str> {
    let title = tool_call["title"].as_str()?;
    let kind = tool_call["kind"].as_str().unwrap_or("");
    let raw = &tool_call["rawInput"];
    if carries_path_or_command(raw) {
        return None;
    }
    allowed_tools().find(|t| {
        titles(t).iter().any(|x| x == title)
            && match kind {
                "other" => true,
                // Codex files its MCP calls under execute; require the MCP shape, not a shell command.
                "execute" => title.starts_with("mcp.") && raw["arguments"].is_object(),
                _ => false,
            }
    })
}

/// The option to pick and what to tell the UI, when `tool_call` is a team member's call to one of
/// our tools. `session` must fill a seat on a live team.
pub fn allow_team_tool(data_dir: &Path, session: &str, tool_call: &Value, options: &Value) -> Option<(String, String)> {
    let tool = matched_tool(tool_call)?;
    // No teams.db means no teams; don't create one just to look.
    if !data_dir.join("teams.db").is_file() {
        return None;
    }
    TeamsDb::open(data_dir).ok()?.membership(session).ok()??;
    Some((allow_option(options)?, format!("Auto-approved: team tool {tool}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(title: &str, kind: &str, raw: Value) -> Value {
        json!({ "title": title, "kind": kind, "rawInput": raw })
    }

    #[test]
    fn each_agents_title_form_matches_and_nothing_else_does() {
        // Titles as the agents sent them (main Patchbay DB, 2026-11-09).
        for (title, kind, raw) in [
            ("mcp__patchbay-agent-messaging__send_agent_message", "other", json!({ "message": "hi" })),
            ("mcp.patchbay-agent-messaging.send_agent_message", "execute", json!({ "arguments": { "message": "hi" } })),
            ("patchbay-agent-messaging_item_set", "other", json!({ "arguments": { "itemId": "x" } })),
            ("patchbay-agent-messaging-send_agent_message: send_agent_message", "other", Value::Null),
            ("patchbay-agent-messaging: dispatch_item", "other", Value::Null),
            ("send_agent_message (patchbay-agent-messaging)", "other", json!({})),
            ("mcp__patchbay-agent-messaging__member_release", "other", json!({})),
        ] {
            assert!(matched_tool(&call(title, kind, raw)).is_some(), "{title}");
        }
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
            assert!(matched_tool(&call(title, kind, raw)).is_none(), "{title}");
        }
    }

    #[test]
    fn only_live_team_members_get_it_and_only_once() {
        let d = std::env::temp_dir().join(format!("pb-allow-{}", crate::db::new_id()));
        std::fs::create_dir_all(d.join("proj")).unwrap();
        let tc = call("mcp__patchbay-agent-messaging__team_roster", "other", json!({}));
        let options = json!([{ "kind": "allow_always", "optionId": "always" }, { "kind": "allow_once", "optionId": "once" }]);
        assert!(allow_team_tool(&d, "s1", &tc, &options).is_none(), "no teams.db yet");
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
        assert!(allow_team_tool(&d, "s1", &tc, &options).is_none(), "not a member");
        t.attach(&team.id, "s1", "coordinator").unwrap();
        assert_eq!(allow_team_tool(&d, "s1", &tc, &options).unwrap().0, "once");
        assert!(allow_team_tool(&d, "s1", &tc, &json!([{ "kind": "allow_always", "optionId": "a" }])).is_none(), "allow-once only");
        t.detach(&team.id, "s1").unwrap();
        assert!(allow_team_tool(&d, "s1", &tc, &options).is_none(), "released");
    }
}
