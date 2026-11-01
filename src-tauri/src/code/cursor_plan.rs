//! Cursor subscription plan usage, polled from Cursor's dashboard API (not over ACP).
//!
//! `cursor-agent acp` does not emit `usage_update` or plan limits. The interactive TUI and the
//! web dashboard get them from `GetCurrentPeriodUsage` on `api2.cursor.sh`. Patchbay polls that
//! endpoint with the local Cursor login, shows Auto / API / total percentages, and can stop
//! Cursor ACP turns once included plan spend is exhausted and free-on-us bonus is gone
//! (`prefs.cursor_usage_limit`, off by default).
//!
//! Reports are tagged with the Cursor account id and fan out over the mesh so peers on the same
//! account can skip re-polling. A peer on a different account ignores the report.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prefs::UsageLimit;

/// How long a report stays fresh enough that this node (or a peer on the same account) need not poll.
pub const FRESH_SECS: i64 = 270;
/// How often the background loop considers whether to poll.
pub const TICK: Duration = Duration::from_secs(30);

const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";

/// One Cursor account's plan usage as the dashboard reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorPlanLimits {
    /// Stable account key: Cursor `userId` when known, else email, else a token fingerprint.
    pub account_id: String,
    /// Display email when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub observed_at: i64,
    /// End of the billing cycle (unix seconds), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// Auto / first-party pool, 0–100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<f64>,
    /// API / named-model pool, 0–100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<f64>,
    /// Combined included usage, 0–100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Included-plan spend so far, in cents (`planUsage.includedSpend`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub included_spend_cents: Option<i64>,
    /// Included-plan allowance, in cents (`planUsage.limit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_cents: Option<i64>,
    /// "Free usage on us" already consumed, in cents (`planUsage.bonusSpend`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bonus_spend_cents: Option<i64>,
    /// Whether Cursor still has bonus / free-on-us capacity (`planUsage.remainingBonus`).
    /// Absent on older mesh reports; the guard only treats an explicit `true` as still available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_bonus: Option<bool>,
}

/// Included plan and free-on-us bonus are both spent; turns should stop.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub resets_at: i64,
}

impl CursorPlanLimits {
    /// Parse a `GetCurrentPeriodUsage` JSON body. `None` when there is no `planUsage` (team seats
    /// often omit it).
    pub fn from_response(account_id: String, email: Option<String>, body: &Value, observed_at: i64) -> Option<Self> {
        let plan = body.get("planUsage")?;
        if !plan.is_object() {
            return None;
        }
        let pct = |k: &str| plan.get(k).and_then(|v| v.as_f64()).filter(|n| n.is_finite());
        let cents = |k: &str| {
            plan.get(k).and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_f64().map(|n| n.round() as i64))
                    .or_else(|| v.as_str()?.parse().ok())
            })
        };
        let auto = pct("autoPercentUsed");
        let api = pct("apiPercentUsed");
        let total = pct("totalPercentUsed");
        let included_spend_cents = cents("includedSpend");
        let limit_cents = cents("limit");
        let bonus_spend_cents = cents("bonusSpend");
        let remaining_bonus = plan.get("remainingBonus").and_then(Value::as_bool);
        if auto.is_none()
            && api.is_none()
            && total.is_none()
            && included_spend_cents.is_none()
            && bonus_spend_cents.is_none()
        {
            return None;
        }
        let resets_at = ms_to_secs(body.get("billingCycleEnd"));
        Some(Self {
            account_id,
            email,
            observed_at,
            resets_at,
            auto,
            api,
            total,
            included_spend_cents,
            limit_cents,
            bonus_spend_cents,
            remaining_bonus,
        })
    }

    /// True when this report is new enough that another poll would be wasteful.
    pub fn is_fresh(&self, now: i64) -> bool {
        now.saturating_sub(self.observed_at) < FRESH_SECS
    }

    /// Included plan allowance is used up (`includedSpend >= limit`).
    pub fn plan_exhausted(&self) -> bool {
        match (self.included_spend_cents, self.limit_cents) {
            (Some(used), Some(limit)) if limit > 0 => used >= limit,
            // No dollar fields: treat a pool at 100% as the included plan being done.
            _ => [self.auto, self.api, self.total]
                .into_iter()
                .flatten()
                .any(|p| p >= 100.0 - 1e-9),
        }
    }

    /// Cursor still has "free usage on us" capacity. Only an explicit `true` counts; absent means unknown.
    pub fn bonus_available(&self) -> bool {
        self.remaining_bonus == Some(true)
    }

    /// Stop once the included plan is exhausted **and** free-on-us bonus is no longer available.
    /// While `remainingBonus` is true, turns keep going even past the included limit.
    /// After `resets_at`, nothing hits. No percent threshold — Cursor's pools are not request caps.
    pub fn hit(&self, now: i64) -> Option<Hit> {
        if self.resets_at.is_some_and(|t| t <= now) {
            return None;
        }
        if self.bonus_available() || !self.plan_exhausted() {
            return None;
        }
        Some(Hit { resets_at: self.resets_at.unwrap_or(now + 7 * 86400) })
    }
}

