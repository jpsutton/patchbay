//! Shared Antigravity / Gemini OAuth credentials for providers and plan polling.
//!
//! Credential sources remain disk-first: the agy token file, Antigravity's OAuth credential
//! file, then (only for the default Gemini home) the macOS Keychain. Blocking file and Keychain
//! access always runs on Tokio's blocking pool. OAuth client credentials are discovered from
//! the installed agy / Antigravity language-server binary; none are embedded in Patchbay.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use base64::Engine;
use serde_json::Value;

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const KEYCHAIN_SERVICE: &str = "gemini";
const KEYCHAIN_ACCOUNT: &str = "antigravity";
const KEYRING_B64_PREFIX: &str = "go-keyring-base64:";
const AUTH_CACHE_SECS: i64 = 600;
const AGY_VERSION_FALLBACK: &str = "1.4.12";

#[derive(Clone)]
pub struct AntigravityAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub access_expires_at: Option<i64>,
    pub account_id: String,
    pub email: Option<String>,
    pub client_version: String,
}

impl std::fmt::Debug for AntigravityAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AntigravityAuth")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "[REDACTED]"))
            .field("access_expires_at", &self.access_expires_at)
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("client_version", &self.client_version)
            .finish()
    }
}

impl AntigravityAuth {
    pub fn user_agent(&self) -> String {
        format!("antigravity/cli/{}", self.client_version)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthSource {
    CliTokenFile,
    OauthCreds,
    Keychain,
}

const AUTH_SOURCE_ORDER: [AuthSource; 3] = [AuthSource::CliTokenFile, AuthSource::OauthCreds, AuthSource::Keychain];

#[derive(Clone)]
struct CachedAuth {
    auth: AntigravityAuth,
    resolved_at: i64,
}

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    expires_at: i64,
}

#[derive(Default)]
struct KeychainFallback {
    in_flight: bool,
    failures: u32,
    next_attempt_at: i64,
}

#[derive(Default)]
struct CacheState {
    auth: Option<CachedAuth>,
    refreshed: Option<CachedToken>,
    keychain: KeychainFallback,
}

#[derive(Default)]
struct AuthSlot {
    /// One resolver/refresh operation at a time for this Gemini home.
    serial: tokio::sync::Mutex<()>,
    state: Mutex<CacheState>,
}

fn slots() -> &'static Mutex<HashMap<PathBuf, Arc<AuthSlot>>> {
    static SLOTS: OnceLock<Mutex<HashMap<PathBuf, Arc<AuthSlot>>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn slot(home: &Path) -> Arc<AuthSlot> {
    let mut slots = slots().lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(slots.entry(home.to_path_buf()).or_default())
}

pub fn resolve_dir(dir: Option<&str>) -> Result<PathBuf> {
    #[cfg(test)]
    let env = crate::test_support::real_home_requested().then(|| std::env::var("GEMINI_HOME").ok()).flatten();
    #[cfg(not(test))]
    let env = std::env::var("GEMINI_HOME").ok();
    resolve_dir_with(dir, env)
}

fn resolve_dir_with(dir: Option<&str>, env_home: Option<String>) -> Result<PathBuf> {
    if let Some(path) = dir.map(str::trim).filter(|s| !s.is_empty()) {
        return expand_home(path);
    }
    if let Some(path) = env_home.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) {
        return expand_home(&path);
    }
    Ok(crate::claude_auth::home()?.join(".gemini"))
}

