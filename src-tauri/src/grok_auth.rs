//! Grok Build subscription OAuth for the Chat tab and Patchbay's agent.
//!
//! Reads the login that `grok login` keeps in `<grok home>/auth.json` (`$GROK_HOME`, else
//! `~/.grok`), refreshes the access token when it is near expiry, and writes the rotated tokens
//! back so `grok` stays signed in.
//!
//! Grok's refresh tokens rotate and the identity provider detects reuse: spending a rotated token
//! again revokes its successor, which signs `grok` out. So a refresh here follows the protocol
//! `grok` itself uses:
//!
//! 1. Take an exclusive lock on `auth.json.lock` (the same advisory lock `grok` takes; the file is
//!    never deleted).
//! 2. Read `auth.json` again under the lock. Another process may have rotated the token while this
//!    one waited; if it did, use that token and spend nothing.
//! 3. Spend the refresh token once. A failure is never retried.
//! 4. Write the new tokens (atomically, mode 0600) before releasing the lock, keeping every other
//!    key and every other login in the file.
//!
//! Each provider may name its own Grok home, so one machine can use several Grok accounts side by
//! side.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

/// The identity provider `grok login` signs in through.
pub const DEFAULT_ISSUER: &str = "https://auth.x.ai";
/// Grok Build's public OAuth client id (from the open-source Grok Build client). A login made
/// with another client keeps its own id in `auth.json`; that one is used for its refresh.
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// Refresh when fewer than this many ms remain until expiry.
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
/// Grok's advisory lock file, next to `auth.json`.
const LOCK_FILE_NAME: &str = "auth.json.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a token lives when the token endpoint doesn't say.
const DEFAULT_LIFETIME_SECS: i64 = 3600;

/// What a request needs from a login.
#[derive(Debug, Clone, PartialEq)]
pub struct Credentials {
    pub token: String,
    pub user_id: String,
    pub team_id: Option<String>,
}

struct Cached {
    creds: Credentials,
    expires_at_ms: i64,
}

/// Tokens by Grok home: one entry per Grok account in use.
static CACHE: RwLock<BTreeMap<PathBuf, Cached>> = RwLock::new(BTreeMap::new());
/// Token endpoints by issuer, from OIDC discovery.
static ENDPOINTS: RwLock<BTreeMap<String, String>> = RwLock::new(BTreeMap::new());
static REFRESH_LOCK: Mutex<()> = Mutex::const_new(());

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn expand_home(d: &str) -> Result<PathBuf> {
    if d == "~" {
        return crate::claude_auth::home();
    }
    match d.strip_prefix("~/") {
        Some(rest) => Ok(crate::claude_auth::home()?.join(rest)),
        None => Ok(PathBuf::from(d)),
    }
}

/// A provider's Grok home (`~/` expanded). With none named: `$GROK_HOME` when it is set and not
/// empty, else `~/.grok`.
pub fn resolve_dir(dir: Option<&str>) -> Result<PathBuf> {
    // Tests never read the shell's `GROK_HOME` (a login lives there) unless PB_TEST_REAL_HOME=1.
    #[cfg(test)]
    let env = crate::test_support::real_home_requested().then(|| std::env::var("GROK_HOME").ok()).flatten();
    #[cfg(not(test))]
    let env = std::env::var("GROK_HOME").ok();
    resolve_dir_with(dir, env)
}

fn resolve_dir_with(dir: Option<&str>, env_home: Option<String>) -> Result<PathBuf> {
    if let Some(d) = dir.map(str::trim).filter(|d| !d.is_empty()) {
        return expand_home(d);
    }
    if let Some(e) = env_home.map(|e| e.trim().to_owned()).filter(|e| !e.is_empty()) {
        return expand_home(&e);
    }
    Ok(crate::claude_auth::home()?.join(".grok"))
}

fn auth_path(home: &Path) -> PathBuf {
    home.join("auth.json")
}

