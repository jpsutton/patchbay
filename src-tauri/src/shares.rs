//! Explicit, revocable, expiring HTTP shares of files a coding agent (or a person) wrote.
//!
//! A share is `{ token -> one canonical, absolute file }`. Nothing else on the host is fetchable:
//! there is no directory listing, no path parameter on the route, and no "serve the session cwd"
//! mode. The high-entropy token is the only capability, so a link is safe to hand to a browser
//! that knows nothing else about this app.
//!
//! The route is deliberately served with an isolated, script-free origin:
//! `Content-Security-Policy: sandbox` (no `allow-same-origin`), `X-Content-Type-Options: nosniff`.
//! That way an HTML artifact cannot script against the app/gateway origin or read its cookies or
//! tokens, even when the UI and the share happen to be served from the same host:port.
//!
//! The HTTP handler (`handle`) is framework-light and shared by:
//! - the daemon's axum UI server (`web.rs`, behind the `web` feature): `GET /share/{token}`;
//! - a small loopback axum server for the standalone desktop / a daemon built without `web`
//!   (`spawn`). Both use the *same* `AppState.shares` registry, so a share created over IPC is
//!   served no matter which process owns the data folder.
//!
//! Shares persist to `<data_dir>/shares.json` so they survive a restart. The URL's base comes from
//! the per-node `artifacts.baseUrl` setting (default `http://127.0.0.1:4420`): Phase 2 can point
//! it at the gateway proxy without changing anything here.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

/// Per-node setting holding the public base URL for share links (see module docs).
pub const BASE_URL_SETTING: &str = "artifacts.baseUrl";
/// Used when the setting is absent. Also the loopback address the standalone server binds to.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:4420";
const SHARES_FILE: &str = "shares.json";
/// Default lifetime of a share. Callers may pass a shorter one.
const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;

/// One share as persisted in `shares.json` and held in memory.
#[derive(Clone, Serialize, Deserialize)]
struct ShareRecord {
    token: String,
    /// Canonicalized absolute path of the (single) shared file. Re-canonicalized at serve time.
    path: String,
    /// Display filename for `Content-Disposition`.
    name: String,
    /// Exact `Content-Type` header value.
    mime: String,
    /// Full URL handed to the user, rooted at the configured base URL for this node.
    url: String,
    created_at: i64,
    expires_at: i64,
    revoked: bool,
}

/// What an IPC caller gets back for a share (never includes anything but the capability + metadata).
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareInfo {
    pub token: String,
    pub path: String,
    pub name: String,
    pub mime: String,
    pub url: String,
    pub created_at: i64,
    pub expires_at: i64,
}

fn info_of(r: &ShareRecord) -> ShareInfo {
    ShareInfo {
        token: r.token.clone(),
        path: r.path.clone(),
        name: r.name.clone(),
        mime: r.mime.clone(),
        url: r.url.clone(),
        created_at: r.created_at,
        expires_at: r.expires_at,
    }
}

/// Everything needed to answer one valid share request. `headers` are the exact response headers.
struct ResolvedShare {
    path: PathBuf,
    headers: Vec<(&'static str, String)>,
}

/// The share registry, held as `Arc<ShareRegistry>` on `AppState` and shared by IPC and HTTP.
pub struct ShareRegistry {
    records: Mutex<HashMap<String, ShareRecord>>,
    file: PathBuf,
    base_url: String,
}

impl ShareRegistry {
    /// Load persisted shares for `data_dir`, pruning expired/revoked ones so they are never served.
    pub fn load(data_dir: &Path, base_url: String) -> Self {
        let file = data_dir.join(SHARES_FILE);
        let mut records = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Ok(list) = serde_json::from_str::<Vec<ShareRecord>>(&text) {
                let now = crate::db::now_ms();
                for rec in list {
                    if rec.revoked || rec.expires_at <= now {
                        continue;
                    }
                    records.insert(rec.token.clone(), rec);
                }
            }
        }
        let reg = ShareRegistry { records: Mutex::new(records), file, base_url };
        // Persist the pruned set so expired records don't linger on disk.
        if let Ok(map) = reg.records.lock() {
            let _ = reg.persist(&map);
        }
        reg
    }

