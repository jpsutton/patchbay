//! The system prompt: a static, cacheable block and a dynamic one that follows it.

use std::path::Path;

use patchbay_llm::SystemBlock;

use crate::claude::Skill;
use crate::host::ClaudeConfig;
use crate::prompts::Prompts;
use crate::session::Mode;

/// Text the harness injects into the conversation, wrapped so the model can tell it from the user.
pub fn reminder(text: &str) -> String {
    format!("<system-reminder>\n{}\n</system-reminder>", text.trim())
}

pub fn is_reminder(text: &str) -> bool {
    text.trim_start().starts_with("<system-reminder>")
}

/// What the system prompt adds from the user's Claude Code setup.
#[derive(Default)]
pub struct Extras<'a> {
    pub claude: ClaudeConfig,
    pub skills: &'a [Skill],
}

/// One piece of the system prompt, in the order it is sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// `persona`, `terse`, `environment`, `plan_mode`, `custom_instructions`, `skills`, or
    /// `instructions:<path>` for one instruction file.
    pub id: String,
    /// Which system block it goes in: 0 is the static, cacheable one, 1 the dynamic one.
    pub block: usize,
    /// The text as the model gets it.
    pub text: String,
}

fn section(id: &str, block: usize, text: String) -> Section {
    Section { id: id.to_owned(), block, text }
}

/// The text one instruction file adds to the prompt.
pub fn instruction_text(p: Prompts, path: &Path, text: &str) -> String {
    p.msg("project_instructions", &[("path", &path.display().to_string()), ("text", text.trim())])
}

/// The system prompt piece by piece. [`system_blocks`] joins these, so the two never differ.
/// `home` is the Claude Code directory the instruction files come from (`claude::home()`).
pub fn sections(home: Option<&Path>, cwd: &Path, model_name: &str, instructions: &str, mode: Mode, p: Prompts, extras: &Extras) -> Vec<Section> {
    // Block 0 is the same for every session, so the provider can cache it.
    let mut out = vec![section("persona", 0, p.system().trim().to_owned())];
    if let Some(terse) = p.terse() {
        out.push(section("terse", 0, terse.trim().to_owned()));
    }
    out.push(section("environment", 1, environment(cwd, model_name, p)));
    if mode == Mode::Plan {
        out.push(section("plan_mode", 1, p.msg("plan_mode", &[])));
    }
    if !instructions.trim().is_empty() {
        out.push(section("custom_instructions", 1, p.msg("user_instructions", &[("text", instructions.trim())])));
    }
    for (path, text) in crate::claude::instructions(home, cwd, &extras.claude) {
        out.push(section(&format!("instructions:{}", path.display()), 1, instruction_text(p, &path, &text)));
    }
    if !extras.skills.is_empty() {
        out.push(section("skills", 1, skills_text(p, extras.skills)));
    }
    out
}

/// The skills list the prompt carries: each skill's name and description, not its body.
pub fn skills_text(p: Prompts, skills: &[Skill]) -> String {
    let list: Vec<String> = skills.iter().map(|s| format!("- {}: {}", s.name, s.description)).collect();
    p.msg("skills", &[("list", &list.join("\n"))])
}

pub fn system_blocks(cwd: &Path, model_name: &str, instructions: &str, mode: Mode, p: Prompts, extras: &Extras) -> Vec<SystemBlock> {
    let all = sections(crate::claude::home().as_deref(), cwd, model_name, instructions, mode, p, extras);
    let joined = |block: usize| all.iter().filter(|s| s.block == block).map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n\n");
    vec![SystemBlock::cached(joined(0)), SystemBlock::new(joined(1))]
}

fn environment(cwd: &Path, model_name: &str, p: Prompts) -> String {
    let git = cwd.ancestors().any(|d| d.join(".git").exists());
    let shell = if cfg!(windows) {
        "cmd".to_owned()
    } else {
        std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "bash".into())
    };
    p.environment()
        .trim()
        .replace("{cwd}", &cwd.to_string_lossy())
        .replace("{git}", if git { "yes" } else { "no" })
        .replace("{platform}", &format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH))
        .replace("{shell}", &shell)
        .replace("{date}", &today())
        .replace("{model}", model_name)
}

/// Today's date (UTC) as `YYYY-MM-DD`.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_convert() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_513), (2026, 3, 1));
    }

    #[test]
    fn static_block_is_cached_and_dynamic_is_not() {
        let b = system_blocks(Path::new("/tmp"), "m", " be terse ", Mode::Plan, Prompts::new(false), &Extras::default());
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].cache, patchbay_llm::Cache::Breakpoint);
        assert_eq!(b[1].cache, patchbay_llm::Cache::None);
        assert!(b[1].text.contains("plan mode") && b[1].text.contains("be terse"));
        assert!(!b[0].text.contains("be terse"));
    }

    #[test]
    fn minified_blocks_are_shorter_and_keep_the_same_shape() {
        let full = system_blocks(Path::new("/tmp"), "m", "be terse", Mode::Plan, Prompts::new(false), &Extras::default());
        let min = system_blocks(Path::new("/tmp"), "m", "be terse", Mode::Plan, Prompts::new(true), &Extras::default());
        assert_eq!(full.len(), min.len());
        // The persona shrinks (its terse reply instruction is extra); the dynamic block shrinks.
        assert!(min[0].text.starts_with(Prompts::new(true).system().trim()));
        assert!(Prompts::new(true).system().len() < Prompts::new(false).system().len());
        assert!(min[1].text.len() < full[1].text.len());
        assert_eq!(min[0].cache, patchbay_llm::Cache::Breakpoint);
        assert!(min[1].text.contains("be terse") && min[1].text.contains("ExitPlanMode") && min[1].text.contains("/tmp"));
        // Only the minified form asks for terse replies.
        assert!(min[0].text.contains("Reply terse") && !full[0].text.contains("Reply terse"));
    }

    #[test]
    fn blocks_are_the_sections_joined() {
        for minify in [false, true] {
            let p = Prompts::new(minify);
            // Instruction files off, so the real ~/.claude does not leak into the test.
            let extras = Extras { claude: ClaudeConfig { instructions: false, ..Default::default() }, skills: &[] };
            let secs = sections(None, Path::new("/tmp"), "m", "be terse", Mode::Plan, p, &extras);
            let ids: Vec<&str> = secs.iter().map(|s| s.id.as_str()).collect();
            let want: &[&str] = if minify {
                &["persona", "terse", "environment", "plan_mode", "custom_instructions"]
            } else {
                &["persona", "environment", "plan_mode", "custom_instructions"]
            };
            assert_eq!(ids, want);
            let blocks = system_blocks(Path::new("/tmp"), "m", "be terse", Mode::Plan, p, &extras);
            for (i, b) in blocks.iter().enumerate() {
                let joined = secs.iter().filter(|s| s.block == i).map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n\n");
                assert_eq!(b.text, joined);
            }
        }
    }

    #[test]
    fn reminders_are_recognized() {
        assert!(is_reminder(&reminder("x")));
        assert!(!is_reminder("hello"));
    }
}
