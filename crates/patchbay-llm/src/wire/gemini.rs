//! Google Gemini (`models/{model}:streamGenerateContent`).

use serde_json::{Value, json};

use super::*;

fn model_name(cfg: &Config) -> &str {
    cfg.model.strip_prefix("models/").unwrap_or(&cfg.model)
}

pub fn url(cfg: &Config) -> String {
    format!("{}/models/{}:streamGenerateContent?alt=sse", cfg.base_url, model_name(cfg))
}

pub fn body(cfg: &Config, req: &Request) -> Value {
    let mut contents = Vec::new();
    for m in &req.messages {
        let parts = match m.role {
            Role::User => user_parts(m),
            Role::Assistant => model_parts(m),
        };
        if parts.is_empty() {
            continue;
        }
        contents.push(json!({"role": if m.role == Role::User { "user" } else { "model" }, "parts": parts}));
    }
    let mut b = json!({"contents": contents});
    let sys = system_text(req);
    if !sys.is_empty() {
        b["systemInstruction"] = json!({"parts": [{"text": sys}]});
    }
    let mut gen_cfg = serde_json::Map::new();
    if let Some(n) = cfg.max_output_tokens {
        gen_cfg.insert("maxOutputTokens".into(), json!(n));
    }
    if let Some(t) = cfg.temperature {
        gen_cfg.insert("temperature".into(), json!(t));
    }
    if let Some(p) = cfg.top_p {
        gen_cfg.insert("topP".into(), json!(p));
    }
    if let Some(e) = cfg.effort {
        gen_cfg.insert("thinkingConfig".into(), thinking(model_name(cfg), e));
    }
    if !gen_cfg.is_empty() {
        b["generationConfig"] = Value::Object(gen_cfg);
    }
    if !req.tools.is_empty() {
        b["tools"] = json!([{"functionDeclarations": req.tools.iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
            "parametersJsonSchema": t.parameters,
        })).collect::<Vec<_>>()}]);
        b["toolConfig"] = json!({"functionCallingConfig": match &req.tool_choice {
            ToolChoice::Auto => json!({"mode": "AUTO"}),
            ToolChoice::Required => json!({"mode": "ANY"}),
            ToolChoice::None => json!({"mode": "NONE"}),
            ToolChoice::Tool(n) => json!({"mode": "ANY", "allowedFunctionNames": [n]}),
        }});
    }
    b
}

/// Gemini 3 takes a level, 2.x a token budget.
fn thinking(model: &str, e: Effort) -> Value {
    if model.contains("gemini-3") {
        let level = match e {
            Effort::None | Effort::Minimal | Effort::Low => "low",
            Effort::Medium => "medium",
            _ => "high",
        };
        json!({"includeThoughts": e != Effort::None, "thinkingLevel": level})
    } else {
        let budget: i64 = match e {
            Effort::None => 0,
            Effort::Minimal => 512,
            Effort::Low => 1024,
            Effort::Medium => 8192,
            Effort::High => 24576,
            Effort::Xhigh | Effort::Max => -1,
        };
        json!({"includeThoughts": e != Effort::None, "thinkingBudget": budget})
    }
}

fn data_part(p: &Part) -> Option<Value> {
    match p {
        Part::Text(t) if !t.is_empty() => Some(json!({"text": t})),
        Part::Image { mime, data } | Part::Document { mime, data, .. } => {
            Some(json!({"inlineData": {"mimeType": mime, "data": data}}))
        }
        _ => None,
    }
}

fn user_parts(m: &Message) -> Vec<Value> {
    let (results, rest) = split_results(&m.parts);
    let mut out = Vec::new();
    let mut images = Vec::new();
    for r in results {
        let text = r.joined_text();
        let response = if r.is_error { json!({"error": text}) } else { json!({"output": text}) };
        out.push(json!({"functionResponse": {"name": r.name, "response": response}}));
        images.extend(r.content.iter().filter(|p| matches!(p, Part::Image { .. })).filter_map(data_part));
    }
    out.extend(images);
    out.extend(rest.into_iter().filter_map(data_part));
    out
}

fn model_parts(m: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    for p in &m.parts {
        match p {
            Part::ToolCall(c) => {
                let mut v = json!({"functionCall": {
                    "name": c.name,
                    "args": if c.arguments.is_object() { c.arguments.clone() } else { json!({}) },
                }});
                if let Some(s) = &c.signature {
                    v["thoughtSignature"] = json!(s);
                }
                out.push(v);
            }
            Part::Text(t) if !t.is_empty() => out.push(json!({"text": t})),
            _ => {}
        }
    }
    out
}

#[derive(Default)]
pub struct GeminiParser {
    parts: Vec<Part>,
    calls: usize,
    finish: Option<String>,
    usage: Option<Usage>,
    model: Option<String>,
}

