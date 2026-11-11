//! `GET /api/v1/sessions` (pb-20261110-04): one read-only status surface for local observers
//! (Patchbay Central, Patchbay Teams) over loopback TCP, `Authorization: Bearer <web-token>`.
//!
//! The same handler is mounted on **both** hosts that can be running on a machine:
//! `patchbayd`'s web Router (`web.rs`) and the desktop app's share-server Router (`shares.rs`).
//! The app's 4420 listener is the share server and it has no web UI, so without this the
//! app-only case — the common one — had no status API at all. Both create and read the same
//! `data_dir/web-token` (0600, `web_token::load_token`), which is the single discovery path
//! documented in `docs/headless.md`. Port 4420 is double-booked by design: whichever process
//! binds it first serves it; the token file is the same either way, so discovery does not
//! depend on which one won.
//!
//! Scope: read only. No writes, no SSE, no deep-link (pb-20261110-05), no command surface.
//! The token already carries full command power over patchbayd's `/ws`, so this adds no reach;
//! the app's router in particular gets this route and nothing else (no `/ws`, no commands).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{Value, json};

use crate::db::CodeSession;
use crate::mesh::remote::Live;
use crate::{AppState, ipc::token_matches};

/// Which process is answering, and how the feed is labelled for a consumer.
#[derive(Debug, Clone, Copy)]
pub struct Surface {
    /// Envelope `provider` (`"patchbayd"` on the daemon). The contract pins keys and types,
    /// not this literal.
    pub provider: &'static str,
    /// Session record `source`.
    pub source: &'static str,
}

/// The daemon's web server (a `web`-feature build; the desktop-only build never mounts it).
#[cfg_attr(not(feature = "web"), allow(dead_code))]
pub const DAEMON: Surface = Surface { provider: "patchbayd", source: "patchbay_daemon_api" };
/// The desktop app's share-server listener.
pub const APP: Surface = Surface { provider: "patchbay-app", source: "patchbay_app_api" };

/// The route, state already applied, for a host to `merge` into its own router.
pub fn router(state: AppState, token: String, surface: Surface) -> Router {
    Router::new()
        .route("/api/v1/sessions", get(sessions))
        .with_state(Ctx { state, token: token.into(), surface })
}

#[derive(Clone)]
struct Ctx {
    state: AppState,
    token: Arc<str>,
    surface: Surface,
}

async fn sessions(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    match bearer(&headers) {
        Some(given) if token_matches(given, &ctx.token) => {}
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    }
    match envelope(&ctx.state, ctx.surface) {
        // axum is built here without its `json` feature (see Cargo.toml), so the body goes out
        // as bytes with the content type spelled out.
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            serde_json::to_vec(&body).unwrap_or_default(),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("session status unavailable: {e:#}")).into_response(),
    }
}

/// The token out of `Authorization: Bearer …`; empty tokens never match.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// The agreed envelope: exactly `observed_at`, `provider`, `sessions` — the version lives in
/// the path (`/api/v1`), never in the body.
pub fn envelope(state: &AppState, surface: Surface) -> anyhow::Result<Value> {
    let observed_at = crate::db::now_ms();
    let rows = state.db.list_code_sessions()?;
    let live = state.mesh.live();
    let sessions: Vec<Value> = rows.iter().map(|s| record(state, &live, s, surface, observed_at)).collect();
    Ok(json!({ "observed_at": observed_at, "provider": surface.provider, "sessions": sessions }))
}

/// One session's record, from affirmative evidence only (contract §3, §6).
fn record(state: &AppState, live: &HashMap<String, Live>, s: &CodeSession, surface: Surface, observed_at: i64) -> Value {
    let ev = evidence(state, live, s);
    let (wire, stuck_reason) = state_of(&ev);
    let question = ev.question.as_ref().map(question_json);
    json!({
        "id": s.id,
        "short_id": short_id(&s.id),
        "title": title(s),
        "cwd": s.cwd,
        "project_name": project_name(&s.cwd),
        "agent": s.agent,
        "source": surface.source,
        "state": wire.as_str(),
        "archived": s.archived_at.is_some(),
        "updated_at": s.updated_at,
        "last_activity_at": ev.last_activity_at,
        "question": question,
        "stuck_reason": stuck_reason,
        "observation_freshness": {
            "basis": if ev.live { "live_memory" } else { "sqlite_poll" },
            "observed_at": observed_at,
        },
        "field_provenance": {
            "id": "sqlite",
            "state": if ev.live { "live_memory" } else { "unknown" },
            "cwd": "sqlite",
            "agent": "sqlite",
            "updated_at": "sqlite",
        },
        "navigation_target_id": s.id,
        "navigation_target_uri": null,
        "capabilities": { "can_focus": false, "can_answer": false, "answer_type": "none" },
        "teams": null,
    })
}

