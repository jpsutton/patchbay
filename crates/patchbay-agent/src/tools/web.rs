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

/// The page as plain text. HTML becomes the main content as Markdown, without the navigation,
/// banners, sidebars, footers and scripts around it; everything else passes through.
fn page_text(content_type: &str, body: &str, url: &reqwest::Url) -> String {
    if !content_type.contains("html") {
        return body.to_owned();
    }
    let page = crate::extract::extract(body, Some(url));
    let mut out = String::new();
    if let Some(t) = &page.title {
        out.push_str(&format!("Title: {t}\n"));
    }
    out.push_str(&format!("URL: {url}\n\n{}", page.markdown));
    out
}

/// Most of a page returned as it is, when no question was asked about it.
const MAX_RETURNED_CHARS: usize = 40_000;

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "WebFetch".into(),
            description: "Fetch a URL and keep what matters: the page's main content as Markdown, without \
navigation, banners, sidebars, footers, scripts and styles. With `prompt`, a small model answers it from the \
page and you get the answer (quote at most a sentence or two). Without `prompt` you get the cleaned page \
itself, up to about 40,000 characters, for when you need the text. It fails for pages that need a login and \
for localhost. HTTP links are upgraded to HTTPS. If the page redirects to another host, the new URL is \
returned and you must call WebFetch again with it."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"},
                    "prompt": {"type": "string", "description": "What to find out from the page; leave out to get the cleaned page"}
                },
                "required": ["url"]
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
        let Some(url) = str_arg(&input, "url") else {
            return ToolOutput::error("url is required");
        };
        let prompt = str_arg(&input, "prompt").map(str::trim).filter(|p| !p.is_empty());
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
        let mut text = page_text(&content_type, &body, &url);
        let raw = json!({"url": url.as_str(), "bytes": body.len(), "kept_chars": text.chars().count()});
        let Some(prompt) = prompt else {
            let mut out = super::ToolOutput::text(clip_middle(&text, MAX_RETURNED_CHARS));
            out.raw = raw;
            return out;
        };
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
            Ok(answer) => {
                let mut out = ToolOutput::text(answer);
                out.raw = raw;
                out
            }
            Err(e) => ToolOutput::error(format!("Could not read the page: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_html_and_passes_text() {
        let url = reqwest::Url::parse("https://example.com/a").unwrap();
        let html = "<html><head><title>T</title></head><body><nav>MENU-JUNK</nav><h1>Title</h1><p>Hello <b>world</b></p></body></html>";
        let md = page_text("text/html; charset=utf-8", html, &url);
        assert!(md.starts_with("Title: T\nURL: https://example.com/a\n\n"), "{md}");
        assert!(md.contains("# Title") && md.contains("**world**") && !md.contains("MENU-JUNK"), "{md}");
        assert_eq!(page_text("text/plain", "<b>raw</b>", &url), "<b>raw</b>");
    }

    #[test]
    fn www_does_not_make_a_new_host() {
        let a = reqwest::Url::parse("https://example.com/a").unwrap();
        let b = reqwest::Url::parse("https://www.example.com/b").unwrap();
        let c = reqwest::Url::parse("https://other.com/").unwrap();
        assert!(same_host(&a, &b) && !same_host(&a, &c));
    }
}

// --- WebSearch (Brave Search) ---

pub struct WebSearch;

/// Brave marks matches with <strong> and escapes entities; show plain text.
fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn brave_base() -> String {
    // Overridable so tests can point at a mock.
    std::env::var("PATCHBAY_BRAVE_URL").unwrap_or_else(|_| "https://api.search.brave.com".into())
}

/// The query with domain filters as Brave's `site:` operators.
fn search_query(input: &Value) -> Option<String> {
    let mut q = str_arg(input, "query")?.trim().to_owned();
    let list = |key: &str| -> Vec<String> {
        input[key].as_array().into_iter().flatten().filter_map(Value::as_str).map(|d| d.trim().to_owned()).filter(|d| !d.is_empty() && !d.contains('*')).collect()
    };
    let allowed = list("allowed_domains");
    if !allowed.is_empty() {
        q.push_str(&format!(" ({})", allowed.iter().map(|d| format!("site:{d}")).collect::<Vec<_>>().join(" OR ")));
    }
    for d in list("blocked_domains") {
        q.push_str(&format!(" -site:{d}"));
    }
    Some(q)
}

