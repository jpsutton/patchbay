//! The HTTP client: request, retry, idle timeout, cancellation.

use std::time::Duration;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

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
    /// Use Google's internal Cloud Code `v1internal` envelope for Gemini requests.
    pub cloud_code: bool,
    /// Optional Workspace project required by Cloud Code, cached per Antigravity login.
    pub cloud_code_project: Option<String>,
    /// Stable local login identity used only as the in-memory project-cache key.
    pub cloud_code_login_id: Option<String>,
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
            cloud_code: false,
            cloud_code_project: None,
            cloud_code_login_id: None,
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

/// The Google token goes only to `https://*.googleapis.com`: `base_url` syncs over the mesh and a
/// peer or a typo could otherwise send every node's login token anywhere, over http too. The one
/// check every place that attaches that token passes through (pb-20261117-10).
pub fn require_google_https(base_url: &str) -> Result<()> {
    // What is checked is what is sent, so nothing a parser would quietly normalize is let through:
    // surrounding whitespace, a backslash (some parsers read it as a slash), userinfo, and a query
    // or fragment (a token or key could ride in one, and it would reach logs and error text).
    // `Url` takes the host after any `user@`, so `https://x.googleapis.com@evil.example` is evil.example.
    let ok = base_url == base_url.trim()
        && !base_url.contains('\\')
        && reqwest::Url::parse(base_url).is_ok_and(|u| {
            u.scheme() == "https"
                && u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none()
                && u.host_str().is_some_and(|h| h.ends_with(".googleapis.com"))
        });
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            "Antigravity login refused: its base URL must be https on a *.googleapis.com host, so the Google token is never sent anywhere else. Fix the provider's base URL.",
        ))
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
pub(crate) fn request_id() -> String {
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

/// How long the server wants us to wait, in milliseconds, from whatever it told us.
///
/// The `Retry-After` header first (seconds, or an HTTP date). Then the body: providers name a
/// reset time in their own shape, and a 429 body often says more than the header does, e.g.
/// `{"error":{"code":"rate_limit","reset_at":...}}`, `{"retry_after_ms":...}`, or Gemini's
/// `RetryInfo` detail `{"retryDelay":"12s"}`. Last, a "try again in 12.5s" / "reset in 30 seconds"
/// phrase in the error text. Zero hints are dropped: they mean "no idea", not "now".
fn retry_hint(headers: &reqwest::header::HeaderMap, body: &str, message: &str) -> Option<u64> {    let header = headers.get("retry-after").and_then(|v| v.to_str().ok());
    let from_header = header.and_then(|v| {
        if let Ok(s) = v.trim().parse::<u64>() {
            return Some(s);
        }
        // The header may be an HTTP date instead of a number of seconds.
        let when = http_date_epoch(v)?;
        let now = unix_now_secs();
        when.checked_sub(now)
    });
    if let Some(s) = from_header.filter(|s| *s > 0) {
        return Some(s.saturating_mul(1000));
    }
    // Any key whose name says "retry after" or "reset", at any nesting depth: provider shapes differ.
    let json: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let numeric = json.as_ref().and_then(|j| {
        let mut found = None;
        walk(j, &mut |value| {
            if found.is_some() {
                return;
            }
            let Some(obj) = value.as_object() else { return };
            for (k, v) in obj {
                let k = k.to_ascii_lowercase();
                let n = v.as_u64().or_else(|| v.as_f64().map(|f| f as u64));
                if k.contains("retry_after_ms") {
                    found = n;
                } else if k.contains("retry_after") || k.contains("retrydelay") || k.contains("reset_after") {
                    // A number is seconds. Gemini writes a protobuf duration, `"12s"`.
                    found = n
                        .map(|s| s.saturating_mul(1000))
                        .or_else(|| v.as_str().and_then(duration_string_ms));
                } else if k.contains("reset") && (k.contains("at") || k.contains("in")) {
                    // Epoch seconds, epoch ms or a plain duration: only a timestamp is that large.
                    let now = unix_now_secs();
                    found = n.map(|n| {
                        if n > 1_000_000_000_000 {
                            n.saturating_sub(now * 1000)
                        } else if n > 1_000_000_000 {
                            n.saturating_sub(now).saturating_mul(1000)
                        } else {
                            n.saturating_mul(1000)
                        }
                    });
                }
            }
        });
        found
    });
    if let Some(ms) = numeric.filter(|ms| *ms > 0) {
        return Some(ms);
    }
    // Last resort: a number in the message, when the message talks about retrying or a reset.
    let lower = message.to_ascii_lowercase();
    if !(lower.contains("retry") || lower.contains("reset") || lower.contains("try again")) {
        return None;
    }
    let secs = message
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .filter_map(|w| w.parse::<f64>().ok())
        .next();
    secs.filter(|s| *s > 0.0).map(|s| (s * 1000.0) as u64)
}

/// `"12s"`, `"12.5s"`, `"500ms"`. Anything else is not a duration.
fn duration_string_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, millis) = if let Some(n) = s.strip_suffix("ms") {
        (n, true)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, false)
    } else {
        return None;
    };
    let n: f64 = num.trim().parse().ok()?;
    if n <= 0.0 {
        return None;
    }
    Some(if millis { n as u64 } else { (n * 1000.0) as u64 })
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Visit every value of the JSON tree, depth first.
fn walk(v: &serde_json::Value, f: &mut impl FnMut(&serde_json::Value)) {
    f(v);
    match v {
        serde_json::Value::Object(o) => o.values().for_each(|c| walk(c, f)),
        serde_json::Value::Array(a) => a.iter().for_each(|c| walk(c, f)),
        _ => {}
    }
}

