//! Code session titles. A new session takes its first prompt's first line at once. After that it
//! takes the agent's own titles (ACP `session_info_update`), or, from agents that send none, a
//! Patchbay model call after the first and third turns. A title someone typed pins the session
//! until they ask for an automatic one again. A handoff to another agent or computer keeps the
//! title: the agent that takes over sees only the handoff note.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, anyhow};

use super::agents::AgentKind;
use crate::db::{CodeSession, Db};
use crate::handoff::{conversation_text, transcript_lines};
use crate::providers;

pub const NEW_SESSION_TITLE: &str = "New session";

const TITLE_PROMPT: &str = "You name coding-agent sessions. Reply with a short title for the session \
below that says what is being worked on: at most 6 words, no quotes, no trailing punctuation. \
Reply with the title only.";

/// User turns after which Patchbay names a session its agent hasn't.
const TITLE_TURNS: &[usize] = &[1, 3];
/// Longest single message passed to the title call.
const MAX_LINE: usize = 1_500;
/// Most conversation passed to the title call.
const MAX_CONTEXT: usize = 12_000;

#[derive(Default)]
struct Track {
    /// User turns so far, including ones from before this process.
    turns: usize,
    lines: Vec<String>,
    /// The agent's reply to the current turn, so far.
    reply: String,
    /// The agent sent its own title: Patchbay doesn't make one too.
    agent_titled: bool,
    /// The agent took the session over from a handoff note, so its title would name the note.
    handed_off: bool,
}

/// What each session's conversation has been, for naming it. Kept on the node running its agent.
#[derive(Default)]
pub struct Titles(Mutex<HashMap<String, Track>>);

impl Titles {
    /// A prompt went to the agent. `transcript` (the saved one) seeds a session not seen yet.
    pub fn on_prompt(&self, id: &str, text: &str, transcript: impl FnOnce() -> Option<String>) {
        let mut all = self.0.lock().unwrap();
        let t = all.entry(id.to_owned()).or_insert_with(|| {
            let lines = transcript_lines(transcript().as_deref(), false);
            let turns = lines.iter().filter(|l| l.starts_with("User: ")).count();
            Track { turns, lines: lines.iter().map(|l| crate::chat::clip(l, MAX_LINE)).collect(), ..Default::default() }
        });
        t.turns += 1;
        t.reply.clear();
        let text = text.trim();
        if !text.is_empty() {
            t.lines.push(crate::chat::clip(&format!("User: {text}"), MAX_LINE));
        }
    }

    /// Reply text from the agent (`agent_message_chunk`).
    pub fn on_chunk(&self, id: &str, text: &str) {
        if let Some(t) = self.0.lock().unwrap().get_mut(id)
            && t.reply.len() < MAX_LINE * 4
        {
            t.reply.push_str(text);
        }
    }

    /// The prompt just sent carried a handoff note (another agent or computer ran the session).
    pub fn on_handoff(&self, id: &str) {
        self.0.lock().unwrap().entry(id.to_owned()).or_default().handed_off = true;
    }

    /// The agent named the session. Returns whether to use its title: not after a handoff, when
    /// the session already has its name and the agent only saw the note.
    pub fn on_agent_title(&self, id: &str) -> bool {
        let mut all = self.0.lock().unwrap();
        let t = all.entry(id.to_owned()).or_default();
        t.agent_titled = true;
        !t.handed_off
    }

    pub fn agent_titled(&self, id: &str) -> bool {
        self.0.lock().unwrap().get(id).is_some_and(|t| t.agent_titled)
    }

    /// The turn ended. Returns the conversation so far when Patchbay should name the session now.
    pub fn on_turn_end(&self, id: &str, completed: bool) -> Option<String> {
        let mut all = self.0.lock().unwrap();
        let t = all.get_mut(id)?;
        let reply = std::mem::take(&mut t.reply);
        if !reply.trim().is_empty() {
            t.lines.push(crate::chat::clip(&format!("Assistant: {}", reply.trim()), MAX_LINE));
        }
        (completed && !t.agent_titled && !t.handed_off && TITLE_TURNS.contains(&t.turns)).then(|| context(&t.lines))
    }

    pub fn forget(&self, id: &str) {
        self.0.lock().unwrap().remove(id);
    }
}

/// The conversation for a title call: its latest part, plus its first request if that fell off.
fn context(lines: &[String]) -> String {
    let body = conversation_text(lines, MAX_CONTEXT);
    match lines.first() {
        Some(first) if !body.contains(first.as_str()) => format!("{first}\n\n{body}"),
        _ => body,
    }
}

/// The first prompt's first line, for a session still named by default. `None` once named.
pub fn first_line_title(session: &CodeSession, text: &str) -> Option<String> {
    let folder = session.cwd.rsplit(['/', '\\']).find(|p| !p.is_empty());
    if session.title_manual || (session.title != NEW_SESSION_TITLE && Some(session.title.as_str()) != folder) {
        return None;
    }
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(crate::chat::clip(line, 60))
}

