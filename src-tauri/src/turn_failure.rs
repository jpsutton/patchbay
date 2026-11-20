//! pb-20261111-28: the turn-lifecycle detector for a peer message's turn.
//!
//! ONE place decides a turn ended and how: [`TurnTracker::observe`] reads the code events of a
//! session in order and, when a turn ends, returns one [`TurnEnded`] (outcome `ok`, `cancelled`,
//! `error` or `aborted`, the turn id and every peer message the turn carried, including ones
//! steered into it). The delivery log writes the `msg_turn_ended` / `msg_steered` lines from it
//! and the router (`lib.rs`) turns an `error` end into a `turn_failure_notice` for each origin
//! sender. Neither of them infers a failure on its own.
//!
//! Message text is held in memory only, so the one bounded re-wake can replay it. It is never
//! logged and never put in a notice.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::code::acp::{CodeEvent, Delivery, is_agent_message_origin};

/// Notice texts start with one of these. A notice that lands in a turn that then fails bounces
/// nothing back (loop guard): this is the one place that knows what a notice looks like.
pub const NOTICE_TAG: &str = "turn_failure_notice";
const DROPPED_NOTICE_PREFIX: &str = "Patchbay dropped your queued message";

/// Whether `text` is one of Patchbay's own notices to a sender.
pub fn is_notice_text(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with(NOTICE_TAG) || t.starts_with(DROPPED_NOTICE_PREFIX)
}

/// A peer message that was part of a turn.
#[derive(Clone)]
pub struct TurnMessage {
    pub id: String,
    /// `agent:<node>:<session>`.
    pub origin: String,
    /// Kept for the bounded re-wake only (memory, never logged).
    pub text: String,
}

/// What a failed turn reported: a structured code and, when the agent gave one, the reset time.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub kind: String,
    pub resets_at: Option<u64>,
}

/// A turn that has ended.
#[derive(Clone)]
pub struct TurnEnded {
    pub session_id: String,
    pub turn_id: String,
    /// `ok`, `cancelled`, `error` or `aborted`.
    pub outcome: &'static str,
    pub messages: Vec<TurnMessage>,
    pub tool_calls: u32,
    pub failure: Option<Failure>,
}

/// A peer message joined a running turn.
#[derive(Clone, Debug, PartialEq)]
pub struct Steered {
    pub turn_id: String,
    pub message_id: String,
    pub origin: String,
}

#[derive(Default)]
pub struct Observed {
    pub steered: Option<Steered>,
    pub ended: Option<TurnEnded>,
}

struct OpenTurn {
    turn_id: String,
    messages: Vec<TurnMessage>,
    tool_calls: u32,
    failure: Option<Failure>,
    /// The agent process reported an error during the turn (a crash looks like this before exit).
    errored: bool,
}

#[derive(Default)]
pub struct TurnTracker {
    open: Mutex<HashMap<String, OpenTurn>>,
}

