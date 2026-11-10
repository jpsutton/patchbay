//! Grok subscription usage: the weekly pool of a SuperGrok, X Subscription or Grok Team seat.
//!
//! Grok Build asks its proxy for it (`GET /v1/billing?format=credits`, behind its `x.ai/billing`
//! extension and `/usage`). Patchbay asks the same question with the login a Grok provider uses and
//! reports the answer in the shape the Claude plan guard reads (`PlanLimits`: one weekly window
//! here, no 5-hour one), so the gauge, the usage limit, auto-resume and the mesh sharing work for a
//! Grok seat with no code of their own.
//!
//! The answer has a percentage of the included allowance, the period it belongs to, and the prepaid
//! and on-demand credit that is spent once the allowance is gone:
//!
//! ```json
//! {"config": {"creditUsagePercent": 1.0,
//!             "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY", "start": "...", "end": "..."},
//!             "prepaidBalance": {"val": 0}, "isUnifiedBillingUser": true},
//!  "subscription_tier": "SuperGrok"}
//! ```
//!
//! Older servers send the monthly shape (`monthlyLimit`, `used`, `billingPeriodEnd`). A monthly
//! period is not a weekly window, so it reports nothing rather than a gauge with the wrong span.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use super::plan_limits::{self, PlanLimits, Window};

/// How often the background loop looks for a running session on a Grok login.
pub const TICK: Duration = Duration::from_secs(60);
/// The least time between two requests for one login, however many turns report meanwhile.
pub const MIN_GAP_SECS: i64 = 15;

/// What one poll found.
#[derive(Debug, Clone, PartialEq)]
pub struct GrokPlan {
    /// `grok_auth::account`: the key the guard and the mesh know the seat by.
    pub account: String,
    /// Names the seat, not the team: each seat has its own weekly pool. `Grok seat · <team>`, else `Grok seat · <tier>`.
    pub name: Option<String>,
    pub limits: PlanLimits,
}

/// A `{"val": cents}` amount; proto3 JSON leaves out `val` when it is zero.
fn cents(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.get("val")).and_then(Value::as_i64).unwrap_or(0)
}

fn rfc3339_secs(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

/// The plan usage in a billing answer, as the guard reads it. `None` when the answer holds no
/// weekly window and no extra usage.
pub fn parse_billing(body: &Value, now: i64) -> Option<PlanLimits> {
    let c = body.get("config").filter(|c| c.is_object())?;
    let period = c.get("currentPeriod");
    let weekly = period
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t.ends_with("WEEKLY"));
    let end = period
        .and_then(|p| p.get("end"))
        .and_then(Value::as_str)
        .or_else(|| c.get("billingPeriodEnd").and_then(Value::as_str))
        .and_then(rfc3339_secs);
    // proto3 JSON leaves out zero, so a missing percentage next to a period means none used yet.
    let legacy = || {
        let limit = cents(c.get("monthlyLimit"));
        (limit > 0).then(|| cents(c.get("used")) as f64 / limit as f64 * 100.0)
    };
    let percent = c.get("creditUsagePercent").and_then(Value::as_f64).or_else(legacy);
    let percent = percent.or_else(|| (weekly && end.is_some()).then_some(0.0));
    let seven_day = match (weekly, percent, end) {
        (true, Some(p), Some(end)) => Some(Window { utilization: (p / 100.0).max(0.0), resets_at: end }),
        _ => None,
    };
    // Past the included allowance, a call is paid from prepaid credit or on demand.
    let spending = cents(c.get("onDemandUsed")) > 0 || cents(c.get("prepaidBalance")) > 0;
    let overage_until = (percent.is_some_and(|p| p >= 100.0) && spending).then_some(end).flatten();
    (seven_day.is_some() || overage_until.is_some()).then_some(PlanLimits {
        five_hour: None,
        seven_day,
        overage_until,
        observed_at: now,
    })
}

/// Last request per Grok home, for the minimum gap.
static LAST: Mutex<BTreeMap<String, i64>> = Mutex::new(BTreeMap::new());

/// True when `key` (a Grok home) may be asked now, and notes that it was.
pub fn may_poll(key: &str, now: i64) -> bool {
    let mut last = LAST.lock().unwrap();
    if last.get(key).is_some_and(|t| now - t < MIN_GAP_SECS) {
        return false;
    }
    last.insert(key.to_owned(), now);
    true
}