    /// Create a share for one file. `path` may be absolute, `~/…`, or relative to the session's
    /// `cwd`. The file is canonicalized (symlinks resolved, `..` resolved) and confined to that
    /// single regular file.
    pub fn create(&self, path: &str, cwd: Option<&str>, ttl_secs: Option<i64>) -> Result<ShareInfo> {
        let canonical = resolve_path(path, cwd)?;
        let mime = infer_mime(&canonical);
        let name = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".to_owned());
        let token = new_token();
        let now = crate::db::now_ms();
        let ttl = ttl_secs.unwrap_or(DEFAULT_TTL_SECS).max(1);
        let expires_at = now.saturating_add(ttl.saturating_mul(1000));
        let url = format!("{}/share/{}", self.base_url.trim_end_matches('/'), token);
        let rec = ShareRecord {
            token: token.clone(),
            path: canonical.to_string_lossy().into_owned(),
            name: name.clone(),
            mime: mime.clone(),
            url: url.clone(),
            created_at: now,
            expires_at,
            revoked: false,
        };
        {
            let mut map = self.records.lock().map_err(|_| anyhow!("share registry lock poisoned"))?;
            map.insert(token.clone(), rec);
            self.persist(&map)?;
        }
        Ok(ShareInfo {
            token,
            path: canonical.to_string_lossy().into_owned(),
            name,
            mime,
            url,
            created_at: now,
            expires_at,
        })
    }