impl Parser for GeminiParser {
    fn event(&mut self, _name: Option<&str>, data: &str, out: &mut Vec<Event>) -> Result<()> {
        let v = json_parse(data)?;
        if let Some(e) = stream_error(&v) {
            return Err(e);
        }
        if let Some(r) = v.pointer("/promptFeedback/blockReason").and_then(Value::as_str) {
            return Err(Error::new(format!("the prompt was blocked: {r}")));
        }
        if let Some(m) = v.get("modelVersion").and_then(Value::as_str) {
            self.model = Some(m.to_owned());
        }
        if let Some(u) = v.get("usageMetadata").filter(|u| u.is_object()) {
            let thoughts = u64_at(u, &["thoughtsTokenCount"]);
            self.usage = Some(Usage {
                input: u64_at(u, &["promptTokenCount"]),
                cached_read: u64_at(u, &["cachedContentTokenCount"]),
                cache_write: 0,
                output: u64_at(u, &["candidatesTokenCount"]) + thoughts,
                reasoning: thoughts,
                cost_usd: None,
            });
        }
        let Some(cand) = v.pointer("/candidates/0") else { return Ok(()) };
        if let Some(f) = cand.get("finishReason").and_then(Value::as_str) {
            self.finish = Some(f.to_owned());
        }
        let Some(parts) = cand.pointer("/content/parts").and_then(Value::as_array) else {
            return Ok(());
        };
        for p in parts {
            if let Some(fc) = p.get("functionCall") {
                let n = self.calls;
                self.calls += 1;
                self.parts.push(Part::ToolCall(ToolCall {
                    id: fc.get("id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("call_{n}")),
                    name: fc.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    arguments: fc.get("args").cloned().filter(Value::is_object).unwrap_or_else(|| json!({})),
                    signature: p.get("thoughtSignature").and_then(Value::as_str).map(str::to_owned),
                }));
                continue;
            }
            let Some(text) = p.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
                continue;
            };
            if p.get("thought").and_then(Value::as_bool) == Some(true) {
                out.push(Event::Reasoning(text.to_owned()));
                match self.parts.last_mut() {
                    Some(Part::Reasoning { text: t, .. }) => t.push_str(text),
                    _ => self.parts.push(Part::Reasoning { text: text.to_owned(), signature: None, opaque: None }),
                }
            } else {
                out.push(Event::Text(text.to_owned()));
                match self.parts.last_mut() {
                    Some(Part::Text(t)) => t.push_str(text),
                    _ => self.parts.push(Part::Text(text.to_owned())),
                }
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Response> {
        if self.parts.is_empty() && self.finish.is_none() {
            return Err(Error {
                message: "the stream ended without a reply".into(),
                status: None,
                retryable: true,
                cancelled: false,
            });
        }
        let has_calls = self.calls > 0;
        let stop = match self.finish.as_deref() {
            _ if has_calls => StopReason::ToolUse,
            Some("MAX_TOKENS") => StopReason::MaxTokens,
            Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY") => {
                StopReason::Refusal
            }
            Some("STOP") | None => StopReason::EndTurn,
            Some(o) => StopReason::Other(o.to_owned()),
        };
        Ok(Response {
            message: Message { role: Role::Assistant, parts: self.parts },
            usage: self.usage,
            stop,
            model: self.model,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(model: &str) -> Config {
        Config::new(WireFormat::Gemini, "https://generativelanguage.googleapis.com/v1beta", model)
    }

    #[test]
    fn builds_function_round_trip_and_thinking() {
        let mut c = cfg("models/gemini-3-pro");
        c.effort = Some(Effort::High);
        assert_eq!(
            url(&c),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse"
        );
        let call = ToolCall { id: "call_0".into(), name: "ls".into(), arguments: json!({"d": "."}), signature: Some("SIG".into()) };
        let mut req = Request::simple("sys", "hi");
        req.messages.push(Message::with_parts(Role::Assistant, vec![Part::ToolCall(call.clone())]));
        req.messages.push(Message::with_parts(Role::User, vec![Part::ToolResult(ToolResult::text(&call, "files", false))]));
        req.tools.push(ToolSpec { name: "ls".into(), description: "d".into(), parameters: json!({"type": "object"}) });
        let b = body(&c, &req);
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(b["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high");
        assert_eq!(b["contents"][1]["role"], "model");
        assert_eq!(b["contents"][1]["parts"][0]["thoughtSignature"], "SIG");
        assert_eq!(b["contents"][2]["parts"][0]["functionResponse"]["response"]["output"], "files");
        assert_eq!(b["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["type"], "object");

        let mut c = cfg("gemini-2.5-flash");
        c.effort = Some(Effort::None);
        let b = body(&c, &Request::simple("s", "hi"));
        assert_eq!(b["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0);
    }

    #[test]
    fn parses_thoughts_text_and_calls() {
        let mut p: Box<dyn Parser> = Box::new(GeminiParser::default());
        let mut out = Vec::new();
        for e in [
            r#"{"candidates":[{"content":{"parts":[{"text":"plan","thought":true}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"lo"},{"functionCall":{"name":"ls","args":{"d":"."}},"thoughtSignature":"S"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":3,"thoughtsTokenCount":2,"cachedContentTokenCount":1},"modelVersion":"g"}"#,
        ] {
            p.event(None, e, &mut out).unwrap();
        }
        let r = p.finish().unwrap();
        assert_eq!(out[0], Event::Reasoning("plan".into()));
        assert_eq!(r.message.text(), "Hello");
        let c = r.message.tool_calls().next().unwrap();
        assert_eq!((c.id.as_str(), c.signature.as_deref()), ("call_0", Some("S")));
        assert_eq!(r.stop, StopReason::ToolUse);
        let u = r.usage.unwrap();
        assert_eq!((u.input, u.output, u.reasoning, u.cached_read), (8, 5, 2, 1));
    }

    #[test]
    fn blocked_prompt_is_an_error() {
        let mut p: Box<dyn Parser> = Box::new(GeminiParser::default());
        assert!(p.event(None, r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#, &mut Vec::new()).is_err());
    }
}