fn format_results(body: &Value) -> String {
    let results = body["web"]["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        return "No results.".into();
    }
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, plain(r["title"].as_str().unwrap_or("")), r["url"].as_str().unwrap_or("")));
        let desc = plain(r["description"].as_str().unwrap_or(""));
        if !desc.is_empty() {
            out.push_str(&format!("   {desc}"));
            if let Some(age) = r["age"].as_str() {
                out.push_str(&format!(" ({age})"));
            }
            out.push('\n');
        }
        for extra in r["extra_snippets"].as_array().into_iter().flatten().filter_map(Value::as_str).take(2) {
            out.push_str(&format!("   - {}\n", plain(extra)));
        }
    }
    out.push_str("\nWhen you use these results, cite the pages as Markdown links in a Sources section at the end of your answer.");
    out
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "WebSearch".into(),
            description: "Search the web (Brave Search) for current information past your training data: recent \
events, new library versions, documentation. Returns titles, URLs and snippets; use WebFetch to read a page. \
Include the current year in queries about recent things. After answering from results, end with a \"Sources:\" \
list of Markdown links to the pages you used. `allowed_domains` limits results to those sites and \
`blocked_domains` removes them (no wildcards)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query"},
                    "allowed_domains": {"type": "array", "items": {"type": "string"}},
                    "blocked_domains": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["query"]
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
        format!("Search the web: {}", str_arg(input, "query").unwrap_or(""))
    }

    fn needs_permission(&self, _: &Value, _: &crate::session::Session) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(query) = search_query(&input).filter(|q| q.trim().len() >= 2) else {
            return ToolOutput::error("query is required (at least two characters)");
        };
        let Some(key) = ctx.session.host.web_search_key() else {
            return ToolOutput::error("Web search has no API key. Add a Brave Search API key in Settings.");
        };
        let http = match reqwest::Client::builder().timeout(Duration::from_secs(20)).build() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let req = http
            .get(format!("{}/res/v1/web/search", brave_base()))
            .query(&[("q", query.as_str()), ("count", "10"), ("extra_snippets", "true")])
            .header("accept", "application/json")
            .header("x-subscription-token", key);
        let resp = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("Cancelled."),
            r = req.send() => match r {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("Search failed: {e}")),
            },
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["detail"].as_str().or(v["error"]["message"].as_str()).map(str::to_owned))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return ToolOutput::error(match status.as_u16() {
                401 | 403 | 422 => format!("Brave Search refused the API key ({status}). Check it in Settings. {detail}"),
                429 => format!("Brave Search rate limit reached. Wait a moment, or check your plan. {detail}"),
                _ => format!("Brave Search failed ({status}). {detail}"),
            });
        }
        match resp.json::<Value>().await {
            Ok(body) => ToolOutput::text(format_results(&body)),
            Err(e) => ToolOutput::error(format!("Brave Search sent an unreadable answer: {e}")),
        }
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn builds_queries_with_site_filters() {
        let q = search_query(&json!({"query": "rust async", "allowed_domains": ["docs.rs", "rust-lang.org"], "blocked_domains": ["spam.com", "*.bad.com"]})).unwrap();
        assert_eq!(q, "rust async (site:docs.rs OR site:rust-lang.org) -site:spam.com");
    }

    #[test]
    fn formats_results_as_plain_text() {
        let body = json!({"web": {"results": [
            {"title": "The <strong>Rust</strong> Book", "url": "https://doc.rust-lang.org/book/", "description": "Learn &amp; build", "age": "2 days ago", "extra_snippets": ["more"]},
        ]}});
        let t = format_results(&body);
        assert!(t.starts_with("1. The Rust Book\n   https://doc.rust-lang.org/book/\n   Learn & build (2 days ago)\n   - more"), "{t}");
        assert!(t.contains("Sources section"));
        assert_eq!(format_results(&json!({"web": {"results": []}})), "No results.");
    }
}
