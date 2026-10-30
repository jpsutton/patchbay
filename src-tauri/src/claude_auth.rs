//! Claude Code subscription OAuth for the Chat tab and Patchbay's agent.
//!
//! Reads a Claude Code config directory's login: `<dir>/.credentials.json` (what Claude Code
//! writes on Linux and Windows), or on macOS the Keychain item Claude Code keeps for that
//! directory. Refreshes the access token when it is near expiry, and writes rotated tokens
//! back to the same place so Claude Code stays signed in.
//!
//! Each provider may name its own directory (another `CLAUDE_CONFIG_DIR`), so one machine can
//! use several Claude accounts side by side.

use std::collections::BTreeMap;
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

/// Tokens by config directory: one entry per Claude account in use.
static CACHE: RwLock<BTreeMap<PathBuf, Cached>> = RwLock::new(BTreeMap::new());
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

fn home() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot find home directory"))
}

/// Directory Claude Code stores config in (`$CLAUDE_CONFIG_DIR` or `~/.claude`).
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let p = PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    Ok(home()?.join(".claude"))
}

/// A provider's Claude config directory (`~/` expanded), or the default when it names none.
pub fn resolve_dir(dir: Option<&str>) -> Result<PathBuf> {
    match dir.map(str::trim).filter(|d| !d.is_empty()) {
        None => config_dir(),
        Some("~") => home(),
        Some(d) => match d.strip_prefix("~/") {
            Some(rest) => Ok(home()?.join(rest)),
            None => Ok(PathBuf::from(d)),
        },
    }
}

/// The Claude account a config directory is signed in to, as a key that is the same on every
/// computer (`<accountUuid>:<organizationUuid>`, from Claude Code's `.claude.json`), and the
/// organization's name. Falls back to the directory's path when Claude Code hasn't recorded one.
pub fn account(dir: &Path) -> (String, Option<String>) {
    static SEEN: std::sync::Mutex<BTreeMap<PathBuf, (SystemTime, String, Option<String>)>> =
        std::sync::Mutex::new(BTreeMap::new());
    let fallback = || (dir.to_string_lossy().into_owned(), None);
    // `~/.claude` keeps it beside the directory; any other directory, inside it.
    let file = match home() {
        Ok(h) if dir == h.join(".claude") => h.join(".claude.json"),
        _ => dir.join(".claude.json"),
    };
    let Ok(modified) = std::fs::metadata(&file).and_then(|m| m.modified()) else { return fallback() };
    // The file can run to megabytes, so read it again only when it changes.
    if let Some((at, key, name)) = SEEN.lock().unwrap().get(&file)
        && *at == modified
    {
        return (key.clone(), name.clone());
    }
    let Some(v) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return fallback();
    };
    let a = &v["oauthAccount"];
    let (Some(user), Some(org)) = (a["accountUuid"].as_str(), a["organizationUuid"].as_str()) else { return fallback() };
    let (key, name) = (format!("{user}:{org}"), a["organizationName"].as_str().map(str::to_owned));
    SEEN.lock().unwrap().insert(file, (modified, key.clone(), name.clone()));
    (key, name)
}

/// Where a config directory's login lives.
enum Store {
    File(PathBuf),
    #[cfg(target_os = "macos")]
    Keychain { service: String, account: String },
}

impl Store {
    /// The credentials file when there is one; on macOS, otherwise Claude Code's Keychain item.
    fn locate(dir: &Path) -> Result<Store> {
        let file = dir.join(".credentials.json");
        #[cfg(target_os = "macos")]
        if !file.exists() {
            let account = std::env::var("USER").context("USER is not set")?;
            return Ok(Store::Keychain { service: keychain_service(dir, &home()?), account });
        }
        Ok(Store::File(file))
    }