fn expand_home(path: &str) -> Result<PathBuf> {
    if path == "~" {
        return crate::claude_auth::home();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(crate::claude_auth::home()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

/// Load and refresh one login. Provider requests and the plan poller share this per-home cache.
pub async fn credentials(gemini_home: Option<&str>) -> Result<AntigravityAuth> {
    let home = resolve_dir(gemini_home)?;
    load_resolved(&home)
        .await?
        .ok_or_else(|| anyhow!("Antigravity login not found in the selected Gemini home; sign in with agy or Antigravity IDE"))
}

/// Validate that the provider's selected home contains a reusable login.
pub async fn check(gemini_home: Option<&str>) -> Result<()> {
    credentials(gemini_home).await.map(|_| ())
}

/// Optional form used by plan polling: no login is a normal state.
pub async fn load_auth(gemini_home: Option<&str>) -> Option<AntigravityAuth> {
    let home = resolve_dir(gemini_home).ok()?;
    load_resolved(&home).await.ok().flatten()
}

async fn load_resolved(home: &Path) -> Result<Option<AntigravityAuth>> {
    let slot = slot(home);
    let _serial = slot.serial.lock().await;
    let now = now();

    let cached = {
        let state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .auth
            .as_ref()
            .filter(|cached| now.saturating_sub(cached.resolved_at) < AUTH_CACHE_SECS)
            .map(|cached| cached.auth.clone())
    };
    let mut auth = match cached {
        Some(auth) => auth,
        None => {
            let Some(auth) = resolve_auth(home, &slot).await? else { return Ok(None) };
            slot.state.lock().unwrap_or_else(|e| e.into_inner()).auth = Some(CachedAuth { auth: auth.clone(), resolved_at: now });
            auth
        }
    };

    let cached_access = {
        let state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        state.refreshed.as_ref().and_then(|token| (token.expires_at > now + 60).then(|| token.access_token.clone()))
    };
    if let Some(access) = cached_access {
        auth.access_token = access;
        auth.access_expires_at = None;
        return Ok(Some(auth));
    }
    if auth.access_expires_at.is_some_and(|expiry| expiry > now + 60) {
        return Ok(Some(auth));
    }
    let Some(refresh) = auth.refresh_token.clone() else {
        return Ok(Some(auth));
    };
    match refresh_access_token(home.to_path_buf(), refresh).await {
        Ok((access_token, expires_in)) => {
            let expires_at = now + expires_in.max(0);
            slot.state.lock().unwrap_or_else(|e| e.into_inner()).refreshed = Some(CachedToken {
                access_token: access_token.clone(),
                expires_at,
            });
            auth.access_token = access_token;
            auth.access_expires_at = Some(expires_at);
            Ok(Some(auth))
        }
        Err(error) => {
            tracing::debug!("antigravity auth: token refresh failed: {error:#}");
            Ok((!auth.access_token.is_empty()).then_some(auth))
        }
    }
}

async fn resolve_auth(home: &Path, slot: &AuthSlot) -> Result<Option<AntigravityAuth>> {
    let home_for_read = home.to_path_buf();
    let disk = tokio::task::spawn_blocking(move || read_disk_auth(&home_for_read)).await?;
    if disk.is_some() {
        return Ok(disk);
    }
    if !should_use_keychain(home, &resolve_dir(None).unwrap_or_default()) {
        return Ok(None);
    }
    let now = now();
    {
        let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
        if !begin_keychain_attempt(&mut state.keychain, now) {
            return Ok(None);
        }
    }
    let read = tokio::task::spawn_blocking(read_keychain_auth).await?;
    let mut state = slot.state.lock().unwrap_or_else(|e| e.into_inner());
    match read {
        Some(auth) => {
            finish_keychain_attempt(&mut state.keychain, now, true);
            Ok(Some(auth))
        }
        None => {
            finish_keychain_attempt(&mut state.keychain, now, false);
            Ok(None)
        }
    }
}

fn should_use_keychain(home: &Path, default_home: &Path) -> bool {
    home == default_home
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn keychain_backoff_secs(failures: u32) -> i64 {
    match failures {
        0 => 0,
        1 => 300,
        2 => 1_800,
        3 => 21_600,
        _ => 86_400,
    }
}

fn begin_keychain_attempt(keychain: &mut KeychainFallback, now: i64) -> bool {
    if keychain.in_flight || now < keychain.next_attempt_at { return false; }
    keychain.in_flight = true;
    true
}

fn finish_keychain_attempt(keychain: &mut KeychainFallback, now: i64, succeeded: bool) {
    keychain.in_flight = false;
    if succeeded {
        keychain.failures = 0;
        keychain.next_attempt_at = 0;
    } else {
        keychain.failures = keychain.failures.saturating_add(1);
        keychain.next_attempt_at = now + keychain_backoff_secs(keychain.failures);
    }
}

fn read_disk_auth(home: &Path) -> Option<AntigravityAuth> {
    select_auth(&AUTH_SOURCE_ORDER[..2], |source| read_auth_source(source, home))
}

fn select_auth(order: &[AuthSource], mut read: impl FnMut(AuthSource) -> Option<AntigravityAuth>) -> Option<AntigravityAuth> {
    order.iter().find_map(|source| read(*source))
}

fn read_auth_source(source: AuthSource, home: &Path) -> Option<AntigravityAuth> {
    match source {
        AuthSource::CliTokenFile => read_cli_token_file(home),
        AuthSource::OauthCreds => read_oauth_creds(home),
        AuthSource::Keychain => read_keychain_auth(),
    }
}

/// This can raise a modal SecurityAgent dialog. Only `resolve_auth` calls it from spawn_blocking,
/// and only after both disk sources miss, under the per-home single-flight/backoff gate.
fn read_keychain_auth() -> Option<AntigravityAuth> {
    let raw = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).ok()?.get_password().ok()?;
    let value = if let Some(encoded) = raw.strip_prefix(KEYRING_B64_PREFIX) {
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
        serde_json::from_slice::<Value>(&bytes).ok()?
    } else {
        serde_json::from_str::<Value>(&raw).ok()?
    };
    let mut auth = auth_from_wrapped_token(&value, value.get("id_token_email").and_then(Value::as_str).map(str::to_owned), None)?;
    auth.client_version = AGY_VERSION_FALLBACK.to_owned();
    Some(auth)
}

fn read_cli_token_file(home: &Path) -> Option<AntigravityAuth> {
    let value = read_json(&home.join("antigravity-cli/antigravity-oauth-token"))?;
    let email = value
        .get("id_token_email")
        .or_else(|| value.get("email"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| email_from_id_token(value.get("id_token").and_then(Value::as_str)));
    auth_from_wrapped_token(&value, email, Some(home))
}

fn read_oauth_creds(home: &Path) -> Option<AntigravityAuth> {
    let value = read_json(&home.join("oauth_creds.json"))?;
    let access = value.get("access_token")?.as_str()?.trim();
    if access.is_empty() {
        return None;
    }
    let refresh = value.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let expiry = expiry_from_number(value.get("expiry_date"));
    let email = email_from_id_token(value.get("id_token").and_then(Value::as_str)).or_else(|| active_google_email(home));
    let account_id = email.clone().unwrap_or_else(|| token_id(access));
    Some(AntigravityAuth {
        access_token: access.to_owned(),
        refresh_token: refresh,
        access_expires_at: expiry,
        account_id,
        email,
        client_version: read_agy_version(home).unwrap_or_else(|| AGY_VERSION_FALLBACK.to_owned()),
    })
}

/// A login's stand-in identity when no email is known: a short hash of the token, never any of its
/// text (an id reaches plan reports and the mesh, and slicing token bytes can panic mid-character).
fn token_id(access: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("token:{}", &hex::encode(Sha256::digest(access.as_bytes()))[..12])
}

fn auth_from_wrapped_token(value: &Value, email_hint: Option<String>, home: Option<&Path>) -> Option<AntigravityAuth> {
    let token = value.get("token").unwrap_or(value);
    let access = token.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_owned();
    let refresh = token.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let expiry = token
        .get("expiry")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .or_else(|| expiry_from_number(token.get("expiry_date")));
    let email = email_hint
        .or_else(|| email_from_id_token(value.get("id_token").and_then(Value::as_str)))
        .or_else(|| home.and_then(active_google_email));
    let account_id = email.clone().unwrap_or_else(|| token_id(&access));
    Some(AntigravityAuth {
        access_token: access,
        refresh_token: refresh,
        access_expires_at: expiry,
        account_id,
        email,
        client_version: home
            .and_then(read_agy_version)
            .unwrap_or_else(|| AGY_VERSION_FALLBACK.to_owned()),
    })
}

fn expiry_from_number(value: Option<&Value>) -> Option<i64> {
    value.and_then(|n| n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)))
        .map(|ms| if ms > 10_000_000_000 { ms / 1000 } else { ms })
}

fn active_google_email(home: &Path) -> Option<String> {
    read_json(&home.join("google_accounts.json"))?
        .get("active")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn email_from_id_token(token: Option<&str>) -> Option<String> {
    jwt_claims(token?)?.get("email")?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let padded = match payload.len() % 4 {
        0 => payload.to_owned(),
        2 => format!("{payload}=="),
        3 => format!("{payload}="),
        _ => return None,
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(padded.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&padded))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn parse_rfc3339(value: &str) -> Option<i64> {
    let value = value.strip_suffix('Z').unwrap_or(value);
    let (date, time) = value.split_once('T')?;
    let mut date = date.split('-');
    let (y, m, d) = (date.next()?.parse::<i64>().ok()?, date.next()?.parse::<i64>().ok()?, date.next()?.parse::<i64>().ok()?);
    let time = time.split(['+', '-']).next().unwrap_or(time);
    let mut time = time.split(':');
    let (h, min) = (time.next()?.parse::<i64>().ok()?, time.next()?.parse::<i64>().ok()?);
    let sec = time.next()?.split('.').next()?.parse::<i64>().ok()?;
    Some(days_from_civil(y, m, d)? * 86_400 + h * 3_600 + min * 60 + sec)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) { return None; }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_agy_version(home: &Path) -> Option<String> {
    for path in [home.join("bin/agy.version.json"), home.join("antigravity-cli/agy.version.json")] {
        if let Some(value) = read_json(&path)
            && let Some(version) = value.get("version").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
        { return Some(version.to_owned()); }
    }
    None
}

struct OauthClient { client_id: String, client_secret: String }

fn oauth_clients(home: &Path) -> Result<Vec<OauthClient>> {
    let clients = discover_oauth_clients_from_install(home);
    if clients.is_empty() {
        return Err(anyhow!("Antigravity OAuth client not found in a local install (agy / language_server)"));
    }
    Ok(clients)
}

fn discover_oauth_clients_from_install(home: &Path) -> Vec<OauthClient> {
    for path in antigravity_binary_candidates(home) {
        let clients = scan_binary_for_oauth_clients(&path);
        if !clients.is_empty() { return clients; }
    }
    Vec::new()
}

fn antigravity_binary_candidates(home: &Path) -> Vec<PathBuf> {
    let mut paths = vec![home.join("bin/agy"), home.join("antigravity/bin/agy"), home.join("antigravity-cli/bin/agy")];
    if let Some(path) = crate::code::agents::which("agy") { paths.push(path); }
    #[cfg(target_os = "macos")]
    paths.extend([
        PathBuf::from("/Applications/Antigravity.app/Contents/Resources/bin/language_server"),
        PathBuf::from("/Applications/Antigravity IDE.app/Contents/Resources/app/extensions/antigravity/bin/language_server"),
    ]);
    paths
}

fn scan_binary_for_oauth_clients(path: &Path) -> Vec<OauthClient> {
    let Ok(data) = std::fs::read(path) else { return Vec::new() };
    let ids = find_google_client_ids(&data);
    let secrets = find_gocspx_secrets(&data);
    ids.iter()
        .flat_map(|id| secrets.iter().map(move |secret| OauthClient { client_id: id.clone(), client_secret: secret.clone() }))
        .collect()
}

fn find_google_client_ids(data: &[u8]) -> Vec<String> {
    let needle = b".apps.googleusercontent.com";
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = data[i..].windows(needle.len()).position(|w| w == needle) {
        let end = i + rel + needle.len();
        let mut start = i + rel;
        while start > 0 && (data[start - 1].is_ascii_digit() || data[start - 1].is_ascii_lowercase() || data[start - 1] == b'-') { start -= 1; }
        let slice = &data[start..end];
        let first = slice.iter().position(u8::is_ascii_digit).unwrap_or(slice.len());
        let id = String::from_utf8_lossy(&slice[first..]).into_owned();
        if id.contains('-') && id.starts_with(|c: char| c.is_ascii_digit()) && !out.contains(&id) { out.push(id); }
        i = end;
    }
    out
}

fn find_gocspx_secrets(data: &[u8]) -> Vec<String> {
    let needle = b"GOCSPX-";
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = data[i..].windows(needle.len()).position(|w| w == needle) {
        let start = i + rel;
        let rest = &data[start..];
        let end = rest.iter().position(|b| !(b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')).unwrap_or(rest.len());
        let mut secret = String::from_utf8_lossy(&rest[..end]).into_owned();
        for suffix in ["decoding", "https", "field"] {
            if let Some(index) = secret.find(suffix) { if index >= 16 { secret.truncate(index); break; } }
        }
        if secret.len() >= 20 && secret.starts_with("GOCSPX-") && !out.contains(&secret) { out.push(secret); }
        i = start + needle.len();
    }
    out
}

async fn refresh_access_token(home: PathBuf, refresh_token: String) -> Result<(String, i64)> {
    let clients = tokio::task::spawn_blocking(move || oauth_clients(&home)).await??;
    refresh_at(TOKEN_URL, &clients, &refresh_token).await
}

/// The refresh POST against `url` (a test points it at a loopback server).
async fn refresh_at(url: &str, clients: &[OauthClient], refresh_token: &str) -> Result<(String, i64)> {
    // No redirects: a 3xx would send the refresh token and client secret wherever the server points.
    let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).redirect(reqwest::redirect::Policy::none()).build()?;
    let mut last_status = None;
    for client in clients {
        let response = http
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("client_id", client.client_id.as_str()),
                ("client_secret", client.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let status = response.status();
        if status.is_redirection() {
            return Err(anyhow!("Google token refresh answered a redirect (HTTP {status}); Patchbay does not follow redirects with a login token"));
        }
        let body: Value = response.json().await?;
        if !status.is_success() { last_status = Some(status); continue; }
        let access = body.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("token refresh response missing access_token"))?;
        let expires_in = body.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
        return Ok((access.to_owned(), expires_in));
    }
    Err(anyhow!("Google token refresh HTTP {}", last_status.map(|s| s.to_string()).unwrap_or_else(|| "failed".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_auth(id: &str) -> AntigravityAuth {
        AntigravityAuth {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            access_expires_at: None,
            account_id: id.into(),
            email: Some(id.into()),
            client_version: AGY_VERSION_FALLBACK.into(),
        }
    }

    /// No email: the account id is a hash, never token text, and a non-ASCII token (a byte-16 slice
    /// would split a character and panic) is fine.
    /// The refresh call carries the refresh token and the client secret: it must not follow a 302/307.
    #[tokio::test]
    async fn the_refresh_call_does_not_follow_a_redirect() {
        use std::sync::atomic::Ordering;
        let clients = [OauthClient { client_id: "ID".into(), client_secret: "CSECRET".into() }];
        for status in [302u16, 307] {
            let (base, first, second) = crate::test_support::redirect_trap(status).await;
            let e = refresh_at(&base, &clients, "RTOKEN").await.unwrap_err().to_string();
            assert!(e.contains("redirect") && !e.contains("RTOKEN") && !e.contains("CSECRET"), "{status}: {e}");
            assert_eq!((first.load(Ordering::SeqCst), second.load(Ordering::SeqCst)), (1, 0), "{status}: followed");
        }
    }

    #[test]
    fn the_fallback_account_id_is_a_hash_never_token_text() {
        let token = format!("a{}SECRETTAIL", "é".repeat(10));
        let a = auth_from_wrapped_token(&serde_json::json!({ "access_token": token }), None, None).unwrap();
        assert!(a.account_id.starts_with("token:") && a.account_id.len() == "token:".len() + 12, "{}", a.account_id);
        assert!(a.account_id["token:".len()..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!a.account_id.contains('é') && !a.account_id.contains("SECRET"));
        assert_eq!(a.account_id, token_id(&token), "stable per token");
        assert_ne!(a.account_id, token_id("another-token"));
    }

    #[test]
    fn credential_sources_are_disk_first_and_alternate_homes_never_use_keychain() {
        assert_eq!(AUTH_SOURCE_ORDER, [AuthSource::CliTokenFile, AuthSource::OauthCreds, AuthSource::Keychain]);
        let mut seen = Vec::new();
        let selected = select_auth(&AUTH_SOURCE_ORDER[..2], |source| {
            seen.push(source);
            (source == AuthSource::OauthCreds).then(|| fake_auth("disk@example.test"))
        });
        assert_eq!(selected.unwrap().account_id, "disk@example.test");
        assert_eq!(seen, vec![AuthSource::CliTokenFile, AuthSource::OauthCreds]);
        let mut fallback_seen = Vec::new();
        let _ = select_auth(&AUTH_SOURCE_ORDER, |source| {
            fallback_seen.push(source);
            (source == AuthSource::Keychain).then(|| fake_auth("keychain@example.test"))
        });
        assert_eq!(fallback_seen, AUTH_SOURCE_ORDER);
        assert!(select_auth(&AUTH_SOURCE_ORDER[..2], |_| None).is_none());
        let default = PathBuf::from("/tmp/gemini-default");
        assert!(should_use_keychain(&default, &default));
        assert!(!should_use_keychain(Path::new("/tmp/gemini-other"), &default));
    }

    #[test]
    fn keychain_backoff_is_exponential_and_never_reprompts_each_poll() {
        assert!(keychain_backoff_secs(1) > 30);
        assert!(keychain_backoff_secs(2) > keychain_backoff_secs(1));
        assert!(keychain_backoff_secs(3) > keychain_backoff_secs(2));
        assert!(keychain_backoff_secs(4) > keychain_backoff_secs(3));
        let mut keychain = KeychainFallback::default();
        assert!(begin_keychain_attempt(&mut keychain, 1000));
        assert!(!begin_keychain_attempt(&mut keychain, 1001), "only one keychain attempt may run at once");
        finish_keychain_attempt(&mut keychain, 1000, false);
        assert!(!begin_keychain_attempt(&mut keychain, 1000 + keychain_backoff_secs(1) - 1));
        assert!(begin_keychain_attempt(&mut keychain, 1000 + keychain_backoff_secs(1)));
        finish_keychain_attempt(&mut keychain, 1000 + keychain_backoff_secs(1), true);
        assert_eq!(keychain.failures, 0, "success clears the backoff");
    }

    #[test]
    fn oauth_client_ids_and_secrets_are_discovered_from_binary_bytes() {
        let id = b"x999999999999-example.apps.googleusercontent.com\0";
        let secret = b"GOCSPX-EXAMPLE-TEST-SECRET-0000decoding";
        let mut bytes = id.to_vec();
        bytes.extend_from_slice(secret);
        assert_eq!(find_google_client_ids(&bytes), vec!["999999999999-example.apps.googleusercontent.com"]);
        assert_eq!(find_gocspx_secrets(&bytes), vec!["GOCSPX-EXAMPLE-TEST-SECRET-0000"]);
    }

    #[test]
    fn provider_home_precedes_environment_and_default_is_gemini_dir() {
        let explicit = resolve_dir_with(Some("~/other"), Some("~/env".into())).unwrap();
        assert!(explicit.ends_with("other"));
        assert_eq!(resolve_dir_with(None, Some("~/env".into())).unwrap().file_name().unwrap(), "env");
        assert_eq!(resolve_dir_with(None, None).unwrap().file_name().unwrap(), ".gemini");
    }

    #[test]
    fn auth_identity_uses_email_or_short_token_fingerprint() {
        let with_email = auth_from_wrapped_token(
            &serde_json::json!({"access_token":"fixture-access-token-long-enough","id_token":"e30.eyJlbWFpbCI6Im1lQGV4YW1wbGUudGVzdCJ9.sig"}),
            None,
            None,
        ).unwrap();
        assert_eq!(with_email.account_id, "me@example.test");
        let fingerprint = auth_from_wrapped_token(&serde_json::json!({"access_token":"fixture-access-token-long-enough"}), None, None).unwrap();
        assert!(fingerprint.account_id.starts_with("token:") && !fingerprint.account_id.contains("fixture"), "a hash, not token text");
        let shown = format!("{fingerprint:?}");
        assert!(shown.contains("[REDACTED]"));
        assert!(!shown.contains("fixture-access-token-long-enough"));
    }
}