/// What the manager and the event loop know about one session right now. Every field is
/// affirmative evidence; nothing here is inferred from silence.
struct Evidence {
    running: bool,
    busy: bool,
    /// An open permission, question or sign-in *right now*.
    waiting: bool,
    /// The last turn's explicit outcome, when one was recorded.
    turn_end: Option<crate::code::TurnEnd>,
    /// Pending question evidence. Kept out of the state decision on purpose: a question alone
    /// never proves the session still wants the user (contract §6.2).
    question: Option<Question>,
    last_activity_at: Option<i64>,
    /// Anything live was seen for this session (vs. a row read from sqlite alone).
    live: bool,
}

fn evidence(state: &AppState, live: &HashMap<String, Live>, s: &CodeSession) -> Evidence {
    let running = state.code.is_running(&s.id);
    let waiting = running && crate::waiting_on_person(state, &s.id);
    let l = live.get(&s.id);
    let last_activity_at = state
        .code
        .stall
        .last_activity_ms(&s.id)
        .or_else(|| l.map(|l| l.last).filter(|t| *t > 0));
    Evidence {
        running,
        busy: state.code.is_busy(&s.id),
        waiting,
        turn_end: state.code.turn_end(&s.id),
        question: if waiting { live_question(state, &s.id) } else { None },
        last_activity_at,
        live: running || l.is_some() || last_activity_at.is_some() || state.code.turn_end(&s.id).is_some(),
    }
}

/// The five wire states (contract §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireState {
    NeedsYou,
    Stuck,
    Running,
    Done,
    Unknown,
}

impl WireState {
    pub fn as_str(self) -> &'static str {
        match self {
            WireState::NeedsYou => "needs_you",
            WireState::Stuck => "stuck",
            WireState::Running => "running",
            WireState::Done => "done",
            WireState::Unknown => "unknown",
        }
    }
}

/// The state from the evidence, applying the contract §6 rules in order:
/// an explicit unrecovered error is `stuck`; a live open request is `needs_you`; a turn in
/// flight is `running`; a turn that ended without error is `done`; anything else — including
/// no evidence at all — is `unknown`. Silence never becomes a state.
fn state_of(e: &Evidence) -> (WireState, Option<String>) {
    if let Some(crate::code::TurnEnd::Error(message)) = &e.turn_end {
        return (WireState::Stuck, Some(sanitize_reason(message)));
    }
    if e.running && e.waiting {
        return (WireState::NeedsYou, None);
    }
    if e.running && e.busy {
        // A stalled turn is still running: quiet is not an error (contract §6.3, §6.4).
        return (WireState::Running, None);
    }
    if matches!(e.turn_end, Some(crate::code::TurnEnd::Clean)) {
        return (WireState::Done, None);
    }
    (WireState::Unknown, None)
}

