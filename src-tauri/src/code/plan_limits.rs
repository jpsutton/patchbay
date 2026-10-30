//! Claude subscription limits (the 5-hour and weekly windows) as Claude Code and Patchbay's agent report them, and
//! the guard that stops its turns before they run past the plan into extra usage.
//!
//! claude-agent-acp forwards the SDK's `rate_limit_event` as a `usage_update` carrying
//! `_meta["_claude/rateLimit"]`. The SDK sends one only when the numbers change, and the adapter
//! drops one that comes before the turn's first reply, so a crossing can show up a step late.
//! Patchbay's own agent, on a Claude login, reads the same numbers from Anthropic's
//! `anthropic-ratelimit-unified-*` response headers and reports them in the same shape.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prefs::UsageLimit;

/// One usage window: the fraction used (0 to 1) and when it resets (unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub utilization: f64,
    pub resets_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanLimits {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Turns are billed as extra usage, past the plan, until this time (unix seconds).
    pub overage_until: Option<i64>,
    /// When Claude Code reported these (unix seconds).
    pub observed_at: i64,
}

/// One report kept for the usage panel's history of busy periods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub at: i64,
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

/// Samples older than this can't fall in a live window any more (the longest is seven days).
const HISTORY_SECS: i64 = 8 * 86400;

/// Adds a report to the history unless it repeats the last one, and drops samples too old to matter.
pub fn push_sample(history: &mut Vec<Sample>, limits: &PlanLimits) -> bool {
    let sample = Sample { at: limits.observed_at, five_hour: limits.five_hour, seven_day: limits.seven_day };
    if sample.five_hour.is_none() && sample.seven_day.is_none() {
        return false;
    }
    if history.last().is_some_and(|l| l.five_hour == sample.five_hour && l.seven_day == sample.seven_day) {
        return false;
    }
    history.push(sample);
    history.retain(|s| s.at > limits.observed_at - HISTORY_SECS);
    true
}

/// The window over the limit, for the message that explains a stop.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub what: &'static str,
    pub percent: u32,
    pub resets_at: i64,
}

impl PlanLimits {
    /// The limits in an ACP `usage_update`, if it carries any.
    pub fn from_update(update: &Value) -> Option<Self> {
        let info = &update["_meta"]["_claude/rateLimit"];
        if !info.is_object() {
            return None;
        }
        let window = |w: &Value| {
            Some(Window { utilization: w["utilization"].as_f64()?, resets_at: w["resetsAt"].as_i64()? })
        };
        let unified = &info["unifiedWindows"];
        let mut l = PlanLimits {
            five_hour: window(&unified["five_hour"]),
            seven_day: window(&unified["seven_day"]),
            overage_until: None,
            observed_at: now(),
        };
        // Older CLIs report only the window that is closest to its limit.
        if l.five_hour.is_none() && l.seven_day.is_none()
            && let Some(w) = window(info)
        {
            match info["rateLimitType"].as_str() {
                Some("five_hour") => l.five_hour = Some(w),
                Some(t) if t.starts_with("seven_day") => l.seven_day = Some(w),
                _ => {}
            }
        }
        if info["isUsingOverage"] == true {
            l.overage_until = info["resetsAt"]
                .as_i64()
                .or(l.five_hour.map(|w| w.resets_at))
                .or(l.seven_day.map(|w| w.resets_at));
        }
        Some(l)
    }

    /// The window at or past `percent` that resets last, ignoring windows that have reset.
    pub fn hit(&self, percent: u8, now: i64) -> Option<Hit> {
        let over = |what, w: Option<Window>| {
            w.filter(|w| w.resets_at > now && w.utilization * 100.0 >= f64::from(percent) - 1e-9)
                .map(|w| Hit { what, percent: (w.utilization * 100.0).round() as u32, resets_at: w.resets_at })
        };
        let overage = self
            .overage_until
            .filter(|t| *t > now)
            .map(|t| Hit { what: "extra usage", percent: 100, resets_at: t });
        [over("5-hour", self.five_hour), over("weekly", self.seven_day), overage]
            .into_iter()
            .flatten()
            .max_by_key(|h| h.resets_at)
    }
}

