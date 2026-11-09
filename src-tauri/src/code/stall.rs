//! Turns that go quiet (pb-20261107-17). An agent can hold `session/prompt` open with nothing
//! more to say: opencode did when its free-tier model hit a usage limit mid-turn, and the turn
//! then looked busy for hours. Every event a session's agent sends counts as activity; a turn with
//! none for `Preferences::stall_notice_minutes` is marked stalled once, until the agent sends
//! anything again or the turn ends. Nothing is cancelled: a long shell command or slow thinking
//! looks the same from here, so the user decides.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often busy sessions are checked.
pub const TICK: Duration = Duration::from_secs(30);
/// `stall_notice_minutes` when unset.
pub const DEFAULT_MINUTES: u32 = 10;

#[derive(Default)]
pub struct StallWatch {
    /// When each session's agent last sent anything (monotonic, for the check) and the same
    /// moment as Unix milliseconds (for anyone asking when).
    last: Mutex<HashMap<String, (Instant, i64)>>,
    /// Sessions currently marked stalled.
    stalled: Mutex<HashSet<String>>,
}

/// What a session is doing, as `list_agent_sessions` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// The agent isn't running.
    Stopped,
    /// Running, between turns.
    Idle,
    /// Mid-turn.
    Running,
    /// Mid-turn and waiting on a person: a permission, a question or a sign-in.
    Waiting,
    /// Mid-turn with nothing from the agent for the stall limit.
    Stalled,
}

impl Activity {
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Stopped => "stopped",
            Activity::Idle => "idle",
            Activity::Running => "running",
            Activity::Waiting => "waiting",
            Activity::Stalled => "stalled",
        }
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl StallWatch {
    /// The agent sent something. Returns true when that ends a stall.
    pub fn touch(&self, id: &str) -> bool {
        self.touch_at(id, Instant::now(), now_ms())
    }

    pub fn touch_at(&self, id: &str, at: Instant, at_ms: i64) -> bool {
        self.last.lock().unwrap().insert(id.to_owned(), (at, at_ms));
        self.stalled.lock().unwrap().remove(id)
    }

    /// The session's agent is gone: forget it.
    pub fn forget(&self, id: &str) {
        self.last.lock().unwrap().remove(id);
        self.stalled.lock().unwrap().remove(id);
    }

    pub fn is_stalled(&self, id: &str) -> bool {
        self.stalled.lock().unwrap().contains(id)
    }

    /// When the session's agent last sent anything this run, as Unix milliseconds.
    pub fn last_activity_ms(&self, id: &str) -> Option<i64> {
        self.last.lock().unwrap().get(id).map(|(_, ms)| *ms)
    }

    /// Of the `busy` sessions not waiting on a person, the ones quiet for `limit` that aren't
    /// marked yet, with how long they have been quiet. They are marked as they are returned, so
    /// each stall is reported once. A busy session never seen is timed from now.
    pub fn due(&self, busy: &[(String, bool)], now: Instant, limit: Duration) -> Vec<(String, Duration)> {
        let mut last = self.last.lock().unwrap();
        let mut stalled = self.stalled.lock().unwrap();
        let mut out = Vec::new();
        for (id, waiting_on_person) in busy {
            let (since, _) = *last.entry(id.clone()).or_insert((now, now_ms()));
            if *waiting_on_person || stalled.contains(id) {
                continue;
            }
            let quiet = now.saturating_duration_since(since);
            if quiet >= limit {
                stalled.insert(id.clone());
                out.push((id.clone(), quiet));
            }
        }
        out
    }
}

/// Classify a session from what the manager knows about it.
pub fn activity(running: bool, busy: bool, waiting_on_person: bool, stalled: bool) -> Activity {
    match (running, busy) {
        (false, _) => Activity::Stopped,
        (true, false) => Activity::Idle,
        (true, true) if waiting_on_person => Activity::Waiting,
        (true, true) if stalled => Activity::Stalled,
        (true, true) => Activity::Running,
    }
}

