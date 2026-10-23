//! Anthropic Messages (`/messages`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;

const DEFAULT_MAX_TOKENS: u32 = 8192;

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

pub fn body(cfg: &Config, req: &Request) -> Value {
    let mut system: Vec<Value> = req
        .system
        .iter()
        .filter(|b| !b.text.trim().is_empty())
        .map(|b| {
            let mut v = json!({"type": "text", "text": b.text});
            if b.cache == Cache::Breakpoint {
                v["cache_control"] = ephemeral();
            }
            v
        })
        .collect();
    // The API allows four breakpoints; the last one goes on the transcript tip.
    let mut seen = 0;
    for v in system.iter_mut().rev() {
        if v.get("cache_control").is_some() {
            seen += 1;
            if seen > 2 {
                v.as_object_mut().unwrap().remove("cache_control");
            }
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    for m in &req.messages {
        let content = match m.role {
            Role::User => user_blocks(m),
            Role::Assistant => assistant_blocks(m),
        };
        if content.is_empty() {
            continue;
        }
        messages.push(json!({"role": if m.role == Role::User { "user" } else { "assistant" }, "content": content}));
    }
    if let Some(last) = messages.last_mut()
        && let Some(block) = last["content"].as_array_mut().and_then(|c| c.last_mut())
        && block["type"] != "thinking"
        && block["type"] != "redacted_thinking"
    {
        block["cache_control"] = ephemeral();
    }

    let thinking = matches!(cfg.effort, Some(e) if e > Effort::Minimal);
    let mut b = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        b["system"] = json!(system);
    }
    if thinking {
        b["thinking"] = json!({"type": "adaptive", "display": "summarized"});
        b["output_config"] = json!({"effort": cfg.effort.unwrap().as_str()});
        if needs_budget_thinking(&cfg.model) {
            downgrade_thinking(&mut b);
        }
    } else {
        // Extended thinking only runs at the default temperature.
        if let Some(t) = cfg.temperature {
            b["temperature"] = json!(t);
        }
        if let Some(p) = cfg.top_p {
            b["top_p"] = json!(p);
        }
    }
    if !req.tools.is_empty() {
        b["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
            .collect();
        b["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => json!({"type": "auto"}),
            ToolChoice::Required => json!({"type": "any"}),
            ToolChoice::None => json!({"type": "none"}),
            ToolChoice::Tool(n) => json!({"type": "tool", "name": n}),
        };
    }
    b
}

/// Models that predate adaptive thinking take an explicit token budget instead. Names the API
/// has refused are listed here; for the rest [`downgrade_thinking`] answers the refusal.
pub fn needs_budget_thinking(model: &str) -> bool {
    model.contains("haiku-4") || model.contains("claude-3")
}

/// Whether an API error says the model cannot do adaptive thinking.
pub fn refuses_adaptive(message: &str) -> bool {
    message.to_ascii_lowercase().contains("adaptive thinking is not supported")
}

/// Switch a request from adaptive thinking to a token budget sized from its effort. Returns false
/// when the request has no adaptive thinking to change.
pub fn downgrade_thinking(body: &mut Value) -> bool {
    if body["thinking"]["type"] != "adaptive" {
        return false;
    }
    let budget: u64 = match body["output_config"]["effort"].as_str() {
        Some("low") => 2048,
        Some("medium") => 8192,
        Some("high") => 16384,
        Some("xhigh") => 24576,
        _ => 32000,
    };
    let max = body["max_tokens"].as_u64().unwrap_or(8192);
    // The budget has to leave room for the answer.
    let budget = budget.min(max.saturating_sub(1024));
    if let Some(o) = body.as_object_mut() {
        o.remove("output_config");
        if budget >= 1024 {
            o.insert("thinking".into(), json!({"type": "enabled", "budget_tokens": budget}));
        } else {
            o.remove("thinking");
        }
    }
    true
}

fn block(p: &Part) -> Option<Value> {
    match p {
        Part::Text(t) if !t.trim().is_empty() => Some(json!({"type": "text", "text": t})),
        Part::Image { mime, data } => Some(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}})),
        Part::Document { mime, data, .. } => {
            Some(json!({"type": "document", "source": {"type": "base64", "media_type": mime, "data": data}}))
        }
        _ => None,
    }
}

fn user_blocks(m: &Message) -> Vec<Value> {
    let (results, rest) = split_results(&m.parts);
    let mut out = Vec::new();
    for r in results {
        let blocks: Vec<Value> = r.content.iter().filter_map(block).collect();
        let only_text = r.content.iter().all(|p| matches!(p, Part::Text(_)));
        let content = if only_text || blocks.is_empty() {
            json!(r.joined_text())
        } else {
            json!(blocks)
        };
        let mut v = json!({"type": "tool_result", "tool_use_id": r.call_id, "content": content});
        if r.is_error {
            v["is_error"] = json!(true);
        }
        out.push(v);
    }
    out.extend(rest.into_iter().filter_map(block));
    out
}

