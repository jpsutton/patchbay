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
    /// One extra model call after the first reply to name the conversation.
    pub generate_titles: bool,

    // ---- Code agent (Grok Build) ----
    /// default | auto | always-approve
    pub permission_mode: String,
    /// Appended to the agent's system prompt for every session.
    pub agent_instructions: String,
    pub web_fetch: bool,
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
            permission_mode: "default".into(),
            agent_instructions: String::new(),
            web_fetch: false,
            auto_compact_percent: None,
            background_summaries: true,
            claude_config: ClaudeConfig::default(),
            terminal_shell: String::new(),
            terminal_font_size: 13,
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
        one_of(
            "permission mode",
            &self.permission_mode,
            &["default", "auto", "always-approve"],
        )?;
        if let Some(p) = self.auto_compact_percent
            && !(10..=99).contains(&p)
        {
            return Err("auto-compact threshold must be between 10 and 99%".into());
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
        assert!(bad(|p| p.terminal_shell = "/no/such/shell".into()));
    }
}
