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
}

/// Progress while a reply streams.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Text(String),
    Reasoning(String),
    /// A tool call whose arguments are complete. Sent after the stream ends, in order.
    ToolCall(ToolCall),
    Retrying { attempt: u32, max: u32, reason: String },
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}{}", status.map(|s| format!(" (HTTP {s})")).unwrap_or_default())]
pub struct Error {
    pub message: String,
    pub status: Option<u16>,
    pub retryable: bool,
    pub cancelled: bool,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error { message: message.into(), status: None, retryable: false, cancelled: false }
    }

    pub fn cancelled() -> Self {
        Error { message: "cancelled".into(), status: None, retryable: false, cancelled: true }
    }

    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            status: Some(status),
            retryable: matches!(status, 408 | 409 | 425 | 429) || status >= 500,
            cancelled: false,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
