//! App preferences, stored as one JSON document in the `settings` table.
//! UI-only fields are opaque here; Rust reads the ones that shape the agent and terminal.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::Db;

const KEY: &str = "preferences";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Preferences {
    // ---- General (UI) ----
    /// system | light | dark
    pub theme: String,
    /// small | default | large
    pub text_size: String,
    /// enter | mod-enter
    pub send_key: String,
    /// chat | code | last
    pub launch_tab: String,
    /// OS notifications when a reply or agent turn finishes, or the agent needs approval,
    /// while Patchbay is in the background.
    pub notifications: bool,
    /// Global shortcut that opens the quick-entry window (Tauri accelerator syntax); empty = off.
    pub quick_entry_shortcut: String,
    /// system = the window manager's title bar | app = none; Patchbay draws the window controls.
    pub title_bar: String,

    // ---- Chat ----
    /// Copied into each new conversation; editable per conversation afterwards.
    pub default_system_prompt: String,
    pub expand_reasoning: bool,
    /// One extra model call after the first reply to name the conversation, and after a Code
    /// session's first and third turns when its agent doesn't name it (`code::titles`).
    pub generate_titles: bool,

    // ---- Code agents ----
    /// Agent for new Code sessions: an `AgentKind` id (patchbay, grok, claude-code, opencode, cursor, kilo).
    pub default_agent: String,
    /// Executable overrides, agent id -> path; agents not listed are looked up on PATH.
    pub agent_paths: crate::code::agents::AgentPaths,
    /// Agents set to run on Patchbay's providers instead of their own sign-in: agent id ->
    /// Patchbay model ref. Claude Code uses that model; OpenCode and Kilo get every Patchbay
    /// model, with that one as their default. Agents not listed keep their own setup.
    pub agent_providers: std::collections::BTreeMap<String, String>,
    /// Stop Claude Code turns before its sign-in's plan limits run into extra usage.
    pub usage_limit: UsageLimit,

    // ---- Grok Build ----
    /// default | auto | always-approve
    pub permission_mode: String,
    /// Appended to the agent's system prompt for every session (agents that accept one).
    pub agent_instructions: String,
    pub web_fetch: bool,
    /// Send Patchbay's own agent's prompts in a shortened form to save input tokens (a session
    /// can override it).
    pub minify_prompts: bool,
    /// Default auto-compact threshold; a model's own setting wins.
    pub auto_compact_percent: Option<u8>,
    /// Extra model calls for session titles and recaps.
    pub background_summaries: bool,
    /// What the agent may pick up from your Claude Code setup (`~/.claude`, project `.claude/`).
    pub claude_config: ClaudeConfig,

    // ---- Terminal ----
    /// Empty = the user's login shell.
    pub terminal_shell: String,
    pub terminal_font_size: u8,
    /// auto | on | off: draw typed characters before the shell echoes them (UI-only).
    pub terminal_local_echo: String,
}

/// Claude Code configuration the agent loads alongside its own. All on by default.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaudeConfig {
    /// Skills, slash commands and plugins.
    pub skills: bool,
    /// CLAUDE.md and `.claude/rules`.
    pub instructions: bool,
    /// Subagent definitions (`.claude/agents`).
    pub agents: bool,
    /// MCP servers from Claude's config.
    pub mcp_servers: bool,
    /// Hooks from Claude's settings; these run commands.
    pub hooks: bool,
    /// Claude Code plugins (which can bundle skills, agents, hooks and MCP servers).
    pub plugins: bool,
}

/// When Claude Code runs on a Claude plan: stop its turns once the 5-hour or weekly window
/// reaches `percent`, or once it bills extra usage.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageLimit {
    pub enabled: bool,
    pub percent: u8,
}

