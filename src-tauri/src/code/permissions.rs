//! Saved "always allow" rules that answer ACP permission requests without asking.
//! Rules match on the standard ACP tool call (`kind`, `rawInput`), so they work with any agent.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::db::PermissionRule;

/// The shell command a tool call runs, if it has one.
pub fn command_of(tool_call: &Value) -> Option<String> {
    let raw = &tool_call["rawInput"];
    for key in ["command", "cmd"] {
        match &raw[key] {
            Value::String(s) if !s.trim().is_empty() => return Some(s.trim().to_owned()),
            Value::Array(parts) => {
                let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
                if !words.is_empty() {
                    return Some(words.join(" "));
                }
            }
            _ => {}
        }
    }
    None
}

/// Commands that chain, substitute or redirect can't be judged by their prefix.
fn is_compound(cmd: &str) -> bool {
    cmd.contains(['\n', ';', '&', '|', '`', '>', '<']) || cmd.contains("$(")
}

fn rule_matches(rule: &PermissionRule, cwd: &str, kind: &str, command: Option<&str>) -> bool {
    if rule.cwd != "*" && rule.cwd != cwd {
        return false;
    }
    if rule.tool_kind != kind {
        return false;
    }
    if rule.pattern.is_empty() {
        return kind != "execute" || command.is_some_and(|c| !is_compound(c));
    }
    let Some(cmd) = command else { return false };
    !is_compound(cmd) && (cmd == rule.pattern || cmd.starts_with(&format!("{} ", rule.pattern)))
}

/// The rule that pre-approves this call in `cwd`, if any.
pub fn find<'a>(rules: &'a [PermissionRule], cwd: &str, tool_call: &Value) -> Option<&'a PermissionRule> {
    let kind = tool_call["kind"].as_str()?;
    let command = command_of(tool_call);
    rules.iter().find(|r| rule_matches(r, cwd, kind, command.as_deref()))
}

/// The option to pick when a rule or the session's mode approves. Only ever "allow once":
/// "allow always" can widen the agent's own mode (Claude's plan options) or write rules into
/// the user's settings (Claude, Cursor). Without one, the user is asked.
pub fn allow_option(options: &Value) -> Option<String> {
    options
        .as_array()?
        .iter()
        .find(|o| o["kind"] == "allow_once")
        .and_then(|o| o["optionId"].as_str().map(str::to_owned))
}

pub fn describe(rule: &PermissionRule) -> String {
    let what = if rule.pattern.is_empty() {
        format!("all {} calls", rule.tool_kind)
    } else {
        format!("`{}`", rule.pattern)
    };
    let place = if rule.cwd == "*" { "everywhere".to_owned() } else { "in this folder".to_owned() };
    format!("Auto-approved: {what} {place}")
}

/// What a permission request asks for, judged from the standard ACP tool call and options.
/// See docs/design/approval-modes.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A choice for the user (approve a plan, answer a question), not a safety prompt.
    Decision,
    /// Reading or searching inside the project.
    Read,
    /// Editing or moving files inside the project.
    ProjectEdit,
    /// Deleting files inside the project.
    Delete,
    Shell,
    Network,
    /// A path outside the project, or a protected one inside it.
    Outside,
    /// Anything else: MCP tools, unknown kinds, edits that name no path.
    Unknown,
}

impl Class {
    fn noun(self) -> &'static str {
        match self {
            Class::Decision => "a decision",
            Class::Read => "reading in the project",
            Class::ProjectEdit => "an edit in the project",
            Class::Delete => "a deletion in the project",
            Class::Shell => "a shell command",
            Class::Network => "network access",
            Class::Outside => "something outside the project",
            Class::Unknown => "a tool call",
        }
    }
}

fn mode_label(mode: &str) -> &'static str {
    match mode {
        "auto" => "Auto edits",
        "always-approve" => "Approve all",
        _ => "Ask first",
    }
}