impl Hit {
    pub fn message(&self, limit: u8, now: i64) -> String {
        let used = if self.what == "extra usage" {
            "Your Claude plan is past its limits and using extra usage".to_owned()
        } else {
            format!("You have used {}% of your Claude plan's {} limit", self.percent, self.what)
        };
        format!(
            "{used}, so Patchbay stops its turns at {limit}%. The limit resets in {}. Change this under Settings › Coding agents.",
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

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The account's latest limits, shared by every session on a Claude plan on this node.
#[derive(Default)]
pub struct PlanGuard {
    latest: Mutex<Option<PlanLimits>>,
    history: Mutex<Vec<Sample>>,
    /// Sessions cancelled for the limit whose turn hasn't ended yet.
    stopping: Mutex<HashSet<String>>,
    /// Sessions the guard stopped, to prompt again once the limit lifts: id -> when it resets.
    resume: Mutex<BTreeMap<String, i64>>,
}

impl PlanGuard {
    pub fn record(&self, limits: PlanLimits) {
        *self.latest.lock().unwrap() = Some(limits);
    }

    /// Adds the report to the history; true if that changed it.
    pub fn record_history(&self, limits: &PlanLimits) -> bool {
        push_sample(&mut self.history.lock().unwrap(), limits)
    }

    pub fn set_history(&self, history: Vec<Sample>) {
        *self.history.lock().unwrap() = history;
    }

    pub fn history(&self) -> Vec<Sample> {
        self.history.lock().unwrap().clone()
    }

    /// The last limits Claude Code reported, kept for the usage panel and across restarts.
    pub fn latest(&self) -> Option<PlanLimits> {
        self.latest.lock().unwrap().clone()
    }

    pub fn hit(&self, limit: &UsageLimit, now: i64) -> Option<Hit> {
        if !limit.enabled {
            return None;
        }
        self.latest.lock().unwrap().as_ref()?.hit(limit.percent, now)
    }

    /// True the first time; the guard stops a turn once.
    pub fn start_stopping(&self, code_session_id: &str) -> bool {
        self.stopping.lock().unwrap().insert(code_session_id.to_owned())
    }

    pub fn turn_ended(&self, code_session_id: &str) {
        self.stopping.lock().unwrap().remove(code_session_id);
    }

    /// Prompt the session again once `resets_at` has passed and the limit no longer holds.
    pub fn schedule_resume(&self, code_session_id: &str, resets_at: i64) {
        self.resume.lock().unwrap().insert(code_session_id.to_owned(), resets_at);
    }

    /// The user prompted the session themselves, so it needn't be resumed. True if it was waiting.
    pub fn cancel_resume(&self, code_session_id: &str) -> bool {
        self.resume.lock().unwrap().remove(code_session_id).is_some()
    }

    /// Drop every waiting session; true if there were any.
    pub fn clear_resumes(&self) -> bool {
        let mut r = self.resume.lock().unwrap();
        let any = !r.is_empty();
        r.clear();
        any
    }

    /// Take the sessions whose limit reset at or before `now`.
    pub fn take_due(&self, now: i64) -> Vec<String> {
        let mut r = self.resume.lock().unwrap();
        let due: Vec<String> = r.iter().filter(|(_, at)| **at <= now).map(|(id, _)| id.clone()).collect();
        for id in &due {
            r.remove(id);
        }
        due
    }

    pub fn resumes(&self) -> BTreeMap<String, i64> {
        self.resume.lock().unwrap().clone()
    }

    pub fn set_resumes(&self, resumes: BTreeMap<String, i64>) {
        *self.resume.lock().unwrap() = resumes;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape Claude Code 2.1 sends, through claude-agent-acp.
    fn update(five: f64, seven: f64, overage: bool) -> Value {
        json!({
            "sessionUpdate": "usage_update", "used": 1000, "size": 200000,
            "_meta": { "_claude/rateLimit": {
                "status": "allowed", "resetsAt": 5000, "rateLimitType": "five_hour",
                "isUsingOverage": overage,
                "unifiedWindows": {
                    "five_hour": { "utilization": five, "resetsAt": 5000 },
                    "seven_day": { "utilization": seven, "resetsAt": 90000 }
                }
            }}
        })
    }

    #[test]
    fn parses_windows_and_ignores_plain_usage() {
        let l = PlanLimits::from_update(&update(0.19, 0.2, false)).unwrap();
        assert_eq!(l.five_hour, Some(Window { utilization: 0.19, resets_at: 5000 }));
        assert_eq!(l.seven_day.map(|w| w.resets_at), Some(90000));
        assert_eq!(l.overage_until, None);
        assert!(PlanLimits::from_update(&json!({ "used": 1, "size": 2 })).is_none());
    }

    #[test]
    fn history_skips_repeats_and_old_samples() {
        let at = |t: i64, five: f64| PlanLimits {
            five_hour: Some(Window { utilization: five, resets_at: t + 100 }),
            observed_at: t,
            ..Default::default()
        };
        let mut h = Vec::new();
        assert!(push_sample(&mut h, &at(1000, 0.1)));
        // The same windows reported later add nothing, and neither does a report with no windows.
        let repeat = PlanLimits { observed_at: 2000, ..at(1000, 0.1) };
        assert!(!push_sample(&mut h, &repeat));
        assert!(!push_sample(&mut h, &PlanLimits { observed_at: 3000, ..Default::default() }));
        assert!(push_sample(&mut h, &at(1000 + HISTORY_SECS + 5, 0.2)));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn falls_back_to_the_representative_window() {
        let u = json!({ "_meta": { "_claude/rateLimit": {
            "status": "allowed_warning", "utilization": 0.97, "resetsAt": 9000, "rateLimitType": "seven_day"
        }}});
        let l = PlanLimits::from_update(&u).unwrap();
        assert_eq!(l.five_hour, None);
        assert_eq!(l.seven_day, Some(Window { utilization: 0.97, resets_at: 9000 }));
    }

    #[test]
    fn hits_at_the_threshold_until_the_window_resets() {
        let l = PlanLimits::from_update(&update(0.95, 0.4, false)).unwrap();
        let hit = l.hit(95, 1000).unwrap();
        assert_eq!((hit.what, hit.percent, hit.resets_at), ("5-hour", 95, 5000));
        assert!(l.hit(96, 1000).is_none());
        // Once the 5-hour window has reset, its old numbers no longer count.
        assert!(l.hit(95, 5000).is_none());
    }

    #[test]
    fn reports_the_window_that_resets_last() {
        let l = PlanLimits::from_update(&update(0.99, 0.96, false)).unwrap();
        assert_eq!(l.hit(95, 1000).unwrap().what, "weekly");
    }

    #[test]
    fn extra_usage_always_hits() {
        let l = PlanLimits::from_update(&update(0.5, 0.5, true)).unwrap();
        let hit = l.hit(100, 1000).unwrap();
        assert_eq!((hit.what, hit.resets_at), ("extra usage", 5000));
        assert!(hit.message(100, 1000).contains("extra usage"));
    }

    #[test]
    fn guard_respects_the_switch_and_stops_once() {
        let g = PlanGuard::default();
        g.record(PlanLimits::from_update(&update(0.97, 0.1, false)).unwrap());
        let on = UsageLimit { enabled: true, percent: 95, auto_resume: false };
        assert!(g.hit(&on, 1000).is_some());
        assert!(g.hit(&UsageLimit { enabled: false, ..on }, 1000).is_none());
        assert!(g.start_stopping("s1"));
        assert!(!g.start_stopping("s1"));
        g.turn_ended("s1");
        assert!(g.start_stopping("s1"));
    }

    #[test]
    fn resumes_come_due_at_their_reset() {
        let g = PlanGuard::default();
        g.schedule_resume("a", 1000);
        g.schedule_resume("b", 2000);
        g.schedule_resume("c", 3000);
        assert!(g.take_due(999).is_empty());
        assert_eq!(g.take_due(2000), ["a", "b"]);
        assert!(g.take_due(2500).is_empty(), "taken once");
        assert!(g.cancel_resume("c"));
        assert!(!g.cancel_resume("c"));
        g.schedule_resume("d", 4000);
        assert!(g.clear_resumes());
        assert!(g.resumes().is_empty());
    }

    #[test]
    fn message_says_when_it_resets() {
        let hit = Hit { what: "5-hour", percent: 96, resets_at: 1000 + 72 * 60 };
        assert_eq!(
            hit.message(95, 1000),
            "You have used 96% of your Claude plan's 5-hour limit, so Patchbay stops its turns at 95%. The limit resets in 1h 12m. Change this under Settings › Coding agents."
        );
        assert_eq!(fmt_wait(3 * 86400 + 7200), "3d 2h");
        assert_eq!(fmt_wait(10), "1m");
    }
}
