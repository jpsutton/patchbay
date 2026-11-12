//! The HTTP client: request, retry, idle timeout, cancellation.

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::diag;
use crate::sse::Sse;
use crate::types::*;
use crate::wire;

/// Everything about the endpoint and the sampling parameters; the conversation is the [`Request`].
#[derive(Clone)]
pub struct Config {
    pub format: WireFormat,
    pub base_url: String,
    pub model: String,
    pub key: Option<String>,
    pub auth: AuthScheme,
    pub extra_headers: Vec<(String, String)>,
    /// Query parameters added to every request URL (Azure OpenAI's `api-version`, a gateway's key).
    pub extra_query: Vec<(String, String)>,
    /// Headers that carry the request's `cache_key` (the session id), for endpoints that group
    /// a conversation's requests by header (Grok's `x-grok-conv-id`, `x-grok-session-id`).
    pub session_headers: Vec<String>,
    /// A header that carries a fresh id on every attempt (Grok's `x-grok-req-id`).
    pub request_id_header: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub effort: Option<Effort>,
    pub summary: Option<Summary>,
    /// Chat Completions: send earlier turns' reasoning back as `reasoning_content`, so models
    /// with interleaved thinking (Qwen3.x, DeepSeek, GLM) keep it across tool calls. Other
    /// formats ignore it; they always return reasoning the way their API requires.
    pub send_reasoning: bool,
    /// Retries after the first attempt, for failures before any output arrived.
    pub max_retries: u32,
    /// Give up when the stream is silent this long.
    pub idle_timeout: Duration,
    /// How long Anthropic keeps this request's cache entries. Other formats ignore it.
    pub cache_ttl: CacheTtl,
}

/// Never prints a secret: the key, header values and query values show only that they are set.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn names(l: &[(String, String)]) -> Vec<&str> {
            l.iter().map(|(n, _)| n.as_str()).collect()
        }
        f.debug_struct("Config")
            .field("format", &self.format)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field("auth", &self.auth)
            .field("extra_headers", &names(&self.extra_headers))
            .field("extra_query", &names(&self.extra_query))
            .field("session_headers", &self.session_headers)
            .field("request_id_header", &self.request_id_header)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("max_retries", &self.max_retries)
            .field("idle_timeout", &self.idle_timeout)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn new(format: WireFormat, base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Config {
            format,
            base_url: base_url.into().trim().trim_end_matches('/').to_owned(),
            model: model.into(),
            key: None,
            auth: match format {
                WireFormat::Anthropic => AuthScheme::XApiKey,
                WireFormat::Gemini => AuthScheme::GoogApiKey,
                _ => AuthScheme::Bearer,
            },
            extra_headers: Vec::new(),
            extra_query: Vec::new(),
            session_headers: Vec::new(),
            request_id_header: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            effort: None,
            summary: None,
            send_reasoning: true,
            max_retries: 2,
            idle_timeout: Duration::from_secs(180),
            cache_ttl: CacheTtl::Default,
        }
    }

    /// Seconds the prompt cache lives after its last use, for the wire format that lets the
    /// client know it (Anthropic). `None`: the endpoint decides, so there is nothing to count down.
    pub fn cache_ttl_secs(&self) -> Option<u64> {
        (self.format == WireFormat::Anthropic).then(|| self.cache_ttl.secs())
    }
}

/// The headers that differ per request: the session id under each of `cfg.session_headers`
/// (when the request has one), and a fresh id under `cfg.request_id_header`.
fn request_headers(cfg: &Config, session: Option<&str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(s) = session.filter(|s| !s.is_empty()) {
        for name in &cfg.session_headers {
            out.push((name.clone(), s.to_owned()));
        }
    }
    if let Some(name) = &cfg.request_id_header {
        out.push((name.clone(), request_id()));
    }
    out
}

/// A random id in UUID layout. It needs no uniqueness guarantee beyond "different each time", so
/// it draws on the standard library's per-process random hasher keys instead of a new dependency.
fn request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let word = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    let (a, b) = (word(), word());
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a & 0xfff,
        ((b >> 48) as u16 & 0x3fff) | 0x8000,
        b & 0xffff_ffff_ffff,
    )
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    pub config: Config,
}