impl TurnTracker {
    pub fn observe(&self, session: &str, event: &CodeEvent) -> Observed {
        let mut open = self.open.lock().unwrap();
        let mut out = Observed::default();
        match event {
            CodeEvent::UserPrompt { prompt_id, text, origin, delivery: Delivery::Sent, .. } => {
                let messages = if is_agent_message_origin(origin) {
                    vec![TurnMessage { id: prompt_id.clone(), origin: origin.clone(), text: text.clone() }]
                } else {
                    Vec::new()
                };
                open.insert(
                    session.to_owned(),
                    OpenTurn { turn_id: prompt_id.clone(), messages, tool_calls: 0, failure: None, errored: false },
                );
            }
            CodeEvent::UserPrompt { prompt_id, text, origin, delivery: Delivery::Steered, .. }
                if is_agent_message_origin(origin) =>
            {
                if let Some(t) = open.get_mut(session) {
                    t.messages.push(TurnMessage { id: prompt_id.clone(), origin: origin.clone(), text: text.clone() });
                    out.steered = Some(Steered {
                        turn_id: t.turn_id.clone(),
                        message_id: prompt_id.clone(),
                        origin: origin.clone(),
                    });
                }
            }
            CodeEvent::Update { update } if update["sessionUpdate"] == "tool_call" => {
                if let Some(t) = open.get_mut(session) {
                    t.tool_calls += 1;
                }
            }
            CodeEvent::Error { .. } => {
                if let Some(t) = open.get_mut(session) {
                    t.errored = true;
                }
            }
            CodeEvent::TurnFailed { prompt_id, origin, kind, resets_at, .. } => {
                let failure = Failure { kind: kind.clone(), resets_at: *resets_at };
                match open.get_mut(session) {
                    Some(t) => t.failure = Some(failure),
                    // A failure for a turn whose start was never seen (the app restarted into it):
                    // it still ended, and its message still has a sender waiting.
                    None => {
                        let messages = if is_agent_message_origin(origin) {
                            vec![TurnMessage { id: prompt_id.clone(), origin: origin.clone(), text: String::new() }]
                        } else {
                            Vec::new()
                        };
                        open.insert(
                            session.to_owned(),
                            OpenTurn { turn_id: prompt_id.clone(), messages, tool_calls: 0, failure: Some(failure), errored: false },
                        );
                    }
                }
            }
            CodeEvent::PromptDone { stop_reason } => {
                if let Some(t) = open.remove(session) {
                    let outcome = if t.failure.is_some() {
                        "error"
                    } else if stop_reason == "cancelled" {
                        "cancelled"
                    } else {
                        "ok"
                    };
                    out.ended = Some(ended(session, t, outcome, None));
                }
            }
            CodeEvent::Status { state, .. } if state == "exited" => {
                if let Some(t) = open.remove(session) {
                    // A crash reports an error before the process goes; a Stop or a quit does not,
                    // and nobody failed then.
                    let crash = t.errored.then(|| Failure { kind: "adapter_crash".into(), resets_at: None });
                    out.ended = Some(ended(session, t, "aborted", crash));
                }
            }
            _ => {}
        }
        out
    }
}

fn ended(session: &str, t: OpenTurn, outcome: &'static str, failure: Option<Failure>) -> TurnEnded {
    TurnEnded {
        session_id: session.to_owned(),
        turn_id: t.turn_id,
        outcome,
        messages: t.messages,
        tool_calls: t.tool_calls,
        failure: t.failure.or(failure),
    }
}

/// What the router has already done, so a message gets one notice and one re-wake at most.
#[derive(Default)]
pub struct NoticeBook {
    noticed: Mutex<HashSet<String>>,
    rewoken: Mutex<HashSet<String>>,
}

impl NoticeBook {
    /// True the first time `message_id` is asked about: exactly one notice per origin message.
    pub fn first_notice(&self, message_id: &str) -> bool {
        self.noticed.lock().unwrap().insert(message_id.to_owned())
    }

    /// True the first time for this message AND this text to this session: one re-wake at most. The
    /// re-delivery is a new message to the same session with the same text, so it is never re-woken.
    pub fn first_rewake(&self, message_id: &str, session: &str, text: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (session, text).hash(&mut h);
        let mut r = self.rewoken.lock().unwrap();
        let (a, b) = (r.insert(message_id.to_owned()), r.insert(format!("{session}:{:x}", h.finish())));
        a && b
    }
}

/// A usage or rate limit: the one class a re-wake may replay, and only for a turn that made no
/// tool call (replaying one that did could repeat its side effects).
pub fn is_limit_kind(kind: &str) -> bool {
    kind == "usage_limit_exceeded" || kind.contains("rate_limit")
}

/// Whether a failed turn may get the one re-wake: a limit, and no tool call before it failed (a
/// replay of a turn that acted could repeat its side effects).
pub fn may_rewake(failure: &Failure, tool_calls: u32) -> bool {
    is_limit_kind(&failure.kind) && tool_calls == 0
}

