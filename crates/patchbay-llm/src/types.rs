//! The neutral conversation model shared by every wire format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The API a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFormat {
    /// OpenAI Chat Completions, and every server that imitates it (Ollama, LM Studio, vLLM,
    /// OpenRouter, Groq, xAI, DeepSeek, Mistral, Gemini's compatibility endpoint…).
    ChatCompletions,
    /// OpenAI Responses.
    Responses,
    /// Anthropic Messages.
    Anthropic,
    /// Google Gemini `generateContent`.
    Gemini,
}

/// How the API key travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthScheme {
    /// `Authorization: Bearer <key>`.
    #[default]
    Bearer,
    /// `x-api-key: <key>` (Anthropic).
    XApiKey,
    /// `x-goog-api-key: <key>` (Gemini).
    GoogApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn parse(s: &str) -> Option<Effort> {
        serde_json::from_value(Value::String(s.trim().to_lowercase())).ok()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::None => "none",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// Responses API reasoning summary detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Summary {
    None,
    #[default]
    Auto,
    Concise,
    Detailed,
}

impl Summary {
    pub fn parse(s: &str) -> Option<Summary> {
        serde_json::from_value(Value::String(s.trim().to_lowercase())).ok()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Summary::None => "none",
            Summary::Auto => "auto",
            Summary::Concise => "concise",
            Summary::Detailed => "detailed",
        }
    }
}

/// Where the prompt cache may cut. Anthropic gets a `cache_control` breakpoint after a block
/// marked `Breakpoint`; the other formats ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cache {
    #[default]
    None,
    Breakpoint,
}

/// How long Anthropic keeps a prompt cache entry after its last read. The default is the API's
/// own (5 minutes); `OneHour` asks for the longer entry, whose writes cost more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    #[default]
    Default,
    OneHour,
}

impl CacheTtl {
    /// Seconds an Anthropic cache entry lives after its last use.
    pub fn secs(self) -> u64 {
        match self {
            CacheTtl::Default => 300,
            CacheTtl::OneHour => 3600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    #[serde(default)]
    pub cache: Cache,
}

impl SystemBlock {
    pub fn new(text: impl Into<String>) -> Self {
        SystemBlock { text: text.into(), cache: Cache::None }
    }

    pub fn cached(text: impl Into<String>) -> Self {
        SystemBlock { text: text.into(), cache: Cache::Breakpoint }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message { role: Role::User, parts: vec![Part::Text(text.into())] }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Message { role: Role::Assistant, parts: vec![Part::Text(text.into())] }
    }

    pub fn with_parts(role: Role, parts: Vec<Part>) -> Self {
        Message { role, parts }
    }

    /// The visible text of the message.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            if let Part::Text(t) = p {
                out.push_str(t);
            }
        }
        out
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|p| match p {
            Part::ToolCall(c) => Some(c),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Part {
    Text(String),
    /// Base64 image data.
    Image { mime: String, data: String },
    /// A base64 document (PDF). Only Anthropic, Gemini and OpenAI read these natively.
    Document { mime: String, name: String, data: String },
    /// Model reasoning. `signature` (Anthropic, Gemini) and `opaque` (a Responses reasoning
    /// item, or an Anthropic redacted block) must go back to the provider unchanged.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<Value>,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    /// Gemini's `thoughtSignature` for this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    /// `Text` and `Image` parts only.
    pub content: Vec<Part>,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(call: &ToolCall, text: impl Into<String>, is_error: bool) -> Self {
        ToolResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            content: vec![Part::Text(text.into())],
            is_error,
        }
    }

    pub fn joined_text(&self) -> String {
        let mut out = String::new();
        for p in &self.content {
            if let Part::Text(t) = p {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// A JSON Schema object.
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum ToolChoice {
    #[default]
    Auto,
    Required,
    None,
    Tool(String),
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    /// Offer the provider's own web search (Anthropic `web_search`, OpenAI Responses
    /// `web_search`, Gemini `google_search`). The provider runs it inside the same reply, so no
    /// tool call comes back. Wire formats without one ignore this.
    pub web_search: bool,
    /// Groups requests that share a prefix (OpenAI `prompt_cache_key`).
    pub cache_key: Option<String>,
}

impl Request {
    /// One system prompt and one user message.
    pub fn simple(system: &str, user: impl Into<String>) -> Request {
        Request {
            system: vec![SystemBlock::new(system)],
            messages: vec![Message::user(user)],
            ..Default::default()
        }
    }
}

/// Token counts of one call. `input` includes the cached buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub cached_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    /// Cost the endpoint itself reported, in USD.
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other(String),
}

impl StopReason {
    pub fn label(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            StopReason::Other(s) => s,
        }
    }
}

/// The final result of a call.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub message: Message,
    pub usage: Option<Usage>,
    pub stop: StopReason,
    pub model: Option<String>,
    /// Claude subscription usage, from the response headers of a Claude login.
    pub plan: Option<PlanUsage>,
}

/// One Claude subscription window: the fraction used (0 to 1) and when it resets (unix seconds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanWindow {
    pub utilization: f64,
    pub resets_at: i64,
}

/// Claude subscription limits, from Anthropic's `anthropic-ratelimit-unified-*` response headers
/// (sent to Claude logins, not to API keys).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PlanUsage {
    pub five_hour: Option<PlanWindow>,
    pub seven_day: Option<PlanWindow>,
    /// The plan's limits are used up and requests bill extra usage.
    pub using_overage: bool,
    /// When the binding limit resets (unix seconds).
    pub resets_at: Option<i64>,
}

