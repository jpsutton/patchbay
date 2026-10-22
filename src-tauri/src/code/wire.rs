//! The agent's stdio, between the child process and the ACP connection. Every line passes
//! through here, for two things the ACP crate doesn't do:
//!
//! - Frames it can't decode. The crate drops a `session/update` it can't read with only a log
//!   line, and answers a `session/request_permission` it can't read with an error, so the user
//!   sees neither. Those frames are renamed to Patchbay extension methods ([`RAW_UPDATE`],
//!   [`RAW_PERMISSION`]), which the crate hands over as raw JSON, in order with the rest.
//! - The wire log (Settings > Diagnostics): raw frames both ways with secrets redacted, kept in
//!   memory only, recording for [`WINDOW`] each time it is turned on.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream};

/// Extension methods (without the leading `_`) that carry frames the ACP crate can't decode.
pub const RAW_UPDATE: &str = "patchbay/raw/session/update";
pub const RAW_PERMISSION: &str = "patchbay/raw/session/request_permission";

/// How long the wire log records once turned on.
pub const WINDOW: Duration = Duration::from_secs(30 * 60);
/// A longer frame is cut, with a marker saying how much went.
const MAX_FRAME: usize = 64 * 1024;
/// A longer string value is cut inside the frame (file contents, images), so most frames stay
/// valid JSON.
const MAX_STRING: usize = 4 * 1024;
/// The log drops its oldest frames past this.
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// Who a tapped agent is, for the wire log and the once-per-agent notes.
#[derive(Debug, Clone)]
pub struct Tap {
    pub session: String,
    pub agent: String,
}

/// Run the agent's stdio through Patchbay: returns the ends the ACP connection writes to and
/// reads from. The two pump tasks run on the current `LocalSet` and end when either side closes.
pub fn tap(
    tap: Tap,
    stdin: impl AsyncWrite + Unpin + 'static,
    stdout: impl AsyncRead + Unpin + 'static,
) -> (DuplexStream, DuplexStream) {
    let (conn_out, from_conn) = tokio::io::duplex(1 << 16);
    let (mut to_conn, conn_in) = tokio::io::duplex(1 << 16);
    {
        let tap = tap.clone();
        let mut stdin = stdin;
        tokio::task::spawn_local(async move {
            let mut r = BufReader::new(from_conn);
            let mut line = Vec::new();
            loop {
                line.clear();
                if !matches!(r.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
                    break;
                }
                record(&tap, Dir::Out, &line);
                if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
    }
    tokio::task::spawn_local(async move {
        let mut r = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            if !matches!(r.read_until(b'\n', &mut line).await, Ok(n) if n > 0) {
                break;
            }
            record(&tap, Dir::In, &line);
            let rerouted = std::str::from_utf8(&line).ok().and_then(reroute);
            let out = match &rerouted {
                Some(r) => {
                    note_once(&tap.agent, &r.what, &r.error);
                    r.line.as_slice()
                }
                None => line.as_slice(),
            };
            if to_conn.write_all(out).await.is_err() {
                break;
            }
        }
    });
    (conn_out, conn_in)
}

/// An incoming frame the ACP crate would drop or refuse, renamed for Patchbay to take raw.
#[derive(Debug)]
pub struct Rerouted {
    pub line: Vec<u8>,
    /// The method, and the update type for `session/update`.
    pub what: String,
    pub error: String,
}

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(borrow, default)]
    id: Option<&'a RawValue>,
    #[serde(borrow, default)]
    method: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    params: Option<&'a RawValue>,
}

