//! Why an opencode turn went quiet. When a turn stalls (`code::stall`) on an opencode session,
//! opencode's own database often says why, even when nothing reaches Patchbay over ACP. Read
//! read-only, it answers three questions for the stall notice:
//!
//! - the newest error of the current turn (`provider.free-limit`, a 429, …), from the session or
//!   one of its subagents (pb-20261107-18);
//! - whether opencode is running a turn Patchbay can't see (pb-20261107-25): when a background
//!   shell or a subagent finishes after a turn ended, opencode starts a new turn by itself, and
//!   none of it (its text, its tool calls, its permission requests) is sent over ACP. A tool call
//!   in such a turn that needs a permission waits for an answer nobody is asked for;
//! - how many messages are queued behind that turn in opencode's inbox.
//!
//! Deliberately not used: `session_v2.time_suspended`. Despite the name it is stamped whenever a
//! background shell finishes and wakes the turn, not when opencode stops at a limit.
//!
//! opencode's schema is its own and has changed before (`session` became `session_v2`). The
//! hidden-turn and queue answers fail loud: when the tables or columns they need aren't there, the
//! readout says it can't read opencode's state, never "nothing waiting", because a silent zero
//! would recreate the blind spot this exists to close. The error lookup stays best-effort on top.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;

/// What opencode's database says about a session's current turn.
#[derive(Debug, PartialEq)]
pub enum Readout {
    /// opencode's state couldn't be read, and why. Nothing is known: not "no hidden turn", not
    /// "nothing waiting".
    Unreadable(String),
    Read(Report),
}

#[derive(Debug, PartialEq, Default)]
pub struct Report {
    pub error: Option<TurnError>,
    pub hidden: Option<HiddenTurn>,
    /// Messages in opencode's inbox for the session, waiting for the running turn to end.
    pub waiting: u32,
}

/// The newest error opencode recorded in a session's current turn.
#[derive(Debug, PartialEq)]
pub struct TurnError {
    pub error: String,
    /// The subagent's title, when the error came from one.
    pub from: Option<String>,
}

/// A turn opencode started by itself, which Patchbay is never sent.
#[derive(Debug, PartialEq)]
pub struct HiddenTurn {
    /// What woke it: "a background shell finishing" or "a subagent finishing".
    pub woken_by: &'static str,
    /// The tool call it is on, if any.
    pub tool: Option<ToolState>,
}

#[derive(Debug, PartialEq)]
pub struct ToolState {
    pub name: String,
    pub command: Option<String>,
}

/// The tables and columns the hidden-turn and queue answers need.
const NEEDED: &[(&str, &[&str])] = &[
    (
        "session_message",
        &["session_id", "type", "time_created", "data"],
    ),
    ("session_inbox", &["session_id"]),
    ("session_v2", &["id", "parent_id", "title"]),
];

/// opencode's database: `$XDG_DATA_HOME/opencode/opencode.db`, else under `~/.local/share` (opencode
/// uses that layout on every platform). Whether it exists is `read`'s to report.
pub fn db_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".local/share"))
        })?;
    Some(base.join("opencode").join("opencode.db"))
}

/// What opencode's database at `path` says about `session`'s current turn. Read-only.
pub fn read(path: &Path, session: &str) -> Readout {
    if !path.is_file() {
        return Readout::Unreadable(format!(
            "there is no opencode database at {}",
            path.display()
        ));
    }
    let conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => return Readout::Unreadable(format!("opening opencode's database failed: {e}")),
    };
    // opencode may be writing: wait a moment for its lock, never long.
    if let Err(e) = conn.busy_timeout(Duration::from_millis(250)) {
        return Readout::Unreadable(format!("opencode's database: {e}"));
    }
    read_conn(&conn, session)
}

fn read_conn(conn: &Connection, session: &str) -> Readout {
    match report(conn, session) {
        Ok(r) => Readout::Read(r),
        Err(why) => Readout::Unreadable(why),
    }
}