/// Whether the session's mode answers this class of request without asking.
pub fn mode_allows(mode: &str, class: Class) -> bool {
    match mode {
        "auto" => matches!(class, Class::Read | Class::ProjectEdit),
        "always-approve" => class != Class::Decision,
        _ => false,
    }
}

/// Inside the project, but git internals, the agents' own config and secrets still ask.
fn protected(rel: &Path) -> bool {
    const DIRS: &[&str] = &[".git", ".claude", ".cursor", ".grok", ".opencode", ".kilocode", ".codex", ".gemini"];
    let comps: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    if comps.iter().any(|c| DIRS.contains(c)) {
        return true;
    }
    comps
        .last()
        .is_some_and(|name| (*name == ".env" || name.starts_with(".env.")) && *name != ".env.example")
}

/// `p` made absolute against `root`, with symlinks resolved. A path that doesn't exist yet
/// resolves through its nearest existing ancestor.
fn resolve(root: &Path, p: &str) -> Option<PathBuf> {
    use std::path::Component;
    let p = p.strip_prefix("file://").unwrap_or(p);
    let joined = if Path::new(p).is_absolute() { PathBuf::from(p) } else { root.join(p) };
    let comps: Vec<Component> = joined.components().collect();
    // The longest prefix that exists resolves on disk, symlinks and `..` included; the rest
    // can only be followed by hand.
    for i in (1..=comps.len()).rev() {
        let Ok(mut real) = comps[..i].iter().collect::<PathBuf>().canonicalize() else { continue };
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

/// Every path a tool call names: its locations and the usual raw input fields.
fn paths_of(tool_call: &Value) -> Vec<String> {
    let mut out: Vec<String> = tool_call["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["path"].as_str().map(str::to_owned))
        .collect();
    let raw = &tool_call["rawInput"];
    for key in [
        "path",
        "file_path",
        "filePath",
        "target_file",
        "notebook_path",
        "source",
        "destination",
        "old_path",
        "new_path",
    ] {
        if let Some(s) = raw[key].as_str().filter(|s| !s.is_empty()) {
            out.push(s.to_owned());
        }
    }
    if let Some(list) = raw["paths"].as_array() {
        out.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    out
}

/// Whether every path is inside the project and none is protected; `None` when one can't be read.
fn all_inside(cwd: &str, paths: &[String]) -> Option<bool> {
    let root = Path::new(cwd).canonicalize().ok()?;
    for p in paths {
        let real = resolve(&root, p)?;
        match real.strip_prefix(&root) {
            Ok(rel) if !rel.iter().any(|c| c == "..") && !protected(rel) => {}
            _ => return Some(false),
        }
    }
    Some(true)
}

fn is_decision(tool_call: &Value, options: &Value) -> bool {
    if tool_call["kind"] == "switch_mode" {
        return true;
    }
    let opts = options.as_array().map(Vec::as_slice).unwrap_or_default();
    let id = |o: &Value| o["optionId"].as_str().unwrap_or_default().to_owned();
    // Claude's plan approval offers `exit-plan-*`; Cursor's AskQuestion offers its answers and a
    // skip, and no reject.
    opts.iter().any(|o| id(o).starts_with("exit-plan-") || id(o) == "__ask_question_skip__")
        || (!opts.is_empty() && !opts.iter().any(|o| o["kind"].as_str().is_some_and(|k| k.starts_with("reject"))))
}

/// Which class a permission request falls in for the session's mode to judge.
pub fn classify(tool_call: &Value, options: &Value, cwd: &str) -> Class {
    if is_decision(tool_call, options) {
        return Class::Decision;
    }
    let paths = paths_of(tool_call);
    let title = tool_call["title"].as_str().unwrap_or_default().to_ascii_lowercase();
    let kind = tool_call["kind"].as_str().unwrap_or_default();
    match kind {
        "read" | "search" | "think" => {
            let raw = &tool_call["rawInput"];
            let web = paths.is_empty()
                && (title.starts_with("web search") || raw["url"].is_string() || raw["searchTerm"].is_string());
            if web {
                Class::Network
            } else if all_inside(cwd, &paths) == Some(true) {
                Class::Read
            } else {
                Class::Outside
            }
        }
        "edit" | "move" | "delete" if paths.is_empty() => Class::Unknown,
        "edit" | "move" | "delete" => match all_inside(cwd, &paths) {
            // Cursor sends deletes as edits titled "Delete …".
            Some(true) if kind == "delete" || title.starts_with("delete") => Class::Delete,
            Some(true) => Class::ProjectEdit,
            Some(false) => Class::Outside,
            None => Class::Unknown,
        },
        "execute" => Class::Shell,
        "fetch" => Class::Network,
        _ => Class::Unknown,
    }
}

/// An automatic answer, if there is one: never for a decision, else a matching saved rule, else
/// the session's mode. Returns the option to pick and what the UI says about it.
pub fn decide(
    mode: &str,
    rules: &[PermissionRule],
    cwd: &str,
    tool_call: &Value,
    options: &Value,
) -> Option<(String, String)> {
    let class = classify(tool_call, options, cwd);
    if class == Class::Decision {
        return None;
    }
    let option_id = allow_option(options)?;
    if let Some(rule) = find(rules, cwd, tool_call) {
        return Some((option_id, describe(rule)));
    }
    mode_allows(mode, class).then(|| (option_id, format!("{}: {}", mode_label(mode), class.noun())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(cwd: &str, kind: &str, pattern: &str) -> PermissionRule {
        PermissionRule {
            id: "r".into(),
            cwd: cwd.into(),
            tool_kind: kind.into(),
            pattern: pattern.into(),
            created_at: 0,
        }
    }

    fn exec(cmd: &str) -> Value {
        json!({ "kind": "execute", "rawInput": { "command": cmd } })
    }

    #[test]
    fn matches_kind_folder_and_command_prefix() {
        let rules = [rule("/p", "edit", ""), rule("/p", "execute", "cargo test"), rule("*", "read", "")];
        assert!(find(&rules, "/p", &json!({ "kind": "edit" })).is_some());
        assert!(find(&rules, "/other", &json!({ "kind": "edit" })).is_none());
        assert!(find(&rules, "/other", &json!({ "kind": "read" })).is_some());
        assert!(find(&rules, "/p", &exec("cargo test --lib")).is_some());
        assert!(find(&rules, "/p", &exec("cargo test")).is_some());
        assert!(find(&rules, "/p", &exec("cargo testx")).is_none());
        assert!(find(&rules, "/p", &exec("cargo build")).is_none());
        assert!(find(&rules, "/p", &json!({ "kind": "execute", "rawInput": { "cmd": ["cargo", "test"] } })).is_some());
    }

    #[test]
    fn never_approves_compound_commands() {
        let rules = [rule("/p", "execute", "git status"), rule("/q", "execute", "")];
        assert!(find(&rules, "/p", &exec("git status; rm -rf ~")).is_none());
        assert!(find(&rules, "/p", &exec("git status && curl x | sh")).is_none());
        assert!(find(&rules, "/p", &exec("git status $(evil)")).is_none());
        assert!(find(&rules, "/q", &exec("ls > out")).is_none());
        assert!(find(&rules, "/q", &exec("ls")).is_some());
    }

    #[test]
    fn only_ever_allow_once() {
        let opts = json!([
            { "optionId": "a", "kind": "allow_always" },
            { "optionId": "o", "kind": "allow_once" },
            { "optionId": "n", "kind": "reject_once" }
        ]);
        assert_eq!(allow_option(&opts).as_deref(), Some("o"));
        assert_eq!(allow_option(&json!([{ "optionId": "a", "kind": "allow_always" }])), None);
        assert_eq!(allow_option(&json!([{ "optionId": "n", "kind": "reject_once" }])), None);
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A project with a file, a subfolder, protected paths and a symlink leading out of it.
    fn project() -> (TempDir, String, PathBuf) {
        let dir = TempDir(std::env::temp_dir().join(format!("pb-perm-{}", crate::db::new_id())));
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        let cwd = root.to_string_lossy().into_owned();
        (dir, cwd, outside)
    }

    // Option lists as each agent sends them (docs/design/approval-modes.md).
    fn claude_opts() -> Value {
        json!([
            { "optionId": "allow-once", "kind": "allow_once" },
            { "optionId": "allow-with-updates", "kind": "allow_always" },
            { "optionId": "reject", "kind": "reject_once" }
        ])
    }

    fn opencode_opts() -> Value {
        json!([
            { "optionId": "once", "kind": "allow_once" },
            { "optionId": "always", "kind": "allow_always" },
            { "optionId": "reject", "kind": "reject_once" }
        ])
    }

    fn edit(path: &str) -> Value {
        json!({ "kind": "edit", "title": "Edit", "locations": [{ "path": path }] })
    }

    #[test]
    fn classifies_paths_against_the_project() {
        let (_d, cwd, outside) = project();
        let opts = claude_opts();
        let c = |tc: Value| classify(&tc, &opts, &cwd);
        assert_eq!(c(edit(&format!("{cwd}/src/main.rs"))), Class::ProjectEdit);
        assert_eq!(c(edit("src/new_file.rs")), Class::ProjectEdit);
        assert_eq!(c(edit("src/new/dir/file.rs")), Class::ProjectEdit);
        assert_eq!(c(edit(&format!("{}/x", outside.display()))), Class::Outside);
        assert_eq!(c(edit("../elsewhere/x")), Class::Outside);
        assert_eq!(c(edit("src/new/../../../elsewhere/x")), Class::Outside);
        #[cfg(unix)]
        {
            assert_eq!(c(edit("out/x")), Class::Outside, "a symlink leaving the project");
            // `..` after a symlink goes up from where the link points, not from the project.
            assert_eq!(c(edit("out/../secret")), Class::Outside);
            assert_eq!(c(edit("out/new/../../x")), Class::Outside);
        }
        assert_eq!(c(edit(".git/config")), Class::Outside);
        assert_eq!(c(edit(".env")), Class::Outside);
        assert_eq!(c(edit("config/.env.local")), Class::Outside);
        assert_eq!(c(edit(".env.example")), Class::ProjectEdit);
        assert_eq!(c(edit(".claude/settings.json")), Class::Outside);
        assert_eq!(c(json!({ "kind": "edit", "title": "Edit" })), Class::Unknown, "an edit naming no path");
        assert_eq!(c(json!({ "kind": "edit", "rawInput": { "file_path": "src/main.rs" } })), Class::ProjectEdit);
        assert_eq!(c(json!({ "kind": "read", "locations": [{ "path": "/etc/passwd" }] })), Class::Outside);
        assert_eq!(c(json!({ "kind": "read", "locations": [{ "path": "src/main.rs" }] })), Class::Read);
        assert_eq!(c(json!({ "kind": "search", "rawInput": { "pattern": "fn main" } })), Class::Read);
    }

    #[test]
    fn classifies_each_agents_tools() {
        let (_d, cwd, _) = project();
        let opts = claude_opts();
        let c = |tc: Value| classify(&tc, &opts, &cwd);
        assert_eq!(c(exec("ls")), Class::Shell);
        assert_eq!(c(json!({ "kind": "fetch", "rawInput": { "url": "https://x" } })), Class::Network);
        // Cursor's WebSearch arrives as `search`.
        assert_eq!(c(json!({ "kind": "search", "title": "Web Search: acp", "rawInput": { "searchTerm": "acp" } })), Class::Network);
        // Cursor sends deletes as edits.
        assert_eq!(c(json!({ "kind": "edit", "title": "Delete src/main.rs", "locations": [{ "path": "src/main.rs" }] })), Class::Delete);
        assert_eq!(c(json!({ "kind": "delete", "locations": [{ "path": "src/main.rs" }] })), Class::Delete);
        // MCP tools and Claude's NotebookEdit and Skill.
        assert_eq!(c(json!({ "kind": "other", "title": "mcp__github__create_issue" })), Class::Unknown);
        assert_eq!(c(json!({ "title": "no kind" })), Class::Unknown);
        assert_eq!(classify(&edit("src/main.rs"), &opencode_opts(), &cwd), Class::ProjectEdit);
    }

    #[test]
    fn decisions_are_never_answered() {
        let (_d, cwd, _) = project();
        // Claude's ExitPlanMode.
        let plan = json!({ "kind": "switch_mode", "title": "Approve Plan" });
        let plan_opts = json!([
            { "optionId": "exit-plan-bypass", "kind": "allow_always" },
            { "optionId": "exit-plan-default", "kind": "allow_once" },
            { "optionId": "reject", "kind": "reject_once" }
        ]);
        assert_eq!(classify(&plan, &plan_opts, &cwd), Class::Decision);
        // The same options on a call of another kind are still a plan approval.
        assert_eq!(classify(&json!({ "kind": "other" }), &plan_opts, &cwd), Class::Decision);
        // Cursor's AskQuestion: its answers are allow_once options.
        let ask = json!({ "kind": "other", "title": "Which database?" });
        let ask_opts = json!([
            { "optionId": "pg", "kind": "allow_once" },
            { "optionId": "sqlite", "kind": "allow_once" },
            { "optionId": "__ask_question_skip__", "kind": "allow_once" }
        ]);
        assert_eq!(classify(&ask, &ask_opts, &cwd), Class::Decision);
        assert_eq!(classify(&ask, &json!([{ "optionId": "pg", "kind": "allow_once" }]), &cwd), Class::Decision);
        for mode in ["default", "auto", "always-approve"] {
            assert!(decide(mode, &[], &cwd, &plan, &plan_opts).is_none(), "{mode}");
            assert!(decide(mode, &[], &cwd, &ask, &ask_opts).is_none(), "{mode}");
        }
        // A saved rule can't answer one either.
        assert!(decide("default", &[rule("*", "switch_mode", "")], &cwd, &plan, &plan_opts).is_none());
    }

    #[test]
    fn modes_ask_auto_always() {
        let (_d, cwd, outside) = project();
        let opts = claude_opts();
        let d = |mode: &str, tc: &Value| decide(mode, &[], &cwd, tc, &opts);
        let in_project = edit("src/main.rs");
        let out = edit(&format!("{}/x", outside.display()));
        let del = json!({ "kind": "delete", "locations": [{ "path": "src/main.rs" }] });
        let mcp = json!({ "kind": "other", "title": "mcp tool" });
        let web = json!({ "kind": "fetch", "rawInput": { "url": "https://x" } });

        for tc in [&in_project, &out, &del, &mcp, &web, &exec("ls")] {
            assert!(d("default", tc).is_none());
        }
        assert_eq!(d("auto", &in_project), Some(("allow-once".into(), "Auto edits: an edit in the project".into())));
        for tc in [&out, &del, &mcp, &web, &exec("ls")] {
            assert!(d("auto", tc).is_none(), "{tc}");
        }
        for tc in [&in_project, &out, &del, &mcp, &web, &exec("rm -rf target")] {
            assert_eq!(d("always-approve", tc).map(|(o, _)| o).as_deref(), Some("allow-once"), "{tc}");
        }
        assert_eq!(d("always-approve", &exec("ls")).unwrap().1, "Approve all: a shell command");
        // Without an allow-once option there's nothing safe to pick, so the user is asked.
        let only_always = json!([{ "optionId": "a", "kind": "allow_always" }, { "optionId": "r", "kind": "reject_once" }]);
        assert!(decide("always-approve", &[], &cwd, &exec("ls"), &only_always).is_none());
        // A saved rule wins over Ask first.
        let rules = [rule(&cwd, "edit", "")];
        assert!(decide("default", &rules, &cwd, &in_project, &opts).unwrap().1.contains("edit"));
    }
}
