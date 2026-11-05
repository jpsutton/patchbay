//! AskUserQuestion: put a multiple-choice question to the user.

use async_trait::async_trait;
use patchbay_llm::ToolSpec;
use serde_json::{Value, json};

use super::*;

pub struct AskUserQuestion;

/// The option id a client treats as "skip": it never auto-answers a question that offers it.
const SKIP: &str = "__ask_question_skip__";

/// An option no client has to show as a button: a client that can take typed text offers a text
/// field for it, and answers with this id, a colon and the text. A client that cannot treats it as
/// one more choice, so it comes back bare.
const OTHER: &str = "__ask_question_other__";

#[async_trait]
impl Tool for AskUserQuestion {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "AskUserQuestion".into(),
            description: "Ask the user a multiple-choice question when you are blocked on a decision only \
they can make (which of two approaches, a name, a scope). Do not use it for things you can find out \
yourself or for approval of a plan. Give two to four options; put the one you recommend first and \
say so in its label. The user can always skip, type their own answer, or answer in chat instead."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "One to four questions, asked in order",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string", "description": "The question, ending in a question mark"},
                                "header": {"type": "string", "description": "A short label for the question"},
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string", "description": "The choice, a few words"},
                                            "description": {"type": "string", "description": "What choosing it means"}
                                        },
                                        "required": ["label"]
                                    }
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "other"
    }

    fn allowed_in_plan(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        let n = input["questions"].as_array().map(Vec::len).unwrap_or(0);
        if n > 1 { format!("{n} questions") } else { "A question".into() }
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(questions) = input["questions"].as_array().filter(|q| !q.is_empty()) else {
            return ToolOutput::error("questions must be a non-empty list");
        };
        let mut answers = Vec::new();
        for q in questions.iter().take(4) {
            let (Some(text), Some(options)) = (q["question"].as_str(), q["options"].as_array()) else {
                return ToolOutput::error("each question needs `question` and `options`");
            };
            let labels: Vec<&str> = options.iter().filter_map(|o| o["label"].as_str()).collect();
            if labels.len() < 2 {
                return ToolOutput::error("each question needs at least two options");
            }
            let detail: String = options
                .iter()
                .filter_map(|o| Some(format!("- {}: {}", o["label"].as_str()?, o["description"].as_str().unwrap_or(""))))
                .collect::<Vec<_>>()
                .join("\n");
            let mut offered: Vec<Value> = labels
                .iter()
                .enumerate()
                .map(|(i, l)| json!({"optionId": format!("opt-{i}"), "name": l, "kind": "allow_once"}))
                .collect();
            offered.push(json!({"optionId": OTHER, "name": "Something else...", "kind": "allow_once"}));
            offered.push(json!({"optionId": SKIP, "name": "Skip", "kind": "reject_once"}));
            let req = ctx.session.peer.request(
                "session/request_permission",
                json!({
                    "sessionId": ctx.session.id,
                    "toolCall": {
                        "toolCallId": ctx.call_id,
                        "title": text,
                        "kind": "other",
                        "status": "pending",
                        "content": [{"type": "content", "content": {"type": "text", "text": detail}}],
                    },
                    "options": offered,
                }),
            );
            let reply = tokio::select! {
                _ = ctx.cancel.cancelled() => return ToolOutput::error("Cancelled."),
                r = req => r,
            };
            let selected = reply
                .ok()
                .filter(|v| v["outcome"]["outcome"] == "selected")
                .and_then(|v| v["outcome"]["optionId"].as_str().map(str::to_owned));
            answers.push(answer_text(text, &labels, selected.as_deref()));
        }
        ToolOutput::text(format!("The user answered: {}.", answers.join("; ")))
    }
}

/// How one answer reads in the tool result: a picked option, the user's own words, or a skip.
fn answer_text(question: &str, labels: &[&str], selected: Option<&str>) -> String {
    let id = selected.unwrap_or_default();
    if let Some(rest) = id.strip_prefix(OTHER) {
        return match rest.strip_prefix(':').map(str::trim).filter(|t| !t.is_empty()) {
            Some(own) => format!("\"{question}\" = the user wrote their own answer: \"{own}\""),
            None => format!("\"{question}\" = the user wanted something else but gave no text; ask in chat"),
        };
    }
    match id.strip_prefix("opt-").and_then(|i| i.parse::<usize>().ok()).and_then(|i| labels.get(i)) {
        Some(l) => format!("\"{question}\" = \"{l}\""),
        None => format!("\"{question}\" was skipped; ask in chat or decide yourself"),
    }
}

#[cfg(test)]
mod tests {
    use super::answer_text;

    const LABELS: [&str; 2] = ["Alpha", "Beta"];

    #[test]
    fn a_picked_option_reads_as_its_label() {
        assert_eq!(answer_text("Which?", &LABELS, Some("opt-1")), "\"Which?\" = \"Beta\"");
    }

    #[test]
    fn typed_text_reads_as_the_users_own_answer() {
        let got = answer_text("Which?", &LABELS, Some("__ask_question_other__:  Both, in order "));
        assert_eq!(got, "\"Which?\" = the user wrote their own answer: \"Both, in order\"");
    }

    #[test]
    fn something_else_without_text_asks_for_chat() {
        for id in ["__ask_question_other__", "__ask_question_other__:  "] {
            let got = answer_text("Which?", &LABELS, Some(id));
            assert!(got.contains("gave no text"), "{got}");
        }
    }

    #[test]
    fn skip_unknown_ids_and_no_reply_read_as_skipped() {
        for id in [Some("__ask_question_skip__"), Some("opt-9"), None] {
            assert!(answer_text("Which?", &LABELS, id).contains("was skipped"), "{id:?}");
        }
    }
}
