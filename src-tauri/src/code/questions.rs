//! Questions an agent asks the user, normalized into one form.
//!
//! Grok sends `x.ai/ask_user_question`. Claude sends `elicitation/create` (rewritten onto the
//! extension channel, which this SDK builds). Cursor and others send a permission request whose
//! options are the choices. The card stays up until the user submits; skipping a question omits it.

use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionForm {
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    pub prompt: String,
    pub multi: bool,
    pub free_text: bool,
    pub skippable: bool,
    pub options: Vec<QuestionOption>,
    /// Permission option chosen when this question is skipped. Not shown.
    #[serde(skip)]
    pub skip_option_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// What the card submitted. A skipped question is absent.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionReply {
    #[serde(default)]
    pub answers: Vec<Answered>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Answered {
    pub id: String,
    #[serde(default)]
    pub picks: Vec<String>,
    #[serde(default)]
    pub note: String,
}

/// Rewrite a client-bound JSON-RPC line so form elicitation arrives as an extension call.
/// The ACP crate this app builds only dispatches extension methods (a leading `_`) besides its
/// own list, and Claude's adapter calls `elicitation/create`.
pub fn rewrite_client_method(line: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(line) else {
        return line.to_owned();
    };
    let Some(method) = v.get("method").and_then(|m| m.as_str()) else {
        return line.to_owned();
    };
    if method == "elicitation/create" || method == "session/elicitation" {
        v["method"] = json!("_elicitation/create");
        return serde_json::to_string(&v).unwrap_or_else(|_| line.to_owned());
    }
    line.to_owned()
}

/// `x.ai/ask_user_question` or a form elicitation. `None` when this payload is not a form.
pub fn from_ext(method: &str, raw: &Value) -> Option<QuestionForm> {
    if method.contains("ask_user_question") {
        return grok_form(raw);
    }
    if method.contains("elicitation") {
        return elicitation_form(raw);
    }
    None
}

/// A permission request that is a menu of choices, not a safety prompt.
pub fn from_permission(tool: &Value, options: &Value) -> Option<QuestionForm> {
    let opts = options.as_array()?;
    if opts.iter().any(|o| id_of(o).starts_with("exit-plan-")) {
        return None;
    }
    let skip = opts.iter().find(|o| id_of(o) == "__ask_question_skip__").map(id_of);
    let allows: Vec<&Value> = opts
        .iter()
        .filter(|o| o["kind"] == "allow_once" && id_of(o) != "__ask_question_skip__")
        .collect();
    let safety = opts.iter().any(|o| {
        let kind = o["kind"].as_str().unwrap_or("");
        kind == "allow_always" || kind.starts_with("reject")
    });
    // Cursor's AskQuestion, or any agent that offers only one-off choices.
    if skip.is_none() && (safety || allows.len() < 2) {
        return None;
    }
    if allows.is_empty() {
        return None;
    }
    let prompt = tool["title"]
        .as_str()
        .filter(|t| !t.is_empty())
        .unwrap_or("Which do you want?")
        .to_owned();
    let choices = allows
        .into_iter()
        .map(|o| QuestionOption {
            id: id_of(o),
            label: o["name"].as_str().unwrap_or("Choose").to_owned(),
            description: o["description"].as_str().map(str::to_owned),
        })
        .collect();
    Some(QuestionForm {
        questions: vec![Question {
            id: "choice".into(),
            prompt,
            multi: false,
            free_text: false,
            skippable: skip.is_some(),
            options: choices,
            skip_option_id: skip,
        }],
    })
}

pub fn grok_response(answers: &[Answered]) -> Value {
    let mut picked = Map::new();
    let mut notes = Map::new();
    for a in answers {
        let mut labels = a.picks.clone();
        let note = a.note.trim();
        if labels.is_empty() && !note.is_empty() {
            labels.push("Other".into());
        }
        if labels.is_empty() {
            continue;
        }
        picked.insert(a.id.clone(), json!(labels));
        if !note.is_empty() {
            notes.insert(a.id.clone(), json!({ "notes": note }));
        }
    }
    let mut resp = json!({ "outcome": "accepted", "answers": picked });
    if !notes.is_empty() {
        resp["annotations"] = Value::Object(notes);
    }
    resp
}

pub fn elicitation_response(form: &QuestionForm, answers: &[Answered]) -> Value {
    let mut content = Map::new();
    for a in answers {
        let multi = form.questions.iter().find(|q| q.id == a.id).is_some_and(|q| q.multi);
        if !a.picks.is_empty() {
            content.insert(
                a.id.clone(),
                if multi { json!(a.picks) } else { json!(&a.picks[0]) },
            );
        }
        let note = a.note.trim();
        if !note.is_empty() {
            content.insert(format!("{}_custom", a.id), json!(note));
        }
    }
    json!({ "action": "accept", "content": content })
}

/// The permission option id the user picked, or the skip option when they left it blank.
pub fn permission_option(form: &QuestionForm, answers: &[Answered]) -> Option<String> {
    let q = form.questions.first()?;
    let answer = answers.iter().find(|a| a.id == q.id);
    if let Some(id) = answer.and_then(|a| a.picks.first()).filter(|s| !s.is_empty()) {
        return Some(id.clone());
    }
    q.skip_option_id.clone()
}

fn grok_form(raw: &Value) -> Option<QuestionForm> {
    let list = raw["questions"].as_array()?;
    let questions: Vec<Question> = list.iter().filter_map(grok_question).collect();
    if questions.is_empty() { None } else { Some(QuestionForm { questions }) }
}

fn grok_question(q: &Value) -> Option<Question> {
    let prompt = q["question"].as_str().filter(|s| !s.is_empty())?.to_owned();
    let options = q["options"].as_array()?.iter().filter_map(grok_option).collect::<Vec<_>>();
    if options.is_empty() {
        return None;
    }
    Some(Question {
        id: prompt.clone(),
        prompt,
        multi: q["multiSelect"].as_bool().unwrap_or(false),
        free_text: true,
        skippable: true,
        options,
        skip_option_id: None,
    })
}

fn grok_option(o: &Value) -> Option<QuestionOption> {
    let label = o["label"].as_str().filter(|s| !s.is_empty())?.to_owned();
    Some(QuestionOption {
        id: label.clone(),
        label,
        description: o["description"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
    })
}

fn elicitation_form(raw: &Value) -> Option<QuestionForm> {
    if raw["mode"] != "form" {
        return None;
    }
    let props = raw["requestedSchema"]["properties"].as_object()?;
    let message = raw["message"].as_str().unwrap_or("").trim();
    // `question_N` is Claude's AskUserQuestion shape. Other enum fields (a refusal
    // fallback's `choice`) are questions too. Companion `*_custom` boxes are not.
    let mut keys: Vec<&String> = props
        .keys()
        .filter(|k| !k.ends_with("_custom") && !enum_options(&props[*k]).is_empty())
        .collect();
    keys.sort_by(|a, b| field_rank(a).cmp(&field_rank(b)).then(a.cmp(b)));
    let single = keys.len() == 1;
    let mut questions = Vec::new();
    for key in keys {
        let field = &props[key];
        let options = enum_options(field);
        if options.is_empty() {
            continue;
        }
        let own = field["description"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let prompt = own.unwrap_or_else(|| {
            if single && !message.is_empty() {
                message.to_owned()
            } else {
                field["title"].as_str().filter(|s| !s.is_empty()).unwrap_or(message).to_owned()
            }
        });
        if prompt.is_empty() {
            continue;
        }
        questions.push(Question {
            id: key.clone(),
            prompt,
            multi: field["type"] == "array",
            free_text: props.contains_key(&format!("{key}_custom")),
            skippable: true,
            options,
            skip_option_id: None,
        });
    }
    if questions.is_empty() { None } else { Some(QuestionForm { questions }) }
}

/// `question_N` stays in the order Claude sent. Every other field follows.
fn field_rank(key: &str) -> (u8, usize) {
    let Some(rest) = key.strip_prefix("question_") else {
        return (1, 0);
    };
    rest.parse::<usize>().map(|n| (0, n)).unwrap_or((1, 0))
}

fn enum_options(field: &Value) -> Vec<QuestionOption> {
    let list = if field["type"] == "array" {
        field["items"]["anyOf"].as_array().or_else(|| field["items"]["oneOf"].as_array())
    } else {
        field["oneOf"].as_array().or_else(|| field["anyOf"].as_array())
    };
    list.map(|opts| opts.iter().filter_map(enum_option).collect()).unwrap_or_default()
}

fn enum_option(o: &Value) -> Option<QuestionOption> {
    let id = o["const"].as_str().filter(|s| !s.is_empty())?.to_owned();
    let label = o["title"].as_str().filter(|s| !s.is_empty()).unwrap_or(&id).to_owned();
    Some(QuestionOption {
        id,
        label,
        description: o["description"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
    })
}

fn id_of(o: &Value) -> String {
    o["optionId"].as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_only_elicitation_methods() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"elicitation/create","params":{"mode":"form"}}"#;
        let out: Value = serde_json::from_str(&rewrite_client_method(line)).unwrap();
        assert_eq!(out["method"], "_elicitation/create");
        let other = r#"{"method":"session/request_permission"}"#;
        assert_eq!(rewrite_client_method(other), other);
        assert_eq!(rewrite_client_method("not json"), "not json");
    }

    #[test]
    fn grok_form_keeps_descriptions_and_other() {
        let raw = json!({
            "questions": [{
                "question": "When should it be torn down?",
                "multiSelect": false,
                "options": [
                    { "label": "After the policy read (Recommended)", "description": "About 6 hours." },
                    { "label": "Keep through tomorrow", "description": "A day more of cost." }
                ]
            }]
        });
        let form = from_ext("x.ai/ask_user_question", &raw).unwrap();
        assert_eq!(form.questions.len(), 1);
        assert!(form.questions[0].free_text);
        assert_eq!(form.questions[0].options[0].description.as_deref(), Some("About 6 hours."));
        let resp = grok_response(&[Answered {
            id: form.questions[0].id.clone(),
            picks: vec![],
            note: "Friday".into(),
        }]);
        assert_eq!(resp["outcome"], "accepted");
        assert_eq!(resp["answers"]["When should it be torn down?"][0], "Other");
        assert_eq!(resp["annotations"]["When should it be torn down?"]["notes"], "Friday");
    }

    #[test]
    fn skipped_grok_question_is_omitted() {
        let resp = grok_response(&[]);
        assert_eq!(resp["answers"], json!({}));
        assert!(resp.get("annotations").is_none());
    }

    #[test]
    fn elicitation_form_uses_the_message_for_one_question() {
        let raw = json!({
            "mode": "form",
            "message": "When should it be torn down?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "question_0": {
                        "type": "string",
                        "oneOf": [
                            { "const": "After the policy read", "title": "After the policy read", "description": "About 6 hours." }
                        ]
                    },
                    "question_0_custom": { "type": "string", "title": "Other" },
                    "question_1": {
                        "type": "array",
                        "description": "Which logs?",
                        "items": { "anyOf": [ { "const": "Firewall", "title": "Firewall" } ] }
                    }
                }
            }
        });
        let form = from_ext("elicitation/create", &raw).unwrap();
        assert_eq!(form.questions[0].prompt, "When should it be torn down?");
        assert!(!form.questions[0].multi);
        assert!(form.questions[0].free_text);
        assert!(form.questions[1].multi);
        assert_eq!(form.questions[1].prompt, "Which logs?");
        let resp = elicitation_response(&form, &[Answered {
            id: "question_0".into(),
            picks: vec!["After the policy read".into()],
            note: "and tell me".into(),
        }]);
        assert_eq!(resp["action"], "accept");
        assert_eq!(resp["content"]["question_0"], "After the policy read");
        assert_eq!(resp["content"]["question_0_custom"], "and tell me");
    }

    #[test]
    fn a_form_whose_field_is_not_question_n_still_asks() {
        let raw = json!({
            "mode": "form",
            "message": "Opus declined this request. Retry with Sonnet?",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "choice": {
                        "type": "string",
                        "oneOf": [
                            { "const": "retry_fallback", "title": "Retry with Sonnet" },
                            { "const": "cancelled", "title": "Keep the refusal" }
                        ]
                    }
                }
            }
        });
        let form = from_ext("elicitation/create", &raw).unwrap();
        assert_eq!(form.questions[0].id, "choice");
        assert_eq!(form.questions[0].prompt, "Opus declined this request. Retry with Sonnet?");
        assert!(!form.questions[0].free_text);
        let resp = elicitation_response(
            &form,
            &[Answered { id: "choice".into(), picks: vec!["retry_fallback".into()], note: String::new() }],
        );
        assert_eq!(resp["content"]["choice"], "retry_fallback");
    }

    #[test]
    fn url_elicitation_is_not_a_form() {
        let raw = json!({ "mode": "url", "url": "https://example.com", "message": "Sign in" });
        assert!(from_ext("elicitation/create", &raw).is_none());
    }

    #[test]
    fn cursor_question_is_a_form_and_a_plan_approval_is_not() {
        let ask = json!({ "title": "Which database?" });
        let ask_opts = json!([
            { "optionId": "pg", "name": "Postgres", "kind": "allow_once" },
            { "optionId": "sqlite", "name": "SQLite", "kind": "allow_once" },
            { "optionId": "__ask_question_skip__", "name": "Skip", "kind": "allow_once" }
        ]);
        let form = from_permission(&ask, &ask_opts).unwrap();
        assert_eq!(form.questions[0].options.len(), 2);
        assert!(form.questions[0].skippable);
        assert!(!form.questions[0].free_text);
        assert_eq!(
            permission_option(&form, &[Answered { id: "choice".into(), picks: vec!["pg".into()], note: String::new() }]).as_deref(),
            Some("pg")
        );
        assert_eq!(permission_option(&form, &[]).as_deref(), Some("__ask_question_skip__"));

        let plan = json!({ "title": "Approve Plan", "kind": "switch_mode" });
        let plan_opts = json!([
            { "optionId": "exit-plan-default", "name": "Approve", "kind": "allow_once" },
            { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
        ]);
        assert!(from_permission(&plan, &plan_opts).is_none());

        let shell = json!([
            { "optionId": "once", "name": "Allow", "kind": "allow_once" },
            { "optionId": "always", "name": "Always", "kind": "allow_always" },
            { "optionId": "no", "name": "Deny", "kind": "reject_once" }
        ]);
        assert!(from_permission(&json!({ "title": "Run tests" }), &shell).is_none());
    }
}