fn report(conn: &Connection, session: &str) -> Result<Report, String> {
    check_schema(conn)?;
    let known: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_v2 WHERE id = ?1)",
            [session],
            |r| r.get(0),
        )
        .map_err(|e| format!("reading opencode's sessions failed: {e}"))?;
    if !known {
        return Err(format!("opencode's database has no session {session}"));
    }
    let waiting: u32 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_inbox WHERE session_id = ?1",
            [session],
            |r| r.get(0),
        )
        .map_err(|e| format!("reading opencode's inbox failed: {e}"))?;
    Ok(Report {
        error: turn_error(conn, session),
        hidden: hidden_turn(conn, session)?,
        waiting,
    })
}

fn check_schema(conn: &Connection) -> Result<(), String> {
    for (table, cols) in NEEDED {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|e| format!("reading opencode's schema failed: {e}"))?;
        let have: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| format!("reading opencode's schema failed: {e}"))?
            .flatten()
            .collect();
        if have.is_empty() {
            return Err(format!(
                "opencode's database has no {table} table (its schema changed)"
            ));
        }
        if let Some(c) = cols.iter().find(|c| !have.iter().any(|h| h == *c)) {
            return Err(format!(
                "opencode's {table} table has no {c} column (its schema changed)"
            ));
        }
    }
    Ok(())
}

/// The turn opencode is running by itself, if it is: the session's newest message is an
/// unfinished assistant reply, and what started it was a background shell's or a subagent's
/// result rather than a prompt (which is what Patchbay sends).
fn hidden_turn(conn: &Connection, session: &str) -> Result<Option<HiddenTurn>, String> {
    let failed = |e: rusqlite::Error| format!("reading opencode's messages failed: {e}");
    let newest: Option<(String, String)> = conn
        .query_row(
            "SELECT type, data FROM session_message WHERE session_id = ?1 ORDER BY time_created DESC, rowid DESC LIMIT 1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(failed(e)),
        })?;
    let Some((kind, data)) = newest else {
        return Ok(None);
    };
    let reply: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
    if kind != "assistant" || !reply["time"]["completed"].is_null() {
        return Ok(None);
    }
    let starter: Option<(String, String)> = conn
        .query_row(
            "SELECT type, data FROM session_message WHERE session_id = ?1 AND type IN ('user', 'synthetic') \
             ORDER BY time_created DESC, rowid DESC LIMIT 1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(failed(e)),
        })?;
    let Some(("synthetic", data)) = starter.as_ref().map(|(k, d)| (k.as_str(), d)) else {
        return Ok(None);
    };
    let woken_by =
        match serde_json::from_str::<Value>(data).unwrap_or(Value::Null)["metadata"]["source"]
            .as_str()
        {
            Some("shell") => "a background shell finishing",
            Some("subagent") => "a subagent finishing",
            _ => "opencode itself",
        };
    Ok(Some(HiddenTurn {
        woken_by,
        tool: running_tool(&reply),
    }))
}

/// The reply's tool call that is still running, the last one if several.
fn running_tool(reply: &Value) -> Option<ToolState> {
    let part = reply["content"]
        .as_array()?
        .iter()
        .rev()
        .find(|p| p["type"] == "tool" && p["state"]["status"] == "running")?;
    Some(ToolState {
        name: part["name"].as_str().unwrap_or("a tool").to_owned(),
        command: part["state"]["input"]["command"]
            .as_str()
            .map(|c| clip(&c.split_whitespace().collect::<Vec<_>>().join(" "), 120)),
    })
}

/// The newest error of the session's current turn (which began with its latest user message),
/// from the session or its subagents. Best-effort: any failure is no error found.
fn turn_error(conn: &Connection, session: &str) -> Option<TurnError> {
    let turn_start: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(time_created), 0) FROM session_message WHERE session_id = ?1 AND type = 'user'",
            [session],
            |r| r.get(0),
        )
        .ok()?;
    let mut stmt = conn
        .prepare(
            "SELECT m.session_id, m.data, s.title FROM session_message m LEFT JOIN session_v2 s ON s.id = m.session_id \
             WHERE (m.session_id = ?1 OR m.session_id IN (SELECT id FROM session_v2 WHERE parent_id = ?1)) \
             AND m.time_created >= ?2 AND m.data LIKE '%error%' ORDER BY m.time_created DESC LIMIT 20",
        )
        .ok()?;
    let rows = stmt
        .query_map(params![session, turn_start], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .ok()?;
    rows.flatten().find_map(|(sid, data, title)| {
        let error = error_text(&serde_json::from_str::<Value>(&data).ok()?)?;
        Some(TurnError {
            error,
            from: (sid != session).then_some(title).flatten(),
        })
    })
}

