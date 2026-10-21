//! Remote members' live view, after mosh's state synchronization: every live event belongs to a
//! topic and carries a point `(stream, seq)`. A remote member that comes back with the last point
//! it applied gets the events after it, merged into the smallest equivalent set, or a snapshot when
//! they're gone. See docs/design/mobile.md, section 3.

use std::collections::{BTreeSet, HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Code session events (an envelope with `codeSessionId` and `event`).
pub const EV_CODE: &str = "code://event";
/// Chat stream events (`conversationId`, `streamer`, `streamerName`, `event`).
pub const EV_CHAT: &str = "mesh://chat";
/// Gateway to remote: a Code session's live summary changed (`id`, `live`).
pub const EV_LIVE: &str = "remote://live";
/// Gateway to remote: conversations or Code sessions changed; refetch the lists.
pub const EV_INDEX: &str = "remote://index-changed";
/// The topic carrying [`EV_LIVE`] and [`EV_INDEX`].
pub const TOPIC_INDEX: &str = "index";
/// Where an event's point sits in its payload.
pub const POINT_KEY: &str = "_pt";

/// Events kept per topic for resuming.
const RING_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Point {
    /// Stream id: new whenever the topic restarts (an agent start, a new reply, a new host).
    pub s: String,
    /// Sequence within the stream, from 1.
    pub n: u64,
}

pub fn session_topic(id: &str) -> String {
    format!("session:{id}")
}

pub fn chat_topic(id: &str) -> String {
    format!("chat:{id}")
}

/// The topic a live event belongs to, if it has one.
pub fn topic_of(name: &str, payload: &Value) -> Option<String> {
    match name {
        EV_CODE => payload["codeSessionId"].as_str().map(session_topic),
        EV_CHAT => payload["conversationId"].as_str().map(chat_topic),
        _ => None,
    }
}

pub fn point_of(payload: &Value) -> Option<Point> {
    serde_json::from_value(payload.get(POINT_KEY)?.clone()).ok()
}

/// Whether this event begins a new stream on its origin: an agent starting, or a reply starting.
pub fn starts_stream(name: &str, payload: &Value) -> bool {
    let ev = &payload["event"];
    match name {
        EV_CODE => ev["type"] == "status" && ev["state"] == "starting",
        EV_CHAT => ev["type"] == "started",
        _ => false,
    }
}

/// The origin's numbering: assigns points to the events it emits.
#[derive(Default)]
pub struct Numbering(HashMap<String, Point>);

impl Numbering {
    /// Stamp `payload` with the next point of its topic. Returns the topic.
    pub fn stamp(&mut self, name: &str, payload: &mut Value) -> Option<String> {
        let topic = topic_of(name, payload)?;
        let fresh = starts_stream(name, payload);
        let p = self.0.entry(topic.clone()).or_insert_with(|| Point { s: new_stream(), n: 0 });
        if fresh && p.n > 0 {
            *p = Point { s: new_stream(), n: 0 };
        }
        p.n += 1;
        if let Some(o) = payload.as_object_mut() {
            o.insert(POINT_KEY.into(), json!(p));
        }
        Some(topic)
    }
}

fn new_stream() -> String {
    hex::encode(rand::random::<[u8; 8]>())
}

/// Recent events of one topic's current stream.
#[derive(Default)]
pub struct Ring {
    stream: String,
    events: VecDeque<(u64, String, Value)>,
    bytes: usize,
    /// Holds the stream from its first event.
    complete: bool,
    /// What fed it went away: the stream may have gone on without it.
    stale: bool,
}

impl Ring {
    pub fn push(&mut self, pt: &Point, name: &str, payload: &Value) {
        if pt.s != self.stream {
            *self = Ring { stream: pt.s.clone(), complete: pt.n == 1, ..Ring::default() };
        } else if let Some((last, _, _)) = self.events.back() {
            if pt.n <= *last {
                return;
            }
            if pt.n != last + 1 {
                // A gap (events missed while disconnected): what came before no longer resumes.
                self.events.clear();
                self.bytes = 0;
                self.complete = false;
            }
        }
        self.stale = false;
        let size = payload.to_string().len() + name.len();
        self.events.push_back((pt.n, name.to_owned(), payload.clone()));
        self.bytes += size;
        while self.bytes > RING_BYTES && self.events.len() > 1 {
            if let Some((_, n, p)) = self.events.pop_front() {
                self.bytes -= p.to_string().len() + n.len();
                self.complete = false;
            }
        }
    }

    /// The connection it came over dropped. Until the stream continues, a snapshot takes nothing
    /// from it: the saved transcript may already hold the turn it shows unfinished.
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    pub fn last(&self) -> Option<Point> {
        self.events.back().map(|(n, _, _)| Point { s: self.stream.clone(), n: *n })
    }

    /// The events after `pt`, when this ring still holds all of them.
    pub fn since(&self, pt: &Point) -> Option<Vec<(String, Value)>> {
        if pt.s != self.stream {
            return None;
        }
        let first = self.events.front().map(|e| e.0)?;
        let last = self.events.back().map(|e| e.0)?;
        if pt.n >= last {
            return Some(Vec::new());
        }
        if pt.n + 1 < first {
            return None;
        }
        Some(self.events.iter().filter(|e| e.0 > pt.n).map(|(_, n, p)| (n.clone(), p.clone())).collect())
    }

    /// For a snapshot: the events after the last one matching `boundary` (a finished turn), or
    /// the whole stream when none matches. `covered` is false when the start of that span is gone.
    pub fn tail(&self, boundary: impl Fn(&str, &Value) -> bool) -> (Vec<(String, Value)>, bool) {
        if self.stale {
            return (Vec::new(), false);
        }
        let cut = self.events.iter().rposition(|(_, n, p)| boundary(n, p));
        let covered = cut.is_some() || self.complete;
        let start = cut.map(|i| i + 1).unwrap_or(0);
        (self.events.iter().skip(start).map(|(_, n, p)| (n.clone(), p.clone())).collect(), covered)
    }
}

fn kind(e: &(String, Value)) -> &str {
    e.1["event"]["type"].as_str().unwrap_or_default()
}

/// Text chunk kind of a Code update (agent, thought or user text), for merging neighbours.
fn code_chunk(e: &(String, Value)) -> Option<&str> {
    if e.0 != EV_CODE || kind(e) != "update" {
        return None;
    }
    let u = &e.1["event"]["update"];
    let k = u["sessionUpdate"].as_str()?;
    (matches!(k, "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk") && u["content"]["type"] == "text")
        .then_some(k)
}

fn tool_update_id(e: &(String, Value)) -> Option<String> {
    if e.0 != EV_CODE || kind(e) != "update" {
        return None;
    }
    let u = &e.1["event"]["update"];
    matches!(u["sessionUpdate"].as_str()?, "tool_call" | "tool_call_update").then(|| u["toolCallId"].as_str().map(str::to_owned))?
}

/// Merge a run of events into the smallest set the UI reduces to the same state.
///
/// Neighbouring text chunks join, updates to one tool call fold into its first appearance,
/// only the latest usage and config remain, and permission requests answered meanwhile vanish.
pub fn coalesce(events: Vec<(String, Value)>) -> Vec<(String, Value)> {
    let resolved: BTreeSet<String> = events
        .iter()
        .filter(|e| e.0 == EV_CODE && kind(e) == "permissionResolved")
        .filter_map(|e| e.1["event"]["requestId"].as_str().map(str::to_owned))
        .collect();
    let last_of = |t: &str| events.iter().rposition(|e| e.0 == EV_CODE && kind(e) == t);
    let (last_usage, last_config) = (last_of("usage"), last_of("config"));
    let mut out: Vec<(String, Value)> = Vec::with_capacity(events.len());
    let mut tool_at: HashMap<String, usize> = HashMap::new();
    for (i, e) in events.into_iter().enumerate() {
        if e.0 == EV_CODE {
            let k = kind(&e);
            let rid = e.1["event"]["requestId"].as_str().unwrap_or_default();
            if (k == "permission" || k == "permissionResolved") && resolved.contains(rid) {
                continue;
            }
            if (k == "usage" && Some(i) != last_usage) || (k == "config" && Some(i) != last_config) {
                continue;
            }
            if let Some(chunk) = code_chunk(&e)
                && let Some(prev) = out.last_mut()
                && code_chunk(prev) == Some(chunk)
            {
                let add = e.1["event"]["update"]["content"]["text"].as_str().unwrap_or_default().to_owned();
                if let Some(t) = prev.1["event"]["update"]["content"]["text"].as_str().map(|t| t.to_owned() + &add) {
                    prev.1["event"]["update"]["content"]["text"] = Value::String(t);
                }
                if let Some(pt) = e.1.get(POINT_KEY) {
                    prev.1[POINT_KEY] = pt.clone();
                }
                continue;
            }
            if let Some(id) = tool_update_id(&e) {
                if let Some(&at) = tool_at.get(&id) {
                    let target = &mut out[at].1["event"]["update"];
                    if let (Some(dst), Some(src)) = (target.as_object_mut(), e.1["event"]["update"].as_object()) {
                        for (k, v) in src {
                            if k != "sessionUpdate" && !v.is_null() {
                                dst.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    continue;
                }
                tool_at.insert(id, out.len());
            }
        } else if e.0 == EV_CHAT {
            let k = kind(&e).to_owned();
            if let Some(prev) = out.last_mut()
                && prev.0 == EV_CHAT
                && kind(prev) == k
            {
                match k.as_str() {
                    "text" | "reasoning" => {
                        let add = e.1["event"]["text"].as_str().unwrap_or_default().to_owned();
                        if let Some(t) = prev.1["event"]["text"].as_str().map(|t| t.to_owned() + &add) {
                            prev.1["event"]["text"] = Value::String(t);
                        }
                        if let Some(pt) = e.1.get(POINT_KEY) {
                            prev.1[POINT_KEY] = pt.clone();
                        }
                        continue;
                    }
                    "status" => {
                        *prev = e;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        out.push(e);
    }
    out
}

/// What the session list shows for a Code session, kept by every gateway from the events it sees.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Live {
    /// `starting`, `ready` or `exited`.
    pub status: String,
    /// Mid-turn.
    pub busy: bool,
    /// Open permission requests.
    pub permissions: BTreeSet<String>,
    /// Unix ms of the last event.
    pub last: i64,
}

impl Live {
    /// Fold one Code event in. Returns whether what the list shows changed.
    pub fn apply(&mut self, payload: &Value, now: i64) -> bool {
        let before = (self.status.clone(), self.busy, self.permissions.len());
        let ev = &payload["event"];
        let rid = ev["requestId"].as_str().map(str::to_owned);
        match ev["type"].as_str().unwrap_or_default() {
            "status" => {
                self.status = ev["state"].as_str().unwrap_or_default().to_owned();
                if self.status == "exited" {
                    self.busy = false;
                    self.permissions.clear();
                }
            }
            "userPrompt" => self.busy = true,
            "promptDone" => self.busy = false,
            "permission" => self.permissions.extend(rid),
            "permissionResolved" => {
                if let Some(r) = rid {
                    self.permissions.remove(&r);
                }
            }
            _ => {}
        }
        let moved = now - self.last > 30_000;
        self.last = now;
        before != (self.status.clone(), self.busy, self.permissions.len()) || moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(t: Value) -> (String, Value) {
        (EV_CODE.into(), json!({ "codeSessionId": "s", "event": t }))
    }

    fn chunk(text: &str) -> (String, Value) {
        code(json!({ "type": "update", "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } } }))
    }

    #[test]
    fn numbering_restarts_on_a_new_stream() {
        let mut n = Numbering::default();
        let mut a = code(json!({ "type": "status", "state": "starting" })).1;
        n.stamp(EV_CODE, &mut a);
        let mut b = chunk("x").1;
        n.stamp(EV_CODE, &mut b);
        let (pa, pb) = (point_of(&a).unwrap(), point_of(&b).unwrap());
        assert_eq!((pa.n, pb.n), (1, 2));
        assert_eq!(pa.s, pb.s);
        let mut c = code(json!({ "type": "status", "state": "starting" })).1;
        n.stamp(EV_CODE, &mut c);
        let pc = point_of(&c).unwrap();
        assert_eq!(pc.n, 1);
        assert_ne!(pc.s, pa.s);
    }

    #[test]
    fn ring_resumes_or_asks_for_a_snapshot() {
        let mut r = Ring::default();
        let s = "a".to_string();
        for n in 1..=5 {
            r.push(&Point { s: s.clone(), n }, EV_CODE, &chunk(&n.to_string()).1);
        }
        assert_eq!(r.since(&Point { s: s.clone(), n: 3 }).unwrap().len(), 2);
        assert_eq!(r.since(&Point { s: s.clone(), n: 5 }).unwrap().len(), 0);
        assert!(r.since(&Point { s: "other".into(), n: 3 }).is_none());
        // A gap drops what came before.
        r.push(&Point { s: s.clone(), n: 9 }, EV_CODE, &chunk("9").1);
        assert!(r.since(&Point { s: s.clone(), n: 3 }).is_none());
        assert_eq!(r.since(&Point { s, n: 8 }).unwrap().len(), 1);
    }

    #[test]
    fn ring_tail_starts_after_the_last_finished_turn() {
        let mut r = Ring::default();
        let evs = [
            code(json!({ "type": "status", "state": "starting" })),
            chunk("old"),
            code(json!({ "type": "promptDone", "stopReason": "end_turn" })),
            code(json!({ "type": "userPrompt", "text": "go", "origin": "p" })),
            chunk("new"),
        ];
        for (i, e) in evs.iter().enumerate() {
            r.push(&Point { s: "a".into(), n: i as u64 + 1 }, &e.0, &e.1);
        }
        let (tail, covered) = r.tail(|_, p| p["event"]["type"] == "promptDone");
        assert!(covered);
        assert_eq!(tail.len(), 2);
    }

    #[test]
    fn stale_ring_gives_no_tail_until_the_stream_continues() {
        let mut r = Ring::default();
        let evs = [code(json!({ "type": "userPrompt", "text": "go", "origin": "p" })), chunk("half")];
        for (i, e) in evs.iter().enumerate() {
            r.push(&Point { s: "a".into(), n: i as u64 + 1 }, &e.0, &e.1);
        }
        r.mark_stale();
        let (tail, covered) = r.tail(|_, p| p["event"]["type"] == "promptDone");
        assert!(tail.is_empty() && !covered);
        // Resuming from a point still works: those events did happen.
        assert_eq!(r.since(&Point { s: "a".into(), n: 1 }).unwrap().len(), 1);
        // The stream went on meanwhile: the gap drops the old part and the tail is the new one.
        r.push(&Point { s: "a".into(), n: 7 }, EV_CODE, &chunk("next").1);
        let (tail, covered) = r.tail(|_, p| p["event"]["type"] == "promptDone");
        assert_eq!((tail.len(), covered), (1, false));
        // Nothing was missed: the old part holds again.
        let mut r = Ring::default();
        for (i, e) in evs.iter().enumerate() {
            r.push(&Point { s: "a".into(), n: i as u64 + 1 }, &e.0, &e.1);
        }
        r.mark_stale();
        r.push(&Point { s: "a".into(), n: 3 }, EV_CODE, &chunk("on").1);
        assert_eq!(r.tail(|_, p| p["event"]["type"] == "promptDone").0.len(), 3);
    }

    #[test]
    fn coalesce_merges_without_changing_the_outcome() {
        let evs = vec![
            chunk("Hel"),
            chunk("lo"),
            code(json!({ "type": "update", "update": { "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Read", "status": "pending" } })),
            code(json!({ "type": "permission", "requestId": "r1", "request": {} })),
            code(json!({ "type": "usage", "usage": 1 })),
            code(json!({ "type": "update", "update": { "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed", "title": null } })),
            code(json!({ "type": "permissionResolved", "requestId": "r1" })),
            code(json!({ "type": "permission", "requestId": "r2", "request": {} })),
            chunk(" world"),
            code(json!({ "type": "usage", "usage": 2 })),
        ];
        let out = coalesce(evs);
        let kinds: Vec<&str> = out.iter().map(kind).collect();
        assert_eq!(kinds, ["update", "update", "permission", "update", "usage"]);
        assert_eq!(out[0].1["event"]["update"]["content"]["text"], "Hello");
        let tool = &out[1].1["event"]["update"];
        assert_eq!((tool["status"].as_str(), tool["title"].as_str()), (Some("completed"), Some("Read")));
        assert_eq!(out[2].1["event"]["requestId"], "r2");
        assert_eq!(out[4].1["event"]["usage"], 2);
    }

    #[test]
    fn coalesce_joins_chat_text() {
        let chat = |t: &str, text: &str| (EV_CHAT.to_string(), json!({ "conversationId": "c", "event": { "type": t, "text": text } }));
        let out = coalesce(vec![chat("started", ""), chat("text", "a"), chat("text", "b"), chat("reasoning", "r"), chat("text", "c")]);
        let texts: Vec<&str> = out.iter().map(|e| e.1["event"]["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["", "ab", "r", "c"]);
    }

    #[test]
    fn live_tracks_what_needs_you() {
        let mut l = Live::default();
        assert!(l.apply(&json!({ "event": { "type": "status", "state": "ready" } }), 1));
        assert!(l.apply(&json!({ "event": { "type": "userPrompt" } }), 2));
        assert!(l.apply(&json!({ "event": { "type": "permission", "requestId": "r" } }), 3));
        assert_eq!(l.permissions.len(), 1);
        assert!(!l.apply(&json!({ "event": { "type": "update" } }), 4));
        assert!(l.apply(&json!({ "event": { "type": "permissionResolved", "requestId": "r" } }), 5));
        assert!(l.busy && l.permissions.is_empty());
    }
}