impl Hit {
    pub fn message(&self, now: i64) -> String {
        format!(
            "Your Cursor plan's included usage and free-on-us bonus are used up, so Patchbay stops its Cursor turns. The cycle resets in {}. Change this under Settings › Coding agents › Cursor CLI.",
            fmt_wait(self.resets_at - now)
        )
    }
}

fn fmt_wait(secs: i64) -> String {
    let m = (secs.max(60) + 59) / 60;
    match (m / 1440, m / 60 % 24, m % 60) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

fn ms_to_secs(v: Option<&Value>) -> Option<i64> {
    let n = match v? {
        Value::String(s) => s.parse::<i64>().ok()?,
        Value::Number(n) => n.as_i64()?,
        _ => return None,
    };
    Some(if n > 10_000_000_000 { n / 1000 } else { n })
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Latest Cursor plan report on this node, plus who is watching the blade and which sessions are stopping.
#[derive(Default)]
pub struct CursorPlanGuard {
    latest: Mutex<Option<CursorPlanLimits>>,
    stopping: Mutex<HashSet<String>>,
    resume: Mutex<std::collections::BTreeMap<String, i64>>,
    /// The Cursor plan blade is open in some window on this node.
    blade_open: AtomicBool,
}

impl CursorPlanGuard {
    pub fn record(&self, limits: CursorPlanLimits) {
        *self.latest.lock().unwrap() = Some(limits);
    }

    pub fn latest(&self) -> Option<CursorPlanLimits> {
        self.latest.lock().unwrap().clone()
    }

    pub fn set_blade_open(&self, open: bool) {
        self.blade_open.store(open, Ordering::Relaxed);
    }

    pub fn blade_open(&self) -> bool {
        self.blade_open.load(Ordering::Relaxed)
    }

    pub fn hit(&self, limit: &UsageLimit, now: i64) -> Option<Hit> {
        if !limit.enabled {
            return None;
        }
        self.latest.lock().unwrap().as_ref()?.hit(now)
    }

    /// Adopt a peer's report when it is for the same account and newer than ours.
    pub fn adopt_mesh(&self, limits: CursorPlanLimits, local_account: Option<&str>) -> bool {
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

    pub fn start_stopping(&self, code_session_id: &str) -> bool {
        self.stopping.lock().unwrap().insert(code_session_id.to_owned())
    }

    pub fn turn_ended(&self, code_session_id: &str) {
        self.stopping.lock().unwrap().remove(code_session_id);
    }

    pub fn schedule_resume(&self, code_session_id: &str, resets_at: i64) {
        self.resume.lock().unwrap().insert(code_session_id.to_owned(), resets_at);
    }

    pub fn cancel_resume(&self, code_session_id: &str) -> bool {
        self.resume.lock().unwrap().remove(code_session_id).is_some()
    }

    pub fn clear_resumes(&self) -> bool {
        let mut r = self.resume.lock().unwrap();
        let any = !r.is_empty();
        r.clear();
        any
    }

    pub fn take_due(&self, now: i64) -> Vec<String> {
        let mut r = self.resume.lock().unwrap();
        let due: Vec<String> = r.iter().filter(|(_, at)| **at <= now).map(|(id, _)| id.clone()).collect();
        for id in &due {
            r.remove(id);
        }
        due
    }

    pub fn resumes(&self) -> std::collections::BTreeMap<String, i64> {
        self.resume.lock().unwrap().clone()
    }

    pub fn set_resumes(&self, resumes: std::collections::BTreeMap<String, i64>) {
        *self.resume.lock().unwrap() = resumes;
    }
}

/// Local Cursor login: access token plus account identity for tagging reports.
#[derive(Debug, Clone)]
pub struct CursorAuth {
    pub access_token: String,
    pub account_id: String,
    pub email: Option<String>,
}

/// Read the Cursor CLI / IDE login from disk and, when needed, `cursor-agent status`.
pub fn load_auth(cursor_agent: Option<&Path>) -> Option<CursorAuth> {
    let (token, email_hint) = read_access_token()?;
    let (account_id, email) = identity(cursor_agent, &token, email_hint);
    Some(CursorAuth { access_token: token, account_id, email })
}

fn read_access_token() -> Option<(String, Option<String>)> {
    for path in auth_json_paths() {
        if let Some(v) = read_json(&path) {
            let token = v.get("accessToken").and_then(Value::as_str).filter(|s| !s.is_empty())?;
            let email = v.get("email").and_then(Value::as_str).map(str::to_owned);
            return Some((token.to_owned(), email));
        }
    }
    read_vscdb_token()
}

fn auth_json_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        out.push(PathBuf::from(xdg).join("cursor/auth.json"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        out.push(home.join(".config/cursor/auth.json"));
        out.push(home.join(".cursor/auth.json"));
    }
    out
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_vscdb_token() -> Option<(String, Option<String>)> {
    for path in vscdb_paths() {
        let Ok(conn) = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let token: String = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'",
                [],
                |r| r.get(0),
            )
            .ok()?;
        if token.is_empty() {
            continue;
        }
        let email: Option<String> = conn
            .query_row(
                "SELECT value FROM ItemTable WHERE key = 'cursorAuth/cachedEmail'",
                [],
                |r| r.get(0),
            )
            .ok();
        return Some((token, email));
    }
    None
}

fn vscdb_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let push = |base: PathBuf, out: &mut Vec<PathBuf>| {
        out.push(base.join("Cursor/User/globalStorage/state.vscdb"));
        out.push(base.join("cursor/User/globalStorage/state.vscdb"));
    };
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        push(PathBuf::from(xdg), &mut out);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        push(home.join(".config"), &mut out);
        #[cfg(target_os = "macos")]
        push(home.join("Library/Application Support"), &mut out);
    }
    #[cfg(target_os = "windows")]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        out.push(PathBuf::from(appdata).join("Cursor/User/globalStorage/state.vscdb"));
    }
    out
}