/// One OIDC login out of `auth.json`.
#[derive(Debug, Clone)]
struct Login {
    /// The key the entry sits under in `auth.json` (`<issuer>::<client id>`).
    scope: String,
    access_token: String,
    refresh_token: String,
    expires_at_ms: Option<i64>,
    issuer: String,
    client_id: String,
    principal_type: Option<String>,
    principal_id: Option<String>,
    user_id: String,
    team_id: Option<String>,
    team_name: Option<String>,
}

impl Login {
    fn credentials(&self) -> Credentials {
        Credentials { token: self.access_token.clone(), user_id: self.user_id.clone(), team_id: self.team_id.clone() }
    }

    fn is_fresh(&self) -> bool {
        // No expiry on record counts as stale: one refresh, under the lock, is harmless.
        self.expires_at_ms.is_some_and(|e| e - now_ms() > REFRESH_SKEW_MS)
    }
}

fn str_field(e: &Map<String, Value>, k: &str) -> Option<String> {
    e.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// The OIDC login in `root` (`auth.json`'s parsed content). Other entries, such as an API key,
/// are not logins and are skipped. When several logins exist, the one made with Grok's own client
/// wins.
fn parse_login(root: &Value, place: &str) -> Result<Login> {
    let map = root.as_object().ok_or_else(|| anyhow!("invalid Grok credentials at {place}"))?;
    let preferred = format!("{DEFAULT_ISSUER}::{CLIENT_ID}");
    let mut found: Option<(&String, &Map<String, Value>)> = None;
    for (scope, v) in map {
        let Some(e) = v.as_object() else { continue };
        if e.get("auth_mode").and_then(Value::as_str) != Some("oidc") {
            continue;
        }
        if str_field(e, "key").is_none() || str_field(e, "refresh_token").is_none() {
            continue;
        }
        if found.is_none() || *scope == preferred {
            found = Some((scope, e));
        }
    }
    let (scope, e) = found.ok_or_else(|| anyhow!("no Grok login in {place} — run `grok login`"))?;
    Ok(Login {
        scope: scope.clone(),
        access_token: str_field(e, "key").unwrap_or_default(),
        refresh_token: str_field(e, "refresh_token").unwrap_or_default(),
        expires_at_ms: e
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis()),
        issuer: str_field(e, "oidc_issuer").unwrap_or_else(|| DEFAULT_ISSUER.to_owned()),
        client_id: str_field(e, "oidc_client_id").unwrap_or_else(|| CLIENT_ID.to_owned()),
        principal_type: str_field(e, "principal_type"),
        principal_id: str_field(e, "principal_id"),
        user_id: str_field(e, "user_id").unwrap_or_default(),
        team_id: str_field(e, "team_id"),
        team_name: str_field(e, "team_name"),
    })
}

fn read_login(path: &Path) -> Result<(Value, Login)> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Grok login not found at {} — run `grok login`", path.display()))?;
    let root: Value =
        serde_json::from_str(&raw).with_context(|| format!("invalid Grok credentials at {}", path.display()))?;
    let login = parse_login(&root, &path.display().to_string())?;
    Ok((root, login))
}

/// Check that `dir` (or the default home) holds a Grok login; the error says what to do.
pub fn check(dir: Option<&str>) -> Result<()> {
    let home = resolve_dir(dir)?;
    read_login(&auth_path(&home)).map(|_| ())
}

/// `root` with the login under `scope` given new tokens; every other key and entry is kept.
fn apply_refresh(mut root: Value, scope: &str, access_token: &str, refresh_token: &str, expires_at_ms: i64) -> Result<Value> {
    let expires = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(expires_at_ms)
        .ok_or_else(|| anyhow!("token expiry {expires_at_ms} is out of range"))?;
    let entry = root
        .get_mut(scope)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("the Grok login {scope} is no longer in auth.json"))?;
    entry.insert("key".into(), Value::String(access_token.to_owned()));
    entry.insert("refresh_token".into(), Value::String(refresh_token.to_owned()));
    entry.insert("expires_at".into(), Value::String(expires.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)));
    entry.insert(
        "create_time".into(),
        Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
    );
    Ok(root)
}