/// Contract §3 `stuck_reason` is agreed free text ("Explicit error string"), so the raw
/// message is bounded and scrubbed before it reaches the wire, in this order:
/// 1. control characters flattened (one line in JSON);
/// 2. key-name redaction — the whole `key=value`, `key: value` or `key value` match for
///    `token secret password passwd pwd key auth authorization bearer cookie credential`
///    (case-insensitive, whole word, any value length);
/// 3. URL/bare userinfo — `scheme://user:pass@` and `user:pass@`;
/// 4. path-shaped redaction of every absolute path on both OS families — unix `/…`,
///    `~/…`, drive `C:\…`/`C:/…`, UNC `\\server\…`, including spaces and dots — plus
///    relative `./…` and `../…`; the whole path becomes `[redacted]` (there is no
///    home-prefix rewrite: `/Users/dfrazer2/x` is redacted, never turned into `~2/x`);
/// 5. token-like runs of 16+ `[A-Za-z0-9+/=_-]` as a backstop;
/// 6. clipped to 200 chars + `...`.
/// Redaction errs toward dropping content — an error string is a diagnostic, not a payload.
/// NOT implemented (deliberate): percent/unicode decoding of values, grapheme-boundary
/// clipping (the clip is char-boundary), and any `~`-form path rewriting.
fn sanitize_reason(message: &str) -> String {
    const MAX_CHARS: usize = 200;
    const TOKEN_RUN_MIN: usize = 16;
    let s: String = message.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let s = redact_key_values(&s);
    let s = redact_userinfo(&s);
    let s = redact_paths(&s);
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-') {
            run.push(c);
        } else {
            flush_token_run(&mut out, &mut run, TOKEN_RUN_MIN);
            out.push(c);
        }
    }
    flush_token_run(&mut out, &mut run, TOKEN_RUN_MIN);
    if out.chars().count() > MAX_CHARS {
        let clipped: String = out.chars().take(MAX_CHARS).collect();
        format!("{clipped}...")
    } else {
        out
    }
}

