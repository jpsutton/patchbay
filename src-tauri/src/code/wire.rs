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
use std::sync::{Arc, LazyLock, Mutex};
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
            let rerouted = reroute_bytes(&line);
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

/// [`reroute`] over raw bytes. A frame that isn't valid UTF-8 is checked as its lossy text, so
/// it still reaches the UI when rerouted; otherwise the crate gets the original bytes.
pub fn reroute_bytes(line: &[u8]) -> Option<Rerouted> {
    reroute(&String::from_utf8_lossy(line))
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
            // Cut: it comes from the agent and keys the once-per-kind note.
            let kind: String = v["update"]["sessionUpdate"].as_str().unwrap_or("?").chars().take(64).collect();
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

/// Most distinct (agent, message type) pairs [`note_once`] remembers; past it, notes go to debug.
const MAX_NOTED: usize = 256;

/// Log once per agent and message type that the agent sends frames ACP can't decode.
fn note_once(agent: &str, what: &str, error: &str) {
    static NOTED: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(Mutex::default);
    let first = {
        let mut noted = NOTED.lock().unwrap();
        let key = (agent.to_owned(), what.to_owned());
        match noted.contains(&key) {
            true => Some(false),
            false if noted.len() < MAX_NOTED => Some(noted.insert(key)),
            // Full: an agent inventing types can't grow the set; still say so, quietly.
            false => None,
        }
    };
    match first {
        Some(true) => tracing::warn!("{agent} sent a {what} that ACP can't decode ({error}); Patchbay passes it on raw"),
        None => tracing::debug!("{agent} sent a {what} that ACP can't decode ({error}); Patchbay passes it on raw"),
        Some(false) => {}
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
    /// Shared, so reading the log copies pointers under the lock, not frames.
    frames: VecDeque<Arc<Frame>>,
    /// What the frames cost in memory, by [`cost`].
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

    /// Add `frame`, dropping the oldest while the log costs more than `max` bytes.
    fn push(&mut self, frame: Frame, max: usize) {
        self.bytes += cost(&frame);
        self.frames.push_back(Arc::new(frame));
        while self.bytes > max {
            let Some(old) = self.frames.pop_front() else { break };
            self.bytes -= cost(&old);
            self.dropped += 1;
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

/// What a frame costs the log in memory: the struct, its `Arc` counts and its strings.
fn cost(f: &Frame) -> usize {
    size_of::<Frame>() + 2 * size_of::<usize>() + f.session.len() + f.agent.len() + f.text.len()
}

/// Recorded frames, oldest first; with `session`, only that Code session's; with `last`, only
/// the newest that many. Only pointers are copied under the lock.
pub fn frames(session: Option<&str>, last: Option<usize>) -> Vec<Arc<Frame>> {
    let log = LOG.lock().unwrap();
    let mine = |f: &&Arc<Frame>| session.is_none_or(|s| f.session == s);
    let mut out: Vec<Arc<Frame>> = match last {
        Some(n) => log.frames.iter().rev().filter(mine).take(n).cloned().collect(),
        None => return log.frames.iter().filter(mine).cloned().collect(),
    };
    drop(log);
    out.reverse();
    out
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
    for f in frames(None, None) {
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
    let text = prepare(line);
    let mut log = LOG.lock().unwrap();
    log.expire();
    if log.until.is_none() {
        return;
    }
    log.seq += 1;
    let frame = Frame {
        seq: log.seq,
        at: chrono::Utc::now().timestamp_millis(),
        session: tap.session.clone(),
        agent: tap.agent.clone(),
        dir,
        text,
    };
    log.push(frame, MAX_BYTES);
}

/// One frame as the log keeps it: secrets redacted, long strings and the frame itself cut.
/// A frame over [`MAX_FRAME`] isn't parsed: it is cut first, then scrubbed as text.
pub fn prepare(line: &[u8]) -> String {
    let mut end = line.len();
    while end > 0 && matches!(line[end - 1], b'\r' | b'\n') {
        end -= 1;
    }
    let line = &line[..end];
    if line.len() > MAX_FRAME {
        // Back off to a UTF-8 boundary so the cut doesn't leave a replacement character.
        let mut cut = MAX_FRAME;
        while cut > 0 && line[cut] & 0xC0 == 0x80 {
            cut -= 1;
        }
        let head = String::from_utf8_lossy(&line[..cut]);
        let mut text = scrub(&head).into_owned();
        text.push_str(&format!("…[truncated {} bytes]", line.len() - cut));
        return text;
    }
    let line = String::from_utf8_lossy(line);
    let line = line.as_ref();
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

/// Words ending in "key" that don't name a key.
const NOT_KEYS: &[&str] = &["hotkey", "monkey", "donkey", "turkey", "hockey", "jockey", "whiskey"];

/// A key whose value is a secret (normalized, see [`norm`]): any `…key` (`sessionKey`,
/// `licenseKey`), `…token`, and names with secret words in them. Counts like `outputTokens`
/// aren't.
fn secret_key(k: &str) -> bool {
    k.ends_with("token")
        || (k.ends_with("key") && !NOT_KEYS.iter().any(|w| k.ends_with(w)))
        || ["apikey", "accesskey", "secretkey", "privatekey"].iter().any(|w| k.contains(w))
        || ["secret", "password", "passwd", "authorization", "cookie", "credential", "bearer"]
            .iter()
            .any(|w| k.contains(w))
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
            let next = match embedded_json(s) {
                Some(json) => json,
                None => scrub(s).into_owned(),
            };
            *s = truncate(next, MAX_STRING);
        }
        _ => {}
    }
}

/// A string that is itself a JSON object or array (a tool's input or output, say), redacted as
/// JSON and written back.
fn embedded_json(s: &str) -> Option<String> {
    let t = s.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    let mut v: Value = serde_json::from_str(s).ok()?;
    redact(&mut v);
    serde_json::to_string(&v).ok()
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

/// Auth schemes whose credential follows a space (`Authorization: Basic …`).
const SCHEMES: &[&str] = &["bearer", "basic", "token", "digest", "negotiate"];

/// Best effort over free text: key-like words; `NAME=value`, `name: value` (JSON's
/// `"name": "value"` too) and `--name value` with a secret-looking name, keeping an auth
/// scheme (`Authorization: Basic [redacted]`); `Bearer <token>`; and the password in
/// `scheme://user:pass@host`.
pub fn scrub(s: &str) -> Cow<'_, str> {
    let b = s.as_bytes();
    let mut spans = url_passwords(b);
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
        } else if word.eq_ignore_ascii_case("bearer") && b.get(i) == Some(&b' ') {
            let (v0, v1) = value_at(b, i + 1);
            if v1 - v0 >= 8 {
                spans.push((v0, v1));
                i = v1;
            }
        } else if !b[..start].ends_with(b"://")
            && let Some((v0, v1)) = secret_value(b, i, word.starts_with("--"))
            && secret_key(&norm(word))
        {
            spans.push((v0, v1));
            i = v1;
        }
    }
    if spans.is_empty() {
        return Cow::Borrowed(s);
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, z) in spans {
        // Spans can overlap (a URL password under a secret name); merge them.
        if z <= last {
            continue;
        }
        if a >= last {
            out.push_str(&s[last..a]);
            out.push_str(REDACTED);
        }
        last = z;
    }
    out.push_str(&s[last..]);
    Cow::Owned(out)
}

/// The value after a secret-looking name ending at `i`: after `=` or `:` (past a closing quote
/// and spaces), or after one space for a `--flag`. Past an auth scheme, only the credential.
fn secret_value(b: &[u8], i: usize, flag: bool) -> Option<(usize, usize)> {
    let spaces = |mut j: usize| {
        while b.get(j).is_some_and(|&c| c == b' ' || c == b'\t') {
            j += 1;
        }
        j
    };
    let mut j = i;
    if matches!(b.get(j), Some(b'"' | b'\'')) {
        j += 1;
    }
    j = spaces(j);
    let at = match b.get(j) {
        Some(b'=' | b':') => spaces(j + 1),
        Some(_) if flag && j == i + 1 && b[i] == b' ' && b[j] != b'-' => j,
        _ => return None,
    };
    let (v0, v1) = value_at(b, at);
    if v1 <= v0 {
        return None;
    }
    let quoted = v0 > at;
    if !quoted && b.get(v1) == Some(&b' ') && SCHEMES.iter().any(|w| b[v0..v1].eq_ignore_ascii_case(w.as_bytes())) {
        let (c0, c1) = value_at(b, v1 + 1);
        return (c1 > c0).then_some((c0, c1));
    }
    Some((v0, v1))
}

/// Spans of the passwords in `scheme://user:pass@host` URLs.
fn url_passwords(b: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(p) = b[from..].windows(3).position(|w| w == b"://") {
        let auth = from + p + 3;
        let end = b[auth..]
            .iter()
            .position(|&c| c.is_ascii_whitespace() || matches!(c, b'/' | b'?' | b'#' | b'"' | b'\'' | b'\\'))
            .map_or(b.len(), |q| auth + q);
        if let Some(at) = b[auth..end].iter().rposition(|&c| c == b'@').map(|q| auth + q)
            && let Some(colon) = b[auth..at].iter().position(|&c| c == b':').map(|q| auth + q)
            && at > colon + 1
        {
            spans.push((colon + 1, at));
        }
        from = end.max(auth);
    }
    spans
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
        let frame = prepare(json!({ "a": long }).to_string().as_bytes());
        let v: Value = serde_json::from_str(&frame).expect("still JSON");
        let a = v["a"].as_str().unwrap();
        assert!(a.ends_with(&format!("…[truncated {} bytes]", long.len() - MAX_STRING)), "{}", &a[a.len() - 40..]);

        let many: Vec<String> = (0..100).map(|i| format!("{i}{}", "x".repeat(1000))).collect();
        let frame = prepare(json!(many).to_string().as_bytes());
        assert!(frame.len() < MAX_FRAME + 40);
        assert!(frame.contains("…[truncated "));

        let raw = prepare(format!("not json {}", "y".repeat(MAX_FRAME)).as_bytes());
        assert!(raw.ends_with("…[truncated 9 bytes]"), "{}", &raw[raw.len() - 30..]);
    }

    #[test]
    fn log_records_while_on_and_turns_itself_off() {
        let tap = Tap { session: format!("wire-test-{}", crate::db::new_id()), agent: "fake".into() };
        let mine = || frames(Some(&tap.session), None);
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

    #[test]
    fn non_utf8_frames_are_still_rerouted() {
        let mut line = update_line(json!({ "sessionUpdate": "brand_new", "text": "AB" })).into_bytes();
        let at = line.windows(2).position(|w| w == b"AB").unwrap();
        line[at] = 0xff;
        assert!(std::str::from_utf8(&line).is_err());
        let r = reroute_bytes(&line).expect("rerouted");
        let v: Value = serde_json::from_slice(&r.line).unwrap();
        assert_eq!(v["method"], format!("_{RAW_UPDATE}"));
        assert_eq!(v["params"]["update"]["text"], "\u{fffd}B");
        // Valid frames ACP can read still go through untouched.
        let ok = update_line(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "hi" } }));
        assert!(reroute_bytes(ok.as_bytes()).is_none());
    }

    #[test]
    fn huge_frames_are_cut_before_parsing_and_scrubbed() {
        let line = json!({ "apiKey": "s3cr3tvalue", "pad": "z".repeat(2 * MAX_FRAME) }).to_string();
        let text = prepare(format!("{line}\n").as_bytes());
        assert!(!text.contains("s3cr3tvalue"), "{}", &text[..80]);
        assert!(text.starts_with(r#"{"apiKey":"[redacted]""#), "{}", &text[..80]);
        assert!(text.ends_with(&format!("…[truncated {} bytes]", line.len() - MAX_FRAME)));
        // A cut inside a multi-byte character backs off to its start.
        let wide = format!("{}{}", "a".repeat(MAX_FRAME - 1), "é".repeat(10));
        let text = prepare(wide.as_bytes());
        assert!(!text.contains('\u{fffd}'));
        assert!(text.ends_with("…[truncated 20 bytes]"), "{}", &text[text.len() - 30..]);
    }

    #[test]
    fn size_cap_counts_frame_overhead() {
        let frame = |seq| Frame {
            seq,
            at: 0,
            session: "session".into(),
            agent: "agent".into(),
            dir: Dir::In,
            text: String::new(),
        };
        let each = cost(&frame(0));
        assert!(each > size_of::<Frame>() + "sessionagent".len());
        let mut log = Log::default();
        for seq in 0..10 {
            log.push(frame(seq), 4 * each);
        }
        // Empty texts still count: only the newest four fit.
        assert_eq!((log.frames.len(), log.dropped, log.bytes), (4, 6, 4 * each));
        assert_eq!(log.frames[0].seq, 6);
    }

    #[test]
    fn any_key_name_is_secret() {
        for k in ["sessionKey", "encryptionKey", "signing_key", "consumerKey", "LICENSE_KEY", "key", "x-api-key",
            "AWS_ACCESS_KEY_ID", "apiKeys", "authToken", "client_secret"]
        {
            assert!(secret_key(&norm(k)), "{k}");
        }
        for k in ["sessionUpdate", "toolCallId", "keyboard", "keyboardLayout", "hotkey", "monkey", "keys",
            "outputTokens", "sessionId", "methodId"]
        {
            assert!(!secret_key(&norm(k)), "{k}");
        }
    }

    #[test]
    fn scrubs_more_secret_forms() {
        for (text, want) in [
            ("password: hunter22 ok", "password: [redacted] ok"),
            ("x-api-key:abc123 ok", "x-api-key:[redacted] ok"),
            ("run --token abc123 --verbose", "run --token [redacted] --verbose"),
            ("run --api-key=abc123 --max-tokens 50", "run --api-key=[redacted] --max-tokens 50"),
            ("Authorization: Basic Zm9vOmJhcg== next", "Authorization: Basic [redacted] next"),
            ("Authorization: Bearer abc ok", "Authorization: Bearer [redacted] ok"),
            (r#"{"licenseKey": "L-123", "n": 1}"#, r#"{"licenseKey": "[redacted]", "n": 1}"#),
            ("db postgres://bob:pa55@db.local:5432/x ok", "db postgres://bob:[redacted]@db.local:5432/x ok"),
            ("git https://token:ghx@github.com/a", "git https://token:[redacted]@github.com/a"),
            ("see https://user@host/a and http://h:80/p", "see https://user@host/a and http://h:80/p"),
            ("--token -v and token is fine", "--token -v and token is fine"),
        ] {
            assert_eq!(scrub(text), want, "{text}");
        }
        // A string that is itself JSON is redacted as JSON.
        let mut v = json!({ "rawOutput": r#"{"env":{"GH":"t0k"},"cfg":{"signingKey":"k1"},"note":"ok"}"# });
        redact(&mut v);
        let inner: Value = serde_json::from_str(v["rawOutput"].as_str().unwrap()).unwrap();
        assert_eq!(inner, json!({ "env": { "GH": REDACTED }, "cfg": { "signingKey": REDACTED }, "note": "ok" }));
    }
}