/// The transcript line for a stall.
pub fn notice(quiet: Duration) -> String {
    let mins = (quiet.as_secs() / 60).max(1);
    format!(
        "Nothing from the agent for {mins} minute{}. It may be stuck: a usage limit, a lost connection, \
         or a command that never returns. Patchbay hasn't stopped it; press Stop, or switch the model, \
         if it doesn't carry on.",
        if mins == 1 { "" } else { "s" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(600);

    #[test]
    fn a_quiet_turn_is_reported_once() {
        let w = StallWatch::default();
        let t0 = Instant::now();
        w.touch_at("s", t0, 1);
        let busy = vec![("s".to_owned(), false)];
        assert!(
            w.due(&busy, t0 + Duration::from_secs(599), LIMIT).is_empty(),
            "under the limit"
        );
        let due = w.due(&busy, t0 + LIMIT, LIMIT);
        assert_eq!(due, vec![("s".to_owned(), LIMIT)]);
        assert!(w.is_stalled("s"));
        assert!(w.due(&busy, t0 + 2 * LIMIT, LIMIT).is_empty(), "reported once");
    }

    #[test]
    fn activity_ends_a_stall_and_restarts_the_clock() {
        let w = StallWatch::default();
        let t0 = Instant::now();
        w.touch_at("s", t0, 1);
        let busy = vec![("s".to_owned(), false)];
        assert_eq!(w.due(&busy, t0 + LIMIT, LIMIT).len(), 1);
        assert!(
            w.touch_at("s", t0 + LIMIT + Duration::from_secs(1), 2),
            "touch reports the stall ended"
        );
        assert!(!w.is_stalled("s"));
        assert_eq!(w.last_activity_ms("s"), Some(2));
        assert!(
            w.due(&busy, t0 + LIMIT + Duration::from_secs(60), LIMIT).is_empty(),
            "clock restarted"
        );
        assert!(
            !w.touch_at("s", t0 + LIMIT + Duration::from_secs(61), 3),
            "not stalled: nothing ended"
        );
    }

    #[test]
    fn waiting_on_a_person_is_not_a_stall() {
        let w = StallWatch::default();
        let t0 = Instant::now();
        w.touch_at("s", t0, 1);
        assert!(w.due(&[("s".to_owned(), true)], t0 + 3 * LIMIT, LIMIT).is_empty());
        assert!(!w.is_stalled("s"));
    }

    #[test]
    fn an_unseen_busy_session_is_timed_from_the_first_check() {
        let w = StallWatch::default();
        let t0 = Instant::now();
        let busy = vec![("new".to_owned(), false)];
        assert!(w.due(&busy, t0, LIMIT).is_empty());
        assert_eq!(w.due(&busy, t0 + LIMIT, LIMIT).len(), 1);
    }

    #[test]
    fn forget_clears_everything() {
        let w = StallWatch::default();
        let t0 = Instant::now();
        w.touch_at("s", t0, 1);
        w.due(&[("s".to_owned(), false)], t0 + LIMIT, LIMIT);
        w.forget("s");
        assert!(!w.is_stalled("s"));
        assert_eq!(w.last_activity_ms("s"), None);
    }

    #[test]
    fn activity_classifies() {
        assert_eq!(activity(false, true, false, true), Activity::Stopped);
        assert_eq!(activity(true, false, false, false), Activity::Idle);
        assert_eq!(
            activity(true, true, true, true),
            Activity::Waiting,
            "a person is the reason"
        );
        assert_eq!(activity(true, true, false, true), Activity::Stalled);
        assert_eq!(activity(true, true, false, false), Activity::Running);
    }

    #[test]
    fn notice_says_how_long_and_that_nothing_was_stopped() {
        assert!(notice(Duration::from_secs(600)).starts_with("Nothing from the agent for 10 minutes."));
        assert!(notice(Duration::from_secs(30)).contains("for 1 minute."));
        assert!(notice(LIMIT).contains("hasn't stopped it"));
    }
}
