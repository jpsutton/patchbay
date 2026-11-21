//! Antigravity / Gemini plan quota, polled from Google's Cloud Code quota API (not over ACP).
//!
//! Antigravity's ACP path does not emit plan windows the way Claude's `_claude/rateLimit` does.
//! The CLI `/usage` and IDE quota UI call `POST …/v1internal:retrieveUserQuotaSummary` on
//! `daily-cloudcode-pa.googleapis.com` with the signed-in Google OAuth token. Patchbay polls the
//! same endpoint while an Antigravity session is running or the plan blade is open.
//!
//! OAuth credentials and refresh are provided by `crate::antigravity_auth`, shared with Gemini
//! provider requests. The quota poller keeps only report and blade state here.
//!
//! `gemini-api-key` auth has no OAuth quota: the poller records an `unavailable` report instead.
//! Reports fan out over the mesh tagged with the account id so peers on the same Google login can
//! skip re-polling.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long a report stays fresh enough that this node (or a peer on the same account) need not poll.
pub const FRESH_SECS: i64 = 270;
/// How often the background loop considers whether to poll.
pub const TICK: Duration = Duration::from_secs(30);

const QUOTA_URL: &str = "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";

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
}

impl AntigravityPlanGuard {
    pub fn record(&self, limits: AntigravityPlanLimits) { *self.latest.lock().unwrap() = Some(limits); }
    pub fn latest(&self) -> Option<AntigravityPlanLimits> { self.latest.lock().unwrap().clone() }
    pub fn set_blade_open(&self, open: bool) { self.blade_open.store(open, Ordering::Relaxed); }
    pub fn blade_open(&self) -> bool { self.blade_open.load(Ordering::Relaxed) }
    pub fn adopt_mesh(&self, limits: AntigravityPlanLimits, local_account: Option<&str>) -> bool {
        if local_account.is_some_and(|a| a != limits.account_id) { return false; }
        let mut slot = self.latest.lock().unwrap();
        if slot.as_ref().is_some_and(|old| old.observed_at >= limits.observed_at) { return false; }
        *slot = Some(limits); true
    }
}

/// Shared OAuth credentials used by provider requests and quota polling.
pub use crate::antigravity_auth::AntigravityAuth;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AntigravityAuthKind { Oauth, ApiKey, Missing }

/// Read the local auth marker. Token resolution itself is delegated to the shared auth module.
pub async fn auth_kind() -> AntigravityAuthKind {
    tokio::task::spawn_blocking(|| {
        let Some(path) = gemini_home().map(|h| h.join("antigravity-acp/settings.json")) else { return AntigravityAuthKind::Missing };
        let Ok(bytes) = std::fs::read(path) else { return AntigravityAuthKind::Missing };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { return AntigravityAuthKind::Missing };
        match v.pointer("/auth/type").and_then(Value::as_str) {
            Some("oauth-personal") | Some("oauth-business") => AntigravityAuthKind::Oauth,
            Some("gemini-api-key") | Some("agent-platform") => AntigravityAuthKind::ApiKey,
            Some(_) => AntigravityAuthKind::ApiKey,
            None => AntigravityAuthKind::Missing,
        }
    }).await.unwrap_or(AntigravityAuthKind::Missing)
}

fn gemini_home() -> Option<PathBuf> {
    crate::antigravity_auth::resolve_dir(None).ok()
}

pub async fn load_auth_for(gemini_home: Option<&str>) -> Option<AntigravityAuth> {
    crate::antigravity_auth::load_auth(gemini_home).await
}

/// Fetch the quota summary for the local Antigravity OAuth login.
pub async fn fetch(auth: &AntigravityAuth) -> Result<AntigravityPlanLimits> {
    fetch_from(QUOTA_URL, auth).await
}

/// `fetch` against `url` (a test points it at a loopback server).
async fn fetch_from(url: &str, auth: &AntigravityAuth) -> Result<AntigravityPlanLimits> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        // No redirects: a 3xx would send the bearer token wherever the server points.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(auth.user_agent())
        .build()?;
    // Consumer Antigravity accounts use an empty project; `SetProjectID("")` is what the CLI logs.
    // Do not invent a hard-coded GCP project id.
    let resp = http
        .post(url)
        .header("Authorization", format!("Bearer {}", auth.access_token))
        .header("Content-Type", "application/json")
        .header("x-goog-api-client", "google-cloud-sdk vscode_cloudshelleditor/0.1")
        .header("client-metadata", r#"{"ideType":"ANTIGRAVITY","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}"#)
        .body("{}")
        .send()
        .await?;
    let status = resp.status();
    if status.is_redirection() {
        return Err(anyhow!("Antigravity quota answered a redirect (HTTP {status}); Patchbay does not follow redirects with a login token"));
    }
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

    /// The quota call does not follow a 302/307 (its bearer token would go with it).
    #[tokio::test]
    async fn the_quota_call_does_not_follow_a_redirect() {
        use std::sync::atomic::Ordering;
        let auth = AntigravityAuth {
            access_token: "TOKEN".into(), refresh_token: None, access_expires_at: None,
            account_id: "account@example.test".into(), email: None, client_version: "9.8.7".into(),
        };
        for status in [302u16, 307] {
            let (base, first, second) = crate::test_support::redirect_trap(status).await;
            let e = fetch_from(&base, &auth).await.unwrap_err().to_string();
            assert!(e.contains("redirect") && !e.contains("TOKEN"), "{status}: {e}");
            assert_eq!((first.load(Ordering::SeqCst), second.load(Ordering::SeqCst)), (1, 0), "{status}: followed");
        }
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


}
