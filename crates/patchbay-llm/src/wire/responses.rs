//! OpenAI Responses (`/responses`).

use serde_json::{Value, json};

use super::*;

pub fn body(cfg: &Config, req: &Request) -> Value {
    let mut input = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => user_items(m, &mut input),
            Role::Assistant => assistant_items(m, &mut input),
        }
    }
    let mut b = json!({
        "model": cfg.model,
        "input": input,
        "stream": true,
        // Stateless: every turn resends the history, reasoning included (encrypted).
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    let sys = system_text(req);
    if !sys.is_empty() {
        b["instructions"] = json!(sys);
    }
    if let Some(n) = cfg.max_output_tokens {
        b["max_output_tokens"] = json!(n);
    }
    if let Some(t) = cfg.temperature {
        b["temperature"] = json!(t);
    }
    if let Some(p) = cfg.top_p {
        b["top_p"] = json!(p);
    }
    let mut reasoning = serde_json::Map::new();
    if let Some(e) = cfg.effort {
        reasoning.insert("effort".into(), json!(e.as_str()));
    }
    if let Some(s) = cfg.summary.filter(|s| *s != Summary::None) {
        reasoning.insert("summary".into(), json!(s.as_str()));
    }
    if !reasoning.is_empty() {
        b["reasoning"] = Value::Object(reasoning);
    }
    if let Some(k) = &req.cache_key {
        b["prompt_cache_key"] = json!(k);
    }
    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters}))
        .collect();
    if req.web_search {
        tools.push(json!({"type": "web_search"}));
    }
    if !tools.is_empty() {
        b["tools"] = json!(tools);
    }
    if !req.tools.is_empty() {
        b["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::Required => json!("required"),
            ToolChoice::None => json!("none"),
            ToolChoice::Tool(n) => json!({"type": "function", "name": n}),
        };
    }
    b
}

fn input_part(p: &Part) -> Option<Value> {
    match p {
        Part::Text(t) if !t.is_empty() => Some(json!({"type": "input_text", "text": t})),
        Part::Image { mime, data } => Some(json!({"type": "input_image", "image_url": data_uri(mime, data)})),
        Part::Document { mime, name, data } => {
            Some(json!({"type": "input_file", "filename": name, "file_data": data_uri(mime, data)}))
        }
        _ => None,
    }
}

fn user_items(m: &Message, out: &mut Vec<Value>) {
    let (results, rest) = split_results(&m.parts);
    let mut result_images = Vec::new();
    for r in results {
        let mut text = r.joined_text();
        if r.is_error && !text.starts_with("Error") {
            text = format!("Error: {text}");
        }
        let images: Vec<&Part> = r.content.iter().filter(|p| matches!(p, Part::Image { .. })).collect();
        if !images.is_empty() {
            text.push_str(&format!("\n[{} image(s) attached below]", images.len()));
            result_images.extend(images.into_iter().filter_map(input_part));
        }
        out.push(json!({"type": "function_call_output", "call_id": r.call_id, "output": text}));
    }
    if !result_images.is_empty() {
        result_images.insert(0, json!({"type": "input_text", "text": "Images returned by the tools above:"}));
        out.push(json!({"role": "user", "content": result_images}));
    }
    let parts: Vec<Value> = rest.into_iter().filter_map(input_part).collect();
    if !parts.is_empty() {
        out.push(json!({"role": "user", "content": parts}));
    }
}

fn assistant_items(m: &Message, out: &mut Vec<Value>) {
    for p in &m.parts {
        match p {
            Part::Reasoning { opaque: Some(item), .. } => out.push(item.clone()),
            Part::Text(t) if !t.is_empty() => {
                out.push(json!({"role": "assistant", "content": [{"type": "output_text", "text": t}]}))
            }
            Part::ToolCall(c) => out.push(json!({
                "type": "function_call",
                "call_id": c.id,
                "name": c.name,
                "arguments": match &c.arguments {
                    Value::String(s) => s.clone(),
                    v => v.to_string(),
                },
            })),
            _ => {}
        }
    }
}

#[derive(Default)]
pub struct ResponsesParser {
    parts: Vec<Part>,
    streamed_text: bool,
    usage: Option<Usage>,
    status: Option<String>,
    incomplete: Option<String>,
    model: Option<String>,
    got_any: bool,
}