/// The model that names a session: its own Patchbay model, else the default model.
fn title_model(db: &Db, session: &CodeSession) -> Option<providers::Resolved> {
    let own = AgentKind::parse(&session.agent)
        .is_some_and(|k| k.uses_patchbay_models())
        .then(|| session.model_ref.clone())
        .flatten();
    own.into_iter()
        .chain(db.get_setting("default_model").ok().flatten())
        .find_map(|r| providers::resolve(db, &r).ok())
}

/// Name `session` from `convo` with a Patchbay model.
pub async fn generate(db: &Db, session: &CodeSession, convo: String) -> Result<String> {
    let resolved = title_model(db, session)
        .ok_or_else(|| anyhow!("no model to name it with: choose a default model in Settings"))?;
    crate::chat::name_with(&resolved, TITLE_PROMPT, convo)
        .await?
        .ok_or_else(|| anyhow!("the model didn't suggest a title"))
}

/// The whole saved conversation, for naming a session on request.
pub fn transcript_context(transcript_json: Option<&str>) -> Option<String> {
    let lines: Vec<String> = transcript_lines(transcript_json, false)
        .iter()
        .map(|l| crate::chat::clip(l, MAX_LINE))
        .collect();
    (!lines.is_empty()).then(|| context(&lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(title: &str, manual: bool) -> CodeSession {
        CodeSession {
            id: "s".into(),
            cwd: "/home/me/patchbay".into(),
            acp_session_id: None,
            title: title.into(),
            model_ref: None,
            pinned: false,
            created_at: 0,
            updated_at: 0,
            activity: None,
            agent: "grok".into(),
            agent_model: None,
            host_node: None,
            epoch: 0,
            permission_mode: "default".into(),
            archived_at: None,
            title_manual: manual,
        }
    }

    #[test]
    fn first_prompt_names_only_unnamed_sessions() {
        let t = |title, manual| first_line_title(&session(title, manual), "\n  fix the login bug\nmore");
        assert_eq!(t(NEW_SESSION_TITLE, false).as_deref(), Some("fix the login bug"));
        assert_eq!(t("patchbay", false).as_deref(), Some("fix the login bug"));
        assert_eq!(t("Login fixes", false), None);
        assert_eq!(t(NEW_SESSION_TITLE, true), None);
        assert_eq!(first_line_title(&session(NEW_SESSION_TITLE, false), "  \n "), None);
    }

    #[test]
    fn names_after_the_first_and_third_completed_turns() {
        let titles = Titles::default();
        let turn = |text: &str, completed| {
            titles.on_prompt("s", text, || None);
            titles.on_chunk("s", "ok");
            titles.on_turn_end("s", completed)
        };
        let first = turn("fix the bug", true).expect("named after turn 1");
        assert!(first.contains("User: fix the bug") && first.contains("Assistant: ok"));
        assert_eq!(turn("and the test", true), None);
        assert!(turn("now the docs", true).unwrap().contains("User: now the docs"));
        assert_eq!(turn("more", true), None);
    }

    #[test]
    fn agent_titles_and_cancelled_turns_skip_naming() {
        let titles = Titles::default();
        titles.on_prompt("s", "fix it", || None);
        assert_eq!(titles.on_turn_end("s", false), None, "cancelled");
        let titles = Titles::default();
        titles.on_prompt("s", "fix it", || None);
        assert!(titles.on_agent_title("s"));
        assert_eq!(titles.on_turn_end("s", true), None);
    }

    #[test]
    fn handed_off_sessions_keep_their_title() {
        let titles = Titles::default();
        titles.on_prompt("s", "carry on", || None);
        titles.on_handoff("s");
        assert_eq!(titles.on_turn_end("s", true), None, "no Patchbay title");
        assert!(!titles.on_agent_title("s"), "agent title ignored");
    }

    #[test]
    fn seeds_turns_from_the_saved_transcript() {
        let saved = r#"{"v":1,"entries":[{"kind":"user","text":"a"},{"kind":"agent","text":"b"},{"kind":"user","text":"c"}]}"#;
        let titles = Titles::default();
        titles.on_prompt("s", "third", || Some(saved.into()));
        let ctx = titles.on_turn_end("s", true).expect("third turn");
        assert!(ctx.starts_with("User: a") && ctx.contains("User: third"));
    }

    #[test]
    fn long_context_keeps_the_first_request() {
        let mut lines = vec!["User: build the importer".to_owned()];
        lines.extend((0..40).map(|i| format!("Assistant: {}", "x".repeat(MAX_LINE - 20 + i))));
        let ctx = context(&lines);
        assert!(ctx.starts_with("User: build the importer") && ctx.len() < MAX_CONTEXT + 200);
    }
}