fn assistant_blocks(m: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    for p in &m.parts {
        match p {
            Part::Reasoning { opaque: Some(v), .. } => out.push(v.clone()),
            Part::Reasoning { text, signature: Some(s), .. } => {
                out.push(json!({"type": "thinking", "thinking": text, "signature": s}))
            }
            Part::ToolCall(c) => out.push(json!({
                "type": "tool_use",
                "id": c.id,
                "name": c.name,
                "input": if c.arguments.is_object() { c.arguments.clone() } else { json!({}) },
            })),
            other => out.extend(block(other)),
        }
    }
    out
}

enum Blk {
    Text(String),
    Thinking { text: String, sig: String },
    Opaque(Value),
    Tool { id: String, name: String, json: String },
}

#[derive(Default)]
pub struct AnthropicParser {
    blocks: BTreeMap<usize, Blk>,
    usage: Usage,
    saw_usage: bool,
    stop: Option<String>,
    model: Option<String>,
    done: bool,
}

impl Parser for AnthropicParser {
    fn event(&mut self, name: Option<&str>, data: &str, out: &mut Vec<Event>) -> Result<()> {
        let v = json_parse(data)?;
        let kind = v.get("type").and_then(Value::as_str).or(name).unwrap_or("");
        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match kind {
            "message_start" => {
                let m = &v["message"];
                self.model = m.get("model").and_then(Value::as_str).map(str::to_owned);
                self.take_usage(&m["usage"]);
            }
            "content_block_start" => {
                let b = &v["content_block"];
                let blk = match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            out.push(Event::Text(t.to_owned()));
                        }
                        Blk::Text(t.to_owned())
                    }
                    Some("thinking") => Blk::Thinking {
                        text: b.get("thinking").and_then(Value::as_str).unwrap_or("").to_owned(),
                        sig: String::new(),
                    },
                    Some("tool_use") | Some("server_tool_use") => Blk::Tool {
                        id: b.get("id").and_then(Value::as_str).unwrap_or("").to_owned(),
                        name: b.get("name").and_then(Value::as_str).unwrap_or("").to_owned(),
                        json: String::new(),
                    },
                    Some("redacted_thinking") => Blk::Opaque(b.clone()),
                    _ => return Ok(()),
                };
                self.blocks.insert(index, blk);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match (d.get("type").and_then(Value::as_str), self.blocks.get_mut(&index)) {
                    (Some("text_delta"), Some(Blk::Text(t))) => {
                        let s = d.get("text").and_then(Value::as_str).unwrap_or("");
                        t.push_str(s);
                        if !s.is_empty() {
                            out.push(Event::Text(s.to_owned()));
                        }
                    }
                    (Some("thinking_delta"), Some(Blk::Thinking { text, .. })) => {
                        let s = d.get("thinking").and_then(Value::as_str).unwrap_or("");
                        text.push_str(s);
                        if !s.is_empty() {
                            out.push(Event::Reasoning(s.to_owned()));
                        }
                    }
                    (Some("signature_delta"), Some(Blk::Thinking { sig, .. })) => {
                        sig.push_str(d.get("signature").and_then(Value::as_str).unwrap_or(""));
                    }
                    (Some("input_json_delta"), Some(Blk::Tool { json, .. })) => {
                        json.push_str(d.get("partial_json").and_then(Value::as_str).unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"].get("stop_reason").and_then(Value::as_str) {
                    self.stop = Some(s.to_owned());
                }
                self.take_usage(&v["usage"]);
            }
            "message_stop" => self.done = true,
            "error" => return Err(stream_error(&v).unwrap_or_else(|| Error::new("provider error"))),
            _ => {}
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Response> {
        if !self.done && self.stop.is_none() && self.blocks.is_empty() {
            return Err(Error {
                message: "the stream ended without a reply".into(),
                status: None,
                retryable: true,
                cancelled: false,
            });
        }
        let mut parts = Vec::new();
        for blk in self.blocks.into_values() {
            match blk {
                Blk::Text(t) if !t.is_empty() => parts.push(Part::Text(t)),
                Blk::Text(_) => {}
                Blk::Thinking { text, sig } => parts.push(Part::Reasoning {
                    text,
                    signature: (!sig.is_empty()).then_some(sig),
                    opaque: None,
                }),
                Blk::Opaque(v) => parts.push(Part::Reasoning { text: String::new(), signature: None, opaque: Some(v) }),
                Blk::Tool { id, name, json } => parts.push(Part::ToolCall(ToolCall {
                    id,
                    name,
                    arguments: parse_args(&json),
                    signature: None,
                })),
            }
        }
        let stop = match self.stop.as_deref() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("refusal") => StopReason::Refusal,
            Some("end_turn") | Some("stop_sequence") | None => StopReason::EndTurn,
            Some(o) => StopReason::Other(o.to_owned()),
        };
        Ok(Response {
            message: Message { role: Role::Assistant, parts },
            usage: self.saw_usage.then_some(self.usage),
            stop,
            model: self.model,
            plan: None,
        })
    }
}

