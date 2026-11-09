//! A stored session read back in pages. A window that opens a session its agent kept running
//! without it has no complete copy of the conversation: the cached transcript is written by a
//! window, at the end of a turn, so turns that finished while none was open are missing from it.
//! The session file is complete, so the window reads its newest turns from there and asks for
//! older ones when the user scrolls back.
//!
//! A page is the `session/update` values `session/load` would replay for the same entries
//! (`session::entry_updates`), so the window builds the same cards from it.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::session::{entry_updates, is_user_prompt};
use crate::store::{Entry, Store, effective, valid_session_id};

/// One page of a session's history.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryWindow {
    /// What the entries of the page show, oldest first.
    pub updates: Vec<Value>,
    /// Where the page starts among the session's effective entries: 0 is the start of the
    /// conversation. Ask for the page `before` this one to read the next older.
    pub start: usize,
    /// The entry the page stops before.
    pub end: usize,
    /// How many effective entries the session has now. A rewind changes it, so a window that
    /// paged back across one may be a few entries off.
    pub total: usize,
    /// Prompts the user wrote before the page: what a rewind or fork counts from.
    pub prompts_before: usize,
}

/// A page of session `session_id`: the newest `prompts` user prompts and what followed them, or
/// those just before entry `before`. `in_flight`: a turn is running, so stop before its prompt
/// (the live events of the turn come from the caller). `None` when there is no such session file.
pub fn history_window(
    data_dir: &Path,
    session_id: &str,
    before: Option<usize>,
    prompts: usize,
    in_flight: bool,
) -> Option<HistoryWindow> {
    if !valid_session_id(session_id) {
        return None;
    }
    let store = Store::new(data_dir, session_id);
    if !store.exists() {
        return None;
    }
    Some(window(&effective(&store.load()), before, prompts, in_flight))
}

fn is_prompt(e: &Entry) -> bool {
    matches!(e, Entry::Msg { m } if is_user_prompt(m))
}

fn window(entries: &[Entry], before: Option<usize>, prompts: usize, in_flight: bool) -> HistoryWindow {
    let total = entries.len();
    let mut end = before.map_or(total, |b| b.min(total));
    if in_flight && before.is_none() {
        // The running turn begins at the last prompt. Steered messages join the message that
        // carries the tool results, so they are not prompts of their own.
        if let Some(turn) = entries[..end].iter().rposition(is_prompt) {
            end = turn;
        }
    }
    let mut start = 0;
    let mut seen = 0;
    for i in (0..end).rev() {
        if is_prompt(&entries[i]) {
            seen += 1;
            if seen >= prompts.max(1) {
                start = i;
                break;
            }
        }
    }
    HistoryWindow {
        updates: entries[start..end].iter().flat_map(entry_updates).collect(),
        start,
        end,
        total,
        prompts_before: entries[..start].iter().filter(|e| is_prompt(e)).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use patchbay_llm::Message;
    use serde_json::json;

    fn user(t: &str) -> Entry {
        Entry::Msg { m: Message::user(t) }
    }
    fn assistant(t: &str) -> Entry {
        Entry::Msg { m: Message::assistant(t) }
    }
    fn tool(id: &str) -> Entry {
        Entry::Tool { call: json!({"sessionUpdate": "tool_call_update", "toolCallId": id}) }
    }

    /// Three turns: u1 a1 | u2 tool a2 | u3 a3: seven entries.
    fn three_turns() -> Vec<Entry> {
        vec![user("u1"), assistant("a1"), user("u2"), tool("t"), assistant("a2"), user("u3"), assistant("a3")]
    }

    fn texts(w: &HistoryWindow) -> Vec<String> {
        w.updates.iter().filter_map(|u| u["content"]["text"].as_str().map(str::to_owned)).collect()
    }

    #[test]
    fn the_newest_prompts_come_first_and_the_cursor_walks_back() {
        let e = three_turns();
        let w = window(&e, None, 1, false);
        assert_eq!((w.start, w.end, w.total, w.prompts_before), (5, 7, 7, 2));
        assert_eq!(texts(&w), ["u3", "a3"]);

        let w = window(&e, Some(w.start), 1, false);
        assert_eq!((w.start, w.end, w.prompts_before), (2, 5, 1));
        assert_eq!(texts(&w), ["u2", "a2"]);
        assert_eq!(w.updates.len(), 3, "the tool card is part of the page");

        let w = window(&e, Some(w.start), 1, false);
        assert_eq!((w.start, w.end, w.prompts_before), (0, 2, 0));
        assert_eq!(texts(&w), ["u1", "a1"]);
    }

    #[test]
    fn a_page_can_hold_more_than_one_prompt_and_the_start_is_clamped() {
        let e = three_turns();
        let w = window(&e, None, 2, false);
        assert_eq!((w.start, w.prompts_before), (2, 1));
        let all = window(&e, None, 99, false);
        assert_eq!((all.start, all.prompts_before), (0, 0));
        assert_eq!(texts(&all), ["u1", "a1", "u2", "a2", "u3", "a3"]);
        let past = window(&e, Some(500), 1, false);
        assert_eq!(past.end, 7, "a cursor past the end reads from the end");
        let none = window(&e, Some(0), 1, false);
        assert!(none.updates.is_empty() && none.start == 0 && none.end == 0);
    }

    #[test]
    fn a_running_turn_is_left_to_the_live_events() {
        let e = three_turns();
        let w = window(&e, None, 1, true);
        assert_eq!((w.start, w.end, w.prompts_before), (2, 5, 1));
        assert_eq!(texts(&w), ["u2", "a2"]);
        // Paging back from a cursor is not about the running turn.
        let w = window(&e, Some(5), 1, true);
        assert_eq!((w.start, w.end), (2, 5));
        // No prompt to stop before: the page is whatever there is.
        let w = window(&[assistant("x")], None, 1, true);
        assert_eq!((w.start, w.end), (0, 1));
    }

    #[test]
    fn a_tool_result_message_is_not_a_prompt() {
        use patchbay_llm::{Part, Role, ToolCall, ToolResult};
        let call = ToolCall { id: "c1".into(), name: "read".into(), arguments: json!({}), signature: None };
        let result = Message::with_parts(Role::User, vec![Part::ToolResult(ToolResult::text(&call, "ok", false))]);
        let e = vec![user("u1"), Entry::Msg { m: result }, assistant("a1")];
        let w = window(&e, None, 1, false);
        assert_eq!((w.start, w.prompts_before), (0, 0));
        assert_eq!(texts(&w), ["u1", "a1"], "tool results have no text of their own");
    }

    #[test]
    fn a_rewound_turn_does_not_show() {
        let e = effective(&[user("u1"), assistant("a1"), user("u2"), assistant("a2"), Entry::Truncate { len: 2 }, user("u3")]);
        let w = window(&e, None, 99, false);
        assert_eq!(texts(&w), ["u1", "a1", "u3"]);
    }
}
