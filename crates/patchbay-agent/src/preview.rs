//! What goes into the agent's prompt, for Settings: each piece of the system prompt, what the
//! user's Claude Code setup adds to it, and what else is sent with it.
//!
//! It is built from the functions a turn uses ([`prompt::sections`], [`assemble_registry`]), so
//! the preview shows what the model gets and cannot drift from it. Items that are switched off
//! are listed too, marked `enabled: false`, so a switch can be turned back on from the list.

use std::path::Path;

use serde::Serialize;

use crate::claude::{self, Setup};
use crate::host::ClaudeConfig;
use crate::prompt::{self, Extras};
use crate::prompts::Prompts;
use crate::session::Mode;
use crate::turn::assemble_registry;

/// What to preview.
pub struct PreviewRequest<'a> {
    pub cwd: &'a Path,
    pub model_name: &'a str,
    /// The custom instructions from Settings.
    pub instructions: &'a str,
    pub config: &'a ClaudeConfig,
    pub minify: bool,
    pub web_fetch: bool,
    /// System blocks the model's provider puts ahead of the prompt (a Claude login's identity line).
    pub provider_prefix: Vec<String>,
}

/// One piece of the system prompt.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewSection {
    /// `provider`, `persona`, `terse`, `environment`, `custom_instructions`, `skills`, or
    /// `instructions:<path>`.
    pub id: String,
    /// `standard`, `instructions` or `skills`.
    pub group: &'static str,
    pub title: String,
    /// Where the text comes from: a file path, or a name for built-in text.
    pub source: Option<String>,
    /// The text as the model gets it.
    pub text: String,
    /// Estimated tokens the text takes, whether or not it is switched on.
    pub tokens: usize,
    /// Whether it is in the prompt.
    pub enabled: bool,
    /// For an instruction file: the path its switch (`disabledInstructions`) is keyed by.
    pub path: Option<String>,
}

/// One skill the setup offers.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewSkill {
    /// The key of its switch (`disabledSkills`).
    pub name: String,
    pub description: String,
    pub source: String,
    /// Estimated tokens its line in the skills list takes.
    pub tokens: usize,
    pub enabled: bool,
}

/// One tool offered with every request.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewTool {
    pub name: String,
    pub description: String,
    /// Estimated tokens its name, description and parameter schema take.
    pub tokens: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    /// The system prompt, in the order it is sent, switched-off items included.
    pub sections: Vec<PreviewSection>,
    pub skills: Vec<PreviewSkill>,
    /// Text added to the conversation beside the system prompt.
    pub reminders: Vec<PreviewSection>,
    /// The built-in tools (MCP server tools come on top).
    pub tools: Vec<PreviewTool>,
    /// Estimated tokens of the enabled sections.
    pub tokens: usize,
    pub tools_tokens: usize,
}

/// A rough token count: four characters to a token.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn make(id: &str, group: &'static str, title: &str, source: Option<String>, text: String, enabled: bool) -> PreviewSection {
    PreviewSection { id: id.to_owned(), group, title: title.to_owned(), source, tokens: estimate_tokens(&text), text, enabled, path: None }
}

/// The preview for the user's own `~/.claude`.
pub fn build(req: &PreviewRequest) -> Preview {
    build_in(claude::home().as_deref(), req)
}