/// Atomically rewrite `path` (through a symlink, as dotfile setups use): a temp file in the same
/// directory, owner-only, synced, then renamed over it.
fn write_atomic(path: &Path, root: &Value) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = target.parent().ok_or_else(|| anyhow!("auth.json has no parent directory"))?;
    let tmp = parent.join(format!(".auth.json.patchbay.{}.tmp", std::process::id()));
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("write {}", tmp.display()))?;
    f.write_all(&serde_json::to_vec_pretty(root)?).with_context(|| format!("write {}", tmp.display()))?;
    f.sync_all().with_context(|| format!("sync {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, &target).with_context(|| format!("replace {}", target.display()))?;
    Ok(())
}

/// Holds `auth.json.lock` until dropped (closing the file releases the lock).
struct AuthLock {
    _file: std::fs::File,
}

fn lock_blocking(path: &Path, timeout: Duration) -> Result<AuthLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(AuthLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                if start.elapsed() >= timeout {
                    return Err(anyhow!(
                        "another process (probably grok) holds {} — try again in a moment",
                        path.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(anyhow::Error::new(e).context(format!("lock {}", path.display())));
            }
        }
    }
}

async fn acquire_lock(auth: &Path, timeout: Duration) -> Result<AuthLock> {
    let dir = auth.parent().ok_or_else(|| anyhow!("auth.json has no parent directory"))?;
    let path = dir.join(LOCK_FILE_NAME);
    tokio::task::spawn_blocking(move || lock_blocking(&path, timeout))
        .await
        .context("auth lock task")?
}

#[derive(Deserialize)]
struct Discovery {
    token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[cfg(test)]
fn endpoint_override() -> Option<String> {
    std::env::var("PATCHBAY_GROK_TOKEN_URL").ok().filter(|u| !u.is_empty())
}

#[cfg(not(test))]
fn endpoint_override() -> Option<String> {
    None
}

/// The issuer's token endpoint, from its OIDC discovery document (cached per issuer).
/// In tests `PATCHBAY_GROK_TOKEN_URL` overrides it; a release build never sends a refresh token
/// anywhere the login file's issuer didn't name.
async fn token_endpoint(issuer: &str) -> Result<String> {
    if let Some(u) = endpoint_override() {
        return Ok(u);
    }
    let issuer = issuer.trim_end_matches('/').to_owned();
    if let Some(u) = ENDPOINTS.read().ok().and_then(|g| g.get(&issuer).cloned()) {
        return Ok(u);
    }
    let url = format!("{issuer}/.well-known/openid-configuration");
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let resp = client.get(&url).send().await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("GET {url} returned {}", resp.status()));
    }
    let doc: Discovery = resp.json().await.context("parse Grok OIDC discovery")?;
    if let Ok(mut g) = ENDPOINTS.write() {
        g.insert(issuer, doc.token_endpoint.clone());
    }
    Ok(doc.token_endpoint)
}

/// Spend the login's refresh token once. Team logins also send their principal, as `grok` does.
async fn refresh_http(endpoint: &str, login: &Login) -> Result<TokenResponse> {
    let mut params: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", login.refresh_token.as_str()),
        ("client_id", login.client_id.as_str()),
    ];
    if let Some(t) = login.principal_type.as_deref() {
        params.push(("principal_type", t));
    }
    if let Some(p) = login.principal_id.as_deref() {
        params.push(("principal_id", p));
    }
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
    let resp = client
        .post(endpoint)
        .form(&params)
        .send()
        .await
        .with_context(|| format!("POST {endpoint}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let code = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned));
        if matches!(code.as_deref(), Some("invalid_grant") | Some("invalid_client")) {
            return Err(anyhow!("Grok login expired or was revoked — run `grok login` to sign in again"));
        }
        let clipped: String = body.chars().take(300).collect();
        return Err(anyhow!("Grok token refresh failed ({status}): {clipped}"));
    }
    resp.json::<TokenResponse>().await.context("parse Grok token refresh response")
}