fn identity(cursor_agent: Option<&Path>, token: &str, email_hint: Option<String>) -> (String, Option<String>) {
    if let Some(status) = agent_status(cursor_agent) {
        let user = &status["userInfo"];
        let email = user.get("email").and_then(Value::as_str).map(str::to_owned).or(email_hint.clone());
        if let Some(id) = user
            .get("userId")
            .and_then(|v| v.as_i64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_owned)))
            .filter(|s| !s.is_empty())
        {
            return (id, email);
        }
        if let Some(email) = email.clone() {
            return (email.clone(), Some(email));
        }
    }
    if let Some(claims) = jwt_claims(token) {
        let email = claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(email_hint.clone());
        if let Some(sub) = claims.get("sub").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return (sub.to_owned(), email);
        }
        if let Some(email) = email.clone() {
            return (email.clone(), Some(email));
        }
    }
    if let Some(email) = email_hint {
        return (email.clone(), Some(email));
    }
    // Last resort: fingerprint the token so mesh peers with the same login still match.
    (format!("token:{}", &token[..token.len().min(16)]), None)
}

fn agent_status(cursor_agent: Option<&Path>) -> Option<Value> {
    let program = cursor_agent
        .map(|p| p.to_path_buf())
        .or_else(|| crate::code::agents::which("cursor-agent"))?;
    let out = Command::new(program).args(["status", "--format", "json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let padded = match payload.len() % 4 {
        0 => payload.to_owned(),
        2 => format!("{payload}=="),
        3 => format!("{payload}="),
        _ => return None,
    };
    let bytes = base64_url_decode(&padded)?;
    serde_json::from_slice(&bytes).ok()
}

fn base64_url_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .or_else(|_| {
            let std = s.replace('-', "+").replace('_', "/");
            base64::engine::general_purpose::STANDARD.decode(std)
        })
        .ok()
}

/// Fetch the current period usage for the local Cursor login.
pub async fn fetch(auth: &CursorAuth) -> Result<CursorPlanLimits> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let resp = client
        .post(USAGE_URL)
        .header("Authorization", format!("Bearer {}", auth.access_token))
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .body("{}")
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await?;
    if !status.is_success() {
        return Err(anyhow!("Cursor usage HTTP {status}: {body}"));
    }
    CursorPlanLimits::from_response(auth.account_id.clone(), auth.email.clone(), &body, now())
        .ok_or_else(|| anyhow!("Cursor usage response has no planUsage (team seats often omit it)"))
}

