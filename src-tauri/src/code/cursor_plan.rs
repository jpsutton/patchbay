//! Cursor subscription plan usage, polled from Cursor's dashboard API (not over ACP).
//!
//! `cursor-agent acp` does not emit `usage_update` or plan limits. The interactive TUI and the
//! web dashboard get them from Connect RPCs on `api2.cursor.sh`. Patchbay polls
//! `GetCurrentPeriodUsage` (included plan + "free on us" bonus),
//! `GetUsageLimitStatusAndActiveGrants` (credit grants / slow pool — the IDE's "free credits"),
//! and `GetHardLimit` (whether on-demand spend is allowed). It shows Auto / API percentages plus
//! those free pools, and can stop Cursor ACP turns once included plan, plan bonus, and credit
//! grants are all gone and on-demand is off (`prefs.cursor_usage_limit`, off by default).
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
const GRANTS_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants";
const HARD_LIMIT_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetHardLimit";

/// One promotional / free-credit grant from `GetUsageLimitStatusAndActiveGrants` (IDE "free credits").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorCreditGrant {
    /// Cents still available on this grant.
    pub remaining_cents: i64,
    /// Original grant size, in cents.
    pub total_cents: i64,
    /// When the grant expires (unix seconds), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

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
    /// This is not the IDE's separate credit-grant pool — see `grants`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_bonus: Option<bool>,
    /// Cursor's dashboard sentence for the period (`displayMessage`), when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_message: Option<String>,
    /// Unexpired credit grants (IDE "free credits"), from `GetUsageLimitStatusAndActiveGrants`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<CursorCreditGrant>,
    /// Sum of `grants[].remaining_cents`. `Some(0)` means the grants RPC returned none left;
    /// absent means that RPC was not polled (older mesh reports) or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants_remaining_cents: Option<i64>,
    /// Cursor is serving from the slow pool (`usageLimitPolicyStatus.isInSlowPool`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_pool: Option<bool>,
    /// On-demand / usage-based spend is allowed (`!GetHardLimit.noUsageBasedAllowed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_demand_allowed: Option<bool>,
}

/// Included plan, free-on-us bonus, and credit grants are all spent (and on-demand is off); turns should stop.
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
        let display_message = body
            .get("displayMessage")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
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
            display_message,
            grants: Vec::new(),
            grants_remaining_cents: None,
            slow_pool: None,
            on_demand_allowed: None,
        })
    }

    /// Fold in `GetUsageLimitStatusAndActiveGrants`: credit grants and slow-pool flag.
    pub fn apply_grants(&mut self, body: &Value, now: i64) {
        self.slow_pool = body
            .pointer("/usageLimitPolicyStatus/isInSlowPool")
            .and_then(Value::as_bool);
        let mut grants = Vec::new();
        let mut remaining_sum = 0i64;
        if let Some(arr) = body.get("activeGrants").and_then(Value::as_array) {
            for g in arr {
                let remaining = json_cents(g.get("remainingCents")).unwrap_or(0);
                let total = json_cents(g.get("totalCents")).unwrap_or(0);
                let expires_at = ms_to_secs(g.get("expiresAtMs"));
                if expires_at.is_some_and(|t| t <= now) {
                    continue;
                }
                remaining_sum = remaining_sum.saturating_add(remaining.max(0));
                grants.push(CursorCreditGrant {
                    remaining_cents: remaining.max(0),
                    total_cents: total.max(0),
                    expires_at,
                });
            }
        }
        self.grants = grants;
        self.grants_remaining_cents = Some(remaining_sum);
    }

    /// Fold in `GetHardLimit`: whether on-demand spend is allowed.
    pub fn apply_hard_limit(&mut self, body: &Value) {
        if let Some(blocked) = body.get("noUsageBasedAllowed").and_then(Value::as_bool) {
            self.on_demand_allowed = Some(!blocked);
        }
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

    /// IDE credit grants still have balance. Unknown (`None`) does not count as available.
    pub fn grant_available(&self) -> bool {
        self.grants_remaining_cents.is_some_and(|n| n > 0)
    }

    /// Stop once included plan, free-on-us bonus, and credit grants are all gone, and on-demand is
    /// not allowed. While any of those still covers requests, Cursor keeps serving — so we must
    /// not stop on `remainingBonus: false` alone. If grants were never polled (`None`), refuse to
    /// stop: an incomplete reading used to false-stop accounts that still had free credits.
    /// After `resets_at`, nothing hits. No percent threshold — Cursor's pools are not request caps.
    pub fn hit(&self, now: i64) -> Option<Hit> {
        if self.resets_at.is_some_and(|t| t <= now) {
            return None;
        }
        if self.bonus_available() || !self.plan_exhausted() || self.grant_available() {
            return None;
        }
        if self.on_demand_allowed == Some(true) {
            return None;
        }
        if self.grants_remaining_cents.is_none() {
            return None;
        }
        Some(Hit { resets_at: self.resets_at.unwrap_or(now + 7 * 86400) })
    }
}

impl Hit {
    pub fn message(&self, now: i64) -> String {
        format!(
            "Your Cursor plan's included usage, free-on-us bonus, and free credits are used up, so Patchbay stops its Cursor turns. The cycle resets in {}. Change this under Settings › Coding agents › Cursor CLI.",
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

fn json_cents(v: Option<&Value>) -> Option<i64> {
    v.and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|n| n.round() as i64))
            .or_else(|| v.as_str()?.parse().ok())
    })
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
    use patchbay_agent::proc::NoWindow;
    let out = Command::new(program).args(["status", "--format", "json"]).no_window().output().ok()?;
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