fn cache_get(home: &Path) -> Option<Credentials> {
    let guard = CACHE.read().ok()?;
    let c = guard.get(home)?;
    if c.expires_at_ms - now_ms() > REFRESH_SKEW_MS { Some(c.creds.clone()) } else { None }
}

fn cache_set(home: &Path, creds: Credentials, expires_at_ms: i64) {
    if let Ok(mut g) = CACHE.write() {
        g.insert(home.to_owned(), Cached { creds, expires_at_ms });
    }
}

fn cache_clear(home: &Path) {
    if let Ok(mut g) = CACHE.write() {
        g.remove(home);
    }
}

/// The token `credentials` last fetched for the login in `dir`, while it is still fresh. For launch
/// code that can't wait on a refresh: call `credentials` first, then read this.
pub fn cached_token(dir: Option<&str>) -> Option<String> {
    let home = resolve_dir(dir).ok()?;
    cache_get(&home).map(|c| c.token)
}

/// A usable access token, with who it belongs to, for the Grok login in `dir` (`None`: the
/// default home), refreshed and written back when needed.
pub async fn credentials(dir: Option<&str>) -> Result<Credentials> {
    let home = resolve_dir(dir)?;
    if let Some(c) = cache_get(&home) {
        return Ok(c);
    }
    let _guard = REFRESH_LOCK.lock().await;
    // Another waiter may have refreshed while we queued.
    if let Some(c) = cache_get(&home) {
        return Ok(c);
    }
    token_from(&home, LOCK_TIMEOUT).await
}

async fn token_from(home: &Path, lock_timeout: Duration) -> Result<Credentials> {
    let path = auth_path(home);
    let (_, login) = read_login(&path)?;
    if login.is_fresh() {
        cache_set(home, login.credentials(), login.expires_at_ms.unwrap_or_default());
        return Ok(login.credentials());
    }

    let _lock = acquire_lock(&path, lock_timeout).await?;
    // `grok` may have refreshed while we waited for the lock: its tokens are the ones to use, and
    // the refresh token we read before is spent.
    let (root, login) = read_login(&path)?;
    if login.is_fresh() {
        cache_set(home, login.credentials(), login.expires_at_ms.unwrap_or_default());
        return Ok(login.credentials());
    }

    let endpoint = token_endpoint(&login.issuer).await?;
    let tokens = match refresh_http(&endpoint, &login).await {
        Ok(t) => t,
        Err(e) => {
            cache_clear(home);
            return Err(e);
        }
    };
    let expires_at_ms = now_ms() + tokens.expires_in.unwrap_or(DEFAULT_LIFETIME_SECS).saturating_mul(1000);
    // Keep the old refresh token when the provider didn't rotate it.
    let refresh = tokens.refresh_token.as_deref().unwrap_or(login.refresh_token.as_str());
    let root = apply_refresh(root, &login.scope, &tokens.access_token, refresh, expires_at_ms)?;
    let creds = Credentials { token: tokens.access_token, user_id: login.user_id, team_id: login.team_id };
    // The refresh token is spent: keep the new access token whatever happens to the file, so this
    // process keeps working until it expires. A failed write is loud, because the file still holds
    // the spent refresh token and the next refresh will be refused (`grok login` fixes it).
    cache_set(home, creds.clone(), expires_at_ms);
    if let Err(e) = write_atomic(&path, &root) {
        tracing::warn!("couldn't save the refreshed Grok login to {}: {e:#}", path.display());
    }
    Ok(creds)
}

/// True when `key` is a Grok-home path fallback, not the portable `grok:<team>:<user>` key.
/// Path keys are machine-local: the same login on another computer has a different path.
#[allow(dead_code)] // used by the plan gauge
pub fn is_path_account_key(key: &str) -> bool {
    !key.starts_with("grok:")
}

