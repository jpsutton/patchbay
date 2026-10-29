//! OpenAI Chat Completions (`/chat/completions`) and the servers that imitate it.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;

fn is_openai(cfg: &Config) -> bool {
    cfg.base_url.contains("api.openai.com")
}

pub fn body(cfg: &Config, req: &Request) -> Value {
    let mut messages = Vec::new();
    let sys = system_text(req);
    if !sys.is_empty() {
        messages.push(json!({"role": "system", "content": sys}));
    }
    for m in &req.messages {
        match m.role {
            Role::User => user_messages(m, &mut messages),
            Role::Assistant => messages.push(assistant_message(m, send_reasoning(cfg))),
        }
    }
    let mut b = json!({
        "model": cfg.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(n) = cfg.max_output_tokens {
        // OpenAI's reasoning models reject `max_tokens`; everyone else still expects it.
        let key = if is_openai(cfg) { "max_completion_tokens" } else { "max_tokens" };
        b[key] = json!(n);
    }
    if let Some(t) = cfg.temperature {
        b["temperature"] = json!(t);
    }
    if let Some(p) = cfg.top_p {
        b["top_p"] = json!(p);
    }
    if let Some(e) = cfg.effort {
        b["reasoning_effort"] = json!(if e == Effort::Max { "xhigh" } else { e.as_str() });
    }
    if is_openai(cfg)
        && let Some(k) = &req.cache_key
    {
        b["prompt_cache_key"] = json!(k);
    }
    if !req.tools.is_empty() {
        b["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t.name, "description": t.description, "parameters": t.parameters,
                }})
            })
            .collect();
        b["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::Required => json!("required"),
            ToolChoice::None => json!("none"),
            ToolChoice::Tool(n) => json!({"type": "function", "function": {"name": n}}),
        };
    }
    b
}

fn content_part(p: &Part) -> Option<Value> {
    match p {
        Part::Text(t) if !t.is_empty() => Some(json!({"type": "text", "text": t})),
        Part::Image { mime, data } => {
            Some(json!({"type": "image_url", "image_url": {"url": data_uri(mime, data)}}))
        }
        Part::Document { mime, name, data } => Some(
            json!({"type": "file", "file": {"filename": name, "file_data": data_uri(mime, data)}}),
        ),
        _ => None,
    }
}

fn user_messages(m: &Message, out: &mut Vec<Value>) {
    let (results, rest) = split_results(&m.parts);
    let mut result_images = Vec::new();
    for r in results {
        let mut text = r.joined_text();
        if r.is_error && !text.starts_with("Error") {
            text = format!("Error: {text}");
        }
        let images = r.content.iter().filter(|p| matches!(p, Part::Image { .. }));
        let n = images.clone().count();
        if n > 0 {
            text.push_str(&format!("\n[{n} image(s) attached below]"));
            result_images.extend(images.filter_map(content_part));
        }
        out.push(json!({"role": "tool", "tool_call_id": r.call_id, "content": text}));
    }
    if !result_images.is_empty() {
        result_images.insert(0, json!({"type": "text", "text": "Images returned by the tools above:"}));
        out.push(json!({"role": "user", "content": result_images}));
    }
    let parts: Vec<Value> = rest.into_iter().filter_map(content_part).collect();
    if parts.is_empty() {
        return;
    }
    if parts.iter().all(|p| p["type"] == "text") {
        let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
        out.push(json!({"role": "user", "content": text.join("\n\n")}));
    } else {
        out.push(json!({"role": "user", "content": parts}));
    }
}

/// Whether assistant turns carry their reasoning back. OpenAI's own Chat Completions never
/// returns reasoning text and has no field to take it, so it never gets one.
fn send_reasoning(cfg: &Config) -> bool {
    cfg.send_reasoning && !is_openai(cfg)
}

