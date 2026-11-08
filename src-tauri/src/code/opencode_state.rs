//! Why an opencode turn went quiet (pb-20261107-18). When a turn stalls (`code::stall`) on an
//! opencode session, opencode's own database often says why: it records the error
//! (`provider.free-limit`, a 429, …) on the step or subagent that hit it, even when nothing reaches
//! Patchbay over ACP. This reads the newest such error of the current turn, read-only, so the stall
//! notice can name the cause.
//!
//! Deliberately not used: `session_v2.time_suspended`. Despite the name it is stamped whenever a
//! background shell finishes and wakes the turn, not when opencode stops at a limit, so it would
//! name a cause that isn't one.
//!
//! opencode's schema is its own and has changed before (`session` became `session_v2`): every
//! query is allowed to fail, and any failure means no readout rather than an error.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;

/// The newest error opencode recorded in a session's current turn.
#[derive(Debug, PartialEq)]
pub struct Readout {
    pub error: String,
    /// The subagent's title, when the error came from one.
    pub from: Option<String>,
}

/// opencode's database: `$XDG_DATA_HOME/opencode/opencode.db`, else under `~/.local/share` (opencode
/// uses that layout on every platform).
pub fn db_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".local/share"))
        })?;
    let p = base.join("opencode").join("opencode.db");
    p.is_file().then_some(p)
}

/// The newest error of the session's current turn in opencode's database at `path`. `None` when
/// there is none, or the database, the session or the schema isn't what this expects.
pub fn read(path: &Path, session: &str) -> Option<Readout> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    // opencode may be writing: wait a moment for its lock, never long.
    conn.busy_timeout(Duration::from_millis(250)).ok()?;
    read_conn(&conn, session)
}

fn read_conn(conn: &Connection, session: &str) -> Option<Readout> {
    // The turn began with the session's latest user message.
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
        Some(Readout {
            error,
            from: (sid != session).then_some(title).flatten(),
        })
    })
}

/// The error a stored message carries: an `error.message` (a failed step or idle), or a subagent
/// report whose state is `error`.
fn error_text(v: &Value) -> Option<String> {
    let text = v["error"]["message"].as_str().map(str::to_owned).or_else(|| {
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

/// The sentence added to the stall notice.
pub fn describe(r: &Readout) -> String {
    match &r.from {
        Some(t) => format!("opencode's latest error this turn, from subagent \"{t}\": {}", r.error),
        None => format!("opencode's latest error this turn: {}", r.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The columns of opencode's schema this reads, as seen in opencode 2.1.
    fn fixture() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT, time_suspended INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, data TEXT);
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

    const LIMIT: &str =
        "Free usage limit reached. Connect a provider or subscribe to OpenCode Go to keep going, or switch models.";

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
        msg(&c, "m1", "ses_p", "user", 100, serde_json::json!({ "text": "go" }));
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
            serde_json::json!({ "finish": "error", "error": { "type": "provider.free-limit", "message": LIMIT } }),
        );
        let r = read_conn(&c, "ses_p").unwrap();
        assert_eq!(
            r,
            Readout {
                error: LIMIT.into(),
                from: None
            }
        );
        assert_eq!(describe(&r), format!("opencode's latest error this turn: {LIMIT}"));
    }

    #[test]
    fn a_subagent_error_names_the_subagent() {
        let c = fixture();
        msg(&c, "m1", "ses_p", "user", 100, serde_json::json!({ "text": "go" }));
        msg(
            &c,
            "m2",
            "ses_c",
            "assistant",
            150,
            serde_json::json!({ "error": { "message": LIMIT } }),
        );
        let r = read_conn(&c, "ses_p").unwrap();
        assert_eq!(r.from.as_deref(), Some("Fail-loud fixes"));
        assert!(describe(&r).contains("from subagent \"Fail-loud fixes\": Free usage limit"));
    }

    #[test]
    fn a_subagent_report_delivered_to_the_parent_is_read_without_its_tags() {
        let c = fixture();
        msg(&c, "m1", "ses_p", "user", 100, serde_json::json!({ "text": "go" }));
        let text = format!("<subagent sessionID=\"ses_c\" state=\"error\" description=\"x\">\n{LIMIT}\n</subagent>");
        msg(
            &c,
            "m2",
            "ses_p",
            "synthetic",
            150,
            serde_json::json!({ "metadata": { "source": "subagent", "state": "error" }, "text": text }),
        );
        assert_eq!(read_conn(&c, "ses_p").unwrap().error, LIMIT);
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
        msg(&c, "m1", "ses_p", "user", 100, serde_json::json!({ "text": "go" }));
        // A background shell woke the turn: opencode stamps time_suspended, which is no error.
        c.execute("UPDATE session_v2 SET time_suspended = 150 WHERE id = 'ses_p'", [])
            .unwrap();
        msg(
            &c,
            "m2",
            "ses_p",
            "synthetic",
            150,
            serde_json::json!({ "metadata": { "source": "shell", "state": "completed" }, "text": "no error here" }),
        );
        assert_eq!(read_conn(&c, "ses_p"), None);
    }

    #[test]
    fn an_unknown_schema_session_or_database_is_no_readout() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE session (id TEXT)").unwrap();
        assert_eq!(read_conn(&c, "ses_p"), None, "older opencode without session_message");
        assert_eq!(read_conn(&fixture(), "ses_nope"), None, "a session it doesn't know");
        assert_eq!(
            read(Path::new("/nonexistent/opencode.db"), "ses_p"),
            None,
            "no database"
        );
    }

    #[test]
    fn long_errors_are_clipped() {
        let t = error_text(&serde_json::json!({ "error": { "message": "x".repeat(500) } })).unwrap();
        assert!(t.chars().count() <= 241 && t.ends_with('…'));
    }

    /// Against this machine's real opencode database (read-only):
    /// `PB_OPENCODE_SESSION=ses_… cargo test --lib live_readout -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_readout() {
        let session = std::env::var("PB_OPENCODE_SESSION").expect("set PB_OPENCODE_SESSION");
        let r = read(&db_path().expect("no opencode database here"), &session);
        println!("{r:?}\n{:?}", r.as_ref().map(describe));
    }
}
