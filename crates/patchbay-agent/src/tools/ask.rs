//! AskUserQuestion: put one or more multiple-choice questions to the user.
//!
//! All questions go in one `_patchbay/ask_user_question` form so the UI can page between them
//! (‹ › / “1 of N”) and revise earlier answers before Send — the same card Grok multi-question
//! forms use. A sequential `session/request_permission` per question made that impossible.

use async_trait::async_trait;
use patchbay_llm::ToolSpec;
use serde_json::{Map, Value, json};

use super::*;

pub struct AskUserQuestion;

#[async_trait]
impl Tool for AskUserQuestion {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "AskUserQuestion".into(),
            description: "Ask the user one or more multiple-choice questions when you are blocked on a \
decision only they can make (which of two approaches, a name, a scope). Do not use it for things you \
can find out yourself or for approval of a plan. Give two to four options per question; put the one \
you recommend first and say so in its label. When you ask several questions they appear in one form: \
the user can move back and forth, change earlier answers, then send. They can always skip a question, \
type their own answer, or answer in chat instead."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "One to four questions, shown together so the user can page between them",
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
        let Some(raw_questions) = input["questions"].as_array().filter(|q| !q.is_empty()) else {
            return ToolOutput::error("questions must be a non-empty list");
        };
        let mut form_questions = Vec::new();
        let mut prompts = Vec::new();
        for q in raw_questions.iter().take(4) {
            let (Some(text), Some(options)) = (q["question"].as_str(), q["options"].as_array()) else {
                return ToolOutput::error("each question needs `question` and `options`");
            };
            let opts: Vec<Value> = options
                .iter()
                .filter_map(|o| {
                    let label = o["label"].as_str()?;
                    let mut item = json!({ "label": label });
                    if let Some(d) = o["description"].as_str().filter(|s| !s.is_empty()) {
                        item["description"] = json!(d);
                    }
                    Some(item)
                })
                .collect();
            if opts.len() < 2 {
                return ToolOutput::error("each question needs at least two options");
            }
            prompts.push(text.to_owned());
            let mut item = json!({ "question": text, "options": opts });
            if let Some(h) = q["header"].as_str().filter(|s| !s.is_empty()) {
                item["header"] = json!(h);
            }
            form_questions.push(item);
        }

        // One form for every question: Patchbay's QuestionCard pages with ‹ › when length > 1.
        // `from_ext` treats any `*ask_user_question*` client method as this Grok-shaped payload.
        let req = ctx.session.peer.request(
            "_patchbay/ask_user_question",
            json!({ "questions": form_questions }),
        );
        let reply = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("Cancelled."),
            r = req => r,
        };
        let Ok(body) = reply else {
            return ToolOutput::text(format!(
                "The user answered: {}.",
                prompts.iter().map(|p| format!("\"{p}\" was skipped; ask in chat or decide yourself")).collect::<Vec<_>>().join("; ")
            ));
        };
        if body["outcome"] == "cancelled" {
            return ToolOutput::text(format!(
                "The user answered: {}.",
                prompts.iter().map(|p| format!("\"{p}\" was skipped; ask in chat or decide yourself")).collect::<Vec<_>>().join("; ")
            ));
        }

        let answers_map = body["answers"].as_object().cloned().unwrap_or_default();
        let notes_map = body["annotations"].as_object().cloned().unwrap_or_default();
        let lines: Vec<String> = prompts
            .iter()
            .map(|prompt| format_answer(prompt, &answers_map, &notes_map))
            .collect();
        ToolOutput::text(format!("The user answered: {}.", lines.join("; ")))
    }
}

/// One question's line in the tool result, from a Grok-shaped ask_user_question reply.
fn format_answer(prompt: &str, answers: &Map<String, Value>, notes: &Map<String, Value>) -> String {
    let note = notes
        .get(prompt)
        .and_then(|v| v["notes"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let picks = answers.get(prompt).and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let labels: Vec<&str> = picks.iter().filter_map(|v| v.as_str()).collect();

    if let Some(own) = note {
        // Grok free-text: picks may be ["Other"] with the note in annotations.
        if labels.is_empty() || labels == ["Other"] {
            return format!("\"{prompt}\" = the user wrote their own answer: \"{own}\"");
        }
        return format!("\"{prompt}\" = \"{}\" (also: \"{own}\")", labels.join("\", \""));
    }
    if labels.is_empty() {
        return format!("\"{prompt}\" was skipped; ask in chat or decide yourself");
    }
    if labels == ["Other"] {
        return format!("\"{prompt}\" = the user wanted something else but gave no text; ask in chat");
    }
    format!("\"{prompt}\" = \"{}\"", labels.join("\", \""))
}

#[cfg(test)]
mod tests {
    use super::format_answer;
    use serde_json::{Map, Value, json};

    fn maps(answers: &[(&str, Value)], notes: &[(&str, &str)]) -> (Map<String, Value>, Map<String, Value>) {
        let mut a = Map::new();
        for (k, v) in answers {
            a.insert((*k).into(), v.clone());
        }
        let mut n = Map::new();
        for (k, note) in notes {
            n.insert((*k).into(), json!({ "notes": note }));
        }
        (a, n)
    }

    #[test]
    fn a_picked_option_reads_as_its_label() {
        let (a, n) = maps(&[("Which?", json!(["Beta"]))], &[]);
        assert_eq!(format_answer("Which?", &a, &n), "\"Which?\" = \"Beta\"");
    }

    #[test]
    fn typed_text_reads_as_the_users_own_answer() {
        let (a, n) = maps(&[("Which?", json!(["Other"]))], &[("Which?", "Both, in order")]);
        assert_eq!(
            format_answer("Which?", &a, &n),
            "\"Which?\" = the user wrote their own answer: \"Both, in order\""
        );
    }

    #[test]
    fn something_else_without_text_asks_for_chat() {
        let (a, n) = maps(&[("Which?", json!(["Other"]))], &[]);
        assert!(format_answer("Which?", &a, &n).contains("gave no text"));
    }

    #[test]
    fn missing_answer_reads_as_skipped() {
        let (a, n) = maps(&[], &[]);
        assert!(format_answer("Which?", &a, &n).contains("was skipped"));
    }

    #[test]
    fn multi_select_joins_labels() {
        let (a, n) = maps(&[("Which?", json!(["Alpha", "Beta"]))], &[]);
        assert_eq!(format_answer("Which?", &a, &n), "\"Which?\" = \"Alpha\", \"Beta\"");
    }
}