/// The preview with the Claude Code directory `home` (tests point it at a temporary one).
pub fn build_in(home: Option<&Path>, req: &PreviewRequest) -> Preview {
    let p = Prompts::new(req.minify);
    let setup = Setup::load_from(home, req.config, req.cwd);
    let extras = Extras { claude: req.config.clone(), skills: &setup.skills };
    let actual = prompt::sections(home, req.cwd, req.model_name, req.instructions, Mode::Default, p, &extras);
    let find = |id: &str| actual.iter().find(|s| s.id == id);

    let mut sections = Vec::new();
    for text in &req.provider_prefix {
        sections.push(make("provider", "standard", "Provider prefix", Some("added by the model's provider".into()), text.clone(), true));
    }
    let persona = find("persona").map(|s| s.text.clone()).unwrap_or_default();
    let persona_file = if req.minify { "system.min.md" } else { "system.md" };
    sections.push(make("persona", "standard", "Patchbay agent prompt", Some(persona_file.to_owned()), persona, true));
    if let Some(s) = find("terse") {
        sections.push(make("terse", "standard", "Reply style", Some("terse.md, with compact prompts".into()), s.text.clone(), true));
    }
    let env = find("environment").map(|s| s.text.clone()).unwrap_or_default();
    sections.push(make("environment", "standard", "Environment", Some("worked out for each session".into()), env, true));

    let custom = find("custom_instructions");
    sections.push(make(
        "custom_instructions",
        "instructions",
        "Custom instructions",
        Some("Settings".into()),
        custom.map(|s| s.text.clone()).unwrap_or_default(),
        custom.is_some(),
    ));
    // Every file the setup holds, whatever the switches say; the prompt has the enabled ones.
    for (path, text) in claude::instructions(home, req.cwd, &ClaudeConfig::default()) {
        let id = format!("instructions:{}", path.display());
        let scope = if home.is_some_and(|h| path.starts_with(h)) { "user" } else { "project" };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let enabled = find(&id).is_some();
        let sent = find(&id).map(|s| s.text.clone()).unwrap_or_else(|| prompt::instruction_text(p, &path, &text));
        let mut s = make(&id, "instructions", &format!("{scope}: {name}"), Some(path.display().to_string()), sent, enabled);
        s.path = Some(path.display().to_string());
        sections.push(s);
    }

    let skills_section = find("skills");
    sections.push(make(
        "skills",
        "skills",
        "Skills list",
        Some("each skill's name and description; its body loads when the agent uses it".into()),
        skills_section.map(|s| s.text.clone()).unwrap_or_default(),
        skills_section.is_some(),
    ));
    let roots = claude::roots(home, req.cwd, &ClaudeConfig::default());
    let skills = claude::skills(&roots)
        .into_iter()
        .map(|s| PreviewSkill {
            tokens: estimate_tokens(&format!("- {}: {}", s.name, s.description)),
            enabled: setup.skills.iter().any(|a| a.name == s.name),
            name: s.name,
            description: s.description,
            source: s.source.label(),
        })
        .collect();

    let mut reminders = Vec::new();
    if let Some(nudge) = p.terse_nudge() {
        reminders.push(make("terse_nudge", "standard", "Reply-style reminder", Some("with every prompt, with compact prompts".into()), nudge.to_owned(), true));
    }
    reminders.push(make(
        "context_size",
        "standard",
        "Context size note (example numbers)",
        Some("messages.json".into()),
        p.msg("context_size", &[("context", "12k")]),
        true,
    ));

    let registry = assemble_registry(req.web_fetch, false, Vec::new(), &setup);
    let tools: Vec<PreviewTool> = registry
        .specs(false, p)
        .into_iter()
        .map(|t| PreviewTool { tokens: estimate_tokens(&format!("{}{}{}", t.name, t.description, t.parameters)), name: t.name, description: t.description })
        .collect();

    let tokens = sections.iter().filter(|s| s.enabled).map(|s| s.tokens).sum();
    let tools_tokens = tools.iter().map(|t| t.tokens).sum();
    Preview { sections, skills, reminders, tools, tokens, tools_tokens }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A Claude directory with a CLAUDE.md and a skill, and a project with an AGENTS.md.
    fn fixture(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("pb-preview-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (home, cwd) = (base.join("home/.claude"), base.join("proj"));
        put(&home.join("CLAUDE.md"), "user rules");
        put(&home.join("skills/pdf/SKILL.md"), "---\nname: pdf\ndescription: Work with PDFs\n---\nbody that stays out of the prompt");
        put(&cwd.join("AGENTS.md"), "agents file");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        (base, home, cwd)
    }

    fn request<'a>(cwd: &'a Path, config: &'a ClaudeConfig) -> PreviewRequest<'a> {
        PreviewRequest { cwd, model_name: "m", instructions: "", config, minify: false, web_fetch: true, provider_prefix: Vec::new() }
    }

    fn section<'a>(p: &'a Preview, id_part: &str) -> &'a PreviewSection {
        p.sections.iter().find(|s| s.id.contains(id_part)).unwrap_or_else(|| panic!("no section {id_part}"))
    }

    #[test]
    fn counts_four_characters_to_a_token() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn lists_what_the_prompt_holds_in_order() {
        let (base, home, cwd) = fixture("order");
        let cfg = ClaudeConfig::default();
        let mut req = request(&cwd, &cfg);
        req.instructions = "run the tests";
        req.provider_prefix = vec!["You are an agent.".into()];
        let pv = build_in(Some(&home), &req);
        let ids: Vec<&str> = pv.sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids[..4], ["provider", "persona", "environment", "custom_instructions"]);
        assert!(ids[4].ends_with("CLAUDE.md") && ids[5].ends_with("AGENTS.md"), "{ids:?}");
        assert_eq!(*ids.last().unwrap(), "skills");
        assert!(pv.sections.iter().all(|s| s.enabled));
        assert!(section(&pv, "custom_instructions").text.contains("run the tests"));
        assert!(section(&pv, "skills").text.contains("- pdf: Work with PDFs"));
        assert!(!section(&pv, "skills").text.contains("body that stays out"));
        assert_eq!(pv.sections.iter().map(|s| s.tokens).sum::<usize>(), pv.tokens);
        assert!(pv.tools.iter().any(|t| t.name == "Read") && pv.tools.iter().any(|t| t.name == "Skill"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn switched_off_items_stay_listed_but_leave_the_total() {
        let (base, home, cwd) = fixture("off");
        let on = build_in(Some(&home), &request(&cwd, &ClaudeConfig::default()));
        let agents = cwd.join("AGENTS.md").display().to_string();
        let cfg = ClaudeConfig { disabled_instructions: vec![agents.clone()], disabled_skills: vec!["pdf".into()], ..Default::default() };
        let off = build_in(Some(&home), &request(&cwd, &cfg));
        let file = off.sections.iter().find(|s| s.path.as_deref() == Some(agents.as_str())).unwrap();
        assert!(!file.enabled && file.tokens > 0 && file.text.contains("agents file"));
        assert!(!off.sections.iter().find(|s| s.id == "skills").unwrap().enabled);
        assert!(!off.skills[0].enabled && on.skills[0].enabled);
        assert!(off.tokens < on.tokens);
        assert!(!off.tools.iter().any(|t| t.name == "Skill"));
        // The category switch turns every file but AGENTS.md off, and still lists them.
        let cfg = ClaudeConfig { instructions: false, ..Default::default() };
        let pv = build_in(Some(&home), &request(&cwd, &cfg));
        let user_file = home.join("CLAUDE.md").display().to_string();
        let user = pv.sections.iter().find(|s| s.path.as_deref() == Some(user_file.as_str())).unwrap();
        assert!(!user.enabled);
        assert!(pv.sections.iter().find(|s| s.id.ends_with("AGENTS.md")).unwrap().enabled);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn compact_prompts_add_the_reply_style() {
        let (base, home, cwd) = fixture("min");
        let plain = build_in(Some(&home), &request(&cwd, &ClaudeConfig::default()));
        let cfg = ClaudeConfig::default();
        let mut req = request(&cwd, &cfg);
        req.minify = true;
        let min = build_in(Some(&home), &req);
        assert!(plain.sections.iter().all(|s| s.id != "terse") && min.sections.iter().any(|s| s.id == "terse"));
        assert_eq!(section(&min, "persona").source.as_deref(), Some("system.min.md"));
        assert!(plain.reminders.iter().all(|r| r.id != "terse_nudge") && min.reminders.iter().any(|r| r.id == "terse_nudge"));
        assert!(min.tools_tokens < plain.tools_tokens);
        let _ = std::fs::remove_dir_all(base);
    }
}