/// `Some` when `line` is a `session/update` or `session/request_permission` the ACP types can't
/// decode (an unknown update type or enum value, a missing field): the same frame under the
/// matching `RAW_*` method.
pub fn reroute(line: &str) -> Option<Rerouted> {
    // Most frames are neither; skip parsing those twice.
    if !line.contains("session/update") && !line.contains("session/request_permission") {
        return None;
    }
    let head: Head = serde_json::from_str(line).ok()?;
    let params = head.params?;
    let (to, what, error) = match (head.method.as_deref()?, head.id.is_some()) {
        ("session/update", false) => {
            let e = serde_json::from_str::<acp::SessionNotification>(params.get()).err()?;
            let v: Value = serde_json::from_str(params.get()).ok()?;
            let kind = v["update"]["sessionUpdate"].as_str().unwrap_or("?").to_owned();
            (RAW_UPDATE, format!("session/update {kind}"), e)
        }
        ("session/request_permission", true) => {
            let e = serde_json::from_str::<acp::RequestPermissionRequest>(params.get()).err()?;
            (RAW_PERMISSION, "session/request_permission".to_owned(), e)
        }
        _ => return None,
    };
    let mut v: Value = serde_json::from_str(line).ok()?;
    v["method"] = Value::String(format!("_{to}"));
    let mut line = serde_json::to_vec(&v).ok()?;
    line.push(b'\n');
    Some(Rerouted { line, what, error: error.to_string() })
}

/// Log once per agent and message type that the agent sends frames ACP can't decode.
fn note_once(agent: &str, what: &str, error: &str) {
    static NOTED: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(Mutex::default);
    if NOTED.lock().unwrap().insert((agent.to_owned(), what.to_owned())) {
        tracing::warn!("{agent} sent a {what} that ACP can't decode ({error}); Patchbay passes it on raw");
    }
}

// ---- The wire log ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// From the agent.
    In,
    /// To the agent.
    Out,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Frame {
    pub seq: u64,
    /// Unix ms.
    pub at: i64,
    /// The Code session.
    pub session: String,
    pub agent: String,
    pub dir: Dir,
    /// The frame, redacted and cut to size.
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub on: bool,
    /// Until it turns itself off.
    pub remaining_secs: u64,
    pub frames: usize,
    /// Frames dropped to stay under the size cap.
    pub dropped: u64,
}

#[derive(Default)]
struct Log {
    until: Option<Instant>,
    frames: VecDeque<Frame>,
    bytes: usize,
    seq: u64,
    dropped: u64,
}

impl Log {
    fn expire(&mut self) {
        if self.until.is_some_and(|u| Instant::now() >= u) {
            self.until = None;
            ON.store(false, Ordering::Relaxed);
        }
    }

    fn status(&mut self) -> Status {
        self.expire();
        Status {
            on: self.until.is_some(),
            remaining_secs: self.until.map_or(0, |u| u.saturating_duration_since(Instant::now()).as_secs()),
            frames: self.frames.len(),
            dropped: self.dropped,
        }
    }
}

/// Fast check for the pumps, so a log that is off costs one load per frame.
static ON: AtomicBool = AtomicBool::new(false);
static LOG: LazyLock<Mutex<Log>> = LazyLock::new(Mutex::default);

/// Turn recording on (for [`WINDOW`] from now) or off. Frames stay until cleared.
pub fn set(on: bool) -> Status {
    let mut log = LOG.lock().unwrap();
    log.until = on.then(|| Instant::now() + WINDOW);
    ON.store(on, Ordering::Relaxed);
    log.status()
}

pub fn status() -> Status {
    LOG.lock().unwrap().status()
}

/// Recorded frames, oldest first; with `session`, only that Code session's.
pub fn frames(session: Option<&str>) -> Vec<Frame> {
    let log = LOG.lock().unwrap();
    log.frames.iter().filter(|f| session.is_none_or(|s| f.session == s)).cloned().collect()
}

pub fn clear() {
    let mut log = LOG.lock().unwrap();
    log.frames.clear();
    log.bytes = 0;
    log.dropped = 0;
}

