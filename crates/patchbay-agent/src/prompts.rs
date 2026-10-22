//! Every prompt text the agent sends, in a full and a minified form.
//!
//! The files in `prompts/` hold the texts. `X.md` is the full prompt and `X.min.md` its minified
//! twin; `messages.json` holds the short fragments (reminders, refusals, instructions to subagents)
//! as `{full, min}` pairs with `{placeholders}`; `tools.min.json` holds the minified description of
//! each tool (the full one sits with the tool's schema in `tools/`). Minified text follows the
//! `caveman-compress` rules: no articles, filler or hedging; code, paths, names and numbers exact.
//! With minified prompts the model is also asked, by `terse.md`, to answer in the same style
//! (the caveman plugin's reply rules); it has no full form. Whether it is used is a setting (`Settings::minify_prompts`) a session can override.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

const SYSTEM: (&str, &str) = (include_str!("../prompts/system.md"), include_str!("../prompts/system.min.md"));
const ENVIRONMENT: (&str, &str) = (include_str!("../prompts/environment.md"), include_str!("../prompts/environment.min.md"));
const COMPACT: (&str, &str) = (include_str!("../prompts/compact.md"), include_str!("../prompts/compact.min.md"));
const SUBAGENT: (&str, &str) = (include_str!("../prompts/subagent.md"), include_str!("../prompts/subagent.min.md"));
/// The reply-style instruction that goes with minified prompts (there is no full form of it).
const TERSE: &str = include_str!("../prompts/terse.md");
/// Said again with every user prompt while minifying: a system prompt alone is often forgotten.
const TERSE_NUDGE: &str = "TERSE MODE ACTIVE. Enforce this reply: drop articles, filler, pleasantries and hedging; \
fragments over full sentences. Technical terms, code, commands, paths and errors stay exact. \
Normal prose for security warnings and irreversible-action confirmations.";
const MESSAGES: &str = include_str!("../prompts/messages.json");
const TOOLS_MIN: &str = include_str!("../prompts/tools.min.json");

#[derive(Deserialize)]
struct Pair {
    full: String,
    min: String,
}

fn messages() -> &'static HashMap<String, Pair> {
    static M: OnceLock<HashMap<String, Pair>> = OnceLock::new();
    M.get_or_init(|| serde_json::from_str(MESSAGES).expect("prompts/messages.json"))
}

fn tools_min() -> &'static HashMap<String, String> {
    static M: OnceLock<HashMap<String, String>> = OnceLock::new();
    M.get_or_init(|| serde_json::from_str(TOOLS_MIN).expect("prompts/tools.min.json"))
}

/// Which form of the prompts to send.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Prompts {
    pub minify: bool,
}

impl Prompts {
    pub fn new(minify: bool) -> Prompts {
        Prompts { minify }
    }

