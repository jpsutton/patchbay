//! Antigravity / Gemini plan quota, polled from Google's Cloud Code quota API (not over ACP).
//!
//! Antigravity's ACP path does not emit plan windows the way Claude's `_claude/rateLimit` does.
//! The CLI `/usage` and IDE quota UI call `POST …/v1internal:retrieveUserQuotaSummary` on
//! `daily-cloudcode-pa.googleapis.com` with the signed-in Google OAuth token. Patchbay polls the
//! same endpoint while an Antigravity session is running or the plan blade is open.
//!
//! Auth comes from Antigravity's own login on disk / keychain (`~/.gemini/…`). Token refresh uses
//! Antigravity's public installed-app OAuth client (RFC 8252): the client id/secret are read from
//! a local Antigravity CLI/IDE binary (they are not a confidentiality boundary; authorization is
//! the user's refresh token already on the machine). They are never embedded in this repo. Never
//! log access or refresh tokens.
//!
//! `gemini-api-key` auth has no OAuth quota: the poller records an `unavailable` report instead.
//! Reports fan out over the mesh tagged with the account id so peers on the same Google login can
//! skip re-polling.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long a report stays fresh enough that this node (or a peer on the same account) need not poll.
pub const FRESH_SECS: i64 = 270;
/// How often the background loop considers whether to poll.
pub const TICK: Duration = Duration::from_secs(30);

const QUOTA_URL: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const KEYCHAIN_SERVICE: &str = "gemini";
const KEYCHAIN_ACCOUNT: &str = "antigravity";
const KEYRING_B64_PREFIX: &str = "go-keyring-base64:";

/// One quota bucket from `retrieveUserQuotaSummary` (`gemini-5h`, `3p-weekly`, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityPlanBucket {
    pub bucket_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
    /// Fraction of the bucket still remaining (0–1). Used percent is `1 - remaining_fraction`.
    pub remaining_fraction: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
}

impl AntigravityPlanBucket {
    /// Used percent 0–100 for gauge bars (same direction as Claude / Cursor).
    pub fn used_pct(&self) -> f64 {
        ((1.0 - self.remaining_fraction) * 100.0).clamp(0.0, 100.0)
    }
}

/// One Google account's Antigravity / Gemini plan quota as Cloud Code reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityPlanLimits {
    /// Stable account key: email when known, else a token fingerprint.
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub observed_at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buckets: Vec<AntigravityPlanBucket>,
    /// Set when OAuth quota cannot be fetched (e.g. `gemini-api-key` auth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