/// Resolve the `cursor-agent` binary from prefs overrides, else PATH.
pub fn cursor_agent_bin(overrides: &crate::code::agents::AgentPaths) -> Option<PathBuf> {
    crate::code::agents::resolve(crate::code::agents::AgentKind::Cursor, overrides)
        .ok()
        .map(|r| r.program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(auto: f64, api: f64, total: f64, resets: i64) -> CursorPlanLimits {
        CursorPlanLimits {
            account_id: "42".into(),
            email: Some("a@b.c".into()),
            observed_at: 1000,
            resets_at: Some(resets),
            auto: Some(auto),
            api: Some(api),
            total: Some(total),
            // Exhausted included plan, no bonus left: the guard may apply the percent threshold.
            included_spend_cents: Some(40000),
            limit_cents: Some(40000),
            bonus_spend_cents: Some(1200),
            remaining_bonus: Some(false),
        }
    }

    #[test]
    fn parses_dashboard_body() {
        let body = json!({
            "billingCycleEnd": "1795191155000",
            "planUsage": {
                "autoPercentUsed": 0.91,
                "apiPercentUsed": 100,
                "totalPercentUsed": 16.7,
                "includedSpend": 40000,
                "limit": 40000,
                "bonusSpend": 24201,
                "remainingBonus": false
            }
        });
        let l = CursorPlanLimits::from_response("42".into(), Some("a@b.c".into()), &body, 50).unwrap();
        assert_eq!(l.auto, Some(0.91));
        assert_eq!(l.api, Some(100.0));
        assert_eq!(l.resets_at, Some(1795191155));
        assert_eq!(l.bonus_spend_cents, Some(24201));
        assert_eq!(l.remaining_bonus, Some(false));
        assert!(l.plan_exhausted());
        assert!(!l.bonus_available());
        assert!(CursorPlanLimits::from_response("42".into(), None, &json!({}), 1).is_none());
    }

    #[test]
    fn hits_only_when_plan_and_bonus_are_gone() {
        let l = sample(12.0, 96.0, 40.0, 5000);
        assert!(l.hit(1000).is_some());
        assert!(l.hit(5000).is_none(), "past reset");
        let mut early = l.clone();
        early.included_spend_cents = Some(20000);
        assert!(early.hit(1000).is_none(), "included plan still has room");
    }

    #[test]
    fn guard_waits_while_bonus_remains() {
        let mut l = sample(12.0, 99.0, 50.0, 9000);
        l.remaining_bonus = Some(true);
        assert!(l.hit(1000).is_none(), "bonus still available");
        l.remaining_bonus = Some(false);
        assert!(l.hit(1000).is_some());
    }

    #[test]
    fn without_dollars_falls_back_to_full_pools() {
        let mut l = sample(12.0, 100.0, 40.0, 9000);
        l.included_spend_cents = None;
        l.limit_cents = None;
        assert!(l.hit(1000).is_some());
        l.api = Some(50.0);
        l.auto = Some(12.0);
        l.total = Some(20.0);
        assert!(l.hit(1000).is_none(), "no pool at 100%");
    }

    #[test]
    fn fresh_within_window() {
        let l = sample(1.0, 1.0, 1.0, 9000);
        assert!(l.is_fresh(1000 + FRESH_SECS - 1));
        assert!(!l.is_fresh(1000 + FRESH_SECS));
    }

    #[test]
    fn mesh_adopt_same_account_only() {
        let g = CursorPlanGuard::default();
        let newer = CursorPlanLimits { observed_at: 2000, auto: Some(50.0), ..sample(1.0, 1.0, 1.0, 9000) };
        assert!(g.adopt_mesh(newer.clone(), Some("42")));
        assert!(!g.adopt_mesh(sample(99.0, 99.0, 99.0, 9000), Some("42")), "older ignored");
        assert!(!g.adopt_mesh(CursorPlanLimits { account_id: "other".into(), ..newer }, Some("42")));
        assert_eq!(g.latest().unwrap().auto, Some(50.0));
    }

    #[test]
    fn guard_switch_and_stop_once() {
        let g = CursorPlanGuard::default();
        g.record(sample(10.0, 97.0, 20.0, 9000));
        let on = UsageLimit { enabled: true, percent: 95, auto_resume: false };
        assert!(g.hit(&on, 1000).is_some());
        assert!(g.hit(&UsageLimit { enabled: false, ..on }, 1000).is_none());
        assert!(g.start_stopping("s1"));
        assert!(!g.start_stopping("s1"));
        g.turn_ended("s1");
        assert!(g.start_stopping("s1"));
    }

    #[test]
    fn message_says_when_it_resets() {
        let hit = Hit { resets_at: 1000 + 3600 };
        assert!(hit.message(1000).contains("1h"));
        assert!(hit.message(1000).contains("free-on-us"));
    }
}
