//! Claude Code subscription OAuth for the Chat tab.
//!
//! Reads `~/.claude/.credentials.json` (leading-dot filename; same file Claude Code uses on
//! Linux), refreshes
//! the access token when it is near expiry, and writes rotated tokens back so Claude Code
//! stays signed in.
//!
//! macOS Keychain storage (Claude Code's format there) is out of scope; this machine and
//! the path we support is the Linux credentials file.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// Claude Code's public OAuth client id (from the Claude Code binary).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Refresh when fewer than this many ms remain until expiry.
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;

struct Cached {
    token: String,
    expires_at_ms: i64,
}

static CACHE: RwLock<Option<Cached>> = RwLock::new(None);
static REFRESH_LOCK: Mutex<()> = Mutex::const_new(());

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn token_endpoint() -> String {
    std::env::var("PATCHBAY_CLAUDE_TOKEN_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/oauth/token".into())
}

/// Directory Claude Code stores config in (`$CLAUDE_CONFIG_DIR` or `~/.claude`).
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let p = PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot find home directory"))?;
    Ok(home.join(".claude"))
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(".credentials.json"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAiOauth {
    access_token: String,
    refresh_token: String,
    /// Unix epoch milliseconds.
    expires_at: i64,
}

fn read_oauth(path: &Path) -> Result<(Value, ClaudeAiOauth)> {
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "Claude Code login not found at {} — run `claude` and sign in",
            path.display()
        )
    })?;
    let root: Value = serde_json::from_str(&raw)
        .with_context(|| format!("invalid Claude credentials file at {}", path.display()))?;
    let oauth_val = root
        .get("claudeAiOauth")
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no Claude Code login in {} — run `claude` and sign in",
                path.display()
            )
        })?;
    let oauth: ClaudeAiOauth = serde_json::from_value(oauth_val).with_context(|| {
        format!(
            "Claude credentials at {} are missing access/refresh tokens",
            path.display()
        )
    })?;
    if oauth.access_token.is_empty() || oauth.refresh_token.is_empty() {
        return Err(anyhow!(
            "Claude Code login at {} is incomplete — run `claude` and sign in",
            path.display()
        ));
    }
    Ok((root, oauth))
}

/// Atomically rewrite `path`, preserving sibling top-level JSON keys.
fn write_oauth(path: &Path, mut root: Value, access_token: &str, refresh_token: &str, expires_at_ms: i64) -> Result<()> {
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("credentials root must be a JSON object"))?;
    let entry = obj
        .entry("claudeAiOauth".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let oauth = entry
        .as_object_mut()
        .ok_or_else(|| anyhow!("claudeAiOauth must be a JSON object"))?;
    oauth.insert("accessToken".into(), Value::String(access_token.to_owned()));
    oauth.insert("refreshToken".into(), Value::String(refresh_token.to_owned()));
    oauth.insert("expiresAt".into(), Value::Number(expires_at_ms.into()));

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("credentials path has no parent directory"))?;
    let tmp = parent.join(format!(
        ".credentials.{}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&root)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

async fn refresh_http(refresh_token: &str) -> Result<TokenResponse> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let url = token_endpoint();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("anthropic-beta", OAUTH_BETA)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let clipped: String = body.chars().take(300).collect();
        return Err(anyhow!(
            "Claude Code token refresh failed ({status}): {clipped}"
        ));
    }
    resp.json::<TokenResponse>()
        .await
        .context("parse Claude Code token refresh response")
}

fn cache_get() -> Option<String> {
    let guard = CACHE.read().ok()?;
    let c = guard.as_ref()?;
    if c.expires_at_ms - now_ms() > REFRESH_SKEW_MS {
        Some(c.token.clone())
    } else {
        None
    }
}

fn cache_set(token: String, expires_at_ms: i64) {
    if let Ok(mut g) = CACHE.write() {
        *g = Some(Cached {
            token,
            expires_at_ms,
        });
    }
}

/// Return a usable Claude Code access token, refreshing and rewriting the credentials file when needed.
pub async fn access_token() -> Result<String> {
    access_token_at(&credentials_path()?).await
}

