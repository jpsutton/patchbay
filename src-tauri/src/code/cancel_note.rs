//! Why a turn was cancelled with an approval card open (pb-20261121-01).
//!
//! Cancelling a turn answers every open card with "cancelled", and the agent CLI reports that
//! as "User rejected tool use". A seat that read that stopped for good although nobody
//! rejected anything. So the cancel leaves a note per session: the next prompt to it carries
//! a line saying what happened, and the senders of the cancelled turn's messages are told
//! the same reason on the turn-failure notice channel (pb-20261111-28). Each note is used once.

use std::collections::HashMap;
use std::sync::Mutex;

/// A note this old is not about the turn the agent is thinking of any more.
const MAX_AGE_SECS: u64 = 24 * 3600;

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    /// What the agent is told, after "because": a person can read it as is.
    pub reason: String,
    /// The structured kind the notice carries: `usage_limit_stop`, `send_now`, `shutdown`.
    pub kind: &'static str,
    pub resets_at: Option<u64>,
    at: u64,
}

impl Note {
    pub fn new(reason: impl Into<String>, kind: &'static str, resets_at: Option<u64>) -> Self {
        Self { reason: reason.into(), kind, resets_at, at: now() }
    }

    /// The line the agent's next prompt carries.
    pub fn line(&self) -> String {
        format!(
            "Patchbay cancelled your last tool call because {}; the user did not reject it. Retry it if still needed.",
            self.reason
        )
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[derive(Default)]
pub struct CancelNotes {
    /// For the agent: taken by the next prompt.
    prompt: Mutex<HashMap<String, Note>>,
    /// For the origin senders: taken when the cancelled turn ends.
    senders: Mutex<HashMap<String, Note>>,
}

impl CancelNotes {
    /// `for_agent`: a card was open, so the agent's tool call was cancelled and it must be told.
    /// `for_senders`: the turn's origin senders get the reason too.
    pub fn record(&self, session: &str, note: Note, for_agent: bool, for_senders: bool) {
        if for_agent {
            self.prompt.lock().unwrap().insert(session.to_owned(), note.clone());
        }
        if for_senders {
            self.senders.lock().unwrap().insert(session.to_owned(), note);
        }
    }

    /// The turn ended some other way than cancelled: a note left for it is stale.
    pub fn clear(&self, session: &str) {
        self.prompt.lock().unwrap().remove(session);
        self.senders.lock().unwrap().remove(session);
    }

    pub fn take_for_prompt(&self, session: &str) -> Option<Note> {
        fresh(self.prompt.lock().unwrap().remove(session))
    }

    pub fn take_for_senders(&self, session: &str) -> Option<Note> {
        fresh(self.senders.lock().unwrap().remove(session))
    }
}

fn fresh(note: Option<Note>) -> Option<Note> {
    note.filter(|n| now().saturating_sub(n.at) <= MAX_AGE_SECS)
}

/// Add `line` after a prompt's content blocks as a `<system-reminder>`, the way the agent's own
/// reminders go. It goes AFTER the message, never before: an agent previews a prompt by its first
/// text (the fork and rewind points match the clicked message against it), and a replay hides a
/// trailing reminder instead of showing it as something the user typed.
pub fn append_reminder(blocks: &mut Vec<serde_json::Value>, line: &str) {
    blocks.push(serde_json::json!({ "type": "text", "text": format!("<system-reminder>\n{line}\n</system-reminder>") }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_is_used_once_and_names_the_reason() {
        let n = CancelNotes::default();
        n.record("s", Note::new("the user sent a new message with Send now", "send_now", None), true, true);
        let first = n.take_for_prompt("s").expect("the agent is told");
        assert_eq!(
            first.line(),
            "Patchbay cancelled your last tool call because the user sent a new message with Send now; the user did not reject it. Retry it if still needed."
        );
        assert!(n.take_for_prompt("s").is_none(), "used once, then cleared");
        assert_eq!(n.take_for_senders("s").map(|x| x.kind), Some("send_now"));
        assert!(n.take_for_senders("s").is_none());
    }

    #[test]
    fn the_two_audiences_are_independent() {
        let n = CancelNotes::default();
        n.record("a", Note::new("r", "send_now", None), false, true);
        assert!(n.take_for_prompt("a").is_none(), "no card was open: nothing to tell the agent");
        assert!(n.take_for_senders("a").is_some());
        assert!(n.take_for_prompt("other").is_none(), "notes are per session");
    }

    #[test]
    fn an_old_note_is_dropped() {
        let n = CancelNotes::default();
        let mut old = Note::new("r", "shutdown", None);
        old.at = now() - MAX_AGE_SECS - 5;
        n.record("s", old, true, true);
        assert!(n.take_for_prompt("s").is_none());
        assert!(n.take_for_senders("s").is_none());
    }

    #[test]
    fn the_message_stays_first_so_the_agents_preview_of_it_is_unchanged() {
        let mut blocks = vec![serde_json::json!({ "type": "text", "text": "carry on" })];
        append_reminder(&mut blocks, "x");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "carry on");
        assert_eq!(blocks[1]["text"], "<system-reminder>\nx\n</system-reminder>");
    }
}