/// Write the recorded frames to a new JSON Lines file in `dir`, readable only by the user.
pub fn save(dir: &Path) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("acp-wire-{}.jsonl", chrono::Local::now().format("%Y%m%d-%H%M%S")));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut out = std::io::BufWriter::new(opts.open(&path)?);
    for f in frames(None) {
        serde_json::to_writer(&mut out, &f)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(path)
}

fn record(tap: &Tap, dir: Dir, line: &[u8]) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let text = prepare(&String::from_utf8_lossy(line));
    let mut log = LOG.lock().unwrap();
    log.expire();
    if log.until.is_none() {
        return;
    }
    log.seq += 1;
    log.bytes += text.len();
    let frame = Frame {
        seq: log.seq,
        at: chrono::Utc::now().timestamp_millis(),
        session: tap.session.clone(),
        agent: tap.agent.clone(),
        dir,
        text,
    };
    log.frames.push_back(frame);
    while log.bytes > MAX_BYTES {
        let Some(old) = log.frames.pop_front() else { break };
        log.bytes -= old.text.len();
        log.dropped += 1;
    }
}

/// One frame as the log keeps it: secrets redacted, long strings and the frame itself cut.
pub fn prepare(line: &str) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    let text = match serde_json::from_str::<Value>(line) {
        Ok(mut v) => {
            redact(&mut v);
            serde_json::to_string(&v).unwrap_or_default()
        }
        Err(_) => scrub(line).into_owned(),
    };
    truncate(text, MAX_FRAME)
}

const REDACTED: &str = "[redacted]";

/// `k` lowercased, letters and digits only: `OPENAI_API_KEY` and `apiKey` read alike.
fn norm(k: &str) -> String {
    k.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// A key whose value is a secret (normalized, see [`norm`]). Counts like `outputTokens` aren't.
fn secret_key(k: &str) -> bool {
    k.ends_with("token")
        || (k.ends_with("key") && ["api", "secret", "private", "access", "auth"].iter().any(|w| k.contains(w)))
        || ["secret", "password", "passwd", "authorization", "cookie", "credential", "bearer"]
            .iter()
            .any(|w| k.contains(w))
        || k == "key"
}

/// Redact secrets in a frame: values under secret-looking keys, `env` and `headers` values, and
/// key-like strings anywhere (a `.env` file the agent read, a command with a token in it).
pub fn redact(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, x) in map.iter_mut() {
                let k = norm(k);
                if secret_key(&k) {
                    blank(x);
                } else if matches!(k.as_str(), "env" | "headers" | "environment") {
                    redact_pairs(x);
                } else {
                    redact(x);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        Value::String(s) => {
            let scrubbed = scrub(s);
            let next = truncate(scrubbed.into_owned(), MAX_STRING);
            *s = next;
        }
        _ => {}
    }
}

/// Every string in `v`.
fn blank(v: &mut Value) {
    match v {
        Value::String(s) => *s = REDACTED.into(),
        Value::Array(items) => items.iter_mut().for_each(blank),
        Value::Object(map) => map.values_mut().for_each(blank),
        _ => {}
    }
}

/// `env` / `headers`, as `{NAME: value}` or `[{name, value}]`: names stay, values go.
fn redact_pairs(v: &mut Value) {
    match v {
        Value::Array(items) => {
            for it in items {
                match it.as_object_mut() {
                    Some(o) if o.contains_key("name") => {
                        o.iter_mut().filter(|(k, _)| *k != "name").for_each(|(_, x)| blank(x));
                    }
                    _ => blank(it),
                }
            }
        }
        _ => blank(v),
    }
}

/// Prefixes of API keys and tokens that show up in text.
const KEY_PREFIXES: &[&str] = &[
    "sk-", "sk_", "xai-", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "glpat-", "AKIA", "ASIA", "AIza",
    "xoxb-", "xoxp-", "xapp-", "eyJ", "hf_", "npm_", "pypi-",
];

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Best effort over free text: key-like words, `NAME=value` with a secret-looking name, and
/// `Bearer <token>`.
pub fn scrub(s: &str) -> Cow<'_, str> {
    let b = s.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !is_word(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_word(b[i]) {
            i += 1;
        }
        let word = &s[start..i];
        if word.len() >= 20 && KEY_PREFIXES.iter().any(|p| word.starts_with(p)) {
            spans.push((start, i));
        } else if i < b.len() && b[i] == b'=' && secret_key(&norm(word)) {
            let (v0, v1) = value_at(b, i + 1);
            if v1 > v0 {
                spans.push((v0, v1));
                i = v1;
            }
        } else if word.eq_ignore_ascii_case("bearer") && b.get(i) == Some(&b' ') {
            let (v0, v1) = value_at(b, i + 1);
            if v1 - v0 >= 8 {
                spans.push((v0, v1));
                i = v1;
            }
        }
    }
    if spans.is_empty() {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, z) in spans {
        out.push_str(&s[last..a]);
        out.push_str(REDACTED);
        last = z;
    }
    out.push_str(&s[last..]);
    Cow::Owned(out)
}