/// Ask the billing endpoint under `base_url` (a Grok provider's, `https://cli-chat-proxy.grok.com/v1`)
/// with the login in Grok home `home`.
pub async fn fetch(home: Option<&str>, base_url: &str) -> Result<GrokPlan> {
    let creds = crate::grok_auth::credentials(home).await?;
    let dir = crate::grok_auth::resolve_dir(home)?;
    let (account, team) = crate::grok_auth::account(&dir);
    let url = format!("{}/billing?format=credits", base_url.trim_end_matches('/'));
    let mut req = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?
        .get(&url)
        .bearer_auth(&creds.token);
    for (name, value) in crate::providers::grok_headers(Some(creds.user_id.as_str())) {
        req = req.header(name, value);
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let clipped: String = body.chars().take(300).collect();
        return Err(anyhow!("GET {url} returned {status}: {clipped}"));
    }
    let body: Value = resp.json().await.context("parse Grok billing")?;
    let limits = parse_billing(&body, plan_limits::now()).ok_or_else(|| anyhow!("Grok billing reported no weekly usage"))?;
    let tier = body.get("subscription_tier").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_owned);
    let name = match team.or(tier) {
        Some(n) => format!("Grok seat · {n}"),
        None => "Grok seat".to_owned(),
    };
    Ok(GrokPlan { account, name: Some(name), limits })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: i64 = 1_794_300_000;

    fn weekly(percent: f64) -> Value {
        json!({
            "config": {
                "creditUsagePercent": percent,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-11-05T20:56:22.553+00:00",
                    "end": "2026-11-12T20:56:22.553+00:00"
                },
                "prepaidBalance": { "val": 0 },
                "isUnifiedBillingUser": true
            },
            "subscription_tier": "SuperGrok"
        })
    }

    #[test]
    fn the_weekly_pool_becomes_the_weekly_window() {
        let l = parse_billing(&weekly(42.5), NOW).unwrap();
        assert_eq!(l.five_hour, None);
        let w = l.seven_day.unwrap();
        assert!((w.utilization - 0.425).abs() < 1e-9);
        assert_eq!(w.resets_at, 1_794_516_982);
        assert_eq!(l.overage_until, None);
        assert_eq!(l.observed_at, NOW);
    }

    #[test]
    fn a_pool_with_nothing_used_yet_still_has_a_window() {
        // proto3 JSON leaves out a zero percentage.
        let mut b = weekly(0.0);
        b["config"].as_object_mut().unwrap().remove("creditUsagePercent");
        let w = parse_billing(&b, NOW).unwrap().seven_day.unwrap();
        assert_eq!(w.utilization, 0.0);
    }

    #[test]
    fn spent_allowance_with_credit_to_draw_on_is_extra_usage() {
        let mut b = weekly(100.0);
        b["config"]["prepaidBalance"] = json!({ "val": 2500 });
        let l = parse_billing(&b, NOW).unwrap();
        assert_eq!(l.overage_until, Some(1_794_516_982));
        // Credit in the account while the allowance lasts is not extra usage.
        let mut b = weekly(40.0);
        b["config"]["prepaidBalance"] = json!({ "val": 2500 });
        assert_eq!(parse_billing(&b, NOW).unwrap().overage_until, None);
        // Nothing to draw on past the allowance: the guard's percentage threshold covers it.
        assert_eq!(parse_billing(&weekly(100.0), NOW).unwrap().overage_until, None);
        // On-demand spend counts too.
        let mut b = weekly(100.0);
        b["config"]["onDemandUsed"] = json!({ "val": 120 });
        assert!(parse_billing(&b, NOW).unwrap().overage_until.is_some());
    }

    #[test]
    fn a_monthly_period_is_not_a_weekly_gauge() {
        let mut b = weekly(10.0);
        b["config"]["currentPeriod"]["type"] = json!("USAGE_PERIOD_TYPE_MONTHLY");
        assert_eq!(parse_billing(&b, NOW), None);
        // The legacy shape has a monthly limit and no period type.
        let legacy = json!({ "config": {
            "monthlyLimit": { "val": 10000 }, "used": { "val": 2500 }, "billingPeriodEnd": "2026-12-01T00:00:00Z"
        }});
        assert_eq!(parse_billing(&legacy, NOW), None);
    }

    #[test]
    fn an_answer_with_no_config_reports_nothing() {
        assert_eq!(parse_billing(&json!({}), NOW), None);
        assert_eq!(parse_billing(&json!({ "config": null }), NOW), None);
        assert_eq!(parse_billing(&json!({ "config": {} }), NOW), None);
    }

    #[test]
    fn a_login_is_asked_at_most_once_per_gap() {
        let key = format!("test-home-{}", crate::db::new_id());
        assert!(may_poll(&key, 1000));
        assert!(!may_poll(&key, 1000 + MIN_GAP_SECS - 1));
        assert!(may_poll(&key, 1000 + MIN_GAP_SECS));
        assert!(may_poll(&format!("{key}-other"), 1000), "another login has its own gap");
    }

    /// Live check against xAI with the machine's Grok login.
    /// `cargo test --lib live_grok_billing -- --ignored --nocapture`
    #[tokio::test]
    // Manual test. If it needs your real home or logins, run it with PB_TEST_REAL_HOME=1 (see test_support).
    #[ignore]
    async fn live_grok_billing() {
        let plan = fetch(None, "https://cli-chat-proxy.grok.com/v1").await.expect("Grok billing");
        eprintln!("{plan:?}");
        assert!(plan.account.starts_with("grok:"));
        assert!(plan.limits.seven_day.is_some());
    }
}