/// `key=value` / `key: value` / `key value` for the secret key names: the whole match
/// (key + separators + value tokens) becomes `[redacted]`, any value length. A value that
/// is itself a key chains (`Authorization: Bearer abc123` redacts as one match).
fn redact_key_values(s: &str) -> String {
    const KEYS: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "pwd",
        "key",
        "auth",
        "authorization",
        "bearer",
        "cookie",
        "credential",
    ];
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'scan: while i < b.len() {
        // Whole-word boundary before the key (so `author`/`monkey` never match).
        if i == 0 || !b[i - 1].is_ascii_alphanumeric() {
            // Longest match first: `authorization` must beat its own prefix `auth`.
            let mut best: Option<usize> = None;
            for k in KEYS {
                if i + k.len() <= b.len()
                    && b[i..i + k.len()].eq_ignore_ascii_case(k.as_bytes())
                    && best.is_none_or(|len| k.len() > len)
                {
                    best = Some(k.len());
                }
            }
            if let Some(klen) = best {
                let mut j = i + klen;
                let sep_start = j;
                while j < b.len() && (b[j] == b'=' || b[j] == b':' || b[j].is_ascii_whitespace()) {
                    j += 1;
                }
                // A separator (or end of string) is what makes it `key …`, not `author`.
                if j > sep_start || j >= b.len() {
                    loop {
                        let tok_start = j;
                        while j < b.len() && !b[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if j == tok_start {
                            break;
                        }
                        let is_key = KEYS.iter().any(|k| {
                            j - tok_start == k.len() && b[tok_start..j].eq_ignore_ascii_case(k.as_bytes())
                        });
                        let after = j;
                        while j < b.len() && (b[j] == b'=' || b[j] == b':' || b[j].is_ascii_whitespace()) {
                            j += 1;
                        }
                        if !is_key || j == after {
                            break;
                        }
                    }
                    out.push_str("[redacted]");
                    i = j;
                    continue 'scan;
                }
            }
        }
        let ch = s[i..].chars().next().expect("char at i");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `scheme://user:pass@` and bare `user:pass@`: the `user:pass@` part becomes
/// `[redacted]` (the host after it is not a secret and keeps its shape).
fn redact_userinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'@' {
            // Nearest `:` before the `@` with no whitespace, `/`, `?` or `#` between them.
            let mut c = i;
            let mut colon = None;
            while c > last {
                c -= 1;
                let ch = b[c];
                if ch.is_ascii_whitespace() || ch == b'/' || ch == b'?' || ch == b'#' {
                    break;
                }
                if ch == b':' {
                    colon = Some(c);
                    break;
                }
            }
            if let Some(colon) = colon
                && colon + 1 < i // pass non-empty
            {
                // User part: back from the `:` to whitespace, `/`, `@` or the start.
                let mut u = colon;
                while u > last
                    && !b[u - 1].is_ascii_whitespace()
                    && b[u - 1] != b'/'
                    && b[u - 1] != b'@'
                {
                    u -= 1;
                }
                if u < colon {
                    out.push_str(&s[last..u]);
                    out.push_str("[redacted]");
                    last = i + 1;
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    out
}

/// Every absolute path on both OS families (unix `/…`, `~/…`, drive `C:\…`/`C:/…`, UNC
/// `\\server\…`) plus relative `./…`/`../…`, including spaces and dots, is replaced
/// wholesale with `[redacted]`. A space continues a path only while the next character is
/// still path-shaped (so `…/Application Support/…` is one path), which can swallow trailing
/// prose until a non-path character — over-redaction is the safe direction here.
fn redact_paths(s: &str) -> String {
    let b = s.as_bytes();
    let is_path_char = |c: u8| {
        c.is_ascii_alphanumeric()
            || matches!(c, b'/' | b'\\' | b'.' | b'_' | b'-' | b'+' | b'%' | b'#' | b'@' | b'~' | b':' | b'\'')
    };
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    let mut i = 0;
    while i < b.len() {
        let prev_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let start = prev_ok
            && (b[i] == b'/'
                || (b[i] == b'~' && i + 1 < b.len() && b[i + 1] == b'/')
                || (b[i] == b'.'
                    && i + 1 < b.len()
                    && (b[i + 1] == b'/' || (i + 2 < b.len() && b[i + 1] == b'.' && b[i + 2] == b'/')))
                || (b[i].is_ascii_alphabetic()
                    && i + 2 < b.len()
                    && b[i + 1] == b':'
                    && (b[i + 2] == b'/' || b[i + 2] == b'\\'))
                || (b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'\\'));
        if start {
            let mut j = i;
            while j < b.len() {
                if is_path_char(b[j]) {
                    j += 1;
                } else if b[j] == b' ' && j + 1 < b.len() && is_path_char(b[j + 1]) {
                    j += 1;
                } else {
                    break;
                }
            }
            if j > i {
                out.push_str(&s[last..i]);
                out.push_str("[redacted]");
                last = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    out
}

/// A run long enough to be a token/blob reads as one opaque replacement.
fn flush_token_run(out: &mut String, run: &mut String, min: usize) {
    if run.chars().count() >= min {
        out.push_str("[redacted]");
    } else {
        out.push_str(run);
    }
    run.clear();
}

/// What a pending question carries on the wire (contract §3 `QuestionEvidence`).
#[derive(Debug, Clone)]
struct Question {
    prompt: String,
    options: Vec<String>,
    tool_name: Option<String>,
    source_evidence: String,
}

fn question_json(q: &Question) -> Value {
    json!({
        "prompt": q.prompt,
        "options": q.options,
        "tool_name": q.tool_name,
        "source_evidence": q.source_evidence,
        "status": "pending",
        // Deferred in v1 (contract §3): a pending question has no resolution by definition,
        // and an absent resolution must never make a stale one actionable (§6.2).
        "resolution_evidence": null,
    })
}

/// The question this session is holding open, from the live manager state.
fn live_question(state: &AppState, id: &str) -> Option<Question> {
    let live = state.live.lock().unwrap();
    let agent = live.get(id)?;
    if let Some((_, ev)) = agent.questions.first() {
        return question_from_event(ev);
    }
    if let Some((_, ev)) = agent.permissions.first() {
        return question_from_event(ev);
    }
    agent.auth.as_ref().map(|_| Question {
        prompt: "Sign-in required".into(),
        options: vec![],
        tool_name: None,
        source_evidence: "authRequired (agent)".into(),
    })
}

/// The wire question out of a `question` or `permission` CodeEvent as the event loop stored it.
fn question_from_event(ev: &Value) -> Option<Question> {
    if let Some(form) = ev.get("form") {
        let q = form["questions"].as_array()?.first()?;
        let options = q["options"]
            .as_array()
            .map(|os| os.iter().filter_map(|o| o["label"].as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        return Some(Question {
            prompt: q["prompt"].as_str()?.to_owned(),
            options,
            // An agent's own question form is not a tool call; nothing to name.
            tool_name: None,
            source_evidence: "agent question (live_memory)".into(),
        });
    }
    let req = ev.get("request")?;
    let tool = &req["toolCall"];
    let tool_name = tool["title"].as_str().filter(|t| !t.is_empty()).map(str::to_owned);
    // A permission request that is a menu reads as the card the app shows; a safety prompt has
    // no card, so its options are the raw allow/reject names.
    let (prompt, options) = match crate::code::questions::from_permission(tool, &req["options"]) {
        Some(form) => {
            let q = form.questions.first()?;
            (q.prompt.clone(), q.options.iter().map(|o| o.label.clone()).collect())
        }
        None => {
            let prompt = tool_name.clone().unwrap_or_else(|| "Permission requested".into());
            let options = req["options"]
                .as_array()
                .map(|os| os.iter().filter_map(|o| o["name"].as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            (prompt, options)
        }
    };
    Some(Question {
        prompt,
        options,
        source_evidence: match &tool_name {
            Some(t) => format!("{t} (tool_use)"),
            None => "permission request (live_memory)".into(),
        },
        tool_name,
    })
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The title, falling back to the folder's name as the contract requires.
fn title(s: &CodeSession) -> String {
    if s.title.trim().is_empty() {
        project_name(&s.cwd)
    } else {
        s.title.clone()
    }
}

fn project_name(cwd: &str) -> String {
    std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cwd.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// The fixture Central reviews against, copied **byte-identical** from Central's scrubbed
    /// copy (rewritten commit `fc32611`, pushed main `edde078`, sha256 `eb577521…`). Central's
    /// pre-scrub history carried two real local session UUIDs and a real session title, which
    /// the scrub removed, so no real ids, titles or home-directory paths live in this repo's
    /// tests. Shape, states and keys are unchanged: the route's response must match that
    /// shape — same keys, same types, no envelope `version`.
    const FIXTURE: &str = include_str!("../tests/fixtures/daemon_sessions_golden_synthetic.json");

    fn state() -> (AppState, std::path::PathBuf) {
        let dir = crate::test_support::short_dir("sessions-api");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Arc::new(crate::db::Db::open(&dir.join("db.sqlite")).unwrap());
        let mesh = crate::mesh::Mesh::new(db.clone(), dir.clone(), Arc::new(crate::mesh::keys::MemKeyStore::default())).unwrap();
        let emit: crate::mesh::node::UiSink = Arc::new(|_: &str, _: Value| {});
        (AppState::new(db, dir.clone(), mesh, emit), dir)
    }

    fn get(token: Option<&str>, uri: &str) -> Request<Body> {
        let mut b = Request::builder().uri(uri);
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    async fn body_json(res: Response) -> Value {
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ---- the state mapper (contract §6) ----

    fn ev() -> Evidence {
        Evidence {
            running: false,
            busy: false,
            waiting: false,
            turn_end: None,
            question: None,
            last_activity_at: None,
            live: false,
        }
    }

    fn pending_question() -> Question {
        Question {
            prompt: "Approve?".into(),
            options: vec!["Approve".into(), "Reject".into()],
            tool_name: Some("ask_question".into()),
            source_evidence: "ask_question (tool_use)".into(),
        }
    }

    #[test]
    fn all_five_states_come_from_affirmative_evidence() {
        let mut e = ev();
        e.running = true;
        e.waiting = true;
        e.question = Some(pending_question());
        assert_eq!(state_of(&e).0, WireState::NeedsYou, "live open request");

        let mut e = ev();
        e.turn_end = Some(crate::code::TurnEnd::Error("agent exited mid-turn".into()));
        assert_eq!(state_of(&e).0, WireState::Stuck, "explicit unrecovered error");
        assert_eq!(state_of(&e).1.as_deref(), Some("agent exited mid-turn"), "stuck_reason");

        let mut e = ev();
        e.running = true;
        e.busy = true;
        assert_eq!(state_of(&e).0, WireState::Running, "turn in flight");

        let mut e = ev();
        e.turn_end = Some(crate::code::TurnEnd::Clean);
        assert_eq!(state_of(&e).0, WireState::Done, "turn ended without error");

        assert_eq!(state_of(&ev()).0, WireState::Unknown, "no evidence at all");
    }

    #[test]
    fn silence_never_becomes_a_state() {
        // Stopped, idle, quiet: without affirmative evidence the answer is `unknown`, never
        // `done` (no completion seen) and never `stuck` (no error seen).
        let mut e = ev();
        e.running = false;
        e.busy = false;
        assert_eq!(state_of(&e).0, WireState::Unknown);
        let mut e = ev();
        e.running = true; // alive but never had a turn we saw end
        assert_eq!(state_of(&e).0, WireState::Unknown);
        let mut e = ev();
        e.busy = true; // busy without a running agent proves nothing
        assert_eq!(state_of(&e).0, WireState::Unknown);
    }

    #[test]
    fn a_stale_question_is_never_actionable() {
        // Contract §6.2: question evidence from an earlier turn whose turn has moved on must
        // not come back `needs_you`, and a null resolution must not make it actionable.
        let mut e = ev();
        e.running = true;
        e.busy = false;
        e.turn_end = Some(crate::code::TurnEnd::Clean);
        e.question = Some(pending_question());
        assert_eq!(state_of(&e).0, WireState::Done, "stale question must not be needs_you");
        assert_eq!(state_of(&e).1, None, "a done session carries no stuck_reason");
    }

    #[test]
    fn a_stalled_turn_is_running_not_stuck() {
        // Quiet is not an error (§6.3): a turn with nothing from the agent stays `running`.
        let mut e = ev();
        e.running = true;
        e.busy = true;
        assert_eq!(state_of(&e).0, WireState::Running);
    }

    #[test]
    fn an_explicit_error_wins_over_waiting_and_busy() {
        let mut e = ev();
        e.running = true;
        e.busy = true;
        e.waiting = true;
        e.question = Some(pending_question());
        e.turn_end = Some(crate::code::TurnEnd::Error("the turn ended with an error".into()));
        assert_eq!(state_of(&e).0, WireState::Stuck, "unrecovered error beats everything else");
    }

    #[tokio::test]
    async fn stuck_reason_redacts_the_home_path_and_the_token() {
        // The contract key is agreed free text ("Explicit error string"), so an error
        // string carrying a home-directory path and a token must not reach the response
        // verbatim (Ship's probe: the raw message used to pass through from state_of).
        let (s, dir) = state();
        let id = s
            .db
            .create_code_session(std::env::temp_dir().to_str().unwrap(), None, "patchbay", "default")
            .expect("session row")
            .id
            .clone();
        let home = crate::code::agents::home_dir().expect("home dir");
        let path = home
            .join("Library")
            .join("Application Support")
            .join("dev.patchbay.app")
            .join("web-token");
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef"; // 48 hex, like web-token
        s.code.note_turn_end(
            &id,
            crate::code::TurnEnd::Error(format!("agent died reading {} with token={}", path.display(), token)),
        );
        let tok = crate::web_token::load_token(&dir, false).unwrap();
        let res = router(s.clone(), tok.clone(), DAEMON)
            .oneshot(get(Some(&tok), "/api/v1/sessions"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;
        let session = &body["sessions"][0];
        assert_eq!(session["state"], json!("stuck"), "explicit error is stuck");
        let reason = session["stuck_reason"].as_str().expect("stuck_reason text");
        let home_s = home.to_str().expect("home utf-8");
        let path_s = path.to_str().expect("path utf-8");
        assert!(!reason.contains(home_s), "home directory must be redacted: {reason}");
        assert!(!reason.contains(path_s), "the path must not appear: {reason}");
        assert!(!reason.contains(token), "the token must be redacted: {reason}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stuck_reason_redacts_paths_secrets_and_userinfo() {
        // Ship's battery: every shape must come out redacted, never verbatim.
        let cases: &[&str] = &[
            "/etc/passwd",
            "/home/bob/x",
            r"C:\Users\alice\x",
            r"\\server\share\f",
            "/Users/dfrazer2/x",
            "token=abc",
            "Bearer abc123",
            "user:pass@host",
        ];
        for c in cases {
            let out = sanitize_reason(c);
            assert!(!out.contains(c), "must not survive verbatim: {c:?} -> {out:?}");
            assert!(out.contains("[redacted]"), "expected a redaction marker: {c:?} -> {out:?}");
        }
        // A path is redacted AS A PATH — never rewritten to a `~` form (`~2/x` hazard).
        let out = sanitize_reason("/Users/dfrazer2/x");
        assert!(!out.contains("~2/x"), "no home-prefix rewrite: {out:?}");
    }

    #[test]
    fn a_benign_stuck_reason_is_untouched() {
        // Ordinary diagnostics pass through byte-for-byte (no false redactions).
        assert_eq!(sanitize_reason("agent exited mid-turn"), "agent exited mid-turn");
    }

    // ---- the route ----

    #[tokio::test]
    async fn the_token_file_is_the_one_discovery_path_for_both_surfaces() {
        use crate::web_token::load_token;
        let (s, dir) = state();
        // The same file both processes create and read (app-only and patchbayd-only hosts each
        // read this one path; both running share it).
        let token = load_token(&dir, false).expect("token created");
        assert_eq!(load_token(&dir, false).unwrap(), token, "second reader gets the same token");

        for surface in [DAEMON, APP] {
            let res = router(s.clone(), token.clone(), surface)
                .oneshot(get(Some(&token), "/api/v1/sessions"))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "{}", surface.provider);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_missing_or_wrong_token_is_refused() {
        let (s, dir) = state();
        let r = || router(s.clone(), "right-token".into(), DAEMON);
        for (tok, want) in [(None, StatusCode::UNAUTHORIZED), (Some("wrong"), StatusCode::UNAUTHORIZED)] {
            let res = r().oneshot(get(tok, "/api/v1/sessions")).await.unwrap();
            assert_eq!(res.status(), want, "token {tok:?}");
        }
        let res = r().oneshot(get(Some("right-token"), "/api/v1/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_app_router_answers_404_for_websockets_and_commands() {
        // The app's listener gets the read-only route and nothing else: no `/ws`, which on
        // patchbayd is full command power behind the same token, and no other path a command
        // could be reached through.
        let (s, dir) = state();
        let token = crate::web_token::load_token(&dir, false).unwrap();
        let registry = Arc::new(crate::shares::ShareRegistry::load(
            &dir,
            crate::shares::DEFAULT_BASE_URL.to_owned(),
            "node".to_owned(),
        ));
        let app = crate::shares::app_router(registry, s.clone(), Some(token.clone()));

        for path in ["/ws", "/api", "/api/v1", "/api/v1/commands", "/api/v1/sessions/abc", "/commands", "/code"] {
            let res = app.clone().oneshot(get(Some(&token), path)).await.unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "the app router must not serve {path}");
        }
        let res = app.clone().oneshot(get(Some(&token), "/api/v1/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "but it does serve the read-only route");
        // Read-only: a write-shaped request has no route at all.
        let res = app
            .clone()
            .oneshot(Request::builder().method("POST").uri("/api/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED, "POST must not be handled");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_app_only_host_creates_the_token_and_serves_the_route() {
        // What `shares::spawn` does on an app-only host (the case -04 exists for): no token file
        // in a fresh data folder, `load_token` CREATES it (0600) and only a creation failure
        // would leave the route unmounted — so the app-only host gets live status.
        let (s, dir) = state();
        let token_path = dir.join("web-token");
        assert!(!token_path.exists(), "fresh data folder has no token yet");
        let token = crate::web_token::load_token(&dir, false).expect("spawn's call creates it");
        assert!(token_path.exists(), "the call spawn() makes must create the file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "created 0600");
        }
        let registry = Arc::new(crate::shares::ShareRegistry::load(
            &dir,
            crate::shares::DEFAULT_BASE_URL.to_owned(),
            "node".to_owned(),
        ));
        let app = crate::shares::app_router(registry, s.clone(), Some(token.clone()));
        let res = app.clone().oneshot(get(Some(&token), "/api/v1/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "app-only host answers with live status");
        let body = body_json(res).await;
        assert_eq!(body["provider"], json!(APP.provider), "labelled as this surface");
        let res = app.oneshot(get(Some(&token), "/ws")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "still no /ws");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_port_is_double_booked_by_design_and_that_stays_documented() {
        // `shares::DEFAULT_BASE_URL` and patchbayd's `--listen` default are both
        // `127.0.0.1:4420`, so a host running both has one port for two servers and the first
        // binder wins (docs/headless.md). pb-20261110-04 does not change the port behaviour —
        // it only makes the consequence discoverable, so this pins the note to the code.
        assert_eq!(crate::shares::DEFAULT_BASE_URL, "http://127.0.0.1:4420");
        let bin = include_str!("bin/patchbayd.rs");
        assert!(bin.contains("\"127.0.0.1:4420\""), "patchbayd's --listen default must stay 4420 for the note to hold");
        let docs = include_str!("../../docs/headless.md");
        assert!(docs.contains("double-booked by design"), "the port conflict must stay documented");
    }

    // ---- golden fixture shape (acceptance input 2) ----

    #[tokio::test]
    async fn the_response_matches_the_golden_fixture_shape() {
        let (s, dir) = state();
        // A real row, so per-record shape is exercised.
        s.db
            .create_code_session(std::env::temp_dir().to_str().unwrap(), None, "patchbay", "default")
            .expect("session row");
        let token = crate::web_token::load_token(&dir, false).unwrap();
        let res = router(s.clone(), token.clone(), DAEMON)
            .oneshot(get(Some(&token), "/api/v1/sessions"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_json(res).await;

        // The fixture: exactly `observed_at`, `provider`, `sessions` — no version key.
        let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
        let mut fk: Vec<String> = fixture.as_object().unwrap().keys().cloned().collect();
        fk.sort();
        assert_eq!(fk, ["observed_at", "provider", "sessions"], "fixture envelope keys");
        let mut bk: Vec<String> = body.as_object().unwrap().keys().cloned().collect();
        bk.sort();
        assert_eq!(bk, fk, "response envelope keys must match the fixture's (version lives in the path)");

        let sessions = body["sessions"].as_array().unwrap();
        assert!(!sessions.is_empty(), "the row above must be served");
        let fixture_sessions = fixture["sessions"].as_array().unwrap();
        let template = &fixture_sessions[0];
        for got in sessions {
            assert_same_shape("$.sessions[]", template, got);
            assert!(got["teams"].is_null(), "teams is REQUIRED null in v1 (pb-20261110-19)");
            assert!(got["capabilities"]["can_focus"] == json!(false), "can_focus is unbuilt (pb-20261110-05)");
            assert!(got["capabilities"]["can_answer"] == json!(false), "can_answer is unbuilt");
            assert_eq!(got["capabilities"]["answer_type"], json!("none"));
            assert!(got["navigation_target_uri"].is_null(), "deep-link is pb-20261110-05");
            assert!(matches!(got["state"].as_str(), Some("needs_you" | "stuck" | "running" | "done" | "unknown")));
        }
        // A pending question matches the fixture's question object shape too (nullable fields
        // may be null; the keys and types must match).
        let pending = question_json(&Question {
            prompt: "x".into(),
            options: vec!["a".into()],
            tool_name: None,
            source_evidence: "agent question (live_memory)".into(),
        });
        assert_same_shape("question", &fixture_sessions[0]["question"], &pending);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `expected` (fixture) and `got` (response) have the same shape: same keys and types.
    /// `null` matches only where the contract allows a null (see `NULLABLE`).
    fn assert_same_shape(path: &str, expected: &Value, got: &Value) {
        const NULLABLE: &[&str] = &[
            "$.sessions[].question",
            "$.sessions[].stuck_reason",
            "$.sessions[].last_activity_at",
            "$.sessions[].navigation_target_uri",
            "$.sessions[].question.tool_name",
            "$.sessions[].question.resolution_evidence",
            "$.sessions[].teams",
            "question.tool_name",
            "question.resolution_evidence",
        ];
        if got.is_null() || expected.is_null() {
            // Nullability comes from the contract (§3), not from the fixture: the fixture's
            // synthetic records simply have no stuck/late/deep-link values.
            assert!(
                NULLABLE.iter().any(|n| path == *n || path.ends_with(&n[1..])),
                "{path}: null on a field the contract requires non-null (fixture: {expected}, response: {got})"
            );
            return;
        }
        match (expected, got) {
            (Value::Object(e), Value::Object(g)) => {
                let ek: std::collections::BTreeSet<_> = e.keys().collect();
                let gk: std::collections::BTreeSet<_> = g.keys().collect();
                assert_eq!(ek, gk, "{path}: key set differs from the fixture");
                for (k, ev) in e {
                    assert_same_shape(&format!("{path}.{k}"), ev, &g[k]);
                }
            }
            (Value::Array(e), Value::Array(g)) => {
                assert!(!e.is_empty(), "{path}: fixture has no template element");
                assert!(!g.is_empty(), "{path}: response array is empty where the fixture has records");
                for got in g {
                    assert_same_shape(&format!("{path}[]"), &e[0], got);
                }
            }
            (Value::Number(_), Value::Number(_)) => {}
            (Value::String(_), Value::String(_)) => {}
            (Value::Bool(_), Value::Bool(_)) => {}
            _ => panic!("{path}: type differs — fixture {expected}, response {got}"),
        }
    }
}