/// The value starting at `i`: to the closing quote when quoted, else to whitespace or a quote.
fn value_at(b: &[u8], i: usize) -> (usize, usize) {
    if let Some(&q) = b.get(i).filter(|&&c| c == b'"' || c == b'\'') {
        let end = b[i + 1..].iter().position(|&c| c == q || c == b'\\').map_or(b.len(), |p| i + 1 + p);
        return (i + 1, end);
    }
    let end = b[i..]
        .iter()
        .position(|&c| c.is_ascii_whitespace() || matches!(c, b'"' | b'\'' | b'\\' | b'&' | b';' | b','))
        .map_or(b.len(), |p| i + p);
    (i, end)
}

/// `s` cut to at most `max` bytes (on a char boundary), with a marker saying how much went.
pub fn truncate(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let more = s.len() - cut;
    s.truncate(cut);
    s.push_str(&format!("…[truncated {more} bytes]"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update_line(update: Value) -> String {
        json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": "s1", "update": update } })
            .to_string()
    }

    #[test]
    fn decodable_frames_pass_through() {
        let ok = update_line(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } }));
        assert!(reroute(&ok).is_none());
        let tool = update_line(json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "x", "status": "completed" }));
        assert!(reroute(&tool).is_none());
        assert!(reroute(r#"{"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}"#).is_none());
        assert!(reroute("not json session/update").is_none());
    }

    #[test]
    fn undecodable_updates_are_rerouted() {
        for (update, kind) in [
            (json!({ "sessionUpdate": "brand_new_thing", "x": 1 }), "brand_new_thing"),
            (json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "x", "status": "cancelled" }), "tool_call"),
            (json!({ "sessionUpdate": "tool_call", "toolCallId": "t" }), "tool_call"),
            (json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "e", "content": [{ "type": "diff" }] }), "tool_call"),
            (json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "hologram" } }), "agent_message_chunk"),
            (json!({ "sessionUpdate": "plan", "entries": [{ "content": "a", "priority": "high", "status": "blocked" }] }), "plan"),
        ] {
            let r = reroute(&update_line(update.clone())).unwrap_or_else(|| panic!("not rerouted: {update}"));
            assert_eq!(r.what, format!("session/update {kind}"));
            assert_eq!(*r.line.last().unwrap(), b'\n');
            let v: Value = serde_json::from_slice(&r.line).unwrap();
            assert_eq!(v["method"], format!("_{RAW_UPDATE}"));
            assert_eq!(v["params"]["update"], update);
        }
    }

    #[test]
    fn undecodable_permission_requests_are_rerouted() {
        let req = |kind: &str| {
            json!({ "jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {
                "sessionId": "s1",
                "toolCall": { "toolCallId": "t" },
                "options": [{ "optionId": "a", "name": "Allow", "kind": kind }],
            } })
            .to_string()
        };
        assert!(reroute(&req("allow_once")).is_none());
        let r = reroute(&req("allow_for_session")).unwrap();
        let v: Value = serde_json::from_slice(&r.line).unwrap();
        assert_eq!(v["method"], format!("_{RAW_PERMISSION}"));
        assert_eq!(v["id"], 7);
    }

    #[test]
    fn redacts_secrets() {
        let mut v = json!({
            "method": "authenticate",
            "params": { "methodId": "api-key", "_meta": { "api-key": { "apiKey": "abc" } } },
            "mcpServers": [{ "name": "gh", "command": "x", "env": [{ "name": "GITHUB_TOKEN", "value": "t0k" }],
                             "headers": [{ "name": "Authorization", "value": "Basic Zm9v" }] }],
            "authMethods": [{ "id": "tui", "env": { "FAKE_LOGIN": "1" } }],
            "accessToken": "a", "client_secret": "b", "password": "c",
            "usage": { "outputTokens": 12, "totalTokens": 30 },
            "text": "OPENAI_API_KEY=sk-proj-0123456789abcdefghij and xai-ABCDEFGHIJKLMNOPQRSTUV ok",
            "cmd": "curl -H 'Authorization: Bearer abcdef0123456789' https://x --api-key=\"s3cr3t v\"",
        });
        redact(&mut v);
        let s = v.to_string();
        for secret in ["abc\"", "t0k", "Zm9v", "\"1\"", "sk-proj", "xai-ABC", "abcdef0123456789", "s3cr3t"] {
            assert!(!s.contains(secret), "{secret} left in {s}");
        }
        assert_eq!(v["params"]["methodId"], "api-key");
        assert_eq!(v["mcpServers"][0]["env"][0]["name"], "GITHUB_TOKEN");
        assert_eq!(v["accessToken"], REDACTED);
        assert_eq!(v["usage"]["outputTokens"], 12);
        assert_eq!(v["text"], "OPENAI_API_KEY=[redacted] and [redacted] ok");
        assert_eq!(scrub("nothing to see, sk-short, task-runner"), "nothing to see, sk-short, task-runner");
    }

    #[test]
    fn cuts_long_strings_and_frames() {
        let long = "é".repeat(MAX_STRING);
        let frame = prepare(&json!({ "a": long }).to_string());
        let v: Value = serde_json::from_str(&frame).expect("still JSON");
        let a = v["a"].as_str().unwrap();
        assert!(a.ends_with(&format!("…[truncated {} bytes]", long.len() - MAX_STRING)), "{}", &a[a.len() - 40..]);

        let many: Vec<String> = (0..100).map(|i| format!("{i}{}", "x".repeat(1000))).collect();
        let frame = prepare(&json!(many).to_string());
        assert!(frame.len() < MAX_FRAME + 40);
        assert!(frame.contains("…[truncated "));

        let raw = prepare(&format!("not json {}", "y".repeat(MAX_FRAME)));
        assert!(raw.ends_with("…[truncated 9 bytes]"), "{}", &raw[raw.len() - 30..]);
    }

    #[test]
    fn log_records_while_on_and_turns_itself_off() {
        let tap = Tap { session: format!("wire-test-{}", crate::db::new_id()), agent: "fake".into() };
        let mine = || frames(Some(&tap.session));
        set(false);
        record(&tap, Dir::In, b"{\"x\":1}\n");
        assert!(mine().is_empty());
        assert!(set(true).remaining_secs > 29 * 60);
        record(&tap, Dir::Out, b"{\"token\":\"t\"}\n");
        let got = mine();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].dir, got[0].text.as_str()), (Dir::Out, r#"{"token":"[redacted]"}"#));
        // Past the window: off, and nothing more recorded.
        LOG.lock().unwrap().until = Some(Instant::now() - Duration::from_secs(1));
        record(&tap, Dir::In, b"{\"x\":2}\n");
        assert_eq!(mine().len(), 1);
        assert!(!status().on);
    }
}