    fn read(&self) -> Result<(Value, ClaudeAiOauth)> {
        match self {
            Store::File(path) => read_oauth(path),
            #[cfg(target_os = "macos")]
            Store::Keychain { service, account } => {
                let raw = keyring::Entry::new(service, account)?.get_password().with_context(|| {
                    format!("Claude Code login not found in the Keychain ({service}) — run `claude` and sign in")
                })?;
                parse_oauth(&raw, &format!("Keychain item {service}"))
            }
        }
    }

    fn write(&self, root: Value, access_token: &str, refresh_token: &str, expires_at_ms: i64) -> Result<()> {
        match self {
            Store::File(path) => write_oauth(path, root, access_token, refresh_token, expires_at_ms),
            #[cfg(target_os = "macos")]
            Store::Keychain { service, account } => {
                let root = set_oauth(root, access_token, refresh_token, expires_at_ms)?;
                keyring::Entry::new(service, account)?
                    .set_password(&serde_json::to_string(&root)?)
                    .with_context(|| format!("update Keychain item {service}"))
            }
        }
    }
}

/// Claude Code's Keychain service name for `dir`: plain for `~/.claude`, else suffixed with the
/// first 8 hex digits of the directory's SHA-256.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_service(dir: &Path, home: &Path) -> String {
    use sha2::{Digest, Sha256};
    if dir == home.join(".claude") {
        return "Claude Code-credentials".into();
    }
    let hash = hex::encode(Sha256::digest(dir.to_string_lossy().as_bytes()));
    format!("Claude Code-credentials-{}", &hash[..8])
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
    parse_oauth(&raw, &path.display().to_string())
}

/// `place` names where `raw` came from, for error messages.
fn parse_oauth(raw: &str, place: &str) -> Result<(Value, ClaudeAiOauth)> {
    let root: Value = serde_json::from_str(raw)
        .with_context(|| format!("invalid Claude credentials at {place}"))?;
    let oauth_val = root
        .get("claudeAiOauth")
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "no Claude Code login in {} — run `claude` and sign in",
                place
            )
        })?;
    let oauth: ClaudeAiOauth = serde_json::from_value(oauth_val).with_context(|| {
        format!(
            "Claude credentials at {} are missing access/refresh tokens",
            place
        )
    })?;
    if oauth.access_token.is_empty() || oauth.refresh_token.is_empty() {
        return Err(anyhow!(
            "Claude Code login at {} is incomplete — run `claude` and sign in",
            place
        ));
    }
    Ok((root, oauth))
}

/// `root` with its `claudeAiOauth` tokens replaced, sibling keys kept.
fn set_oauth(mut root: Value, access_token: &str, refresh_token: &str, expires_at_ms: i64) -> Result<Value> {
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
    Ok(root)
}