impl AntigravityPlanLimits {
    pub fn from_response(
        account_id: String,
        email: Option<String>,
        body: &Value,
        observed_at: i64,
    ) -> Option<Self> {
        let groups = body.get("groups")?.as_array()?;
        let mut buckets = Vec::new();
        for g in groups {
            let group_name = g
                .get("displayName")
                .or_else(|| g.get("groupId"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let Some(arr) = g.get("buckets").and_then(Value::as_array) else {
                continue;
            };
            for b in arr {
                let Some(bucket) = parse_bucket(b, group_name.clone()) else {
                    continue;
                };
                buckets.push(bucket);
            }
        }
        if buckets.is_empty() {
            return None;
        }
        sort_buckets(&mut buckets);
        Some(Self {
            account_id,
            email,
            observed_at,
            buckets,
            unavailable: None,
        })
    }

    pub fn unavailable(account_id: String, email: Option<String>, message: String, observed_at: i64) -> Self {
        Self {
            account_id,
            email,
            observed_at,
            buckets: Vec::new(),
            unavailable: Some(message),
        }
    }

    pub fn is_fresh(&self, now: i64) -> bool {
        now.saturating_sub(self.observed_at) < FRESH_SECS
    }
}

fn parse_bucket(b: &Value, group_name: Option<String>) -> Option<AntigravityPlanBucket> {
    let bucket_id = b
        .get("bucketId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_owned();
    let remaining = b.get("remainingFraction").and_then(Value::as_f64).filter(|n| n.is_finite())?;
    let display_name = b
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(bucket_id.as_str())
        .to_owned();
    let window = b
        .get("window")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let resets_at = parse_reset_time(b.get("resetTime"));
    Some(AntigravityPlanBucket {
        bucket_id,
        display_name,
        group_name,
        remaining_fraction: remaining.clamp(0.0, 1.0),
        resets_at,
        window,
    })
}

fn parse_reset_time(v: Option<&Value>) -> Option<i64> {
    let s = v?.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    // RFC 3339 from Cloud Code, e.g. `2026-11-08T12:50:06Z`.
    chrono_parse_rfc3339(s)
}

fn chrono_parse_rfc3339(s: &str) -> Option<i64> {
    // Avoid pulling chrono into this module: accept `YYYY-MM-DDTHH:MM:SSZ` / with fractional seconds.
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let time = time.split(['+', '-']).next().unwrap_or(time);
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec_s = t.next().unwrap_or("0");
    let sec: i64 = sec_s.split('.').next()?.parse().ok()?;
    days_from_civil(y, mo, day).map(|days| days * 86400 + h * 3600 + mi * 60 + sec)
}

/// Civil date → days since Unix epoch (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

fn sort_buckets(buckets: &mut [AntigravityPlanBucket]) {
    buckets.sort_by_key(|b| bucket_rank(&b.bucket_id));
}

fn bucket_rank(id: &str) -> (u8, String) {
    let rank = match id {
        "gemini-5h" => 0,
        "gemini-weekly" => 1,
        "3p-5h" => 2,
        "3p-weekly" => 3,
        other if other.contains("argon") && other.contains("5h") => 4,
        other if other.contains("argon") => 5,
        _ => 9,
    };
    (rank, id.to_owned())
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Latest Antigravity plan report on this node, plus whether the blade is open.
#[derive(Default)]
pub struct AntigravityPlanGuard {
    latest: Mutex<Option<AntigravityPlanLimits>>,
    blade_open: AtomicBool,
    /// In-memory refreshed access token so we do not rewrite Antigravity's credential store.
    refreshed: Mutex<Option<CachedToken>>,
}

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    /// Unix seconds when this access token should be treated as expired.
    expires_at: i64,
}

impl AntigravityPlanGuard {
    pub fn record(&self, limits: AntigravityPlanLimits) {
        *self.latest.lock().unwrap() = Some(limits);
    }

    pub fn latest(&self) -> Option<AntigravityPlanLimits> {
        self.latest.lock().unwrap().clone()
    }

    pub fn set_blade_open(&self, open: bool) {
        self.blade_open.store(open, Ordering::Relaxed);
    }

    pub fn blade_open(&self) -> bool {
        self.blade_open.load(Ordering::Relaxed)
    }

    pub fn adopt_mesh(&self, limits: AntigravityPlanLimits, local_account: Option<&str>) -> bool {
        if local_account.is_some_and(|a| a != limits.account_id) {
            return false;
        }
        let mut slot = self.latest.lock().unwrap();
        if slot.as_ref().is_some_and(|old| old.observed_at >= limits.observed_at) {
            return false;
        }
        *slot = Some(limits);
        true
    }

    fn cached_access(&self, now: i64) -> Option<String> {
        let slot = self.refreshed.lock().unwrap();
        let t = slot.as_ref()?;
        if t.expires_at <= now + 60 {
            return None;
        }
        Some(t.access_token.clone())
    }

    fn store_cached(&self, access_token: String, expires_at: i64) {
        *self.refreshed.lock().unwrap() = Some(CachedToken { access_token, expires_at });
    }
}

/// Local Antigravity / Gemini OAuth login used for the quota poll.
#[derive(Debug, Clone)]
pub struct AntigravityAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Expiry of `access_token` in unix seconds, when known.
    pub access_expires_at: Option<i64>,
    pub account_id: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntigravityAuthKind {
    Oauth,
    /// Gemini API key (or anything else without Cloud Code OAuth quota).
    ApiKey,
    /// No Antigravity auth configured.
    Missing,
}

/// What `~/.gemini/antigravity-acp/settings.json` says about sign-in.
pub fn auth_kind() -> AntigravityAuthKind {
    let Some(path) = antigravity_acp_settings() else {
        return AntigravityAuthKind::Missing;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return AntigravityAuthKind::Missing;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return AntigravityAuthKind::Missing;
    };
    match v.pointer("/auth/type").and_then(Value::as_str) {
        Some("oauth-personal") | Some("oauth-business") => AntigravityAuthKind::Oauth,
        Some("gemini-api-key") | Some("agent-platform") => AntigravityAuthKind::ApiKey,
        Some(_) => AntigravityAuthKind::ApiKey,
        None => AntigravityAuthKind::Missing,
    }
}

fn antigravity_acp_settings() -> Option<PathBuf> {
    Some(gemini_home()?.join("antigravity-acp").join("settings.json"))
}

fn gemini_home() -> Option<PathBuf> {
    std::env::var_os("GEMINI_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::code::agents::home_dir().map(|h| h.join(".gemini")))
}

/// Read a usable access token (refreshing when stale). `None` when there is no OAuth login.
pub async fn load_auth(guard: &AntigravityPlanGuard) -> Option<AntigravityAuth> {
    let now = now();
    if let Some(access) = guard.cached_access(now) {
        // Still need account identity from disk.
        if let Some(mut auth) = read_auth_sources() {
            auth.access_token = access;
            auth.access_expires_at = None;
            return Some(auth);
        }
    }
    let mut auth = read_auth_sources()?;
    if auth.access_expires_at.is_some_and(|t| t > now + 60) {
        return Some(auth);
    }
    let Some(refresh) = auth.refresh_token.clone() else {
        // Use whatever access token we have; a 401 will surface on fetch.
        return Some(auth);
    };
    match refresh_access_token(&refresh).await {
        Ok((access, expires_in)) => {
            let expires_at = now + expires_in.max(0);
            guard.store_cached(access.clone(), expires_at);
            auth.access_token = access;
            auth.access_expires_at = Some(expires_at);
            Some(auth)
        }
        Err(e) => {
            tracing::debug!("antigravity plan: token refresh failed: {e:#}");
            // Fall through with the on-disk access token if any.
            if auth.access_token.is_empty() {
                None
            } else {
                Some(auth)
            }
        }
    }
}

fn read_auth_sources() -> Option<AntigravityAuth> {
    if let Some(auth) = read_keychain_auth() {
        return Some(auth);
    }
    if let Some(auth) = read_cli_token_file() {
        return Some(auth);
    }
    read_oauth_creds()
}

fn read_keychain_auth() -> Option<AntigravityAuth> {
    let raw = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .ok()?
        .get_password()
        .ok()?;
    let json = if let Some(b64) = raw.strip_prefix(KEYRING_B64_PREFIX) {
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
        serde_json::from_slice::<Value>(&bytes).ok()?
    } else {
        serde_json::from_str::<Value>(&raw).ok()?
    };
    auth_from_wrapped_token(&json, json.get("id_token_email").and_then(Value::as_str).map(str::to_owned))
}

fn read_cli_token_file() -> Option<AntigravityAuth> {
    let path = gemini_home()?.join("antigravity-cli").join("antigravity-oauth-token");
    let v = read_json(&path)?;
    let email = v
        .get("id_token_email")
        .or_else(|| v.get("email"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| email_from_id_token(v.get("id_token").and_then(Value::as_str)));
    auth_from_wrapped_token(&v, email)
}

fn read_oauth_creds() -> Option<AntigravityAuth> {
    let path = gemini_home()?.join("oauth_creds.json");
    let v = read_json(&path)?;
    let access = v.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_owned();
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let expiry = v.get("expiry_date").and_then(|n| {
        n.as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .map(|ms| if ms > 10_000_000_000 { ms / 1000 } else { ms })
    });
    let email = email_from_id_token(v.get("id_token").and_then(Value::as_str))
        .or_else(active_google_email);
    let account_id = email
        .clone()
        .unwrap_or_else(|| format!("token:{}", &access[..access.len().min(16)]));
    Some(AntigravityAuth {
        access_token: access,
        refresh_token: refresh,
        access_expires_at: expiry,
        account_id,
        email,
    })
}

fn auth_from_wrapped_token(v: &Value, email_hint: Option<String>) -> Option<AntigravityAuth> {
    let token = v.get("token").unwrap_or(v);
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_owned();
    let refresh = token
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let expiry = token
        .get("expiry")
        .and_then(Value::as_str)
        .and_then(chrono_parse_rfc3339)
        .or_else(|| {
            token.get("expiry_date").and_then(|n| {
                n.as_i64()
                    .or_else(|| n.as_f64().map(|f| f as i64))
                    .map(|ms| if ms > 10_000_000_000 { ms / 1000 } else { ms })
            })
        });
    let email = email_hint
        .or_else(|| email_from_id_token(v.get("id_token").and_then(Value::as_str)))
        .or_else(active_google_email);
    let account_id = email
        .clone()
        .unwrap_or_else(|| format!("token:{}", &access[..access.len().min(16)]));
    Some(AntigravityAuth {
        access_token: access,
        refresh_token: refresh,
        access_expires_at: expiry,
        account_id,
        email,
    })
}

fn active_google_email() -> Option<String> {
    let v = read_json(&gemini_home()?.join("google_accounts.json"))?;
    v.get("active")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn email_from_id_token(token: Option<&str>) -> Option<String> {
    let claims = jwt_claims(token?)?;
    claims
        .get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
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

fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

struct OauthClient {
    client_id: String,
    client_secret: String,
}

fn oauth_clients() -> Result<Vec<OauthClient>> {
    let clients = discover_oauth_clients_from_install();
    if clients.is_empty() {
        return Err(anyhow!(
            "Antigravity OAuth client not found in a local install (agy / language_server)"
        ));
    }
    Ok(clients)
}

fn discover_oauth_clients_from_install() -> Vec<OauthClient> {
    for path in antigravity_binary_candidates() {
        let clients = scan_binary_for_oauth_clients(&path);
        if !clients.is_empty() {
            return clients;
        }
    }
    Vec::new()
}

fn antigravity_binary_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = gemini_home() {
        out.push(home.join("bin/agy"));
        out.push(home.join("antigravity/bin/agy"));
        out.push(home.join("antigravity-cli/bin/agy"));
    }
    if let Some(p) = crate::code::agents::which("agy") {
        out.push(p);
    }
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from(
            "/Applications/Antigravity.app/Contents/Resources/bin/language_server",
        ));
        out.push(PathBuf::from(
            "/Applications/Antigravity IDE.app/Contents/Resources/app/extensions/antigravity/bin/language_server",
        ));
    }
    out
}

fn scan_binary_for_oauth_clients(path: &Path) -> Vec<OauthClient> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    let ids = find_google_client_ids(&data);
    let secrets = find_gocspx_secrets(&data);
    let mut out = Vec::new();
    for id in &ids {
        for secret in &secrets {
            out.push(OauthClient {
                client_id: id.clone(),
                client_secret: secret.clone(),
            });
        }
    }
    out
}

/// Google OAuth installed-app client ids look like `NNNN-xxxx.apps.googleusercontent.com`.
fn find_google_client_ids(data: &[u8]) -> Vec<String> {
    let needle = b".apps.googleusercontent.com";
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = data[i..].windows(needle.len()).position(|w| w == needle) {
        let end = i + rel + needle.len();
        let mut start = i + rel;
        while start > 0 {
            let b = data[start - 1];
            if b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-' {
                start -= 1;
            } else {
                break;
            }
        }
        // Walk-back may eat preceding lowercase letters; client ids always start with a digit.
        let slice = &data[start..end];
        let dig = slice.iter().position(|b| b.is_ascii_digit()).unwrap_or(slice.len());
        let s = String::from_utf8_lossy(&slice[dig..]).into_owned();
        if s.contains('-') && s.chars().next().is_some_and(|c| c.is_ascii_digit()) && !out.contains(&s) {
            out.push(s);
        }
        i = end;
    }
    out
}

/// Go packs adjacent string literals without NULs; strip a trailing English remnant if present.
fn find_gocspx_secrets(data: &[u8]) -> Vec<String> {
    const NEEDLE: &[u8] = b"GOCSPX-";
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = data[i..].windows(NEEDLE.len()).position(|w| w == NEEDLE) {
        let start = i + rel;
        let rest = &data[start..];
        let end = rest
            .iter()
            .position(|&b| !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            .unwrap_or(rest.len());
        let mut s = String::from_utf8_lossy(&rest[..end]).into_owned();
        // Go packs the next string literal with no NUL; cut before known remnants.
        for trail in ["decoding", "https", "field"] {
            if let Some(idx) = s.find(trail) {
                if idx >= 16 {
                    s.truncate(idx);
                    break;
                }
            }
        }
        if s.starts_with("GOCSPX-") && s.len() >= 20 && !out.contains(&s) {
            out.push(s);
        }
        i = start + NEEDLE.len();
    }
    out
}

async fn refresh_access_token(refresh_token: &str) -> Result<(String, i64)> {
    let clients = oauth_clients()?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut last_status = None;
    for client in &clients {
        let resp = http
            .post(TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .form(&[
                ("client_id", client.client_id.as_str()),
                ("client_secret", client.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            last_status = Some(status);
            continue;
        }
        let access = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("token refresh response missing access_token"))?
            .to_owned();
        let expires_in = body.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
        return Ok((access, expires_in));
    }
    Err(anyhow!(
        "Google token refresh HTTP {}",
        last_status.map(|s| s.to_string()).unwrap_or_else(|| "failed".into())
    ))
}

fn user_agent() -> String {
    let ver = read_agy_version().unwrap_or_else(|| "1.4.12".into());
    // Cloud Code gates Gemini buckets on a current Antigravity client identity. Legacy
    // `antigravity/hub/1.0.0` (and bare `antigravity`) can omit Gemini groups entirely.
    format!("antigravity/cli/{ver}")
}

fn read_agy_version() -> Option<String> {
    for path in [
        gemini_home()?.join("bin/agy.version.json"),
        gemini_home()?.join("antigravity-cli/agy.version.json"),
    ] {
        if let Some(v) = read_json(&path)
            && let Some(ver) = v.get("version").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
        {
            return Some(ver.to_owned());
        }
    }
    None
}

/// Fetch the quota summary for the local Antigravity OAuth login.
pub async fn fetch(auth: &AntigravityAuth) -> Result<AntigravityPlanLimits> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(user_agent())
        .build()?;
    // Consumer Antigravity accounts use an empty project; `SetProjectID("")` is what the CLI logs.
    // Do not invent a hard-coded GCP project id.
    let resp = http
        .post(QUOTA_URL)
        .header("Authorization", format!("Bearer {}", auth.access_token))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await?;
    if !status.is_success() {
        return Err(anyhow!("Antigravity quota HTTP {status}"));
    }
    AntigravityPlanLimits::from_response(auth.account_id.clone(), auth.email.clone(), &body, now())
        .ok_or_else(|| anyhow!("Antigravity quota response has no buckets"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_body() -> Value {
        json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        {
                            "bucketId": "gemini-weekly",
                            "displayName": "Weekly Limit Remaining",
                            "remainingFraction": 0.77186614,
                            "resetTime": "2026-11-08T12:50:06Z",
                            "window": "weekly"
                        },
                        {
                            "bucketId": "gemini-5h",
                            "displayName": "Five Hour Limit Remaining",
                            "remainingFraction": 1.0,
                            "resetTime": "2026-11-05T03:45:47Z",
                            "window": "5h"
                        }
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        {
                            "bucketId": "3p-weekly",
                            "displayName": "Weekly Limit Remaining",
                            "remainingFraction": 1.0,
                            "resetTime": "2026-11-11T23:08:48Z",
                            "window": "weekly"
                        },
                        {
                            "bucketId": "3p-5h",
                            "displayName": "Five Hour Limit Remaining",
                            "remainingFraction": 0.42,
                            "resetTime": "2026-11-05T04:08:48Z",
                            "window": "5h"
                        }
                    ]
                },
                {
                    "displayName": "Gemini 4 Argon",
                    "buckets": [
                        {
                            "bucketId": "gemini-argon-wtu-weekly",
                            "displayName": "Gemini 4 Argon Weekly Limit",
                            "remainingFraction": 0.9,
                            "resetTime": "2026-11-11T23:08:48Z",
                            "window": "weekly"
                        }
                    ]
                }
            ]
        })
    }

    #[test]
    fn parses_quota_summary_and_orders_buckets() {
        let l = AntigravityPlanLimits::from_response(
            "a@b.c".into(),
            Some("a@b.c".into()),
            &sample_body(),
            1000,
        )
        .unwrap();
        assert_eq!(l.buckets.len(), 5);
        assert_eq!(l.buckets[0].bucket_id, "gemini-5h");
        assert_eq!(l.buckets[1].bucket_id, "gemini-weekly");
        assert_eq!(l.buckets[2].bucket_id, "3p-5h");
        assert_eq!(l.buckets[3].bucket_id, "3p-weekly");
        assert_eq!(l.buckets[4].bucket_id, "gemini-argon-wtu-weekly");
        assert!((l.buckets[0].remaining_fraction - 1.0).abs() < 1e-9);
        assert!((l.buckets[1].used_pct() - (1.0 - 0.77186614) * 100.0).abs() < 1e-6);
        assert_eq!(l.buckets[0].resets_at, Some(1_793_850_347)); // 2026-11-05T03:45:47Z
        assert!(AntigravityPlanLimits::from_response("x".into(), None, &json!({}), 1).is_none());
        assert!(AntigravityPlanLimits::from_response("x".into(), None, &json!({ "groups": [] }), 1).is_none());
    }

    #[test]
    fn parses_reset_times() {
        assert_eq!(chrono_parse_rfc3339("2026-11-08T12:50:06Z"), Some(1_794_142_206));
        assert_eq!(chrono_parse_rfc3339("2026-01-01T00:00:00.123Z"), Some(1_767_225_600));
        assert!(chrono_parse_rfc3339("").is_none());
        assert!(chrono_parse_rfc3339("not-a-date").is_none());
    }

    #[test]
    fn fresh_within_window() {
        let l = AntigravityPlanLimits {
            account_id: "a".into(),
            email: None,
            observed_at: 1000,
            buckets: vec![],
            unavailable: None,
        };
        assert!(l.is_fresh(1000 + FRESH_SECS - 1));
        assert!(!l.is_fresh(1000 + FRESH_SECS));
    }

    #[test]
    fn mesh_adopt_same_account_only() {
        let g = AntigravityPlanGuard::default();
        let base = AntigravityPlanLimits::from_response("42".into(), None, &sample_body(), 1000).unwrap();
        let newer = AntigravityPlanLimits {
            observed_at: 2000,
            buckets: {
                let mut b = base.buckets.clone();
                b[0].remaining_fraction = 0.5;
                b
            },
            ..base.clone()
        };
        assert!(g.adopt_mesh(newer.clone(), Some("42")));
        assert!(!g.adopt_mesh(base, Some("42")), "older ignored");
        assert!(!g.adopt_mesh(
            AntigravityPlanLimits {
                account_id: "other".into(),
                ..newer.clone()
            },
            Some("42")
        ));
        assert!((g.latest().unwrap().buckets[0].remaining_fraction - 0.5).abs() < 1e-9);
    }

    #[test]
    fn strips_go_concat_secret_suffix() {
        // Go packs string literals without NULs; a real binary often appends the next symbol name.
        let blob = b"xxGOCSPX-TestInstalledAppSecret99decodingyy";
        assert_eq!(
            find_gocspx_secrets(blob),
            vec!["GOCSPX-TestInstalledAppSecret99".to_owned()]
        );
    }

    #[test]
    fn finds_google_client_id_in_blob() {
        let blob = b"pad999000111222-aaaaaaaaaaaaaaaaaaaaaaaaaaaa.apps.googleusercontent.com\0end";
        let ids = find_google_client_ids(blob);
        assert_eq!(ids, vec!["999000111222-aaaaaaaaaaaaaaaaaaaaaaaaaaaa.apps.googleusercontent.com".to_owned()]);
    }
}