fn assistant_message(m: &Message, with_reasoning: bool) -> Value {
    let text = m.text();
    let calls: Vec<Value> = m
        .tool_calls()
        .map(|c| {
            json!({"id": c.id, "type": "function", "function": {
                "name": c.name,
                "arguments": match &c.arguments {
                    Value::String(s) => s.clone(),
                    v => v.to_string(),
                },
            }})
        })
        .collect();
    let mut msg = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
    if !calls.is_empty() {
        msg["tool_calls"] = json!(calls);
    } else if text.is_empty() {
        msg["content"] = json!("");
    }
    if with_reasoning {
        let reasoning: Vec<&str> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Reasoning { text: t, .. } if !t.is_empty() => Some(t.as_str()),
                _ => None,
            })
            .collect();
        if !reasoning.is_empty() {
            msg["reasoning_content"] = json!(reasoning.join("\n\n"));
        }
    }
    msg
}

#[derive(Default)]
struct CallAcc {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
pub struct ChatParser {
    text: String,
    reasoning: String,
    calls: BTreeMap<usize, CallAcc>,
    finish: Option<String>,
    usage: Option<Usage>,
    model: Option<String>,
}

impl Parser for ChatParser {
    fn event(&mut self, _name: Option<&str>, data: &str, out: &mut Vec<Event>) -> Result<()> {
        let v = json_parse(data)?;
        if let Some(e) = stream_error(&v) {
            return Err(e);
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_owned());
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage_of(u));
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            return Ok(());
        };
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(f.to_owned());
        }
        let Some(delta) = choice.get("delta") else { return Ok(()) };
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta.get(key).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                self.reasoning.push_str(t);
                out.push(Event::Reasoning(t.to_owned()));
                break;
            }
        }
        if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            out.push(Event::Text(t.to_owned()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                self.tool_delta(c);
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Response> {
        let mut parts = Vec::new();
        if !self.reasoning.is_empty() {
            parts.push(Part::Reasoning { text: self.reasoning, signature: None, opaque: None });
        }
        if !self.text.is_empty() {
            parts.push(Part::Text(self.text));
        }
        let mut has_calls = false;
        for (n, c) in self.calls.into_values().enumerate() {
            if c.name.is_empty() {
                continue;
            }
            has_calls = true;
            parts.push(Part::ToolCall(ToolCall {
                id: if c.id.is_empty() { format!("call_{n}") } else { c.id },
                name: c.name,
                arguments: parse_args(&c.args),
                signature: None,
            }));
        }
        if parts.is_empty() && self.finish.is_none() && self.usage.is_none() {
            return Err(Error {
                message: "the stream ended without a reply".into(),
                status: None,
                retryable: true,
                cancelled: false,
            });
        }
        let stop = match self.finish.as_deref() {
            _ if has_calls => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            Some("stop") | None => StopReason::EndTurn,
            Some(o) => StopReason::Other(o.to_owned()),
        };
        Ok(Response {
            message: Message { role: Role::Assistant, parts },
            usage: self.usage,
            stop,
            model: self.model,
            plan: None,
        })
    }
}

impl ChatParser {
    fn tool_delta(&mut self, c: &Value) {
        let id = c.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
        let idx = match c.get("index").and_then(Value::as_u64) {
            Some(i) => i as usize,
            // Servers that omit the index send each call whole, or keep appending to the last.
            None => match (id, self.calls.keys().next_back()) {
                (Some(id), Some(&last)) if self.calls[&last].id != id => last + 1,
                (_, Some(&last)) => last,
                (_, None) => 0,
            },
        };
        let acc = self.calls.entry(idx).or_default();
        if let Some(id) = id {
            acc.id = id.to_owned();
        }
        if let Some(f) = c.get("function") {
            if let Some(n) = f.get("name").and_then(Value::as_str) {
                acc.name.push_str(n);
            }
            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                acc.args.push_str(a);
            }
        }
    }
}