impl Parser for ResponsesParser {
    fn event(&mut self, name: Option<&str>, data: &str, out: &mut Vec<Event>) -> Result<()> {
        let v = json_parse(data)?;
        let kind = v.get("type").and_then(Value::as_str).or(name).unwrap_or("");
        let delta = || v.get("delta").and_then(Value::as_str).filter(|d| !d.is_empty());
        match kind {
            "response.output_text.delta" => {
                if let Some(d) = delta() {
                    self.streamed_text = true;
                    out.push(Event::Text(d.to_owned()));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = delta() {
                    out.push(Event::Reasoning(d.to_owned()));
                }
            }
            "response.reasoning_summary_part.added" => {
                // Keep summary paragraphs apart in the UI.
                if self.got_any {
                    out.push(Event::Reasoning("\n\n".into()));
                }
                self.got_any = true;
            }
            "response.output_item.done" => {
                if let Some(item) = v.get("item") {
                    self.item_done(item, out);
                }
            }
            "response.completed" | "response.incomplete" => {
                let r = v.get("response").unwrap_or(&v);
                self.status = r.get("status").and_then(Value::as_str).map(str::to_owned);
                self.incomplete = r
                    .get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.model = r.get("model").and_then(Value::as_str).map(str::to_owned);
                if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
                    self.usage = Some(Usage {
                        input: u64_at(u, &["input_tokens"]),
                        cached_read: u64_at(u, &["input_tokens_details", "cached_tokens"]),
                        cache_write: 0,
                        output: u64_at(u, &["output_tokens"]),
                        reasoning: u64_at(u, &["output_tokens_details", "reasoning_tokens"]),
                        cost_usd: None,
                    });
                }
            }
            "response.failed" => {
                let msg = v
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("the response failed");
                return Err(Error { message: msg.to_owned(), status: None, retryable: false, cancelled: false });
            }
            "error" => {
                return Err(stream_error(&json!({"error": v.clone()}))
                    .unwrap_or_else(|| Error::new("provider error")));
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Response> {
        if self.status.is_none() && self.parts.is_empty() {
            return Err(Error {
                message: "the stream ended without a reply".into(),
                status: None,
                retryable: true,
                cancelled: false,
            });
        }
        let has_calls = self.parts.iter().any(|p| matches!(p, Part::ToolCall(_)));
        let stop = if has_calls {
            StopReason::ToolUse
        } else {
            match self.incomplete.as_deref() {
                Some("max_output_tokens") => StopReason::MaxTokens,
                Some("content_filter") => StopReason::Refusal,
                Some(o) => StopReason::Other(o.to_owned()),
                None => StopReason::EndTurn,
            }
        };
        Ok(Response {
            message: Message { role: Role::Assistant, parts: self.parts },
            usage: self.usage,
            stop,
            model: self.model,
            plan: None,
        })
    }
}

impl ResponsesParser {
    fn item_done(&mut self, item: &Value, out: &mut Vec<Event>) {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let text = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|s| s.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n\n"))
                    .unwrap_or_default();
                self.parts.push(Part::Reasoning { text, signature: None, opaque: Some(item.clone()) });
            }
            Some("message") => {
                let text: String = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|c| c.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect())
                    .unwrap_or_default();
                if !text.is_empty() {
                    if !self.streamed_text {
                        out.push(Event::Text(text.clone()));
                    }
                    self.parts.push(Part::Text(text));
                }
                self.streamed_text = false;
            }
            Some("web_search_call") => {
                if let Some(q) = item.pointer("/action/query").and_then(Value::as_str).filter(|q| !q.is_empty()) {
                    out.push(Event::Search(q.to_owned()));
                }
            }
            Some("function_call") => {
                let args = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                self.parts.push(Part::ToolCall(ToolCall {
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: item.get("name").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    arguments: parse_args(args),
                    signature: None,
                }));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_stateless_request_with_reasoning_round_trip() {
        let mut cfg = Config::new(WireFormat::Responses, "https://api.openai.com/v1", "gpt");
        cfg.effort = Some(Effort::High);
        cfg.summary = Some(Summary::Auto);
        let call = ToolCall { id: "c1".into(), name: "ls".into(), arguments: json!({}), signature: None };
        let mut req = Request::simple("sys", "hi");
        req.messages.push(Message::with_parts(
            Role::Assistant,
            vec![
                Part::Reasoning { text: "t".into(), signature: None, opaque: Some(json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "zz", "summary": []})) },
                Part::ToolCall(call.clone()),
            ],
        ));
        req.messages.push(Message::with_parts(Role::User, vec![Part::ToolResult(ToolResult::text(&call, "out", false))]));
        let b = body(&cfg, &req);
        assert_eq!(b["store"], false);
        assert_eq!(b["instructions"], "sys");
        assert_eq!(b["reasoning"], json!({"effort": "high", "summary": "auto"}));
        assert_eq!(b["input"][1]["encrypted_content"], "zz");
        assert_eq!(b["input"][2]["type"], "function_call");
        assert_eq!(b["input"][3], json!({"type": "function_call_output", "call_id": "c1", "output": "out"}));
    }

    #[test]
    fn parses_stream() {
        let mut p: Box<dyn Parser> = Box::new(ResponsesParser::default());
        let mut out = Vec::new();
        for e in [
            r#"{"type":"response.reasoning_summary_text.delta","delta":"think"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs","summary":[{"type":"summary_text","text":"think"}],"encrypted_content":"e"}}"#,
            r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"Hi"}]}}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c","name":"ls","arguments":"{\"a\":1}"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","model":"m","usage":{"input_tokens":9,"output_tokens":3,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":1}}}}"#,
        ] {
            p.event(None, e, &mut out).unwrap();
        }
        let r = p.finish().unwrap();
        assert_eq!(out, vec![Event::Reasoning("think".into()), Event::Text("Hi".into())]);
        assert!(matches!(&r.message.parts[0], Part::Reasoning { opaque: Some(_), .. }));
        assert_eq!(r.message.text(), "Hi");
        assert_eq!(r.stop, StopReason::ToolUse);
        assert_eq!(r.usage.unwrap().cached_read, 2);
    }
    #[test]
    fn offers_hosted_search_and_reports_its_queries() {
        let cfg = Config::new(WireFormat::Responses, "https://api.openai.com/v1", "gpt");
        let mut req = Request::simple("sys", "news?");
        req.web_search = true;
        let b = body(&cfg, &req);
        assert_eq!(b["tools"], json!([{"type": "web_search"}]));
        assert!(b.get("tool_choice").is_none());

        let mut p: Box<dyn Parser> = Box::new(ResponsesParser::default());
        let mut out = Vec::new();
        for e in [
            r#"{"type":"response.output_item.done","item":{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"rust release"}}}"#,
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"Hi","annotations":[]}]}}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ] {
            p.event(None, e, &mut out).unwrap();
        }
        let r = p.finish().unwrap();
        assert_eq!(out, vec![Event::Search("rust release".into()), Event::Text("Hi".into())]);
        assert_eq!(r.message.parts, vec![Part::Text("Hi".into())]);
    }

}