async fn dashboard_post(client: &reqwest::Client, auth: &CursorAuth, url: &str) -> Result<Value> {
    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {}", auth.access_token))
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .body("{}")
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await?;
    if !status.is_success() {
        return Err(anyhow!("Cursor dashboard HTTP {status} ({url}): {body}"));
    }
    Ok(body)
}

/// Fetch period usage, credit grants, and on-demand allowance for the local Cursor login.
pub async fn fetch(auth: &CursorAuth) -> Result<CursorPlanLimits> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let (usage, grants, hard) = tokio::join!(
        dashboard_post(&client, auth, USAGE_URL),
        dashboard_post(&client, auth, GRANTS_URL),
        dashboard_post(&client, auth, HARD_LIMIT_URL),
    );
    let observed = now();
    let mut limits = CursorPlanLimits::from_response(
        auth.account_id.clone(),
        auth.email.clone(),
        &usage?,
        observed,
    )
    .ok_or_else(|| anyhow!("Cursor usage response has no planUsage (team seats often omit it)"))?;
    match grants {
        Ok(body) => limits.apply_grants(&body, observed),
        Err(e) => tracing::debug!("cursor plan grants: {e:#}"),
    }
    match hard {
        Ok(body) => limits.apply_hard_limit(&body),
        Err(e) => tracing::debug!("cursor plan hard limit: {e:#}"),
    }
    Ok(limits)
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

    /// Every account-id form `identity` can produce without running `cursor-agent` passes the sender gate
    /// (the `userId` form comes from `cursor-agent status` and is covered by the id table in the mesh
    /// tests): JWT `sub`, JWT email, the email hint, and the last-resort token fingerprint.
    #[test]
    fn every_identity_form_passes_the_sender_gate() {
        use base64::Engine as _;
        let gone = Path::new("/nonexistent/cursor-agent");
        let jwt = |claims: &str| format!("h.{}.s", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims));
        let forms = [
            identity(Some(gone), &jwt(r#"{"sub":"user_01HXYZ","email":"a@b.c"}"#), None).0,
            identity(Some(gone), &jwt(r#"{"email":"me@example.com"}"#), None).0,
            identity(Some(gone), "opaque", Some("hint@example.com".into())).0,
            identity(Some(gone), "crsr_0123456789abcdefghij", None).0,
        ];
        assert_eq!(forms[0], "user_01HXYZ");
        assert_eq!(forms[3], "token:crsr_0123456789a", "a truncated 16-character prefix");
        for id in forms {
            assert!(crate::is_portable_provider_account_id(&id), "{id:?} must pass the gate");
        }
    }

    fn sample(auto: f64, api: f64, total: f64, resets: i64) -> CursorPlanLimits {
        CursorPlanLimits {
            account_id: "42".into(),
            email: Some("a@b.c".into()),
            observed_at: 1000,
            resets_at: Some(resets),
            auto: Some(auto),
            api: Some(api),
            total: Some(total),
            // Exhausted included plan, no bonus, no grants, on-demand off: the guard may stop.
            included_spend_cents: Some(40000),
            limit_cents: Some(40000),
            bonus_spend_cents: Some(1200),
            remaining_bonus: Some(false),
            display_message: Some("You've hit your usage limit".into()),
            grants: Vec::new(),
            grants_remaining_cents: Some(0),
            slow_pool: Some(false),
            on_demand_allowed: Some(false),
        }
    }

    #[test]
    fn parses_dashboard_body() {
        let body = json!({
            "billingCycleEnd": "1795191155000",
            "displayMessage": "You've hit your usage limit",
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
        assert_eq!(l.display_message.as_deref(), Some("You've hit your usage limit"));
        assert!(l.plan_exhausted());
        assert!(!l.bonus_available());
        assert!(CursorPlanLimits::from_response("42".into(), None, &json!({}), 1).is_none());
    }

    #[test]
    fn applies_grants_and_hard_limit() {
        let mut l = sample(12.0, 96.0, 40.0, 9000);
        l.apply_grants(
            &json!({
                "usageLimitPolicyStatus": { "isInSlowPool": true },
                "activeGrants": [{
                    "remainingCents": "18903",
                    "totalCents": "30000",
                    "expiresAtMs": "1795191155000"
                }]
            }),
            1000,
        );
        assert_eq!(l.slow_pool, Some(true));
        assert_eq!(l.grants_remaining_cents, Some(18903));
        assert_eq!(l.grants.len(), 1);
        assert!(l.grant_available());
        assert!(l.hit(1000).is_none(), "grant still covers requests");

        l.apply_hard_limit(&json!({ "noUsageBasedAllowed": true }));
        assert_eq!(l.on_demand_allowed, Some(false));
        l.grants.clear();
        l.grants_remaining_cents = Some(0);
        assert!(l.hit(1000).is_some());

        l.apply_hard_limit(&json!({ "noUsageBasedAllowed": false }));
        assert_eq!(l.on_demand_allowed, Some(true));
        assert!(l.hit(1000).is_none(), "on-demand still allowed");
    }

    #[test]
    fn hits_only_when_plan_bonus_and_grants_are_gone() {
        let l = sample(12.0, 96.0, 40.0, 5000);
        assert!(l.hit(1000).is_some());
        assert!(l.hit(5000).is_none(), "past reset");
        let mut early = l.clone();
        early.included_spend_cents = Some(20000);
        assert!(early.hit(1000).is_none(), "included plan still has room");
        let mut unknown_grants = l.clone();
        unknown_grants.grants_remaining_cents = None;
        assert!(unknown_grants.hit(1000).is_none(), "incomplete grants reading must not stop");
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
        assert!(hit.message(1000).contains("free credits"));
    }
}