/// Seconds since the epoch for an IMF-fixdate ("Sun, 06 Nov 1994 08:49:37 GMT"), the date form
/// `Retry-After` allows. No date crate here, so the arithmetic is spelled out.
fn http_date_epoch(s: &str) -> Option<u64> {
    fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        // `doy` is a day count (`u32`). The year terms are `i64`.
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + i64::from(doy);
        era * 146_097 + doe - 719_468
    }
    let s = s.trim();
    // "Sun, 06 Nov 1994 08:49:37 GMT": the weekday and the zone are not needed. The weekday may be
    // missing, so the day is the first plain number.
    let mut words = s.split_whitespace();
    let first = words.next()?;
    let d: u32 = match first.parse() {
        Ok(d) => d,
        Err(_) => words.next()?.parse().ok()?,
    };
    let mon = match words.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let y: i64 = words.next()?.parse().ok()?;
    let mut hms = words.next()?.split(':').filter_map(|n| n.parse::<i64>().ok());
    let (h, m, sec) = (hms.next()?, hms.next().unwrap_or(0), hms.next().unwrap_or(0));
    let days = days_from_civil(y, mon, d);
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + sec).ok()
}

/// How long to wait before retry `attempt` (1-based), for an error with status `status`.
///
/// A hint from the server wins over the guess: the `Retry-After` header, or a reset time in the body
/// (`retry_after_ms`, `retryDelay`, `reset_at`...). On a 429 the window it names is the real one,
/// while a guess may land inside it and burn a retry.
///
/// Without a hint: exponential from a floor. A rate limit is a per-minute window, so a 1s first try
/// is always wasted: a 429 starts at 30s and doubles to a 60s cap (30s, 60s), which outlasts one
/// window even with two retries. Other errors start at 1s and cap at 20s.
/// Every wait is capped, so a bad header cannot stall the turn for long.
fn backoff(attempt: u32, status: Option<u16>, hint: Option<u64>) -> Duration {
    if let Some(ms) = hint {
        return Duration::from_millis(ms.min(60_000));
    }
    let (floor, cap) = if status == Some(429) { (30_000u64, 60_000u64) } else { (1_000, 20_000) };
    Duration::from_millis((floor << (attempt - 1)).min(cap))
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
            // A redirect would carry the key (and any Google token) to wherever the server says.
            .redirect(reqwest::redirect::Policy::none())
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
        let mut project_fallback_attempted = false;
        loop {
            let mut emitted = false;
            match self.attempt(&url, &body, req.cache_key.as_deref(), cancel, &mut emitted, &mut on_event).await {
                Ok(resp) => return Ok(resp),
                Err(e) if e.cancelled => return Err(e),
                Err(e)
                    if cfg.cloud_code
                        && cfg.cloud_code_project.as_deref().is_none_or(str::is_empty)
                        && !project_fallback_attempted
                        && project_required(&e) =>
                {
                    project_fallback_attempted = true;
                    let Some(project) = self.load_code_assist_project().await? else {
                        return Err(e);
                    };
                    body["project"] = serde_json::Value::String(project);
                    continue;
                }
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
                    let wait = backoff(attempt, e.status, e.retry_after);
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
        if cfg.cloud_code && cfg.key.as_deref().is_some_and(|k| !k.is_empty()) {
            require_google_https(&cfg.base_url)?;
        }
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
                    code: None,
                    retryable: true,
                    cancelled: false,
                    retry_after: None,
                }),
                // `without_url`: reqwest prints the full URL, and the query may hold a secret. The
                // host and path (no query) are put back, so the message still says where it failed.
                Ok(r) => r.map_err(|e| {
                    let retryable = e.is_connect() || e.is_timeout() || e.is_request();
                    let target = url.split('?').next().unwrap_or(url);
                    Error {
                        message: format!("request failed: {} (POST {target})", diag::chain(&e.without_url())),
                        status: None,
                        code: None,
                        retryable,
                        cancelled: false,
                        retry_after: None,
                    }
                })?,
            },
        };
        let status = resp.status();
        if status.is_redirection() {
            return Err(Error::new(format!("the server answered a redirect (HTTP {}); Patchbay does not follow redirects with credentials", status.as_u16())));
        }
        if !status.is_success() {
            let headers = resp.headers().clone();
            let text = resp.text().await.unwrap_or_default();
            let (code, msg) = wire::error_details(&text);
            let mut e = Error::http(status.as_u16(), msg);
            e.code = code;
            // Carried on the error instead of slept here: only `complete` knows whether the call is
            // retried at all, and it can be cancelled while it waits.
            e.retry_after = retry_hint(&headers, &text, &e.message).filter(|_| e.retryable);
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
                        code: None,
                        retryable: true,
                        cancelled: false,
                        retry_after: None,
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
                        message: watch.failure(&format!("stream error: {}", diag::chain(&e.without_url())), &sse.pending()),
                        status: None,
                        code: None,
                        retryable: true,
                        cancelled: false,
                        retry_after: None,
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

    async fn load_code_assist_project(&self) -> Result<Option<String>> {
        let cfg = &self.config;
        if let Some(login) = cfg.cloud_code_login_id.as_deref()
            && let Some(project) = cloud_code_project_cache().lock().unwrap_or_else(|e| e.into_inner()).get(login).cloned()
        {
            return Ok(Some(project));
        }
        require_google_https(&cfg.base_url)?;
        let url = format!("{}/v1internal:loadCodeAssist", cfg.base_url.trim_end_matches('/'));
        let metadata = serde_json::json!({
            "ideType": "ANTIGRAVITY",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI"
        });
        let mut request = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&serde_json::json!({"metadata": metadata}));
        if let Some(token) = cfg.key.as_deref().filter(|s| !s.is_empty()) {
            request = request.bearer_auth(token);
        }
        for (name, value) in &cfg.extra_headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| Error::new(format!("loadCodeAssist request failed: {}", e.without_url())))?;
        if response.status().is_redirection() {
            return Err(Error::new(format!("loadCodeAssist answered a redirect (HTTP {}); Patchbay does not follow redirects with a login token", response.status().as_u16())));
        }
        if !response.status().is_success() {
            return Ok(None);
        }
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| Error::new(format!("parse loadCodeAssist response: {}", e.without_url())))?;
        let project = value
            .pointer("/cloudaicompanionProject/id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("cloudaicompanionProject").and_then(serde_json::Value::as_str))
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let (Some(login), Some(project)) = (cfg.cloud_code_login_id.as_deref(), project.as_ref()) {
            cloud_code_project_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(login.to_owned(), project.clone());
        }
        Ok(project)
    }
}