/// The error a stored message carries: an `error.message` (a failed step or idle), or a subagent
/// report whose state is `error`.
fn error_text(v: &Value) -> Option<String> {
    let text = v["error"]["message"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            (v["metadata"]["state"] == "error")
                .then(|| v["text"].as_str().map(strip_tags))
                .flatten()
        })?;
    let text = text.trim();
    (!text.is_empty()).then(|| clip(text, 240))
}

/// `<subagent …>body</subagent>` -> `body`.
fn strip_tags(s: &str) -> String {
    s.lines()
        .filter(|l| !(l.trim_start().starts_with('<') && l.trim_end().ends_with('>')))
        .collect::<Vec<_>>()
        .join(" ")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    format!("{}…", s.chars().take(max).collect::<String>().trim_end())
}

fn messages(n: u32) -> String {
    match n {
        1 => "1 message is".into(),
        n => format!("{n} messages are"),
    }
}

/// The paragraph added to the stall notice.
pub fn describe(r: &Readout) -> String {
    let r = match r {
        Readout::Unreadable(why) => {
            return format!(
                "Patchbay can't read opencode's state ({why}), so it can't tell whether opencode is running a turn \
                 it can't see or how many messages are waiting."
            );
        }
        Readout::Read(r) => r,
    };
    let mut out = Vec::new();
    match &r.hidden {
        Some(h) => {
            let mut s = format!(
                "opencode is running a turn Patchbay can't see (started by {}); its output never reaches Patchbay.",
                h.woken_by
            );
            // Patchbay runs opencode with every tool set to ask, and a hidden turn's permission
            // requests don't reach Patchbay either, so a call it is on is most likely waiting for
            // one. opencode's `executed` flag doesn't tell: it is false on calls that completed.
            if let Some(t) = &h.tool {
                s.push_str(&format!(
                    " It is on a {} call{}, most likely waiting for a permission request Patchbay is never \
                     shown; stopping the session ends it.",
                    t.name,
                    t.command.as_ref().map(|c| format!(" `{c}`")).unwrap_or_default()
                ));
            }
            out.push(s);
        }
        None => out.push("opencode's records show no turn running that Patchbay can't see.".into()),
    }
    out.push(match r.waiting {
        0 => "No messages are waiting in opencode's inbox.".into(),
        n => format!(
            "{} waiting in opencode's inbox behind the running turn.",
            messages(n)
        ),
    });
    if let Some(e) = &r.error {
        out.push(match &e.from {
            Some(t) => format!(
                "opencode's latest error this turn, from subagent \"{t}\": {}",
                e.error
            ),
            None => format!("opencode's latest error this turn: {}", e.error),
        });
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The columns of opencode's schema this reads, as seen in opencode 2.1.3.
    fn fixture() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT, time_suspended INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, data TEXT);
             CREATE TABLE session_inbox (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT, payload TEXT, delivery TEXT, enqueued_seq INTEGER, time_created INTEGER);
             INSERT INTO session_v2 VALUES ('ses_p', NULL, 'Desk', NULL), ('ses_c', 'ses_p', 'Fail-loud fixes', NULL);",
        )
        .unwrap();
        c
    }

    fn msg(c: &Connection, id: &str, session: &str, kind: &str, t: i64, data: Value) {
        c.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, data) VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![id, session, kind, t, data.to_string()],
        )
        .unwrap();
    }

    fn queue(c: &Connection, id: &str, session: &str) {
        c.execute(
            "INSERT INTO session_inbox VALUES (?1, ?2, 'user', '{}', 'queued', 0, 0)",
            params![id, session],
        )
        .unwrap();
    }

    fn report_of(c: &Connection, session: &str) -> Report {
        match read_conn(c, session) {
            Readout::Read(r) => r,
            Readout::Unreadable(why) => panic!("unreadable: {why}"),
        }
    }

    /// The 2026-11-08 desk hang, as opencode stored it: the prompted turn ended (`idle`), a
    /// background shell finished, opencode woke a turn by itself and parked on a shell call, and
    /// a message queued behind it. opencode stored `executed: false` on it, as on calls that ran.
    fn self_woken(c: &Connection) {
        msg(
            c,
            "m1",
            "ses_p",
            "user",
            100,
            serde_json::json!({ "text": "rebuild the app" }),
        );
        msg(
            c,
            "m2",
            "ses_p",
            "assistant",
            110,
            serde_json::json!({ "time": { "created": 110, "completed": 140 } }),
        );
        msg(
            c,
            "m3",
            "ses_p",
            "idle",
            141,
            serde_json::json!({ "outcome": "succeeded" }),
        );
        msg(
            c,
            "m4",
            "ses_p",
            "synthetic",
            200,
            serde_json::json!({ "metadata": { "source": "shell", "state": "completed" }, "text": "<shell …>built</shell>" }),
        );
        msg(
            c,
            "m5",
            "ses_p",
            "assistant",
            201,
            serde_json::json!({ "time": { "created": 201, "streamed": 220 }, "content": [
                { "type": "reasoning", "text": "Build succeeded. Verify the bundle." },
                { "type": "tool", "id": "call_1", "name": "shell", "executed": false,
                  "state": { "status": "running", "input": { "command": "codesign -dv  \"$nb\"\nls -d /Applications/Patchbay*.app" } } },
            ] }),
        );
    }

    const LIMIT: &str = "Free usage limit reached. Connect a provider or subscribe to OpenCode Go to keep going, or switch models.";

    #[test]
    fn a_self_woken_turn_parked_on_a_call_is_named_with_the_queue_behind_it() {
        let c = fixture();
        self_woken(&c);
        queue(&c, "q1", "ses_p");
        queue(&c, "q2", "ses_p");
        queue(&c, "q3", "ses_c"); // another session's queue isn't this one's
        let r = report_of(&c, "ses_p");
        assert_eq!(
            r.hidden,
            Some(HiddenTurn {
                woken_by: "a background shell finishing",
                tool: Some(ToolState {
                    name: "shell".into(),
                    command: Some("codesign -dv \"$nb\" ls -d /Applications/Patchbay*.app".into()),
                }),
            })
        );
        assert_eq!(r.waiting, 2);
        let text = describe(&Readout::Read(r));
        assert!(
            text.contains(
                "running a turn Patchbay can't see (started by a background shell finishing)"
            ),
            "{text}"
        );
        assert!(text.contains("It is on a shell call `codesign"), "{text}");
        assert!(
            text.contains("permission request Patchbay is never shown"),
            "{text}"
        );
        assert!(
            text.contains("2 messages are waiting in opencode's inbox"),
            "{text}"
        );
    }

    #[test]
    fn a_self_woken_turn_with_no_call_open_and_nothing_queued_says_both() {
        let c = fixture();
        self_woken(&c);
        c.execute(
            "UPDATE session_message SET data = json_remove(data, '$.content[1]') WHERE id = 'm5'",
            [],
        )
        .unwrap();
        let r = report_of(&c, "ses_p");
        assert_eq!(r.hidden.as_ref().map(|h| &h.tool), Some(&None));
        let text = describe(&Readout::Read(r));
        assert!(!text.contains("permission"), "{text}");
        assert!(text.contains("No messages are waiting"), "{text}");
    }

    #[test]
    fn a_turn_patchbay_prompted_or_one_that_ended_is_not_hidden() {
        let c = fixture();
        msg(
            &c,
            "m1",
            "ses_p",
            "user",
            100,
            serde_json::json!({ "text": "go" }),
        );
        msg(
            &c,
            "m2",
            "ses_p",
            "assistant",
            110,
            serde_json::json!({ "time": { "created": 110 } }),
        );
        assert_eq!(
            report_of(&c, "ses_p").hidden,
            None,
            "a prompted turn Patchbay is in"
        );

        let c = fixture();
        self_woken(&c);
        c.execute(
            "UPDATE session_message SET data = json_set(data, '$.time.completed', 300) WHERE id = 'm5'",
            [],
        )
        .unwrap();
        msg(
            &c,
            "m6",
            "ses_p",
            "idle",
            301,
            serde_json::json!({ "outcome": "succeeded" }),
        );
        let r = report_of(&c, "ses_p");
        assert_eq!(r.hidden, None, "the self-woken turn ended");
        let text = describe(&Readout::Read(r));
        assert!(
            text.contains("no turn running that Patchbay can't see"),
            "{text}"
        );
        assert!(text.contains("No messages are waiting"), "{text}");
    }

    #[test]
    fn a_turn_woken_by_a_subagent_says_so() {
        let c = fixture();
        msg(
            &c,
            "m1",
            "ses_p",
            "user",
            100,
            serde_json::json!({ "text": "go" }),
        );
        msg(
            &c,
            "m2",
            "ses_p",
            "idle",
            120,
            serde_json::json!({ "outcome": "succeeded" }),
        );
        msg(
            &c,
            "m3",
            "ses_p",
            "synthetic",
            150,
            serde_json::json!({ "metadata": { "source": "subagent", "state": "completed" }, "text": "done" }),
        );
        msg(
            &c,
            "m4",
            "ses_p",
            "assistant",
            151,
            serde_json::json!({ "time": { "created": 151 } }),
        );
        let r = report_of(&c, "ses_p");
        assert_eq!(r.hidden.map(|h| h.woken_by), Some("a subagent finishing"));
    }

    /// The point of pb-20261107-25: a schema this can't read must never come out as "nothing
    /// waiting". Each missing table or column is reported, and the text says nothing is known.
    #[test]
    fn a_schema_it_cannot_read_is_reported_never_a_zero() {
        let cases: &[(&str, &str)] = &[
            ("DROP TABLE session_inbox", "no session_inbox table"),
            (
                "ALTER TABLE session_inbox RENAME COLUMN session_id TO sid",
                "session_inbox table has no session_id column",
            ),
            (
                "ALTER TABLE session_message RENAME COLUMN data TO body",
                "session_message table has no data column",
            ),
            ("DROP TABLE session_message", "no session_message table"),
            (
                "ALTER TABLE session_v2 RENAME TO session",
                "no session_v2 table",
            ),
        ];
        for (change, says) in cases {
            let c = fixture();
            self_woken(&c);
            queue(&c, "q1", "ses_p");
            c.execute_batch(change).unwrap();
            let r = read_conn(&c, "ses_p");
            let Readout::Unreadable(why) = &r else {
                panic!("{change}: read as {r:?}")
            };
            assert!(why.contains(says), "{change}: {why}");
            let text = describe(&r);
            assert!(
                text.starts_with("Patchbay can't read opencode's state"),
                "{text}"
            );
            assert!(!text.contains("No messages"), "{change}: {text}");
            assert!(!text.contains("no turn running"), "{change}: {text}");
        }
    }

    #[test]
    fn a_missing_database_or_session_is_reported() {
        let r = read(Path::new("/nonexistent/opencode.db"), "ses_p");
        assert!(
            matches!(&r, Readout::Unreadable(w) if w.contains("no opencode database at /nonexistent/opencode.db")),
            "{r:?}"
        );
        let r = read_conn(&fixture(), "ses_nope");
        assert!(
            matches!(&r, Readout::Unreadable(w) if w.contains("no session ses_nope")),
            "{r:?}"
        );
    }

    #[test]
    fn the_database_is_opened_read_only() {
        let dir = std::env::temp_dir().join(format!("pb-oc-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode.db");
        let _ = std::fs::remove_file(&path);
        let c = Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE session_inbox (id TEXT PRIMARY KEY, session_id TEXT);
             INSERT INTO session_v2 VALUES ('ses_p', NULL, 'Desk');",
        )
        .unwrap();
        drop(c);
        assert!(matches!(read(&path, "ses_p"), Readout::Read(_)));
        // The same flags `read` uses refuse a write.
        let ro = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        assert!(
            ro.execute("INSERT INTO session_inbox VALUES ('x', 'ses_p')", [])
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_the_newest_error_of_this_turn() {
        let c = fixture();
        msg(
            &c,
            "m0",
            "ses_p",
            "assistant",
            50,
            serde_json::json!({ "error": { "message": "an old failure" } }),
        );
        msg(
            &c,
            "m1",
            "ses_p",
            "user",
            100,
            serde_json::json!({ "text": "go" }),
        );
        msg(
            &c,
            "m2",
            "ses_p",
            "assistant",
            150,
            serde_json::json!({ "error": { "message": "an earlier one this turn" } }),
        );
        msg(
            &c,
            "m3",
            "ses_p",
            "assistant",
            200,
            serde_json::json!({ "finish": "error", "time": { "completed": 201 }, "error": { "type": "provider.free-limit", "message": LIMIT } }),
        );
        let r = report_of(&c, "ses_p");
        assert_eq!(
            r.error,
            Some(TurnError {
                error: LIMIT.into(),
                from: None
            })
        );
        assert!(
            describe(&Readout::Read(r))
                .contains(&format!("opencode's latest error this turn: {LIMIT}"))
        );
    }

    #[test]
    fn a_subagent_error_names_the_subagent() {
        let c = fixture();
        msg(
            &c,
            "m1",
            "ses_p",
            "user",
            100,
            serde_json::json!({ "text": "go" }),
        );
        msg(
            &c,
            "m2",
            "ses_c",
            "assistant",
            150,
            serde_json::json!({ "error": { "message": LIMIT } }),
        );
        let r = report_of(&c, "ses_p");
        assert_eq!(
            r.error.as_ref().unwrap().from.as_deref(),
            Some("Fail-loud fixes")
        );
        assert!(
            describe(&Readout::Read(r))
                .contains("from subagent \"Fail-loud fixes\": Free usage limit")
        );
    }

    #[test]
    fn a_subagent_report_delivered_to_the_parent_is_read_without_its_tags() {
        let c = fixture();
        msg(
            &c,
            "m1",
            "ses_p",
            "user",
            100,
            serde_json::json!({ "text": "go" }),
        );
        let text = format!(
            "<subagent sessionID=\"ses_c\" state=\"error\" description=\"x\">\n{LIMIT}\n</subagent>"
        );
        msg(
            &c,
            "m2",
            "ses_p",
            "synthetic",
            150,
            serde_json::json!({ "metadata": { "source": "subagent", "state": "error" }, "text": text }),
        );
        assert_eq!(report_of(&c, "ses_p").error.unwrap().error, LIMIT);
    }

    #[test]
    fn earlier_turns_and_suspension_stamps_say_nothing() {
        let c = fixture();
        msg(
            &c,
            "m0",
            "ses_p",
            "assistant",
            50,
            serde_json::json!({ "error": { "message": "old" } }),
        );
        msg(
            &c,
            "m1",
            "ses_p",
            "user",
            100,
            serde_json::json!({ "text": "go" }),
        );
        // A background shell woke the turn: opencode stamps time_suspended, which is no error.
        c.execute(
            "UPDATE session_v2 SET time_suspended = 150 WHERE id = 'ses_p'",
            [],
        )
        .unwrap();
        msg(
            &c,
            "m2",
            "ses_p",
            "synthetic",
            150,
            serde_json::json!({ "metadata": { "source": "shell", "state": "completed" }, "text": "no error here" }),
        );
        assert_eq!(report_of(&c, "ses_p").error, None);
    }

    #[test]
    fn long_errors_are_clipped() {
        let t =
            error_text(&serde_json::json!({ "error": { "message": "x".repeat(500) } })).unwrap();
        assert!(t.chars().count() <= 241 && t.ends_with('…'));
    }

    /// Against this machine's real opencode database (read-only):
    /// `PB_OPENCODE_SESSION=ses_… cargo test --lib live_readout -- --ignored --nocapture`
    #[test]
    // Manual test. If it needs your real home or logins, run it with PB_TEST_REAL_HOME=1 (see test_support).
    #[ignore]
    fn live_readout() {
        let session = std::env::var("PB_OPENCODE_SESSION").expect("set PB_OPENCODE_SESSION");
        let r = read(&db_path().expect("no home directory"), &session);
        println!("{r:?}\n{}", describe(&r));
    }
}
