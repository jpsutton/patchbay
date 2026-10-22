//! WebFetch: fetch a page, turn it into Markdown, answer a question about it.

use std::time::Duration;

use async_trait::async_trait;
use patchbay_llm::{Request, ToolSpec};
use reqwest::redirect::Policy;
use serde_json::{Value, json};

use super::*;

const MAX_PAGE_CHARS: usize = 100_000;
const MAX_BYTES: usize = 5 * 1024 * 1024;

pub struct WebFetch;

fn same_host(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    let strip = |u: &reqwest::Url| u.host_str().map(|h| h.trim_start_matches("www.").to_ascii_lowercase());
    strip(a) == strip(b)
}

/// The page as plain text: HTML becomes Markdown, everything else passes through.
fn page_text(content_type: &str, body: &str) -> String {
    if content_type.contains("html") {
        htmd::convert(body).unwrap_or_else(|_| body.to_owned())
    } else {
        body.to_owned()
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "WebFetch".into(),
            description: "Fetch a URL, convert the page to Markdown and answer `prompt` about it. It fails \
for pages that need a login and for localhost. HTTP links are upgraded to HTTPS. If the page redirects to \
another host, the new URL is returned and you must call WebFetch again with it. Results are specific to \
your prompt; quote at most a sentence or two from a page."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"},
                    "prompt": {"type": "string", "description": "What to find out from the page"}
                },
                "required": ["url", "prompt"]
            }),
        }
    }

    fn kind(&self) -> &'static str {
        "fetch"
    }

    fn allowed_in_plan(&self) -> bool {
        true
    }

    fn title(&self, input: &Value) -> String {
        format!("Fetch {}", str_arg(input, "url").unwrap_or(""))
    }

    fn needs_permission(&self, _: &Value, _: &crate::session::Session) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let (Some(url), Some(prompt)) = (str_arg(&input, "url"), str_arg(&input, "prompt")) else {
            return ToolOutput::error("url and prompt are required");
        };
        let mut url = match reqwest::Url::parse(url) {
            Ok(u) => u,
            Err(e) => return ToolOutput::error(format!("Invalid URL: {e}")),
        };
        if url.scheme() == "http" {
            let _ = url.set_scheme("https");
        }
        if url.scheme() != "https" {
            return ToolOutput::error("Only http and https URLs can be fetched.");
        }
        let host = url.host_str().unwrap_or("");
        if !host.contains('.') || host.eq_ignore_ascii_case("localhost") || host.starts_with("127.") {
            return ToolOutput::error("Local and single-label hosts cannot be fetched.");
        }
        let http = match reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("patchbay-agent/", env!("CARGO_PKG_VERSION")))
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let mut hops = 0;
        let (content_type, body) = loop {
            let resp = tokio::select! {
                _ = ctx.cancel.cancelled() => return ToolOutput::error("Cancelled."),
                r = http.get(url.clone()).send() => match r {
                    Ok(r) => r,
                    Err(e) => return ToolOutput::error(format!("Fetch failed: {e}")),
                },
            };
            if resp.status().is_redirection() {
                let Some(next) = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| url.join(l).ok())
                else {
                    return ToolOutput::error("The page redirects without saying where.");
                };
                if !same_host(&url, &next) {
                    return ToolOutput::text(format!(
                        "REDIRECT: the page moved to a different host. Call WebFetch again with this URL if you want it:\n{next}"
                    ));
                }
                hops += 1;
                if hops > 5 {
                    return ToolOutput::error("Too many redirects.");
                }
                url = next;
                continue;
            }
            if !resp.status().is_success() {
                return ToolOutput::error(format!("Fetch failed: HTTP {}", resp.status()));
            }
            let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("Fetch failed: {e}")),
            };
            if bytes.len() > MAX_BYTES {
                return ToolOutput::error("The page is larger than 5 MB.");
            }
            break (ct, String::from_utf8_lossy(&bytes).into_owned());
        };
        let mut text = page_text(&content_type, &body);
        if text.chars().count() > MAX_PAGE_CHARS {
            text = text.chars().take(MAX_PAGE_CHARS).collect::<String>() + "\n…[page truncated]";
        }
        let session = &ctx.session;
        let model_id = session.state.lock().await.model_id.clone();
        let mc = match session.host.model_client(&model_id).await {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let mut req = Request::simple(
            "You answer questions about a web page. Use only the page content. Be concise and quote sparingly.",
            format!("Page ({url}):\n---\n{text}\n---\n\n{prompt}"),
        );
        for (i, b) in mc.system_prefix.iter().enumerate() {
            req.system.insert(i, b.clone());
        }
        match mc.client.complete_text(&req, &ctx.cancel).await {
            Ok(answer) => ToolOutput::text(answer),
            Err(e) => ToolOutput::error(format!("Could not read the page: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_html_and_passes_text() {
        let md = page_text("text/html; charset=utf-8", "<h1>Title</h1><p>Hello <b>world</b></p>");
        assert!(md.contains("# Title") && md.contains("**world**"), "{md}");
        assert_eq!(page_text("text/plain", "<b>raw</b>"), "<b>raw</b>");
    }

    #[test]
    fn www_does_not_make_a_new_host() {
        let a = reqwest::Url::parse("https://example.com/a").unwrap();
        let b = reqwest::Url::parse("https://www.example.com/b").unwrap();
        let c = reqwest::Url::parse("https://other.com/").unwrap();
        assert!(same_host(&a, &b) && !same_host(&a, &c));
    }
}