/// The notice a sender gets. Built from structured fields ONLY: error kind, reset time and the
/// recipient's id and title. Never error text, an account, an org or a plan (privacy matrix).
pub fn notice_json(message_id: &str, target_id: &str, title: &str, failure: &Failure, rewake_at: Option<u64>) -> serde_json::Value {
    let tail = match (rewake_at, failure.resets_at) {
        (Some(_), _) => "Patchbay will send it once more after the reset (that is lost if Patchbay quits first). Nothing else is retried.",
        (None, Some(_)) => "Patchbay will not retry it; decide whether to wait until the reset, log it or reroute.",
        (None, None) => "No reset time was given and Patchbay will not retry it; decide whether to wait, log it or reroute.",
    };
    serde_json::json!({
        "type": NOTICE_TAG,
        "messageId": message_id,
        "targetSessionId": target_id,
        "targetTitle": title,
        "errorKind": failure.kind,
        "resetsAt": failure.resets_at,
        "rewakeAt": rewake_at,
        "message": format!(
            "Your message {message_id} to session '{title}' ({target_id}) was delivered but its turn failed ({}) and did not complete. {tail}",
            failure.kind
        ),
    })
}

/// `resets_at` as epoch seconds: agents send seconds or milliseconds, as sent.
pub fn reset_seconds(resets_at: u64) -> u64 {
    if resets_at > 100_000_000_000 { resets_at / 1000 } else { resets_at }
}