impl PlanUsage {
    /// Read the headers through `get`; `None` when there are none.
    pub fn from_headers<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<PlanUsage> {
        const P: &str = "anthropic-ratelimit-unified-";
        let h = |name: &str| get(&format!("{P}{name}")).map(str::trim);
        let window = |w: &str| {
            Some(PlanWindow {
                utilization: h(&format!("{w}-utilization"))?.parse().ok()?,
                resets_at: h(&format!("{w}-reset"))?.parse().ok()?,
            })
        };
        let u = PlanUsage {
            five_hour: window("5h"),
            seven_day: window("7d"),
            // Past the plan ("rejected"), a request that still goes through runs on extra usage.
            using_overage: h("status").is_some_and(|s| s == "rejected") && h("overage-status").is_some_and(|s| s.starts_with("allowed")),
            resets_at: h("reset").and_then(|r| r.parse().ok()),
        };
        (u.five_hour.is_some() || u.seven_day.is_some() || u.resets_at.is_some()).then_some(u)
    }
}

/// Progress while a reply streams.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Text(String),
    Reasoning(String),
    /// A tool call whose arguments are complete. Sent after the stream ends, in order.
    ToolCall(ToolCall),
    /// The provider ran its own web search for this query while the reply streamed.
    Search(String),
    Retrying { attempt: u32, max: u32, reason: String },
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}{}", status.map(|s| format!(" (HTTP {s})")).unwrap_or_default())]
pub struct Error {
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<String>,
    pub retryable: bool,
    pub cancelled: bool,
    /// The server's `Retry-After`, in seconds: it knows when the limit resets, a backoff only guesses.
    pub retry_after: Option<u64>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error { message: message.into(), status: None, code: None, retryable: false, cancelled: false, retry_after: None }
    }

    pub fn cancelled() -> Self {
        Error { message: "cancelled".into(), status: None, code: None, retryable: false, cancelled: true, retry_after: None }
    }

    pub fn http(status: u16, message: impl Into<String>) -> Self {
        let mut message: String = message.into();
        // Grok's proxy answers a spent plan, or an account without one, with a bare 403.
        if status == 403 && message.contains("active Grok subscription") {
            message.push_str(" (the plan's usage is used up, or this account has no Grok subscription)");
        }
        Error {
            message,
            status: Some(status),
            code: None,
            retryable: matches!(status, 408 | 409 | 425 | 429) || status >= 500,
            cancelled: false,
            retry_after: None,
        }
    }

    /// Whether an API error indicates provider context overflow / prompt-too-long.
    /// Strictly gates on HTTP 400 or 413 and specific provider phrases / error code.
    pub fn is_context_overflow(&self) -> bool {
        let Some(status) = self.status else { return false };
        if status != 400 && status != 413 {
            return false;
        }
        if self.code.as_deref() == Some("context_length_exceeded") {
            return true;
        }
        let lower = self.message.to_ascii_lowercase();
        const PHRASES: &[&str] = &[
            "context_length_exceeded",
            "prompt is too long",
            "maximum context length",
            "context length exceeded",
            "context window exceeded",
            "input is too long",
            "exceeds context length",
        ];
        PHRASES.iter().any(|p| lower.contains(p))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grok_403_says_what_it_means() {
        let e = Error::http(403, "You have either run out of available resources or do not have an active Grok subscription.");
        assert!(e.message.contains("usage is used up"), "{}", e.message);
        assert!(!e.retryable);
        assert_eq!(Error::http(403, "forbidden").message, "forbidden");
    }

    #[test]
    fn context_overflow_error_classification() {
        // Positives: HTTP 400 or 413 + code context_length_exceeded or specific phrases.
        let mut e = Error::http(400, "invalid request");
        e.code = Some("context_length_exceeded".into());
        assert!(e.is_context_overflow());

        for phrase in [
            "context_length_exceeded",
            "This model's maximum context length is 128000 tokens",
            "prompt is too long: 205000 tokens > 200000 maximum",
            "Context length exceeded",
            "context window exceeded",
            "Input is too long for this model",
            "message exceeds context length",
        ] {
            assert!(Error::http(400, phrase).is_context_overflow(), "{phrase}");
            assert!(Error::http(413, phrase).is_context_overflow(), "{phrase} (413)");
        }

        // Negatives:
        // 1. HTTP 429 quota / rate limit (must never trigger compaction)
        assert!(!Error::http(429, "quota exceeds the limit").is_context_overflow());
        assert!(!Error::http(429, "prompt is too long").is_context_overflow());
        // 2. HTTP 400 unrelated invalid_request
        assert!(!Error::http(400, "invalid_request_error: unrecognized field 'foo'").is_context_overflow());
        assert!(!Error::http(400, "parameter 'messages' is required").is_context_overflow());
        // 3. Gemini "Request payload size exceeds the limit" is a byte-payload size limit (e.g. giant image/upload),
        // not a context token overflow, and does not match the prompt/context length phrases.
        assert!(!Error::http(400, "Request payload size exceeds the limit: 10485760 bytes").is_context_overflow());
        // 4. Generic dropped phrases: "exceeds the limit" and "too many tokens"
        assert!(!Error::http(400, "exceeds the limit").is_context_overflow());
        assert!(!Error::http(400, "too many tokens").is_context_overflow());
        // 5. Non-400/413 status codes even with matching phrases
        assert!(!Error::http(401, "prompt is too long").is_context_overflow());
        assert!(!Error::http(403, "maximum context length").is_context_overflow());
        assert!(!Error::http(404, "context_length_exceeded").is_context_overflow());
        assert!(!Error::http(500, "context window exceeded").is_context_overflow());
        assert!(!Error::new("prompt is too long").is_context_overflow()); // status: None
    }

    #[test]
    fn plan_usage_comes_from_the_unified_headers() {
        let headers = [
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.07"),
            ("anthropic-ratelimit-unified-5h-reset", "1774933200"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.53"),
            ("anthropic-ratelimit-unified-7d-reset", "1775400000"),
        ];
        let get = |n: &str| headers.iter().find(|(k, _)| *k == n).map(|(_, v)| *v);
        let u = PlanUsage::from_headers(get).unwrap();
        assert_eq!(u.five_hour, Some(PlanWindow { utilization: 0.07, resets_at: 1774933200 }));
        assert_eq!(u.seven_day.unwrap().utilization, 0.53);
        assert!(!u.using_overage);

        let over = [("anthropic-ratelimit-unified-status", "rejected"), ("anthropic-ratelimit-unified-overage-status", "allowed"), ("anthropic-ratelimit-unified-reset", "1774933200")];
        let u = PlanUsage::from_headers(|n| over.iter().find(|(k, _)| *k == n).map(|(_, v)| *v)).unwrap();
        assert!(u.using_overage);
        assert_eq!(u.resets_at, Some(1774933200));

        assert_eq!(PlanUsage::from_headers(|_| None), None);
    }
}
