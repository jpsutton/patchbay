//! Summarizing a long conversation so the session can go on.

use std::sync::Arc;

use patchbay_llm::{Message, Part, Request, SystemBlock};
use tokio_util::sync::CancellationToken;

use crate::host::ModelClient;
use crate::reqlog::ReqTimer;
use crate::session::Session;
use crate::store::Entry;

/// Summary messages start with this, so rewinding and prompts can tell them from the user's.
pub const SUMMARY_MARK: &str = "<conversation-summary>";
/// Compact at this share of the window when neither the model nor the settings say otherwise.
pub const DEFAULT_PERCENT: u8 = 85;
/// Longest tool result text sent to the summarizer.
const RESULT_CHARS: usize = 4000;

/// Whether `context_tokens` has reached `percent` of `window`.
pub fn needs(context_tokens: u64, window: Option<u64>, percent: u8) -> bool {
    window.is_some_and(|w| w > 0 && percent > 0 && context_tokens.saturating_mul(100) >= w.saturating_mul(percent as u64))
}

/// The history as the summarizer sees it: no images, shorter tool output, no reasoning.
fn shrink(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| Message {
            role: m.role,
            parts: m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(Part::Text(t.clone())),
                    Part::Image { .. } => Some(Part::Text("[image]".into())),
                    Part::Document { name, .. } => Some(Part::Text(format!("[document {name}]"))),
                    Part::Reasoning { .. } => None,
                    Part::ToolCall(c) => Some(Part::ToolCall(c.clone())),
                    Part::ToolResult(r) => {
                        let mut r = r.clone();
                        r.content = vec![Part::Text(crate::tools::clip_middle(&r.joined_text(), RESULT_CHARS))];
                        Some(Part::ToolResult(r))
                    }
                })
                .collect(),
        })
        .filter(|m| !m.parts.is_empty())
        .collect()
}

/// Replace the history with a summary of it. `focus` steers what the summary stresses.
pub async fn compact(
    session: &Arc<Session>,
    mc: &ModelClient,
    focus: Option<&str>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let history = session.state.lock().await.messages.clone();
    if history.is_empty() {
        return Err("there is nothing to compact yet".into());
    }
    let mut messages = shrink(&history);
    let p = session.prompts();
    let mut ask = p.compact().trim().to_owned();
    if let Some(f) = focus.map(str::trim).filter(|f| !f.is_empty()) {
        ask.push_str("\n\n");
        ask.push_str(&p.msg("compact_focus", &[("focus", f)]));
    }
    messages.push(Message::user(ask));
    let mut system = mc.system_prefix.clone();
    system.push(SystemBlock::new(p.msg("compact_system", &[])));
    let request = Request { system, messages, ..Default::default() };
    let mut timer = ReqTimer::start();
    let result = mc.client.complete(&request, cancel, |ev| timer.event(&ev)).await;
    session.store.append(&Entry::Req(timer.finish(&mc.client.config, Some("compact"), &result)));
    let summary = result
        .map(|r| r.message.text())
        .map_err(|e| if e.cancelled { "cancelled".to_owned() } else { format!("could not summarize: {e}") })?;
    if summary.trim().is_empty() {
        return Err("the model returned an empty summary".into());
    }
    let message = Message::user(format!(
        "{SUMMARY_MARK}\n{}\n</conversation-summary>\n\n{}",
        summary.trim(),
        p.msg("compact_continue", &[])
    ));
    session.store.append(&Entry::Compacted { summary: message.clone() });
    {
        let mut st = session.state.lock().await;
        st.context_tokens = (summary.len() as u64) / 3;
        st.messages = vec![message];
    }
    // Rewind points before the summary no longer exist, and neither does the cached prefix.
    session.prompts.lock().unwrap().clear();
    session.cache_reset();
    session.text_chunk("agent_thought_chunk", "Summarized the conversation to free up context.\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use patchbay_llm::{Role, ToolCall, ToolResult};

    #[test]
    fn threshold() {
        assert!(!needs(849, Some(1000), 85));
        assert!(needs(850, Some(1000), 85));
        assert!(!needs(10_000, None, 85));
        assert!(!needs(10_000, Some(1000), 0));
    }

    #[test]
    fn shrinks_for_the_summarizer() {
        let call = ToolCall { id: "1".into(), name: "Read".into(), arguments: serde_json::json!({}), signature: None };
        let history = vec![
            Message::with_parts(Role::User, vec![Part::Image { mime: "image/png".into(), data: "AAAA".into() }, Part::Text("look".into())]),
            Message::with_parts(
                Role::Assistant,
                vec![Part::Reasoning { text: "hm".into(), signature: None, opaque: None }, Part::ToolCall(call.clone())],
            ),
            Message::with_parts(Role::User, vec![Part::ToolResult(ToolResult::text(&call, "x".repeat(20_000), false))]),
        ];
        let s = shrink(&history);
        assert_eq!(s[0].parts[0], Part::Text("[image]".into()));
        assert_eq!(s[1].parts.len(), 1);
        match &s[2].parts[0] {
            Part::ToolResult(r) => assert!(r.joined_text().len() < 5000),
            other => panic!("{other:?}"),
        }
    }
}
