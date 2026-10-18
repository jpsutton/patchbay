//! The daemon's browser UI: the same frontend as the desktop app, served over HTTP, talking to
//! `api::call` over one WebSocket per tab.
//!
//! Wire format (JSON text frames):
//! - client → server: `{"auth": token}` first, then `{"id": n, "cmd": name, "args": {...}}`;
//! - server → client: `{"ready": true}` after auth, `{"id": n, "ok": value}` or
//!   `{"id": n, "err": message}` per command, `{"chan": n, "msg": value}` for a channel passed
//!   as an argument (`__CHANNEL__:n`), and `{"event": name, "payload": value}` for app events.
//!
//! Anyone holding the token can run agents and terminals as this user, so the server binds to
//! loopback unless told otherwise, and every socket must present the token before anything else.

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::AppState;
use crate::api::{self, ChanFactory, ChanSend};

/// App events for every connected tab, already serialized.
pub type Events = broadcast::Sender<Utf8Bytes>;

/// A `UiSink` that hands events to the web clients.
pub fn ui_sink(events: Events) -> crate::mesh::node::UiSink {
    Arc::new(move |name: &str, payload: Value| {
        // No receivers just means no tab is open.
        let _ = events.send(json!({ "event": name, "payload": payload }).to_string().into());
    })
}

pub struct Config {
    pub listen: SocketAddr,
    /// The built frontend (`dist/`).
    pub ui_dir: PathBuf,
    pub token: String,
}

#[derive(Clone)]
struct Ctx {
    state: AppState,
    events: Events,
    ui_dir: Arc<PathBuf>,
    token: Arc<str>,
}

pub async fn serve(state: AppState, events: Events, cfg: Config) -> Result<()> {
    let ctx = Ctx { state, events, ui_dir: Arc::new(cfg.ui_dir), token: cfg.token.into() };
    let app = Router::new().route("/ws", get(ws)).fallback(get(static_file)).with_state(ctx);
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("listening on {}", cfg.listen))?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws(State(ctx): State<Ctx>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-origin WebSocket refused").into_response();
    }
    upgrade.max_message_size(512 * 1024 * 1024).on_upgrade(move |socket| conn(ctx, socket))
}

/// A page on another site must not drive this one through the visitor's browser. Browsers always
/// send `Origin` on WebSocket upgrades; its host must be the one the page was served from.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        // Not a browser (a script or CLI): the token still guards it.
        return true;
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    origin.split_once("://").is_some_and(|(_, h)| h == host)
}

fn token_matches(given: &str, want: &str) -> bool {
    given.len() == want.len() && given.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

async fn conn(ctx: Ctx, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    // The first frame must be the token.
    let authed = match tokio::time::timeout(std::time::Duration::from_secs(10), stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t)
            .ok()
            .and_then(|v| v["auth"].as_str().map(|g| token_matches(g, &ctx.token)))
            .unwrap_or(false),
        _ => false,
    };
    if !authed {
        let _ = sink.send(Message::Text(json!({ "authError": "wrong or missing token" }).to_string().into())).await;
        let _ = sink.close().await;
        return;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Utf8Bytes>();
    let mut events = ctx.events.subscribe();
    let _ = tx.send(json!({ "ready": true }).to_string().into());
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
                e = events.recv() => match e {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("web client fell behind; dropped {n} events");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            };
            if sink.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let chans: ChanFactory = {
        let tx = tx.clone();
        Arc::new(move |id: &str| {
            let n: u64 = id.strip_prefix("__CHANNEL__:")?.parse().ok()?;
            let tx = tx.clone();
            Some(Arc::new(move |msg: Value| tx.send(json!({ "chan": n, "msg": msg }).to_string().into()).is_ok())
                as ChanSend)
        })
    };

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else { continue };
        let Ok(mut req) = serde_json::from_str::<Value>(&text) else { continue };
        let id = req["id"].clone();
        let cmd = req["cmd"].as_str().unwrap_or_default().to_owned();
        let args = req["args"].take();
        let (state, chans, tx) = (ctx.state.clone(), chans.clone(), tx.clone());
        tokio::spawn(async move {
            let reply = match api::call(&state, &cmd, args, &chans).await {
                Ok(v) => json!({ "id": id, "ok": v }),
                Err(e) => json!({ "id": id, "err": e.0 }),
            };
            let _ = tx.send(reply.to_string().into());
        });
    }
    drop(tx);
    writer.abort();
}

async fn static_file(State(ctx): State<Ctx>, req: Request) -> Response {
    let rel = req.uri().path().trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let Some(path) = safe_join(&ctx.ui_dir, rel) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(body) => {
            let mut res = body.into_response();
            let h = res.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime(&path)));
            // Vite fingerprints everything under assets/; index.html must always be fresh.
            let cache = if rel.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
            h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
            res
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `root/rel`, refusing anything that could step outside `root`.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_ui_dir() {
        let root = Path::new("/srv/ui");
        assert_eq!(safe_join(root, "assets/a.js"), Some(PathBuf::from("/srv/ui/assets/a.js")));
        assert_eq!(safe_join(root, "../etc/passwd"), None);
        assert_eq!(safe_join(root, "assets/../../x"), None);
        assert_eq!(safe_join(root, "/etc/passwd"), None);
    }

    #[test]
    fn origin_must_match_host() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4420"));
        assert!(same_origin(&h));
        h.insert(header::ORIGIN, HeaderValue::from_static("http://127.0.0.1:4420"));
        assert!(same_origin(&h));
        h.insert(header::ORIGIN, HeaderValue::from_static("https://evil.example"));
        assert!(!same_origin(&h));
    }

    #[test]
    fn token_comparison() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abd", "abc"));
        assert!(!token_matches("ab", "abc"));
    }
}