pub async fn access_token_at(path: &Path) -> Result<String> {
    if let Some(t) = cache_get() {
        return Ok(t);
    }
    let _guard = REFRESH_LOCK.lock().await;
    // Another waiter may have refreshed while we queued.
    if let Some(t) = cache_get() {
        return Ok(t);
    }

    let (root, oauth) = read_oauth(path)?;
    if oauth.expires_at - now_ms() > REFRESH_SKEW_MS {
        cache_set(oauth.access_token.clone(), oauth.expires_at);
        return Ok(oauth.access_token);
    }

    let tokens = refresh_http(&oauth.refresh_token).await?;
    let expires_at_ms = now_ms() + tokens.expires_in.saturating_mul(1000);
    let refresh = tokens
        .refresh_token
        .as_deref()
        .unwrap_or(&oauth.refresh_token);
    write_oauth(path, root, &tokens.access_token, refresh, expires_at_ms)?;
    cache_set(tokens.access_token.clone(), expires_at_ms);
    Ok(tokens.access_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex as StdMutex;

    /// Serialize tests that touch process-wide cache / env.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn clear_cache() {
        if let Ok(mut g) = CACHE.write() {
            *g = None;
        }
    }

    fn write_creds(path: &Path, access: &str, refresh: &str, expires_at: i64) {
        let v = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at,
                "scopes": ["user:inference"],
                "subscriptionType": "max"
            },
            "otherKey": "keep-me"
        });
        std::fs::write(path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    }

    #[test]
    fn preserves_sibling_keys_on_write() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        write_creds(&path, "old-access", "old-refresh", 1);
        let root = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        write_oauth(&path, root, "new-access", "new-refresh", 99).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["otherKey"], "keep-me");
        assert_eq!(after["claudeAiOauth"]["accessToken"], "new-access");
        assert_eq!(after["claudeAiOauth"]["refreshToken"], "new-refresh");
        assert_eq!(after["claudeAiOauth"]["expiresAt"], 99);
        assert_eq!(after["claudeAiOauth"]["scopes"][0], "user:inference");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_oauth_requires_login() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(&path, r#"{"foo":1}"#).unwrap();
        let err = read_oauth(&path).unwrap_err().to_string();
        assert!(err.contains("no Claude Code login"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn returns_fresh_token_without_http() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_cache();
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        write_creds(&path, "fresh-token", "r", now_ms() + 60 * 60 * 1000);
        let t = access_token_at(&path).await.unwrap();
        assert_eq!(t, "fresh-token");
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live check against Anthropic with the machine's Claude Code login.
    /// `cargo test --lib live_claude_login -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_claude_login_models_and_chat() {
        let token = access_token().await.expect("Claude Code login");
        let models = crate::providers::discover_models(
            &crate::db::ProviderKind::Anthropic,
            "https://api.anthropic.com/v1",
            Some(&token),
            crate::providers::Auth::ClaudeLogin,
        )
        .await
        .expect("discover models");
        assert!(!models.is_empty(), "expected at least one model");
        eprintln!("discovered {} models; first={}", models.len(), models[0].id);

        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .bearer_auth(&token)
            .header("anthropic-version", crate::providers::ANTHROPIC_VERSION)
            .header("anthropic-beta", OAUTH_BETA)
            .json(&serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 32,
                "messages": [{"role": "user", "content": "Reply with exactly: ok"}]
            }))
            .send()
            .await
            .expect("messages request");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "messages {status}: {body}");
        assert!(body.contains("ok") || body.contains("OK") || body.contains("text"), "{body}");
        eprintln!("chat reply ok ({status})");
    }

    #[tokio::test]
    async fn refreshes_expired_token_and_rewrites_file() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_cache();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.contains("refresh_token"));
            assert!(req.contains(CLIENT_ID));
            let body = r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });

        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        write_creds(&path, "stale", "old-refresh", now_ms() - 1000);

        // SAFETY: single-threaded under TEST_LOCK; restored below.
        unsafe {
            std::env::set_var(
                "PATCHBAY_CLAUDE_TOKEN_URL",
                format!("http://{addr}/v1/oauth/token"),
            );
        }
        let t = access_token_at(&path).await.unwrap();
        unsafe {
            std::env::remove_var("PATCHBAY_CLAUDE_TOKEN_URL");
        }
        assert_eq!(t, "rotated-access");
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["claudeAiOauth"]["accessToken"], "rotated-access");
        assert_eq!(after["claudeAiOauth"]["refreshToken"], "rotated-refresh");
        assert_eq!(after["otherKey"], "keep-me");
        server.join().unwrap();
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