    fn pick(self, pair: (&'static str, &'static str)) -> &'static str {
        if self.minify { pair.1 } else { pair.0 }
    }

    pub fn system(self) -> &'static str {
        self.pick(SYSTEM)
    }

    pub fn environment(self) -> &'static str {
        self.pick(ENVIRONMENT)
    }

    pub fn compact(self) -> &'static str {
        self.pick(COMPACT)
    }

    pub fn subagent(self) -> &'static str {
        self.pick(SUBAGENT)
    }

    /// With minified prompts the model is also asked to reply tersely.
    pub fn terse(self) -> Option<&'static str> {
        self.minify.then_some(TERSE)
    }

    /// A short reminder to add to each user prompt, when replies should be terse.
    pub fn terse_nudge(self) -> Option<&'static str> {
        self.minify.then_some(TERSE_NUDGE)
    }

    /// A fragment from `messages.json` with its `{placeholders}` filled in.
    pub fn msg(self, key: &str, vars: &[(&str, &str)]) -> String {
        let pair = messages().get(key).unwrap_or_else(|| panic!("no message {key:?} in prompts/messages.json"));
        let mut s = if self.minify { pair.min.clone() } else { pair.full.clone() };
        for (name, value) in vars {
            s = s.replace(&format!("{{{name}}}"), value);
        }
        s
    }

    /// A tool's description: the minified one when minifying and there is one, else `full`.
    pub fn tool_description(self, name: &str, full: String) -> String {
        if self.minify {
            if let Some(min) = tools_min().get(name) {
                return min.clone();
            }
        }
        full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a minified text must keep from its original.
    fn spans(s: &str) -> Vec<String> {
        let tick = regex::Regex::new(r"`[^`]+`").unwrap();
        tick.find_iter(s).map(|m| m.as_str().to_owned()).collect()
    }

    fn placeholders(s: &str) -> Vec<String> {
        let ph = regex::Regex::new(r"\{[a-z_]+\}").unwrap();
        let mut v: Vec<String> = ph.find_iter(s).map(|m| m.as_str().to_owned()).collect();
        v.sort();
        v.dedup();
        v
    }

    fn headings(s: &str) -> Vec<&str> {
        s.lines().filter(|l| l.starts_with('#')).collect()
    }

    /// Panics unless `min` is a faithful, shorter form of `full`.
    pub fn assert_faithful(what: &str, full: &str, min: &str) {
        assert!(min.len() < full.len() || full.len() < 40, "{what}: the minified form is not shorter");
        assert_eq!(placeholders(full), placeholders(min), "{what}: placeholders differ");
        assert_eq!(headings(full), headings(min), "{what}: headings differ");
        for s in spans(full) {
            assert!(min.contains(&s), "{what}: lost the code span {s}");
        }
    }

    #[test]
    fn every_file_has_a_faithful_minified_twin() {
        for (name, (full, min)) in [("system", SYSTEM), ("environment", ENVIRONMENT), ("compact", COMPACT), ("subagent", SUBAGENT)] {
            assert_faithful(name, full, min);
        }
        for (key, pair) in messages() {
            assert_faithful(key, &pair.full, &pair.min);
        }
    }

    #[test]
    fn minifying_saves_a_good_share() {
        let sum = |f: &dyn Fn((&str, &str)) -> usize| [SYSTEM, ENVIRONMENT, COMPACT, SUBAGENT].into_iter().map(f).sum::<usize>();
        let (full, min) = (sum(&|p| p.0.len()), sum(&|p| p.1.len()));
        println!("prompt files: {full} -> {min} bytes ({}% saved)", 100 - min * 100 / full);
        assert!(min * 100 / full <= 88, "{min} of {full}");
    }

    #[test]
    fn code_uses_only_messages_that_exist() {
        let re = regex::Regex::new(r#"\.msg\(\s*"([a-z_]+)""#).unwrap();
        let mut used = 0;
        let mut dirs = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for c in re.captures_iter(&text) {
                        used += 1;
                        assert!(messages().contains_key(&c[1]), "{}: no message {:?}", path.display(), &c[1]);
                    }
                }
            }
        }
        assert!(used > 10, "found only {used} uses");
    }

    #[test]
    fn terse_replies_are_asked_for_only_when_minifying() {
        assert!(Prompts::new(false).terse().is_none());
        let t = Prompts::new(true).terse().unwrap();
        // The rules that keep terse text safe and useful.
        for must in ["Drop articles", "`not`", "Code blocks unchanged", "security warnings", "commits", "all session"] {
            assert!(t.contains(must), "terse.md lost {must:?}");
        }
        assert!(Prompts::new(false).terse_nudge().is_none());
        let n = Prompts::new(true).terse_nudge().unwrap();
        assert!(n.contains("TERSE MODE ACTIVE") && n.len() < 320, "{n}");
        assert!(t.len() < 1400, "the blurb should stay small: {} bytes", t.len());
    }

    #[test]
    fn fills_placeholders_and_picks_the_form() {
        let full = Prompts::new(false).msg("file_changed", &[("path", "/a.rs")]);
        let min = Prompts::new(true).msg("file_changed", &[("path", "/a.rs")]);
        assert!(full.starts_with("/a.rs changed since you read it") && min.starts_with("/a.rs changed since read"));
        assert!(min.len() < full.len());
        assert_eq!(Prompts::new(false).tool_description("Read", "FULL".into()), "FULL");
        assert_ne!(Prompts::new(true).tool_description("Read", "FULL".into()), "FULL");
        assert_eq!(Prompts::new(true).tool_description("NoSuchTool", "FULL".into()), "FULL");
    }

    #[test]
    fn tool_texts_are_faithful_and_cover_every_builtin() {
        // Every built-in tool, with everything on, has a minified description that keeps its code spans.
        let registry = crate::tools::Registry::builtin(true, true);
        let (mut full_total, mut min_total) = (0, 0);
        for spec in registry.specs(false, Prompts::new(false)) {
            let min = tools_min().get(&spec.name).unwrap_or_else(|| panic!("no minified description for {}", spec.name));
            assert_faithful(&spec.name, &spec.description, min);
            full_total += spec.description.len();
            min_total += min.len();
        }
        println!("tool descriptions: {full_total} -> {min_total} bytes ({}% saved)", 100 - min_total * 100 / full_total);
        assert!(min_total * 100 / full_total <= 90);
        for name in tools_min().keys() {
            assert!(registry.get(name).is_some(), "tools.min.json names {name}, which is not a tool");
        }
    }
}