impl AnthropicParser {
    fn take_usage(&mut self, u: &Value) {
        if !u.is_object() {
            return;
        }
        self.saw_usage = true;
        let fresh = u64_at(u, &["input_tokens"]);
        let read = u64_at(u, &["cache_read_input_tokens"]);
        let write = u64_at(u, &["cache_creation_input_tokens"]);
        let total = fresh + read + write;
        if total > 0 {
            self.usage.input = total;
            self.usage.cached_read = read;
            self.usage.cache_write = write;
        }
        let out = u64_at(u, &["output_tokens"]);
        if out > 0 {
            self.usage.output = out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::new(WireFormat::Anthropic, "https://api.anthropic.com/v1", "claude")
    }

    #[test]
    fn builds_cache_breakpoints_and_tool_results() {
        let call = ToolCall { id: "t1".into(), name: "read".into(), arguments: json!({"p": 1}), signature: None };
        let mut req = Request {
            system: vec![SystemBlock::new("identity"), SystemBlock::cached("static rules"), SystemBlock::new("dynamic")],
            messages: vec![Message::user("go")],
            ..Default::default()
        };
        req.messages.push(Message::with_parts(
            Role::Assistant,
            vec![Part::Reasoning { text: "why".into(), signature: Some("sig".into()), opaque: None }, Part::ToolCall(call.clone())],
        ));
        req.messages.push(Message::with_parts(
            Role::User,
            vec![Part::Text("also".into()), Part::ToolResult(ToolResult::text(&call, "data", true))],
        ));
        let b = body(&cfg(), &req);
        assert_eq!(b["max_tokens"], 8192);
        assert!(b["system"][0].get("cache_control").is_none());
        assert_eq!(b["system"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"][1]["content"][0]["type"], "thinking");
        // tool_result precedes the plain text, and the tip carries the last breakpoint
        let last = &b["messages"][2]["content"];
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[0]["is_error"], true);
        assert_eq!(last[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn thinking_drops_temperature() {
        let mut c = cfg();
        c.temperature = Some(0.3);
        c.effort = Some(Effort::High);
        let b = body(&c, &Request::simple("s", "hi"));
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "high");
        assert!(b.get("temperature").is_none());
        c.effort = Some(Effort::None);
        let b = body(&c, &Request::simple("s", "hi"));
        assert!(b.get("thinking").is_none());
        assert!((b["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn parses_thinking_text_and_tool_stream() {
        let mut p: Box<dyn Parser> = Box::new(AnthropicParser::default());
        let mut out = Vec::new();
        for e in [
            r#"{"type":"message_start","message":{"model":"claude","usage":{"input_tokens":5,"cache_read_input_tokens":10,"cache_creation_input_tokens":2,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tu","name":"ls"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            r#"{"type":"message_stop"}"#,
        ] {
            p.event(None, e, &mut out).unwrap();
        }
        let r = p.finish().unwrap();
        assert_eq!(out, vec![Event::Reasoning("hm".into()), Event::Text("ok".into())]);
        assert!(matches!(&r.message.parts[0], Part::Reasoning { signature: Some(s), .. } if s == "SIG"));
        assert_eq!(r.message.tool_calls().next().unwrap().arguments, json!({"a": 1}));
        assert_eq!(r.stop, StopReason::ToolUse);
        let u = r.usage.unwrap();
        assert_eq!((u.input, u.cached_read, u.cache_write, u.output), (17, 10, 2, 9));
    }

    #[test]
    fn older_models_get_a_thinking_budget() {
        let mut c = cfg();
        c.model = "claude-haiku-4-5-20251001".into();
        c.effort = Some(Effort::Low);
        c.max_output_tokens = Some(4096);
        let b = body(&c, &Request::simple("s", "hi"));
        assert_eq!(b["thinking"], json!({"type": "enabled", "budget_tokens": 2048}));
        assert!(b.get("output_config").is_none());

        // A model not on the list is tried adaptively, and downgraded when the API says no.
        let mut c = cfg();
        c.effort = Some(Effort::High);
        c.max_output_tokens = Some(8000);
        let mut b = body(&c, &Request::simple("s", "hi"));
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert!(refuses_adaptive("adaptive thinking is not supported on this model"));
        assert!(downgrade_thinking(&mut b));
        assert_eq!(b["thinking"], json!({"type": "enabled", "budget_tokens": 6976}));
        assert!(!downgrade_thinking(&mut b), "already downgraded");
    }

    #[test]
    fn overloaded_error_is_retryable() {
        let mut p: Box<dyn Parser> = Box::new(AnthropicParser::default());
        let e = p
            .event(None, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#, &mut Vec::new())
            .unwrap_err();
        assert!(e.retryable);
    }
}