fn usage_of(u: &Value) -> Usage {
    let cached = u64_at(u, &["prompt_tokens_details", "cached_tokens"]).max(u64_at(u, &["prompt_cache_hit_tokens"]));
    let ticks = u.get("cost_in_usd_ticks").and_then(Value::as_i64).filter(|t| *t > 0);
    Usage {
        input: u64_at(u, &["prompt_tokens"]),
        cached_read: cached,
        cache_write: 0,
        output: u64_at(u, &["completion_tokens"]),
        reasoning: u64_at(u, &["completion_tokens_details", "reasoning_tokens"]),
        cost_usd: ticks
            .map(|t| t as f64 / 1e10)
            .or_else(|| u.get("cost").and_then(Value::as_f64).filter(|c| *c > 0.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::new(WireFormat::ChatCompletions, "https://api.example.com/v1", "m");
        c.max_output_tokens = Some(100);
        c
    }

    fn run(events: &[&str]) -> (Vec<Event>, Response) {
        let mut p: Box<dyn Parser> = Box::new(ChatParser::default());
        let mut out = Vec::new();
        for e in events {
            p.event(None, e, &mut out).unwrap();
        }
        (out, p.finish().unwrap())
    }

    #[test]
    fn builds_tool_round_trip() {
        let call = ToolCall { id: "c1".into(), name: "read".into(), arguments: json!({"path": "a"}), signature: None };
        let mut req = Request::simple("be brief", "hi");
        req.messages.push(Message::with_parts(Role::Assistant, vec![Part::Text("ok".into()), Part::ToolCall(call.clone())]));
        req.messages.push(Message::with_parts(
            Role::User,
            vec![Part::ToolResult(ToolResult::text(&call, "contents", false))],
        ));
        req.tools.push(ToolSpec { name: "read".into(), description: "d".into(), parameters: json!({"type": "object"}) });
        let b = body(&cfg(), &req);
        assert_eq!(b["max_tokens"], 100);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a\"}");
        assert_eq!(b["messages"][3], json!({"role": "tool", "tool_call_id": "c1", "content": "contents"}));
        assert_eq!(b["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn sends_reasoning_back_unless_turned_off() {
        let call = ToolCall { id: "c1".into(), name: "read".into(), arguments: json!({}), signature: None };
        let mut req = Request::simple("sys", "hi");
        req.messages.push(Message::with_parts(
            Role::Assistant,
            vec![
                Part::Reasoning { text: "need the file".into(), signature: None, opaque: None },
                // An Anthropic redacted block carries no text and must not add an empty entry.
                Part::Reasoning { text: String::new(), signature: None, opaque: Some(json!({"type": "redacted_thinking"})) },
                Part::ToolCall(call.clone()),
            ],
        ));
        req.messages.push(Message::with_parts(Role::User, vec![Part::ToolResult(ToolResult::text(&call, "x", false))]));
        let assistant = |c: &Config| body(c, &req)["messages"][2].clone();

        let on = assistant(&cfg());
        assert_eq!(on["reasoning_content"], "need the file");
        assert_eq!(on["tool_calls"][0]["id"], "c1");

        let mut off = cfg();
        off.send_reasoning = false;
        assert!(assistant(&off).get("reasoning_content").is_none());

        let openai = Config::new(WireFormat::ChatCompletions, "https://api.openai.com/v1", "m");
        assert!(assistant(&openai).get("reasoning_content").is_none());
    }

    #[test]
    fn streams_text_reasoning_and_split_tool_calls() {
        let (events, r) = run(&[
            r#"{"model":"m","choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo","tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":null,"prompt_tokens_details":{"cached_tokens":4},"cost_in_usd_ticks":5000000000}}"#,
        ]);
        assert_eq!(events[0], Event::Reasoning("hmm".into()));
        assert_eq!(r.message.text(), "Hello");
        let c = r.message.tool_calls().next().unwrap();
        assert_eq!((c.id.as_str(), c.name.as_str()), ("c1", "read"));
        assert_eq!(c.arguments, json!({"path": "x"}));
        assert_eq!(r.stop, StopReason::ToolUse);
        let u = r.usage.unwrap();
        assert_eq!((u.input, u.cached_read, u.output), (10, 4, 0));
        assert_eq!(u.cost_usd, Some(0.5));
    }

    #[test]
    fn calls_without_index_are_told_apart_by_id() {
        let (_, r) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"f","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"g","arguments":"{}"}}]},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(r.message.tool_calls().count(), 2);
        assert_eq!(r.stop, StopReason::ToolUse);
    }
}