fn account_key(login: &Login) -> String {
    format!("grok:{}:{}", login.team_id.as_deref().unwrap_or("personal"), login.user_id)
}

/// The Grok account a home is signed in to, as a key that is the same on every computer
/// (`grok:<team id or "personal">:<user id>`; each seat has its own usage pool), and the team's
/// name. Falls back to the home's path when there is no readable login.
#[allow(dead_code)] // used by the plan gauge
pub fn account(home: &Path) -> (String, Option<String>) {
    match read_login(&auth_path(home)) {
        Ok((_, l)) => (account_key(&l), l.team_name.clone()),
        Err(_) => (home.to_string_lossy().into_owned(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::Mutex as StdMutex;

    /// Serialize tests that touch the process-wide cache or environment.
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// Forget the tokens of this module's temp homes (other modules' tests keep theirs).
    fn clear_cache() {
        CACHE.write().unwrap().retain(|home, _| !home.to_string_lossy().contains("pb-grok-auth-"));
    }

    fn rfc3339_in(ms: i64) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms() + ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }

    fn entry(access: &str, refresh: &str, expires_in_ms: i64) -> Value {
        serde_json::json!({
            "key": access,
            "auth_mode": "oidc",
            "create_time": "2026-11-09T21:42:15.209592739Z",
            "user_id": "user-1",
            "email": "a@example.com",
            "team_id": "team-1",
            "team_name": "Acme",
            "principal_type": "Team",
            "principal_id": "team-1",
            "can_administer_team": false,
            "refresh_token": refresh,
            "expires_at": rfc3339_in(expires_in_ms),
            "oidc_issuer": "https://auth.x.ai",
            "oidc_client_id": CLIENT_ID,
        })
    }

    fn scope() -> String {
        format!("{DEFAULT_ISSUER}::{CLIENT_ID}")
    }

    /// `auth.json`'s content with `entry` as the Grok login.
    fn login_root(entry: Value) -> Value {
        let mut m = Map::new();
        m.insert(scope(), entry);
        Value::Object(m)
    }

    fn write_login(dir: &Path, access: &str, refresh: &str, expires_in_ms: i64) {
        let mut root = login_root(entry(access, refresh, expires_in_ms));
        root["xai::api_key"] = serde_json::json!({ "key": "xai-secret", "auth_mode": "api_key" });
        std::fs::write(dir.join("auth.json"), serde_json::to_vec_pretty(&root).unwrap()).unwrap();
    }

    fn temp_home() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pb-grok-auth-{}", crate::db::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Read one whole HTTP request (the head, then as many body bytes as `Content-Length` says).
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&data).into_owned();
            let Some(head_end) = text.find("\r\n\r\n") else { continue };
            let length = text[..head_end]
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            if data.len() >= head_end + 4 + length {
                break;
            }
        }
        String::from_utf8_lossy(&data).into_owned()
    }

    /// One-shot token endpoint: answers the next request with `body`, and hands the request text
    /// back through the join handle.
    fn token_server(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::Write as _;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let req = read_request(&mut stream);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
            req
        });
        (format!("http://{addr}/oauth2/token"), handle)
    }

    #[test]
    fn the_home_is_the_provider_s_then_grok_home_then_dot_grok() {
        let _env = crate::claude_auth::ENV_LOCK.blocking_lock();
        let h = crate::claude_auth::home().unwrap();
        assert_eq!(resolve_dir_with(Some("/srv/grok"), Some("/env".into())).unwrap(), PathBuf::from("/srv/grok"));
        assert_eq!(resolve_dir_with(Some("~/alt"), None).unwrap(), h.join("alt"));
        assert_eq!(resolve_dir_with(Some("~"), None).unwrap(), h);
        assert_eq!(resolve_dir_with(None, Some("/env/grok".into())).unwrap(), PathBuf::from("/env/grok"));
        assert_eq!(resolve_dir_with(Some("  "), Some("/env/grok".into())).unwrap(), PathBuf::from("/env/grok"));
        // An empty (say, unexpanded) $GROK_HOME counts as unset.
        assert_eq!(resolve_dir_with(None, Some("".into())).unwrap(), h.join(".grok"));
        assert_eq!(resolve_dir_with(None, None).unwrap(), h.join(".grok"));
    }

    #[test]
    fn a_login_is_the_oidc_entry_not_the_api_key() {
        let mut root = login_root(entry("grok-access", "grok-refresh", 3_600_000));
        root["xai::api_key"] = serde_json::json!({ "key": "xai-secret", "auth_mode": "api_key" });
        root["https://other.example::abc"] = entry("other-access", "other-refresh", 3_600_000);
        let l = parse_login(&root, "test").unwrap();
        assert_eq!(l.access_token, "grok-access", "Grok's own client wins");
        assert_eq!(l.scope, scope());
        assert_eq!(l.user_id, "user-1");
        assert_eq!(l.team_id.as_deref(), Some("team-1"));
        assert_eq!(l.principal_type.as_deref(), Some("Team"));
        assert!(l.is_fresh());

        let only_key = serde_json::json!({ "xai::api_key": { "key": "k", "auth_mode": "api_key" } });
        let err = parse_login(&only_key, "p").unwrap_err().to_string();
        assert!(err.contains("run `grok login`"), "{err}");
    }

    #[test]
    fn a_token_near_expiry_or_with_no_expiry_is_stale() {
        let mut e = entry("a", "r", 60_000);
        let l = parse_login(&login_root(e.clone()), "t").unwrap();
        assert!(!l.is_fresh(), "a minute left is inside the skew");
        e.as_object_mut().unwrap().remove("expires_at");
        let l = parse_login(&login_root(e), "t").unwrap();
        assert!(!l.is_fresh());
    }

    /// Run by `test_support::tests::an_exported_grok_home_is_ignored` with a decoy GROK_HOME exported.
    #[test]
    fn grok_home_ignores_the_exported_variable() {
        if crate::test_support::real_home_requested() {
            return;
        }
        let _env = crate::claude_auth::ENV_LOCK.blocking_lock();
        assert_eq!(resolve_dir(None).unwrap(), crate::claude_auth::home().unwrap().join(".grok"));
    }

    #[test]
    fn the_account_key_names_the_team_and_the_user() {
        let l = parse_login(&login_root(entry("a", "r", 3_600_000)), "t").unwrap();
        assert_eq!(account_key(&l), "grok:team-1:user-1");
        let mut personal = entry("a", "r", 3_600_000);
        personal.as_object_mut().unwrap().remove("team_id");
        let l = parse_login(&login_root(personal), "t").unwrap();
        assert_eq!(account_key(&l), "grok:personal:user-1");
        assert!(!is_path_account_key("grok:team-1:user-1"));
        assert!(is_path_account_key("/home/x/.grok"));
        // Both seat forms pass the mesh sender gate; a home-path fallback does not.
        assert!(crate::is_portable_plan_account("grok:team-1:user-1"));
        assert!(crate::is_portable_plan_account(&account_key(&parse_login(&login_root(entry("a", "r", 3_600_000)), "t").unwrap())));
        assert!(crate::is_portable_plan_account("grok:personal:user-1"));
        assert!(!crate::is_portable_plan_account("/home/x/.grok"));
    }

    #[test]
    fn a_refresh_keeps_every_other_key_and_login() {
        let mut root = login_root(entry("old", "old-refresh", -1000));
        root["xai::api_key"] = serde_json::json!({ "key": "xai-secret", "auth_mode": "api_key" });
        root["another"] = serde_json::json!({ "keep": "me" });
        let out = apply_refresh(root, &scope(), "new-access", "new-refresh", now_ms() + 3_600_000).unwrap();
        assert_eq!(out[scope()]["key"], "new-access");
        assert_eq!(out[scope()]["refresh_token"], "new-refresh");
        assert_eq!(out[scope()]["user_id"], "user-1");
        assert_eq!(out[scope()]["oidc_client_id"], CLIENT_ID);
        assert_eq!(out["xai::api_key"]["key"], "xai-secret");
        assert_eq!(out["another"]["keep"], "me");
        let at = out[scope()]["expires_at"].as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(at).is_ok(), "{at}");
        assert!(apply_refresh(out, "missing", "a", "r", 1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_written_atomically_and_stays_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_home();
        write_login(&dir, "a", "r", 1000);
        let path = dir.join("auth.json");
        let (root, _) = read_login(&path).unwrap();
        write_atomic(&path, &root).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_auth_file_is_rewritten_in_place() {
        let dir = temp_home();
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        write_login(&real, "a", "r", 1000);
        let link_home = dir.join("home");
        std::fs::create_dir_all(&link_home).unwrap();
        std::os::unix::fs::symlink(real.join("auth.json"), link_home.join("auth.json")).unwrap();
        let (root, login) = read_login(&link_home.join("auth.json")).unwrap();
        let root = apply_refresh(root, &login.scope, "b", "s", now_ms() + 3_600_000).unwrap();
        write_atomic(&link_home.join("auth.json"), &root).unwrap();
        assert!(std::fs::symlink_metadata(link_home.join("auth.json")).unwrap().file_type().is_symlink());
        let (_, after) = read_login(&real.join("auth.json")).unwrap();
        assert_eq!(after.access_token, "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lock_is_exclusive_and_times_out() {
        let dir = temp_home();
        let path = dir.join(LOCK_FILE_NAME);
        let held = lock_blocking(&path, Duration::from_secs(1)).unwrap();
        let err = lock_blocking(&path, Duration::from_millis(150)).err().expect("second lock must wait").to_string();
        assert!(err.contains("holds"), "{err}");
        drop(held);
        assert!(lock_blocking(&path, Duration::from_secs(1)).is_ok(), "released on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_fresh_token_needs_no_request() {
        let _t = TEST_LOCK.lock().unwrap();
        clear_cache();
        let dir = temp_home();
        write_login(&dir, "fresh-access", "r", 3_600_000);
        let c = credentials(dir.to_str()).await.unwrap();
        assert_eq!(c.token, "fresh-access");
        assert_eq!(c.user_id, "user-1");
        assert_eq!(c.team_id.as_deref(), Some("team-1"));
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_expired_token_is_refreshed_and_written_back() {
        let _t = TEST_LOCK.lock().unwrap();
        clear_cache();
        let (url, server) = token_server(r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#);
        let dir = temp_home();
        write_login(&dir, "stale", "old-refresh", -1000);
        // SAFETY: single-threaded under TEST_LOCK; restored below.
        unsafe { std::env::set_var("PATCHBAY_GROK_TOKEN_URL", &url) };
        let got = credentials(dir.to_str()).await;
        unsafe { std::env::remove_var("PATCHBAY_GROK_TOKEN_URL") };
        assert_eq!(got.unwrap().token, "rotated-access");
        let req = server.join().unwrap();
        assert!(req.contains("grant_type=refresh_token"), "{req}");
        assert!(req.contains("refresh_token=old-refresh"), "{req}");
        assert!(req.contains(CLIENT_ID), "{req}");
        assert!(req.contains("principal_type=Team"), "the team principal is sent: {req}");
        let (root, after) = read_login(&dir.join("auth.json")).unwrap();
        assert_eq!(after.access_token, "rotated-access");
        assert_eq!(after.refresh_token, "rotated-refresh");
        assert!(after.is_fresh());
        assert_eq!(root["xai::api_key"]["key"], "xai-secret", "other entries survive");
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_old_refresh_token_stays_when_none_is_rotated() {
        let _t = TEST_LOCK.lock().unwrap();
        clear_cache();
        let (url, server) = token_server(r#"{"access_token":"new-access","expires_in":3600}"#);
        let dir = temp_home();
        write_login(&dir, "stale", "keep-refresh", -1000);
        unsafe { std::env::set_var("PATCHBAY_GROK_TOKEN_URL", &url) };
        let got = credentials(dir.to_str()).await;
        unsafe { std::env::remove_var("PATCHBAY_GROK_TOKEN_URL") };
        got.unwrap();
        server.join().unwrap();
        let (_, after) = read_login(&dir.join("auth.json")).unwrap();
        assert_eq!(after.access_token, "new-access");
        assert_eq!(after.refresh_token, "keep-refresh");
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `grok` rotates the token while we wait for the lock: its tokens are used and the refresh
    /// token is not spent (no token endpoint is reachable here, so a refresh would fail).
    #[tokio::test]
    async fn a_token_rotated_by_grok_while_we_wait_is_adopted() {
        let _t = TEST_LOCK.lock().unwrap();
        clear_cache();
        let dir = temp_home();
        write_login(&dir, "stale", "spent-refresh", -1000);
        let lock = lock_blocking(&dir.join(LOCK_FILE_NAME), Duration::from_secs(1)).unwrap();
        let writer_dir = dir.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            write_login(&writer_dir, "grok-rotated", "grok-refresh", 3_600_000);
            drop(lock);
        });
        let c = token_from(&dir, Duration::from_secs(5)).await.unwrap();
        writer.join().unwrap();
        assert_eq!(c.token, "grok-rotated");
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_revoked_login_says_to_sign_in_again() {
        let _t = TEST_LOCK.lock().unwrap();
        clear_cache();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::Write as _;
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request(&mut stream);
            let body = r#"{"error":"invalid_grant"}"#;
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });
        let dir = temp_home();
        write_login(&dir, "stale", "revoked", -1000);
        unsafe { std::env::set_var("PATCHBAY_GROK_TOKEN_URL", format!("http://{addr}/oauth2/token")) };
        let err = credentials(dir.to_str()).await.unwrap_err().to_string();
        unsafe { std::env::remove_var("PATCHBAY_GROK_TOKEN_URL") };
        server.join().unwrap();
        assert!(err.contains("run `grok login`"), "{err}");
        let (_, after) = read_login(&dir.join("auth.json")).unwrap();
        assert_eq!(after.refresh_token, "revoked", "a failed refresh changes nothing on disk");
        clear_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn two_homes_are_two_accounts() {
        let _t = TEST_LOCK.lock().unwrap();
        clear_cache();
        let (a, b) = (temp_home(), temp_home());
        write_login(&a, "token-a", "r", 3_600_000);
        write_login(&b, "token-b", "r", 3_600_000);
        assert_eq!(credentials(a.to_str()).await.unwrap().token, "token-a");
        assert_eq!(credentials(b.to_str()).await.unwrap().token, "token-b");
        assert_eq!(credentials(a.to_str()).await.unwrap().token, "token-a");
        clear_cache();
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn check_names_the_fix_when_there_is_no_login() {
        let dir = temp_home();
        let err = check(dir.to_str()).unwrap_err().to_string();
        assert!(err.contains("run `grok login`"), "{err}");
        write_login(&dir, "a", "r", 1000);
        assert!(check(dir.to_str()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live check against xAI with the machine's Grok login.
    /// `cargo test --lib live_grok_login -- --ignored --nocapture`
    #[tokio::test]
    // Manual test. If it needs your real home or logins, run it with PB_TEST_REAL_HOME=1 (see test_support).
    #[ignore]
    async fn live_grok_login_models() {
        let c = credentials(None).await.expect("Grok login");
        let client = reqwest::Client::new();
        let resp = client
            .get("https://cli-chat-proxy.grok.com/v1/models")
            .bearer_auth(&c.token)
            .header("X-XAI-Token-Auth", "xai-grok-cli")
            .header("x-grok-client-version", "1.0.60")
            .header("x-grok-client-identifier", "grok-shell")
            .header("x-userid", &c.user_id)
            .send()
            .await
            .expect("models request");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "models {status}: {body}");
        assert!(body.contains("grok-"), "{body}");
        eprintln!("models ok ({status})");
    }
}