fn cloud_code_project_cache() -> &'static Mutex<BTreeMap<String, String>> {
    static PROJECTS: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
    PROJECTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn project_required(error: &Error) -> bool {
    if !matches!(error.status, Some(400 | 403)) {
        return false;
    }
    let message = error.message.to_ascii_lowercase();
    message.contains("project") && (message.contains("required") || message.contains("missing") || message.contains("provide"))
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

    /// reqwest prints the full URL in "error sending request", and the query may hold a secret.
    /// Every error string (the Retrying reason and the final message) must be URL-stripped, yet
    /// still say which host failed.
    #[tokio::test]
    async fn a_refused_connection_does_not_leak_query_values_into_error_text() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port() // dropped: nothing listens, the connect is refused
        };
        let mut cfg = Config::new(WireFormat::ChatCompletions, format!("http://127.0.0.1:{port}/openai/deployments/d"), "m");
        cfg.max_retries = 1;
        cfg.extra_query = vec![("code".into(), "SECRET-Q".into())];
        let req = Request { messages: vec![Message::user("hi")], ..Default::default() };
        let reasons = std::sync::Mutex::new(Vec::<String>::new());
        let err = Client::new(cfg)
            .complete(&req, &CancellationToken::new(), |e| {
                if let Event::Retrying { reason, .. } = e {
                    reasons.lock().unwrap().push(reason);
                }
            })
            .await
            .unwrap_err();
        let reasons = reasons.into_inner().unwrap();
        assert_eq!(reasons.len(), 1, "one retry was reported: {reasons:?}");
        for text in reasons.iter().chain(std::iter::once(&err.message)) {
            assert!(!text.contains("SECRET-Q"), "query value leaked: {text}");
            assert!(!text.contains("code="), "query string leaked: {text}");
        }
        assert!(err.message.starts_with("request failed:"), "{}", err.message);
        let target = format!("(POST http://127.0.0.1:{port}/openai/deployments/d/chat/completions)");
        assert!(err.message.ends_with(&target), "still names the host and path: {}", err.message);
        assert!(err.message.contains("Connection refused") || err.message.contains("connect"), "and the cause: {}", err.message);
    }

    /// The same for a body that breaks after the headers: the stream error must not carry the URL.
    #[tokio::test]
    async fn a_broken_stream_does_not_leak_query_values_into_error_text() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;
            // A chunk that promises more bytes than it sends, then the connection drops.
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n20\r\ndata: {")
                .await;
        });
        let mut cfg = Config::new(WireFormat::ChatCompletions, format!("http://127.0.0.1:{port}/v1"), "m");
        cfg.max_retries = 0;
        cfg.extra_query = vec![("code".into(), "SECRET-Q".into())];
        let req = Request { messages: vec![Message::user("hi")], ..Default::default() };
        let err = Client::new(cfg).complete(&req, &CancellationToken::new(), |_| {}).await.unwrap_err();
        assert!(!err.message.contains("SECRET-Q") && !err.message.contains("code="), "{}", err.message);
    }

    /// Azure OpenAI through a query string on the base URL (the row's UNKNOWN, now measured): the path
    /// is appended after the query, so the URL is wrong. Use `extra_query` instead.
    #[test]
    fn the_google_token_goes_only_to_https_googleapis() {
        for ok in ["https://daily-cloudcode-pa.googleapis.com", "https://cloudcode-pa.googleapis.com/", "HTTPS://Daily-CloudCode-PA.GoogleAPIs.com"] {
            assert!(require_google_https(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://daily-cloudcode-pa.googleapis.com",
            "https://evil.example",
            "https://evil.googleapis.com.example.com",
            "https://googleapis.com.evil.example",
            "https://daily-cloudcode-pa.googleapis.com@evil.example",
            "https://googleapis.com",
            "http://127.0.0.1:9",
            "not a url",
            "",
            // A trailing dot is a different spelling of the host: refused, not guessed at.
            "https://daily-cloudcode-pa.googleapis.com.",
            "https://[::1]",
            "https://[2001:db8::1]/",
            "https://evil.example\\@daily-cloudcode-pa.googleapis.com",
            "https://daily-cloudcode-pa.googleapis.com\\.evil.example",
            " https://daily-cloudcode-pa.googleapis.com",
            "https://daily-cloudcode-pa.googleapis.com ",
            "https://daily-cloudcode-pa.googleapis.com\n",
            "https://daily-cloudcode-pa.googleapis.com?key=SECRET",
            "https://daily-cloudcode-pa.googleapis.com/v1?a=b",
            "https://daily-cloudcode-pa.googleapis.com?",
            "https://daily-cloudcode-pa.googleapis.com/#frag",
            "https://daily-cloudcode-pa.googleapis.com#",
            "https://user:pw@daily-cloudcode-pa.googleapis.com",
        ] {
            let e = require_google_https(bad).unwrap_err().message;
            assert!(e.contains("Antigravity login refused"), "{bad}");
            assert!(!e.contains("SECRET") && !e.contains("frag"), "the refusal never echoes the URL: {e}");
        }
    }

    /// The request path refuses before anything is sent: the error is the refusal, not a connect
    /// failure, so the token never left the process.
    #[tokio::test]
    async fn a_cloud_code_request_to_another_host_is_refused_before_the_token_is_sent() {
        let mut cfg = Config::new(WireFormat::Gemini, "http://127.0.0.1:9", "m");
        cfg.cloud_code = true;
        cfg.key = Some("TOKEN".into());
        cfg.max_retries = 0;
        let req = Request { messages: vec![Message::user("hi")], ..Default::default() };
        let err = Client::new(cfg).complete(&req, &CancellationToken::new(), |_| {}).await.unwrap_err();
        assert!(err.message.contains("Antigravity login refused") && !err.message.contains("TOKEN"), "{}", err.message);
    }

    /// A server that answers 302/307 is NOT followed: the key would go with it. One hit on the
    /// origin, none on the place it points at, and the error says why.
    #[tokio::test]
    async fn a_redirect_is_never_followed_with_the_key() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for status in [302u16, 307] {
            let (first, second) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target_port = target.local_addr().unwrap().port();
            let hits = second.clone();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = target.accept().await {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                }
            });
            let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin_port = origin.local_addr().unwrap().port();
            let hits = first.clone();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = origin.accept().await {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let mut buf = vec![0u8; 16384];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(format!("HTTP/1.1 {status} Redirect\r\nlocation: http://127.0.0.1:{target_port}/x\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).await;
                }
            });
            let mut cfg = Config::new(WireFormat::Gemini, format!("http://127.0.0.1:{origin_port}"), "m");
            cfg.key = Some("KEY".into());
            cfg.max_retries = 0;
            let req = Request { messages: vec![Message::user("hi")], ..Default::default() };
            let err = Client::new(cfg).complete(&req, &CancellationToken::new(), |_| {}).await.unwrap_err();
            assert!(err.message.contains("redirect") && !err.message.contains("KEY"), "{status}: {}", err.message);
            assert_eq!((first.load(Ordering::SeqCst), second.load(Ordering::SeqCst)), (1, 0), "{status}: followed or retried");
        }
    }

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

    /// A 429 window is longer than a 1s/2s guess, so a hint wins when the server sends one, and a
    /// 429 without one starts from a floor a per-minute limit can actually clear.
    #[test]
    fn a_hint_wins_and_a_429_waits_longer_than_other_errors() {
        assert_eq!(backoff(1, Some(429), Some(12_000)), Duration::from_secs(12));
        assert_eq!(backoff(3, Some(429), Some(500)), Duration::from_millis(500));
        assert_eq!(backoff(1, Some(429), None), Duration::from_secs(30), "a 429 starts at 30s");
        assert_eq!(backoff(2, Some(429), None), Duration::from_secs(60));
        assert_eq!(backoff(4, Some(429), None), Duration::from_secs(60), "capped at one window");
        assert_eq!(backoff(1, Some(503), None), Duration::from_secs(1), "other errors keep a 1s floor");
        assert_eq!(backoff(3, Some(503), None), Duration::from_secs(4));
        assert_eq!(backoff(1, Some(429), Some(3_600_000)), Duration::from_secs(60), "and a wild hint is capped");
    }

    /// Every shape a provider uses to say when the limit resets: header, body keys at any depth,
    /// and the message text. A body with nothing useful yields no hint, so the guess applies.
    #[test]
    fn the_retry_hint_is_read_from_the_header_then_the_body_then_the_message() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(retry_hint(&h, "", ""), Some(7_000), "the header, in ms");
        assert_eq!(retry_hint(&HeaderMap::new(), "", ""), None, "nothing said, nothing assumed");

        let body = r#"{"error":{"code":"rate_limit_exceeded","retry_after_ms":2500}}"#;
        assert_eq!(retry_hint(&HeaderMap::new(), body, "rate limited"), Some(2_500));

        let gemini = r#"{"error":{"code":429,"details":[{"@type":"RetryInfo","retryDelay":"12s"}]}}"#;
        assert_eq!(retry_hint(&HeaderMap::new(), gemini, "quota"), Some(12_000));

        let reset_at = format!(r#"{{"error":{{"reset_at":{}}}}}"#, unix_now_secs() + 30);
        let got = retry_hint(&HeaderMap::new(), &reset_at, "").unwrap();
        assert!((29_000..=30_000).contains(&got), "an epoch reset becomes a wait: {got}");

        assert_eq!(
            retry_hint(&HeaderMap::new(), "", "Input tokens per minute limit exceeded for model 'x' (HTTP 429)"),
            None,
            "no time in the message, no hint invented from its numbers"
        );
        assert_eq!(
            retry_hint(&HeaderMap::new(), "", "Rate limited, try again in 12.5s"),
            Some(12_500),
            "a phrase with a time is used"
        );
    }

    /// RFC 9110's example IMF-fixdate. A date in the past is not a wait, so this checks the parser
    /// itself: the header path only returns a wait when the instant is still ahead.
    #[test]
    fn an_http_date_is_seconds_since_the_epoch() {
        assert_eq!(
            http_date_epoch("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(http_date_epoch("06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(http_date_epoch("not a date"), None);
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
