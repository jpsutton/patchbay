//! One adapter per wire format: build the request body, decode the event stream.

use serde_json::Value;

use crate::client::Config;
use crate::types::*;

pub mod anthropic;
pub mod chat;
pub mod gemini;
pub mod responses;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Turns the server's events into [`Event`]s and, at the end, the assembled [`Response`].
pub(crate) trait Parser: Send {
    fn event(&mut self, name: Option<&str>, data: &str, out: &mut Vec<Event>) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<Response>;
}

pub fn url(cfg: &Config) -> String {
    match cfg.format {
        WireFormat::ChatCompletions => format!("{}/chat/completions", cfg.base_url),
        WireFormat::Responses => format!("{}/responses", cfg.base_url),
        WireFormat::Anthropic => format!("{}/messages", cfg.base_url),
        WireFormat::Gemini => gemini::url(cfg),
    }
}

pub fn body(cfg: &Config, req: &Request) -> Value {
    match cfg.format {
        WireFormat::ChatCompletions => chat::body(cfg, req),
        WireFormat::Responses => responses::body(cfg, req),
        WireFormat::Anthropic => anthropic::body(cfg, req),
        WireFormat::Gemini => gemini::body(cfg, req),
    }
}

/// The output cap a request carries, for the formats that always send one. The rest leave it to
/// the server unless the model's settings set it.
pub fn max_output_tokens(cfg: &Config) -> Option<u32> {
    match cfg.format {
        WireFormat::Anthropic => Some(anthropic::max_tokens(cfg)),
        _ => cfg.max_output_tokens,
    }
}

pub(crate) fn parser(cfg: &Config) -> Box<dyn Parser> {
    match cfg.format {
        WireFormat::ChatCompletions => Box::new(chat::ChatParser::default()),
        WireFormat::Responses => Box::new(responses::ResponsesParser::default()),
        WireFormat::Anthropic => Box::new(anthropic::AnthropicParser::default()),
        WireFormat::Gemini => Box::new(gemini::GeminiParser::default()),
    }
}

pub fn default_headers(cfg: &Config) -> Vec<(String, String)> {
    match cfg.format {
        WireFormat::Anthropic => vec![("anthropic-version".into(), ANTHROPIC_VERSION.into())],
        _ => Vec::new(),
    }
}

/// The human-readable part of an error response body.
pub fn error_message(body: &str) -> String {
    let body = body.trim();
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let e = v.get("error").unwrap_or(&v);
        if let Some(m) = e.get("message").and_then(Value::as_str) {
            return m.to_owned();
        }
        if let Some(m) = e.as_str() {
            return m.to_owned();
        }
        if let Some(m) = v.get("message").and_then(Value::as_str) {
            return m.to_owned();
        }
    }
    if body.is_empty() {
        return "empty error response".into();
    }
    body.chars().take(400).collect()
}

/// An error carried inside a stream event (`{"error": {...}}`).
pub(crate) fn stream_error(v: &Value) -> Option<Error> {
    let e = v.get("error")?;
    if e.is_null() {
        return None;
    }
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| e.as_str())
        .unwrap_or("provider error")
        .to_owned();
    let kind = e.get("type").and_then(Value::as_str).unwrap_or("");
    Some(Error {
        message: msg,
        status: None,
        retryable: matches!(kind, "overloaded_error" | "api_error" | "server_error"),
        cancelled: false,
    })
}

pub(crate) fn data_uri(mime: &str, b64: &str) -> String {
    format!("data:{mime};base64,{b64}")
}

pub(crate) fn u64_at(v: &Value, path: &[&str]) -> u64 {
    let mut cur = v;
    for k in path {
        match cur.get(*k) {
            Some(n) => cur = n,
            None => return 0,
        }
    }
    cur.as_u64().or_else(|| cur.as_f64().map(|f| f as u64)).unwrap_or(0)
}

/// Joins system blocks the way formats without block lists need them.
pub(crate) fn system_text(req: &Request) -> String {
    req.system
        .iter()
        .map(|b| b.text.as_str())
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Parses tool-call arguments the model streamed as text; broken JSON stays a string so the
/// tool can report the problem.
pub(crate) fn parse_args(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

/// Splits a user message into its tool results and everything else; each format places them
/// differently.
pub(crate) fn split_results(parts: &[Part]) -> (Vec<&ToolResult>, Vec<&Part>) {
    let mut results = Vec::new();
    let mut rest = Vec::new();
    for p in parts {
        match p {
            Part::ToolResult(r) => results.push(r),
            other => rest.push(other),
        }
    }
    (results, rest)
}

pub(crate) fn json_parse(data: &str) -> Result<Value> {
    serde_json::from_str(data).map_err(|e| Error::new(format!("bad event from server: {e}: {}", data.chars().take(200).collect::<String>())))
}