/// The reset time as a local clock text, `try again at 2:37 PM`: no date and no zone, so it is the
/// NEXT occurrence of that local time after `now` (a later hour is today, an earlier one tomorrow).
/// `offset_secs` is the local zone's offset from UTC at `now`. Anything else is `None`, and the
/// caller then only notifies, never retries.
pub fn parse_local_reset(text: &str, now: i64, offset_secs: i32) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let after = &lower[lower.find("try again at ")? + "try again at ".len()..];
    let (clock, rest) = after.split_once(' ')?;
    let (h, m) = clock.split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    let hour = match rest.trim_start().get(..2)? {
        "am" if (1..=12).contains(&h) => h % 12,
        "pm" if (1..=12).contains(&h) => h % 12 + 12,
        _ => return None,
    };
    if !(0..60).contains(&m) {
        return None;
    }
    let local_now = now + i64::from(offset_secs);
    let mut at = local_now - local_now.rem_euclid(86_400) + hour * 3600 + m * 60;
    if at <= local_now {
        at += 86_400;
    }
    u64::try_from(at - i64::from(offset_secs)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ORIGIN: &str = "agent:node-a:session-src";

    fn prompt(id: &str, origin: &str, delivery: Delivery) -> CodeEvent {
        CodeEvent::UserPrompt { prompt_id: id.into(), text: "BODY".into(), origin: origin.into(), attachments: json!([]), delivery }
    }

    fn failed(id: &str, kind: &str, resets_at: Option<u64>) -> CodeEvent {
        CodeEvent::TurnFailed { prompt_id: id.into(), origin: ORIGIN.into(), kind: kind.into(), resets_at }
    }

    fn done(reason: &str) -> CodeEvent {
        CodeEvent::PromptDone { stop_reason: json!(reason) }
    }

    fn ends(t: &TurnTracker, events: Vec<CodeEvent>) -> Vec<TurnEnded> {
        events.iter().filter_map(|e| t.observe("tgt", e).ended).collect()
    }

    /// Every way a turn can end says so: ok, cancelled, error and aborted each produce one ended
    /// record naming the turn and its message.
    #[test]
    fn every_outcome_ends_the_turn_with_its_message() {
        let t = TurnTracker::default();
        let ok = ends(&t, vec![prompt("m1", ORIGIN, Delivery::Sent), done("end_turn")]);
        assert_eq!((ok[0].outcome, ok[0].turn_id.as_str(), ok[0].messages[0].id.as_str()), ("ok", "m1", "m1"));
        let stopped = ends(&t, vec![prompt("m2", ORIGIN, Delivery::Sent), done("cancelled")]);
        assert_eq!(stopped[0].outcome, "cancelled");
        assert!(stopped[0].failure.is_none(), "a user Stop is not a failure");
        let err = ends(&t, vec![prompt("m3", ORIGIN, Delivery::Sent), failed("m3", "usage_limit_exceeded", Some(5)), done("error")]);
        assert_eq!(err[0].outcome, "error");
        assert_eq!(err[0].failure, Some(Failure { kind: "usage_limit_exceeded".into(), resets_at: Some(5) }));
        let aborted = ends(&t, vec![prompt("m4", ORIGIN, Delivery::Sent), CodeEvent::Status { state: "exited".into(), message: None }]);
        assert_eq!(aborted[0].outcome, "aborted");
        assert!(aborted[0].failure.is_none(), "a Stop or quit kills the agent without anyone failing");
        // Nothing open: nothing ends.
        assert!(ends(&t, vec![done("end_turn")]).is_empty());
    }

    /// A message steered into a running turn is correlated with it, and the turn's end covers it.
    #[test]
    fn a_steered_message_joins_the_turn_and_its_end_covers_it() {
        let t = TurnTracker::default();
        t.observe("tgt", &prompt("m1", "me", Delivery::Sent));
        let s = t.observe("tgt", &prompt("m2", ORIGIN, Delivery::Steered)).steered.unwrap();
        assert_eq!((s.turn_id.as_str(), s.message_id.as_str()), ("m1", "m2"));
        t.observe("tgt", &failed("m1", "auth_required", None));
        let e = t.observe("tgt", &done("error")).ended.unwrap();
        assert_eq!(e.outcome, "error");
        assert_eq!(e.messages.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["m2"]);
        // A steered message with no running turn joins nothing.
        assert!(t.observe("other", &prompt("m9", ORIGIN, Delivery::Steered)).steered.is_none());
    }

    #[test]
    fn tool_calls_are_counted_per_turn() {
        let t = TurnTracker::default();
        t.observe("tgt", &prompt("m1", ORIGIN, Delivery::Sent));
        let call = CodeEvent::Update { update: json!({ "sessionUpdate": "tool_call", "toolCallId": "t1" }) };
        t.observe("tgt", &call);
        t.observe("tgt", &CodeEvent::Update { update: json!({ "sessionUpdate": "agent_message_chunk" }) });
        t.observe("tgt", &failed("m1", "usage_limit_exceeded", None));
        assert_eq!(t.observe("tgt", &done("error")).ended.unwrap().tool_calls, 1);
        t.observe("tgt", &prompt("m2", ORIGIN, Delivery::Sent));
        t.observe("tgt", &failed("m2", "usage_limit_exceeded", None));
        assert_eq!(t.observe("tgt", &done("error")).ended.unwrap().tool_calls, 0, "a new turn starts at zero");
    }

    #[test]
    fn a_crash_is_a_failure_and_a_stop_is_not() {
        let t = TurnTracker::default();
        t.observe("tgt", &prompt("m1", ORIGIN, Delivery::Sent));
        t.observe("tgt", &CodeEvent::Error { message: "panicked".into() });
        let e = t.observe("tgt", &CodeEvent::Status { state: "exited".into(), message: None }).ended.unwrap();
        assert_eq!((e.outcome, e.failure.map(|f| f.kind)), ("aborted", Some("adapter_crash".to_owned())));
    }

    #[test]
    fn a_notice_is_recognised_by_its_text() {
        assert!(is_notice_text("turn_failure_notice {\"type\":\"turn_failure_notice\"}"));
        assert!(is_notice_text("Patchbay dropped your queued message m1 to session 'x'"));
        assert!(!is_notice_text("please run the tests"));
    }

    #[test]
    fn one_notice_and_one_rewake_per_message() {
        let b = NoticeBook::default();
        assert!(b.first_notice("m1") && !b.first_notice("m1") && b.first_notice("m2"));
        assert!(b.first_rewake("m1", "tgt", "go") && !b.first_rewake("m1", "tgt", "go"));
        // The re-delivery is a new message with the same text to the same session: not re-woken again.
        assert!(!b.first_rewake("m3", "tgt", "go"), "a re-delivery is never re-woken");
        assert!(b.first_rewake("m4", "tgt", "something else") && b.first_rewake("m5", "other", "go"));
    }

    /// A re-wake needs a limit AND zero tool calls: a turn that acted is NEVER replayed.
    #[test]
    fn a_turn_with_a_tool_call_is_never_replayed() {
        let limit = Failure { kind: "usage_limit_exceeded".into(), resets_at: Some(5) };
        assert!(may_rewake(&limit, 0));
        assert!(!may_rewake(&limit, 1), "one tool call and the turn is not replayable");
        assert!(!may_rewake(&limit, 40));
        for kind in ["auth_required", "adapter_crash", "rpc_-32603"] {
            assert!(!may_rewake(&Failure { kind: kind.into(), resets_at: Some(5) }, 0), "{kind} is not a limit");
        }
    }

    /// Privacy matrix: whatever the error carried, the notice holds only the error kind, the reset
    /// time, and the recipient's id and title.
    #[test]
    fn the_notice_carries_only_the_allowed_fields() {
        let f = Failure { kind: "usage_limit_exceeded".into(), resets_at: Some(1_794_449_520) };
        let n = notice_json("m1", "tgt-id", "Build seat", &f, Some(1_794_449_600));
        let mut keys: Vec<_> = n.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["errorKind", "message", "messageId", "resetsAt", "rewakeAt", "targetSessionId", "targetTitle", "type"]);
        assert_eq!((n["errorKind"].as_str(), n["resetsAt"].as_u64(), n["targetTitle"].as_str()), (Some("usage_limit_exceeded"), Some(1_794_449_520), Some("Build seat")));
        // No reset time: the notice says so and offers no re-wake.
        let n = notice_json("m1", "tgt-id", "Build seat", &Failure { kind: "adapter_crash".into(), resets_at: None }, None);
        assert!(n["resetsAt"].is_null() && n["rewakeAt"].is_null());
        assert!(n["message"].as_str().unwrap().contains("will not retry"));
    }

    #[test]
    fn only_limit_kinds_are_replayable() {
        assert!(is_limit_kind("usage_limit_exceeded") && is_limit_kind("rate_limit_exceeded"));
        assert!(!is_limit_kind("auth_required") && !is_limit_kind("adapter_crash") && !is_limit_kind("rpc_-32603"));
        assert_eq!(reset_seconds(1_794_449_520), 1_794_449_520);
        assert_eq!(reset_seconds(1_794_449_520_000), 1_794_449_520);
    }

    /// `try again at H:MM AM/PM` is the NEXT occurrence of that local time: later today, or
    /// tomorrow when it already passed (across midnight), in the zone given.
    #[test]
    fn the_reset_clock_text_is_the_next_occurrence_across_midnight() {
        let day = 1_794_441_600; // a UTC midnight
        let tz = -5 * 3600; // local = UTC-5
        let local = |h: i64, m: i64| day + h * 3600 + m * 60 - i64::from(tz); // epoch of local day-0 H:MM
        // 2:00 PM local now; "2:37 PM" is later today, "9:52 AM" already passed: tomorrow.
        let now = local(14, 0) as i64;
        assert_eq!(parse_local_reset("... try again at 2:37 PM.", now, tz), Some(local(14, 37) as u64));
        assert_eq!(parse_local_reset("try again at 9:52 AM.", now, tz), Some(local(9, 52) as u64 + 86_400));
        // 11:30 PM local now: "12:10 AM" is after midnight (tomorrow), "11:45 PM" is still today.
        let late = local(23, 30) as i64;
        assert_eq!(parse_local_reset("try again at 12:10 AM", late, tz), Some(local(0, 10) as u64 + 86_400));
        assert_eq!(parse_local_reset("try again at 11:45 PM", late, tz), Some(local(23, 45) as u64));
        // 12:xx is noon (PM) or just after midnight (AM); a time that is exactly now means tomorrow.
        assert_eq!(parse_local_reset("try again at 12:00 PM", now, tz), Some(local(12, 0) as u64 + 86_400));
        assert_eq!(parse_local_reset("try again at 2:00 PM", now, tz), Some(local(14, 0) as u64 + 86_400));
        // Unparseable: no time, never a guess.
        for bad in ["try again later", "try again at 25:00 PM", "try again at 2:99 PM", "try again at 2:37", "no clock here", ""] {
            assert_eq!(parse_local_reset(bad, now, tz), None, "{bad}");
        }
    }
}