impl Client {
    pub fn new(config: Config) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .user_agent(concat!("patchbay/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Client { http, config }
    }

    /// One reply, streamed through `on_event`. Failures before the first output are retried.
    pub async fn complete(
        &self,
        req: &Request,
        cancel: &CancellationToken,
        mut on_event: impl FnMut(Event) + Send,
    ) -> Result<Response> {
        let cfg = &self.config;
        let mut body = wire::body(cfg, req);
        let url = wire::url(cfg);
        let mut attempt = 0u32;
        loop {
            let mut emitted = false;
            match self.attempt(&url, &body, req.cache_key.as_deref(), cancel, &mut emitted, &mut on_event).await {
                Ok(resp) => return Ok(resp),
                Err(e) if e.cancelled => return Err(e),
                // Some Anthropic models refuse adaptive thinking; ask again with a token budget.
                Err(e)
                    if cfg.format == WireFormat::Anthropic
                        && wire::anthropic::refuses_adaptive(&e.message)
                        && wire::anthropic::downgrade_thinking(&mut body) =>
                {
                    continue;
                }
                Err(e) if e.retryable && !emitted && attempt < cfg.max_retries => {
                    attempt += 1;
                    on_event(Event::Retrying {
                        attempt,
                        max: cfg.max_retries,
                        reason: e.to_string(),
                    });
                    let wait = Duration::from_millis((1000u64 << (attempt - 1)).min(20_000));
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::cancelled()),
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The visible text of one reply; reasoning is dropped.
    pub async fn complete_text(&self, req: &Request, cancel: &CancellationToken) -> Result<String> {
        Ok(self.complete(req, cancel, |_| {}).await?.message.text())
    }

    async fn attempt(
        &self,
        url: &str,
        body: &serde_json::Value,
        session: Option<&str>,
        cancel: &CancellationToken,
        emitted: &mut bool,
        on_event: &mut (impl FnMut(Event) + Send),
    ) -> Result<Response> {
        let cfg = &self.config;
        let mut rb = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(body);
        if !cfg.extra_query.is_empty() {
            rb = rb.query(&cfg.extra_query);
        }
        if let Some(k) = cfg.key.as_deref().filter(|k| !k.is_empty()) {
            rb = match cfg.auth {
                AuthScheme::Bearer => rb.bearer_auth(k),
                AuthScheme::XApiKey => rb.header("x-api-key", k),
                AuthScheme::GoogApiKey => rb.header("x-goog-api-key", k),
            };
        }
        for (k, v) in wire::default_headers(cfg)
            .iter()
            .chain(cfg.extra_headers.iter())
            .chain(request_headers(cfg, session).iter())
        {
            rb = rb.header(k, v);
        }
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::cancelled()),
            // The connect timeout covers the handshake only; a server that takes the request and
            // never answers would otherwise hold the call open for good.
            r = tokio::time::timeout(cfg.idle_timeout, rb.send()) => match r {
                Err(_) => return Err(Error {
                    message: format!("no response for {}s", cfg.idle_timeout.as_secs()),
                    status: None,
                    retryable: true,
                    cancelled: false,
                }),
                Ok(r) => r.map_err(|e| Error {
                    message: format!("request failed: {}", diag::chain(&e)),
                    status: None,
                    retryable: e.is_connect() || e.is_timeout() || e.is_request(),
                    cancelled: false,
                })?,
            },
        };
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let text = resp.text().await.unwrap_or_default();
            let mut e = Error::http(status.as_u16(), wire::error_message(&text));
            if let Some(s) = retry_after
                && s > 0
                && s <= 20
                && e.retryable
            {
                tokio::time::sleep(Duration::from_secs(s)).await;
            }
            e.message = e.message.trim().to_owned();
            return Err(e);
        }

        let plan = PlanUsage::from_headers(|name| resp.headers().get(name).and_then(|v| v.to_str().ok()));
        let mut parser = wire::parser(cfg);
        let mut sse = Sse::default();
        let mut watch = diag::Stream::new(url, &resp);
        let mut stream = resp.bytes_stream();
        let mut events = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::cancelled()),
                c = tokio::time::timeout(cfg.idle_timeout, stream.next()) => match c {
                    Err(_) => return Err(Error {
                        message: watch.failure(&format!("no data for {}s", cfg.idle_timeout.as_secs()), &sse.pending()),
                        status: None,
                        retryable: true,
                        cancelled: false,
                    }),
                    Ok(c) => c,
                },
            };
            let (sse_events, ended) = match chunk {
                Some(Ok(bytes)) => {
                    watch.chunk(&bytes);
                    (sse.push(&bytes), false)
                }
                Some(Err(e)) => {
                    return Err(Error {
                        message: watch.failure(&format!("stream error: {}", diag::chain(&e)), &sse.pending()),
                        status: None,
                        retryable: true,
                        cancelled: false,
                    });
                }
                None => (sse.flush().into_iter().collect::<Vec<_>>(), true),
            };
            for ev in sse_events {
                if ev.data.trim() == "[DONE]" {
                    continue;
                }
                if let Err(e) = parser.event(ev.name.as_deref(), &ev.data, &mut events) {
                    watch.keep(&e.message);
                    return Err(e);
                }
                for e in events.drain(..) {
                    *emitted = true;
                    on_event(e);
                }
            }
            if ended {
                break;
            }
        }
        let mut response = match parser.finish() {
            Ok(r) => r,
            Err(e) => {
                watch.keep(&e.message);
                return Err(e);
            }
        };
        response.plan = plan;
        for part in &response.message.parts {
            if let Part::ToolCall(c) = part {
                on_event(Event::ToolCall(c.clone()));
            }
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configured secret never shows in a `{:?}` of the config (a log line, a panic message).
    #[test]
    fn debug_output_hides_the_key_header_values_and_query_values() {
        let mut cfg = Config::new(WireFormat::ChatCompletions, "http://x/v1", "m");
        cfg.key = Some("SECRET-KEY-123".into());
        cfg.extra_headers = vec![("api-key".into(), "SECRET-HEADER-456".into())];
        cfg.extra_query = vec![("code".into(), "SECRET-QUERY-789".into()), ("api-version".into(), "2024-10-21".into())];
        let shown = format!("{cfg:?}");
        for secret in ["SECRET-KEY-123", "SECRET-HEADER-456", "SECRET-QUERY-789", "2024-10-21"] {
            assert!(!shown.contains(secret), "{secret} leaked into: {shown}");
        }
        for name in ["api-key", "code", "api-version"] {
            assert!(shown.contains(name), "{name} (a name, not a secret) should show: {shown}");
        }
    }

    /// The extra headers and query parameters reach the server: the request line carries the query
    /// (and keeps one already on the base URL's path), the headers carry the values.
    #[tokio::test]
    async fn extra_headers_and_query_parameters_reach_the_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap();
            let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await;
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let mut cfg = Config::new(WireFormat::ChatCompletions, format!("http://127.0.0.1:{port}/openai/deployments/d"), "m");
        cfg.max_retries = 0;
        cfg.key = Some("the-key".into());
        cfg.extra_headers = vec![("api-key".into(), "azure-value".into())];
        cfg.extra_query = vec![("api-version".into(), "2024-10-21".into()), ("code".into(), "a b&c".into())];
        let req = Request { messages: vec![Message::user("hi")], ..Default::default() };
        let _ = Client::new(cfg).complete(&req, &CancellationToken::new(), |_| {}).await;
        let seen = server.await.unwrap();
        let first = seen.lines().next().unwrap();
        assert!(first.starts_with("POST /openai/deployments/d/chat/completions?"), "{first}");
        assert!(first.contains("api-version=2024-10-21"), "{first}");
        assert!(first.contains("code=a+b%26c") || first.contains("code=a%20b%26c"), "the value is URL-encoded: {first}");
        let lower = seen.to_ascii_lowercase();
        assert!(lower.contains("api-key: azure-value"), "{seen}");
        assert!(lower.contains("authorization: bearer the-key"), "{seen}");
    }

    /// Azure OpenAI through a query string on the base URL (the row's UNKNOWN, now measured): the path
    /// is appended after the query, so the URL is wrong. Use `extra_query` instead.
    #[test]
    fn a_query_string_on_the_base_url_ends_up_before_the_path() {
        let cfg = Config::new(WireFormat::ChatCompletions, "https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21", "m");
        assert_eq!(wire::url(&cfg), "https://x.openai.azure.com/openai/deployments/d?api-version=2024-10-21/chat/completions");
    }

    #[test]
    fn the_session_id_goes_under_each_named_header() {
        let mut cfg = Config::new(WireFormat::Responses, "http://x/v1", "m");
        assert!(request_headers(&cfg, Some("s1")).is_empty(), "nothing configured, nothing sent");
        cfg.session_headers = vec!["x-conv-id".into(), "x-session-id".into()];
        let h = request_headers(&cfg, Some("s1"));
        assert_eq!(h, vec![("x-conv-id".to_owned(), "s1".to_owned()), ("x-session-id".to_owned(), "s1".to_owned())]);
        assert!(request_headers(&cfg, None).is_empty(), "a request with no session sends none");
        assert!(request_headers(&cfg, Some("")).is_empty());
    }

    #[test]
    fn every_attempt_gets_its_own_request_id() {
        let mut cfg = Config::new(WireFormat::Responses, "http://x/v1", "m");
        cfg.request_id_header = Some("x-req-id".into());
        let a = request_headers(&cfg, None);
        let b = request_headers(&cfg, None);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0, "x-req-id");
        assert_ne!(a[0].1, b[0].1);
        let id = &a[0].1;
        assert_eq!(id.len(), 36, "{id}");
        assert_eq!(id.matches('-').count(), 4, "{id}");
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()), "{id}");
        assert_eq!(id.as_bytes()[14], b'4', "version nibble: {id}");
    }
}
