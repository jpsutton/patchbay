//! The daemon's browser UI: the same frontend as the desktop app, served over HTTP, talking to
//! `api::call` over one WebSocket per tab.
//!
//! Wire format (JSON text frames):
//! - client → server: `{"auth": token}` first, then `{"id": n, "cmd": name, "args": {...}}`;
//! - server → client: `{"ready": true, "version": v}` after auth, `{"id": n, "ok": value}`
//!   or `{"id": n, "err": message}` per command, `{"chan": n, "msg": value}` for a channel passed
//!   as an argument (`__CHANNEL__:n`), `{"chanEnd": n}` once the daemon is done with that
//!   channel, and `{"event": name, "payload": value}` for app events.
//!
//! The command loop is `ipc::server::serve_session`, shared with the desktop app's local socket.
//!
//! Anyone holding the token can run agents and terminals as this user, so the server binds to
//! loopback unless told otherwise, and every socket must present the token before anything else.
//! Even then `0.0.0.0` never reaches a public address; only a public IP named in `--listen` does
//! (`net_bind`).

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::AppState;
use crate::ipc::server::{Events, ready_frame, serve_session};
use crate::ipc::token_matches;
use crate::net_bind::{Binder, RESCAN};

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
    // The read-only session-status route (pb-20261110-04), built before the state moves into
    // the context below: the same handler the desktop app's share server serves, on the same
    // token file, so one discovery path covers a daemon-only, app-only or both-running host.
    let status =
        crate::session_status::router(state.clone(), cfg.token.clone(), crate::session_status::DAEMON);
    let ctx = Ctx { state, events, ui_dir: Arc::new(cfg.ui_dir), token: cfg.token.into() };
    // Share tokens are opaque capabilities; this route takes no path argument, so there is nothing
    // to traverse. It is registered before the static fallback so `/share/<token>` is never treated
    // as a UI asset. The sandbox CSP lives in `shares.rs`.
    let app = Router::new()
        .route("/ws", get(ws))
        // Phase 1: a share owned by THIS node, fetched by its bare token.
        .route("/share/{token}", get(share_file))
        // Phase 2: gateway proxy. The node id in the URL names the owner; the gateway relays
        // only the opaque token to that node over the mesh and enforces validity there.
        .route("/share/{node_id}/{token}", get(share_proxy))
        .fallback(get(static_file))
        .with_state(ctx)
        .merge(status);
    // A wildcard `--listen` binds only private, VPN and loopback addresses, never a public one
    // (`net_bind`); the interface list is re-read every `RESCAN`. A failed first bind is fatal.
    let mut binder = Binder::new(cfg.listen);
    let mut first = true;
    loop {
        let synced = binder
            .sync(|listener| {
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::warn!("web UI listener stopped: {e:#}");
                    }
                })
            })
            .await;
        match synced {
            Ok(()) => {}
            Err(e) if first => return Err(e.context(format!("listening on {}", cfg.listen))),
            Err(e) => tracing::warn!("web UI: {e:#}"),
        }
        first = false;
        tokio::time::sleep(RESCAN).await;
    }
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

    let events = ctx.events.subscribe();
    let (tx, mut rx) = mpsc::unbounded_channel::<Arc<str>>();
    let _ = tx.send(ready_frame());
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(Message::Text(m.to_string().into())).await.is_err() {
                break;
            }
        }
    });
    let incoming = stream.take_while(|m| std::future::ready(m.is_ok())).filter_map(|m| {
        std::future::ready(match m {
            Ok(Message::Text(t)) => Some(t.as_str().to_owned()),
            _ => None,
        })
    });
    serve_session(ctx.state.clone(), events, std::pin::pin!(incoming), tx).await;
    writer.abort();
}

/// GET one explicitly-shared artifact. The high-entropy token is the only authentication; a
/// wrong, expired or revoked token yields 404 with no bytes. All the security-sensitive logic
/// (canonicalization, expiry, sandboxed headers, streaming) is in `shares::handle`, shared with
/// the standalone desktop server so both surfaces behave identically — including the `Accept`
/// negotiation that serves a small HTML error page to browsers instead of an empty body.
async fn share_file(State(ctx): State<Ctx>, AxumPath(token): AxumPath<String>, headers: HeaderMap) -> Response {
    crate::shares::handle(&ctx.state.shares, &token, &headers).await
}

/// Gateway proxy for a share owned by another node (or this one). `GET /share/{node_id}/{token}`
/// forwards ONLY the token to the owning node over the existing mesh connection; that node enforces
/// expiry/revocation and returns the bytes plus the sandboxed/nosniff headers, relayed verbatim.
/// A wrong/expired/revoked token, an offline owner, or an RPC failure is a 404 with no bytes.
async fn share_proxy(
    State(ctx): State<Ctx>,
    AxumPath((node_id, token)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let fetch = crate::shares::gateway_fetch(&ctx.state, &node_id, &token).await;
    crate::shares::into_response(fetch, &headers)
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
}