    /// Live (non-expired, non-revoked) shares, newest first, pruning expired ones from disk.
    pub fn list(&self) -> Vec<ShareInfo> {
        let now = crate::db::now_ms();
        let mut out = Vec::new();
        if let Ok(mut map) = self.records.lock() {
            map.retain(|_, r| !r.revoked && r.expires_at > now);
            let _ = self.persist(&map);
            out.extend(map.values().map(info_of));
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    /// Revoke immediately: the token stops resolving at once. Returns whether it existed.
    pub fn revoke(&self, token: &str) -> bool {
        if let Ok(mut map) = self.records.lock() {
            let removed = map.remove(token).is_some();
            if removed {
                let _ = self.persist(&map);
            }
            removed
        } else {
            false
        }
    }

    /// Resolve a token to a serveable file, enforcing expiry server-side. Wrong/expired/revoked
    /// tokens all yield `None` (indistinguishable, so the route can't be used to enumerate them).
    fn resolve(&self, token: &str) -> Option<ResolvedShare> {
        self.resolve_at(token, crate::db::now_ms())
    }

    fn resolve_at(&self, token: &str, now: i64) -> Option<ResolvedShare> {
        let rec = self.records.lock().ok()?.get(token).cloned()?;
        if rec.revoked || rec.expires_at <= now {
            return None;
        }
        // Re-canonicalize at serve time: if the shared path was swapped for a symlink after
        // creation, this resolves elsewhere and we refuse rather than leak the new target.
        let canonical = std::fs::canonicalize(&rec.path).ok()?;
        if canonical.as_path() != Path::new(&rec.path) || !canonical.is_file() {
            return None;
        }
        Some(ResolvedShare {
            path: canonical,
            headers: build_headers(&rec.mime, &rec.name),
        })
    }

    /// Test helper: simulate the clock reaching/past a share's expiry without sleeping.
    #[cfg(test)]
    fn force_expire(&self, token: &str) {
        if let Ok(mut map) = self.records.lock() {
            if let Some(r) = map.get_mut(token) {
                r.expires_at = crate::db::now_ms() - 1;
            }
        }
    }

    fn persist(&self, map: &HashMap<String, ShareRecord>) -> Result<()> {
        let records: Vec<&ShareRecord> = map.values().collect();
        let json = serde_json::to_string(&records)?;
        // Atomic so a crash mid-write never leaves a truncated registry.
        let tmp = self.file.with_file_name("shares.json.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        // The file holds capability tokens: readable by this user only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.file)?;
        Ok(())
    }
}

/// Turn an agent-written path into the canonical absolute path of exactly one regular file.
/// Directories, missing files and relative paths with no folder are rejected; symlinks are
/// resolved to their target (so a share never points at a symlink that could be retargeted later).
fn resolve_path(input: &str, cwd: Option<&str>) -> Result<PathBuf> {
    let joined = if let Some(rest) = input.strip_prefix("~/") {
        crate::code::agents::home_dir()
            .ok_or_else(|| anyhow!("no home directory"))?
            .join(rest)
    } else {
        let p = Path::new(input);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            let c = cwd
                .filter(|c| !c.is_empty())
                .ok_or_else(|| anyhow!("relative path has no folder to resolve against"))?;
            Path::new(c).join(p)
        }
    };
    let canonical = std::fs::canonicalize(&joined)
        .with_context(|| format!("cannot resolve {}", joined.display()))?;
    if !canonical.is_file() {
        bail!("only regular files can be shared (not a directory or a missing file)");
    }
    Ok(canonical)
}

/// 256-bit, single-purpose capability token (64 lowercase hex chars).
fn new_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

/// MIME by content sniff first (binary signatures), then extension, defaulting to octet-stream.
/// Text types keep their extension semantics (an `.html` file must render as HTML, not as the
/// generic XML/text type its leading bytes might sniff as).
fn infer_mime(path: &Path) -> String {
    if let Some(prefix) = read_prefix(path, 8192) {
        if let Some(kind) = infer::get(&prefix) {
            let m = kind.mime_type();
            if !m.starts_with("text/") && m != "application/xml" {
                return m.to_owned();
            }
        }
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" | "jsonl" => "application/json",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "txt" | "log" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
    .to_owned()
}

fn read_prefix(path: &Path, n: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(n).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn is_inline(mime: &str) -> bool {
    let base = mime.split(';').next().unwrap_or("").trim();
    base.starts_with("text/")
        || base.starts_with("image/")
        || matches!(base, "application/pdf" | "application/xml" | "application/json")
}

/// Keep only printable ASCII that is safe inside a quoted `filename="…"`.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && !matches!(c, '"' | '\\' | '/' | ';') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "artifact".to_owned()
    } else {
        cleaned
    }
}

/// The exact headers for a served artifact. A bare `sandbox` (no `allow-same-origin`) gives the
/// document an opaque origin, so it cannot script against the app/gateway or read its storage.
fn build_headers(mime: &str, name: &str) -> Vec<(&'static str, String)> {
    let disposition = if is_inline(mime) { "inline" } else { "attachment" };
    let safe = sanitize_filename(name);
    vec![
        ("content-type", mime.to_owned()),
        ("content-disposition", format!("{disposition}; filename=\"{safe}\"")),
        ("content-security-policy", "sandbox".to_owned()),
        ("x-content-type-options", "nosniff".to_owned()),
        ("cache-control", "no-store".to_owned()),
        ("referrer-policy", "no-referrer".to_owned()),
    ]
}

/// The whole HTTP route. Returns 404 with an empty body for any invalid token (no bytes leaked),
/// otherwise streams the file (never buffered in full) with the sandboxed/nosniff headers.
pub(crate) async fn handle(registry: &ShareRegistry, token: &str) -> Response {
    let Some(resolved) = registry.resolve(token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // The file may have disappeared between resolve() and here; treat that as a missing share.
    let file = match tokio::fs::File::open(&resolved.path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut res = Response::new(Body::from_stream(ReaderStream::new(file)));
    let h = res.headers_mut();
    for (name, value) in &resolved.headers {
        if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            h.insert(k, v);
        }
    }
    res
}

#[derive(Clone)]
struct ShareState {
    registry: Arc<ShareRegistry>,
}

async fn share_handler(State(st): State<ShareState>, AxumPath(token): AxumPath<String>) -> Response {
    handle(&st.registry, &token).await
}

fn router(registry: Arc<ShareRegistry>) -> Router {
    Router::new()
        .route("/share/{token}", get(share_handler))
        .with_state(ShareState { registry })
}

/// Loopback address for the standalone share server. When the configured base is itself a
/// loopback `http://` URL we bind exactly its authority (so the URL resolves locally); Phase 2's
/// gateway base is not loopback, so the server keeps a local port for the gateway to proxy to.
fn listen_addr(base_url: &str) -> SocketAddr {
    if let Some(rest) = base_url.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or("");
        if let Ok(addr) = authority.parse::<SocketAddr>() {
            if addr.ip().is_loopback() {
                return addr;
            }
        }
    }
    "127.0.0.1:4420".parse().unwrap()
}

/// Start the standalone loopback share server (desktop, or a daemon built without `web`).
/// A bind failure is a warning, not fatal: share creation still works, and a `web`-enabled
/// daemon serves shares from its main server instead (see `web.rs`). Takes the whole `AppState`
/// so callers in the `patchbayd` binary (which can't see its private fields) can start it too.
pub fn spawn(state: &crate::AppState) {
    let registry = state.shares.clone();
    crate::api::spawn(async move {
        let addr = listen_addr(&registry.base_url);
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                tracing::info!("artifact share server on http://{addr}");
                if let Err(e) = axum::serve(listener, router(registry)).await {
                    tracing::warn!("artifact share server stopped: {e:#}");
                }
            }
            Err(e) => tracing::warn!("could not start artifact share server on {addr}: {e:#}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    fn registry(dir: &Path, base: &str) -> ShareRegistry {
        ShareRegistry::load(dir, base.to_owned())
    }

    fn write_file(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn canonical(p: &Path) -> String {
        std::fs::canonicalize(p).unwrap().to_string_lossy().into_owned()
    }

    fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
        res.headers().get(name).and_then(|v| v.to_str().ok())
    }

    fn fixture(dir: &Path, records: &[ShareRecord]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(SHARES_FILE), serde_json::to_string(records).unwrap()).unwrap();
    }

    #[test]
    fn token_is_high_entropy_and_single_purpose() {
        let dir = crate::test_support::short_dir("share-tok");
        let r = registry(&dir, DEFAULT_BASE_URL);
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 64, "256-bit token as hex");
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        // A freshly generated token is not valid for an empty registry.
        assert!(r.resolve(&a).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn valid_share_streams_bytes_with_sandboxed_headers() {
        let dir = crate::test_support::short_dir("share-ok");
        let file = write_file(&dir, "report.html", b"<h1>hello</h1><script>x</script>");
        let r = registry(&dir, DEFAULT_BASE_URL);
        let info = r.create(&file.to_string_lossy(), None, None).unwrap();

        // Correct token resolves to the canonical file with the right headers.
        let resolved = r.resolve(&info.token).unwrap();
        assert_eq!(resolved.path, std::fs::canonicalize(&file).unwrap());
        let csp = resolved.headers.iter().find(|(k, _)| *k == "content-security-policy").unwrap();
        assert_eq!(csp.1, "sandbox");
        assert!(!csp.1.contains("allow-same-origin"));
        assert_eq!(
            resolved.headers.iter().find(|(k, _)| *k == "x-content-type-options").unwrap().1,
            "nosniff"
        );

        let res = handle(&r, &info.token).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(header(&res, "content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(header(&res, "content-security-policy"), Some("sandbox"));
        assert_eq!(header(&res, "x-content-type-options"), Some("nosniff"));
        assert!(header(&res, "content-disposition").unwrap().starts_with("inline"));
        assert!(header(&res, "content-disposition").unwrap().contains("report.html"));
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"<h1>hello</h1><script>x</script>");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn wrong_token_is_404_with_no_bytes() {
        let dir = crate::test_support::short_dir("share-wrong");
        let file = write_file(&dir, "report.html", b"secret");
        let r = registry(&dir, DEFAULT_BASE_URL);
        let info = r.create(&file.to_string_lossy(), None, None).unwrap();
        // Opaque tokens: a made-up token, a path, and an empty token (no directory listing) all fail.
        for bad in ["deadbeef", &info.token[..info.token.len() - 1], "../etc/passwd", "/etc/passwd", ""] {
            let res = handle(&r, bad).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "token {bad:?}");
            let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
            assert!(body.is_empty(), "no bytes for {bad:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn expired_share_is_not_served() {
        let dir = crate::test_support::short_dir("share-exp");
        let file = write_file(&dir, "report.html", b"secret");
        let r = registry(&dir, DEFAULT_BASE_URL);
        let info = r.create(&file.to_string_lossy(), None, None).unwrap();
        // Server-side expiry: at (or past) its expiry instant the token stops resolving.
        assert!(r.resolve_at(&info.token, info.expires_at - 1).is_some());
        assert!(r.resolve_at(&info.token, info.expires_at).is_none());
        assert!(r.resolve_at(&info.token, info.expires_at + 1).is_none());
        // Advance the clock past expiry: the route must now refuse with no bytes.
        r.force_expire(&info.token);
        let res = handle(&r, &info.token).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(to_bytes(res.into_body(), usize::MAX).await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn expired_records_are_not_served_after_reload() {
        let dir = crate::test_support::short_dir("share-exp-reload");
        let file = write_file(&dir, "report.html", b"secret");
        let now = crate::db::now_ms();
        let live = ShareRecord {
            token: "livetoken".into(),
            path: canonical(&file),
            name: "report.html".into(),
            mime: "text/html; charset=utf-8".into(),
            url: format!("{}/share/livetoken", DEFAULT_BASE_URL),
            created_at: now,
            expires_at: now + 60_000,
            revoked: false,
        };
        let expired = ShareRecord { token: "oldtoken".into(), expires_at: now - 1, ..live.clone() };
        let revoked = ShareRecord { token: "revokedtoken".into(), revoked: true, ..live.clone() };
        fixture(&dir, &[live.clone(), expired, revoked]);
        let r = registry(&dir, DEFAULT_BASE_URL);
        assert!(r.resolve("livetoken").is_some());
        assert!(r.resolve("oldtoken").is_none(), "expired record pruned/not served");
        assert!(r.resolve("revokedtoken").is_none(), "revoked record pruned/not served");
        // A wrong/expired token never yields bytes through the route.
        let res = handle(&r, "oldtoken").await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert!(to_bytes(res.into_body(), usize::MAX).await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn revoke_takes_effect_immediately() {
        let dir = crate::test_support::short_dir("share-revoke");
        let file = write_file(&dir, "report.html", b"secret");
        let r = registry(&dir, DEFAULT_BASE_URL);
        let info = r.create(&file.to_string_lossy(), None, None).unwrap();
        assert!(r.resolve(&info.token).is_some());
        assert!(r.revoke(&info.token));
        assert!(r.resolve(&info.token).is_none());
        assert_eq!(handle(&r, &info.token).await.status(), StatusCode::NOT_FOUND);
        assert!(!r.revoke(&info.token), "already gone");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn traversal_and_absolute_swaps_are_confined() {
        let dir = crate::test_support::short_dir("share-trav");
        let sub = dir.join("a");
        let public = write_file(&sub, "public.html", b"public");
        let _secret = write_file(&dir, "secret.html", b"secret");
        let r = registry(&dir, DEFAULT_BASE_URL);

        // A `..` in the input is canonicalized away, not used to escape: the share points at exactly
        // the one canonical file, never a directory or a sibling.
        let via_dotdot = sub.join("..").join("a").join("public.html");
        let info = r.create(&via_dotdot.to_string_lossy(), None, None).unwrap();
        assert_eq!(info.path, canonical(&public));
        assert!(!info.path.contains(".."));
        assert_eq!(r.resolve(&info.token).unwrap().path, std::fs::canonicalize(&public).unwrap());

        // An "absolute-path swap" — trying to share a directory — is rejected.
        assert!(r.create(&dir.to_string_lossy(), None, None).is_err(), "directories are not shareable");
        // A relative path with no folder cannot resolve to an arbitrary absolute target.
        assert!(r.create("report.html", None, None).is_err());
        // Only the shared file is reachable: a sibling's path is not a valid token.
        assert!(r.resolve("secret.html").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_canonicalization_and_swap_are_rejected() {
        let dir = crate::test_support::short_dir("share-sym");
        let real = write_file(&dir, "real.html", b"real");
        let other = write_file(&dir, "other.html", b"other");
        let link = dir.join("link.html");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let r = registry(&dir, DEFAULT_BASE_URL);

        // Sharing a symlink canonicalizes to its real target, so the record never holds a link.
        let info = r.create(&link.to_string_lossy(), None, None).unwrap();
        assert_eq!(info.path, canonical(&real));
        assert!(handle(&r, &info.token).await.status().is_success());

        // swapping the shared path for a symlink to a DIFFERENT file is refused at serve time.
        std::fs::remove_file(&real).unwrap();
        std::os::unix::fs::symlink(&other, &real).unwrap();
        assert!(r.resolve(&info.token).is_none(), "re-canonicalize caught the symlink swap");
        assert_eq!(handle(&r, &info.token).await.status(), StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn headers_and_mime_by_type() {
        let inline = build_headers("text/html; charset=utf-8", "weird\"name.html");
        let get = |h: &[(&'static str, String)], k: &str| h.iter().find(|(n, _)| *n == k).unwrap().1.clone();
        assert_eq!(get(&inline, "content-security-policy"), "sandbox");
        assert_eq!(get(&inline, "x-content-type-options"), "nosniff");
        assert_eq!(get(&inline, "content-disposition"), "inline; filename=\"weird_name.html\"");

        let binary = build_headers("application/octet-stream", "blob.bin");
        assert!(get(&binary, "content-disposition").starts_with("attachment;"));
        assert_eq!(get(&binary, "content-type"), "application/octet-stream");

        let dir = crate::test_support::short_dir("share-mime");
        let html = write_file(&dir, "a.html", b"<html>");
        let png = write_file(&dir, "a.png", b"\x89PNG\r\n\x1a\nrest");
        let pdf = write_file(&dir, "a.pdf", b"%PDF-1.7 rest");
        let unknown = write_file(&dir, "a.zzz", b"\x00\x01\x02binary");
        assert_eq!(infer_mime(&html), "text/html; charset=utf-8");
        assert_eq!(infer_mime(&png), "image/png");
        assert_eq!(infer_mime(&pdf), "application/pdf");
        assert_eq!(infer_mime(&unknown), "application/octet-stream");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn route_serves_only_valid_share_tokens() {
        use tower::ServiceExt; // .oneshot on the axum Router
        let dir = crate::test_support::short_dir("share-route");
        let file = write_file(&dir, "report.html", b"<h1>hi</h1>");
        let r = Arc::new(registry(&dir, DEFAULT_BASE_URL));
        let info = r.create(&file.to_string_lossy(), None, None).unwrap();

        let request = |token: &str| {
            axum::http::Request::builder()
                .uri(format!("/share/{token}"))
                .body(Body::empty())
                .unwrap()
        };
        // The real route path serves the file with the sandboxed headers.
        let res = router(r.clone()).oneshot(request(&info.token)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(header(&res, "content-security-policy"), Some("sandbox"));
        assert_eq!(header(&res, "x-content-type-options"), Some("nosniff"));
        assert_eq!(&to_bytes(res.into_body(), usize::MAX).await.unwrap()[..], b"<h1>hi</h1>");
        // A wrong token, an empty token (no listing), a `..` token and a two-segment path all get
        // 404 with no bytes (the route takes only one opaque segment).
        for bad in ["nope", "", "..", "abc/def"] {
            let res = router(r.clone()).oneshot(request(bad)).await.unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "path segment {bad:?}");
            assert!(to_bytes(res.into_body(), usize::MAX).await.unwrap().is_empty());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn url_uses_configurable_base_and_persists_across_reload() {
        let dir = crate::test_support::short_dir("share-url");
        let file = write_file(&dir, "report.html", b"hi");
        let custom = "https://gateway.example/patchbay";
        let r = registry(&dir, custom);
        let info = r.create(&file.to_string_lossy(), None, None).unwrap();
        assert!(info.url.starts_with(&format!("{custom}/share/")), "{}", info.url);
        assert_eq!(info.url, format!("{custom}/share/{}", info.token));
        // Survives a restart (fresh registry instance reading the same file).
        let r2 = registry(&dir, custom);
        let listed = r2.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].url, info.url);
        assert!(r2.resolve(&info.token).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