impl Default for UsageLimit {
    fn default() -> Self {
        Self { enabled: true, percent: 95 }
    }
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self { skills: true, instructions: true, agents: true, mcp_servers: true, hooks: true, plugins: true }
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            text_size: "default".into(),
            send_key: "enter".into(),
            launch_tab: "last".into(),
            notifications: true,
            quick_entry_shortcut: String::new(),
            title_bar: "system".into(),
            default_system_prompt: String::new(),
            expand_reasoning: false,
            generate_titles: true,
            default_agent: "patchbay".into(),
            agent_paths: Default::default(),
            agent_providers: Default::default(),
            usage_limit: UsageLimit::default(),
            permission_mode: "default".into(),
            agent_instructions: String::new(),
            web_fetch: false,
            minify_prompts: false,
            auto_compact_percent: None,
            background_summaries: true,
            claude_config: ClaudeConfig::default(),
            terminal_shell: String::new(),
            terminal_font_size: 13,
            terminal_local_echo: "auto".into(),
        }
    }
}

fn one_of(field: &str, v: &str, allowed: &[&str]) -> std::result::Result<(), String> {
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(format!("{field} must be one of {}", allowed.join(", ")))
    }
}

impl Preferences {
    pub fn validate(&self) -> std::result::Result<(), String> {
        one_of("theme", &self.theme, &["system", "light", "dark"])?;
        one_of("text size", &self.text_size, &["small", "default", "large"])?;
        one_of("send key", &self.send_key, &["enter", "mod-enter"])?;
        one_of("launch tab", &self.launch_tab, &["chat", "code", "last"])?;
        one_of("title bar", &self.title_bar, &["system", "app"])?;
        one_of("terminal local echo", &self.terminal_local_echo, &["auto", "on", "off"])?;
        one_of(
            "permission mode",
            &self.permission_mode,
            &["default", "auto", "always-approve"],
        )?;
        if crate::code::agents::AgentKind::parse(&self.default_agent).is_none() {
            return Err(format!("unknown coding agent {}", self.default_agent));
        }
        if let Some(p) = self.auto_compact_percent
            && !(10..=99).contains(&p)
        {
            return Err("auto-compact threshold must be between 10 and 99%".into());
        }
        if !(1..=100).contains(&self.usage_limit.percent) {
            return Err("usage limit must be between 1 and 100%".into());
        }
        if !(8..=32).contains(&self.terminal_font_size) {
            return Err("terminal font size must be between 8 and 32".into());
        }
        let shell = self.terminal_shell.trim();
        if !shell.is_empty() && !std::path::Path::new(shell).is_file() {
            return Err(format!("terminal shell {shell} does not exist"));
        }
        Ok(())
    }
}

pub fn load(db: &Db) -> Preferences {
    db.get_setting(KEY)
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default()
}

pub fn save(db: &Db, p: &Preferences) -> Result<()> {
    db.set_setting(KEY, &serde_json::to_string(p)?)
}

#[cfg(test)]
mod tests {
    use super::Preferences;

    #[test]
    fn defaults_are_valid_and_partial_json_fills_defaults() {
        assert!(Preferences::default().validate().is_ok());
        let p: Preferences = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(p.theme, "dark");
        assert_eq!(p.terminal_font_size, 13);
        assert!(p.background_summaries);
        assert!(p.generate_titles);
        assert!(p.claude_config.skills && p.claude_config.hooks);
        assert_eq!(p.usage_limit, super::UsageLimit { enabled: true, percent: 95 });
    }

    #[test]
    fn rejects_bad_values() {
        let bad = |f: fn(&mut Preferences)| {
            let mut p = Preferences::default();
            f(&mut p);
            p.validate().is_err()
        };
        assert!(bad(|p| p.theme = "neon".into()));
        assert!(bad(|p| p.title_bar = "floating".into()));
        assert!(bad(|p| p.permission_mode = "yolo".into()));
        assert!(bad(|p| p.auto_compact_percent = Some(100)));
        assert!(bad(|p| p.terminal_font_size = 4));
        assert!(bad(|p| p.usage_limit.percent = 0));
        assert!(bad(|p| p.usage_limit.percent = 101));
        assert!(bad(|p| p.terminal_shell = "/no/such/shell".into()));
    }
}
