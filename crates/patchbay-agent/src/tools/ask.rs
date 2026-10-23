//! AskUserQuestion: put a multiple-choice question to the user.

use async_trait::async_trait;
use patchbay_llm::ToolSpec;
use serde_json::{Value, json};

use super::*;

pub struct AskUserQuestion;

/// The option id a client treats as "skip": it never auto-answers a question that offers it.
const SKIP: &str = "__ask_question_skip__";

#[async_trait]
impl Tool for AskUserQuestion {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "AskUserQuestion".into(),
            description: "Ask the user a multiple-choice question when you are blocked on a decision only \
they can make (which of two approaches, a name, a scope). Do not use it for things you can find out \
yourself or for approval of a plan. Give two to four options; put the one you recommend first and \
say so in its label. The user can always skip or answer in chat instead."
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
            let chosen = reply
                .ok()
                .filter(|v| v["outcome"]["outcome"] == "selected")
                .and_then(|v| v["outcome"]["optionId"].as_str().map(str::to_owned))
                .and_then(|id| id.strip_prefix("opt-")?.parse::<usize>().ok())
                .and_then(|i| labels.get(i).copied());
            answers.push(match chosen {
                Some(l) => format!("\"{text}\" = \"{l}\""),
                None => format!("\"{text}\" was skipped; ask in chat or decide yourself"),
            });
        }
        ToolOutput::text(format!("The user answered: {}.", answers.join("; ")))
    }
}
