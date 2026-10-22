//! The HTTP client: request, retry, idle timeout, cancellation.

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::sse::Sse;
use crate::types::*;
use crate::wire;

/// Everything about the endpoint and the sampling parameters; the conversation is the [`Request`].
#[derive(Debug, Clone)]
pub struct Config {
    pub format: WireFormat,
    pub base_url: String,
    pub model: String,
    pub key: Option<String>,
    pub auth: AuthScheme,
    pub extra_headers: Vec<(String, String)>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub effort: Option<Effort>,
    pub summary: Option<Summary>,
    /// Retries after the first attempt, for failures before any output arrived.
    pub max_retries: u32,
    /// Give up when the stream is silent this long.
    pub idle_timeout: Duration,
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
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            effort: None,
            summary: None,
            max_retries: 2,
            idle_timeout: Duration::from_secs(180),
        }
    }
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
            match self.attempt(&url, &body, cancel, &mut emitted, &mut on_event).await {
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
        if let Some(k) = cfg.key.as_deref().filter(|k| !k.is_empty()) {
            rb = match cfg.auth {
                AuthScheme::Bearer => rb.bearer_auth(k),
                AuthScheme::XApiKey => rb.header("x-api-key", k),
                AuthScheme::GoogApiKey => rb.header("x-goog-api-key", k),
            };
        }
        for (k, v) in wire::default_headers(cfg).iter().chain(cfg.extra_headers.iter()) {
            rb = rb.header(k, v);
        }
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::cancelled()),
            r = rb.send() => r.map_err(|e| Error {
                message: format!("request failed: {e}"),
                status: None,
                retryable: e.is_connect() || e.is_timeout() || e.is_request(),
                cancelled: false,
            })?,
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

        let mut parser = wire::parser(cfg);
        let mut sse = Sse::default();
        let mut stream = resp.bytes_stream();
        let mut events = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(Error::cancelled()),
                c = tokio::time::timeout(cfg.idle_timeout, stream.next()) => match c {
                    Err(_) => return Err(Error {
                        message: format!("no data for {}s", cfg.idle_timeout.as_secs()),
                        status: None,
                        retryable: true,
                        cancelled: false,
                    }),
                    Ok(c) => c,
                },
            };
            let (sse_events, ended) = match chunk {
                Some(Ok(bytes)) => (sse.push(&bytes), false),
                Some(Err(e)) => {
                    return Err(Error {
                        message: format!("stream error: {e}"),
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
                parser.event(ev.name.as_deref(), &ev.data, &mut events)?;
                for e in events.drain(..) {
                    *emitted = true;
                    on_event(e);
                }
            }
            if ended {
                break;
            }
        }
        let response = parser.finish()?;
        for part in &response.message.parts {
            if let Part::ToolCall(c) = part {
                on_event(Event::ToolCall(c.clone()));
            }
        }
        Ok(response)
    }
}