/// Atomically rewrite `path`, preserving sibling top-level JSON keys.
fn write_oauth(path: &Path, root: Value, access_token: &str, refresh_token: &str, expires_at_ms: i64) -> Result<()> {
    let root = set_oauth(root, access_token, refresh_token, expires_at_ms)?;
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

fn cache_get(dir: &Path) -> Option<String> {
    let guard = CACHE.read().ok()?;
    let c = guard.get(dir)?;
    if c.expires_at_ms - now_ms() > REFRESH_SKEW_MS {
        Some(c.token.clone())
    } else {
        None
    }
}

fn cache_set(dir: &Path, token: String, expires_at_ms: i64) {
    if let Ok(mut g) = CACHE.write() {
        g.insert(dir.to_owned(), Cached { token, expires_at_ms });
    }
}

/// Return a usable access token for the Claude login in config directory `dir` (`None`: the
/// default one), refreshing and writing rotated tokens back when needed.
pub async fn access_token(dir: Option<&str>) -> Result<String> {
    let dir = resolve_dir(dir)?;
    if let Some(t) = cache_get(&dir) {
        return Ok(t);
    }
    let _guard = REFRESH_LOCK.lock().await;
    // Another waiter may have refreshed while we queued.
    if let Some(t) = cache_get(&dir) {
        return Ok(t);
    }
    token_from(&dir, &Store::locate(&dir)?).await
}

async fn token_from(dir: &Path, store: &Store) -> Result<String> {
    let (root, oauth) = store.read()?;
    if oauth.expires_at - now_ms() > REFRESH_SKEW_MS {
        cache_set(dir, oauth.access_token.clone(), oauth.expires_at);
        return Ok(oauth.access_token);
    }

    let tokens = refresh_http(&oauth.refresh_token).await?;
    let expires_at_ms = now_ms() + tokens.expires_in.saturating_mul(1000);
    let refresh = tokens
        .refresh_token
        .as_deref()
        .unwrap_or(&oauth.refresh_token);
    store.write(root, &tokens.access_token, refresh, expires_at_ms)?;
    cache_set(dir, tokens.access_token.clone(), expires_at_ms);
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
            g.clear();
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
    fn keychain_service_matches_claude_code() {
        let home = Path::new("/Users/u");
        assert_eq!(keychain_service(&home.join(".claude"), home), "Claude Code-credentials");
        // sha256("/Users/dfrazer/.claudetvg") starts 986364fc: a real Claude Code item.
        assert_eq!(
            keychain_service(Path::new("/Users/dfrazer/.claudetvg"), home),
            "Claude Code-credentials-986364fc"
        );
    }

    #[test]
    fn account_comes_from_claude_json() {
        let dir = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(account(&dir), (dir.to_string_lossy().into_owned(), None), "no record: the path");
        let v = serde_json::json!({ "oauthAccount": { "accountUuid": "u1", "organizationUuid": "o1", "organizationName": "Team" } });
        std::fs::write(dir.join(".claude.json"), v.to_string()).unwrap();
        assert_eq!(account(&dir), ("u1:o1".into(), Some("Team".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_dir_expands_home() {
        let home = home().unwrap();
        assert_eq!(resolve_dir(Some(" ~/.claudetvg ")).unwrap(), home.join(".claudetvg"));
        assert_eq!(resolve_dir(Some("/x/y")).unwrap(), PathBuf::from("/x/y"));
        assert_eq!(resolve_dir(Some("")).unwrap(), config_dir().unwrap());
    }

    /// Two directories are two accounts: neither's cached token answers for the other.
    #[tokio::test]
    async fn caches_each_account_separately() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_cache();
        let base = std::env::temp_dir().join(format!("pb-claude-auth-{}", crate::db::new_id()));
        let (a, b) = (base.join("a"), base.join("b"));
        for (d, t) in [(&a, "token-a"), (&b, "token-b")] {
            std::fs::create_dir_all(d).unwrap();
            write_creds(&d.join(".credentials.json"), t, "r", now_ms() + 60 * 60 * 1000);
        }
        assert_eq!(access_token(a.to_str()).await.unwrap(), "token-a");
        assert_eq!(access_token(b.to_str()).await.unwrap(), "token-b");
        assert_eq!(access_token(a.to_str()).await.unwrap(), "token-a");
        clear_cache();
        let _ = std::fs::remove_dir_all(&base);
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
        let path = dir.join(".credentials.json");
        write_creds(&path, "fresh-token", "r", now_ms() + 60 * 60 * 1000);
        let t = access_token(dir.to_str()).await.unwrap();
        assert_eq!(t, "fresh-token");
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live check against Anthropic with the machine's Claude Code login.
    /// `cargo test --lib live_claude_login -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_claude_login_models_and_chat() {
        let token = access_token(None).await.expect("Claude Code login");
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
        let path = dir.join(".credentials.json");
        write_creds(&path, "stale", "old-refresh", now_ms() - 1000);

        // SAFETY: single-threaded under TEST_LOCK; restored below.
        unsafe {
            std::env::set_var(
                "PATCHBAY_CLAUDE_TOKEN_URL",
                format!("http://{addr}/v1/oauth/token"),
            );
        }
        let t = access_token(dir.to_str()).await.unwrap();
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
