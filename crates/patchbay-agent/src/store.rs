//! A session on disk: one JSON line per entry, appended as the conversation grows.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use patchbay_llm::{Message, StopReason, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Entry {
    /// Session settings; the last one wins.
    Meta {
        cwd: String,
        model: String,
        mode: String,
        /// The session's own choice about minified prompts; `None` follows the host's setting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minify: Option<bool>,
        /// The session's own reasoning effort; `None` follows the model's setting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<patchbay_llm::Effort>,
    },
    Msg { m: Message },
    /// The final `tool_call` update of a call, so a reload shows the same card.
    Tool { call: Value },
    /// The conversation so far was replaced by a summary: everything before is history only.
    Compacted { summary: Message },
    /// Rewind: keep only the first `len` messages since the last compaction.
    Truncate { len: usize },
    /// How one model call went. Diagnostics only: replay and restore skip it, and a build that
    /// predates it skips the line.
    Req(ReqRec),
}

/// What happened to one model call, written when it ends, whichever way. A call that never ends
/// (the app is killed mid-request) leaves nothing, which shows as a message with no `Req` after it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReqRec {
    /// When the call started, in Unix milliseconds.
    pub at: u64,
    /// What the call was for; absent for a step of the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The model ID the endpoint was asked for.
    pub model: String,
    /// The output cap the request carried; absent when the server picks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub outcome: ReqOutcome,
    /// Why the reply ended, when one came back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Milliseconds until the first text or reasoning arrived; absent when none did (a reply that
    /// is only tool calls, or a call that failed first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_ms: Option<u64>,
    /// Milliseconds from the start to the end of the call, retries included.
    pub total_ms: u64,
    /// Retries after the first attempt.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retries: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The HTTP status of the failure, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReqOutcome {
    Ok,
    Error,
    /// The user stopped it.
    Cancelled,
}

/// The entries that still count: a compaction and a rewind drop what they replace.
pub fn effective(entries: &[Entry]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    // Where each message since the last compaction sits in `out`.
    let mut positions: Vec<usize> = Vec::new();
    for e in entries {
        match e {
            Entry::Msg { .. } => {
                positions.push(out.len());
                out.push(e.clone());
            }
            Entry::Compacted { .. } => {
                positions.clear();
                out.push(e.clone());
            }
            Entry::Truncate { len } => {
                if positions.len() > *len {
                    out.truncate(positions[*len]);
                    positions.truncate(*len);
                }
            }
            _ => out.push(e.clone()),
        }
    }
    out
}

pub struct Store {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
    /// A store that keeps nothing.
    disabled: bool,
}

pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("agent").join("sessions")
}

/// Where a session's subagents keep their transcripts (`<agent id>.jsonl`) and `status.json`.
/// Apart from `sessions/`, so they never show up as sessions of their own.
pub fn subagents_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("agent").join("subagents").join(session_id)
}

impl Store {
    pub fn new(data_dir: &Path, id: &str) -> Store {
        Store { path: sessions_dir(data_dir).join(format!("{id}.jsonl")), file: Mutex::new(None), disabled: false }
    }

    /// A store in the file at `path`.
    pub fn at(path: PathBuf) -> Store {
        Store { path, file: Mutex::new(None), disabled: false }
    }

    pub fn disabled() -> Store {
        Store { path: PathBuf::new(), file: Mutex::new(None), disabled: true }
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    pub fn append(&self, entry: &Entry) {
        if self.disabled {
            return;
        }
        let mut guard = self.file.lock().unwrap();
        if guard.is_none() {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            *guard = std::fs::OpenOptions::new().create(true).append(true).open(&self.path).ok();
        }
        let Some(f) = guard.as_mut() else {
            tracing::warn!("cannot write {}", self.path.display());
            return;
        };
        let Ok(mut line) = serde_json::to_string(entry) else { return };
        line.push('\n');
        if let Err(e) = f.write_all(line.as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.path.display());
        }
    }

    /// Every entry in order; lines that no longer parse are skipped.
    pub fn load(&self) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Vec::new() };
        text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewind_drops_later_messages_and_their_cards() {
        let msg = |t: &str| Entry::Msg { m: Message::user(t) };
        let tool = || Entry::Tool { call: serde_json::json!({"toolCallId": "t"}) };
        let entries = vec![
            Entry::Meta { cwd: "/p".into(), model: "m".into(), mode: "default".into(), minify: None, effort: None },
            msg("one"),
            tool(),
            msg("two"),
            tool(),
            Entry::Truncate { len: 1 },
            msg("three"),
        ];
        let kept = effective(&entries);
        let texts: Vec<String> = kept
            .iter()
            .filter_map(|e| if let Entry::Msg { m } = e { Some(m.text()) } else { None })
            .collect();
        assert_eq!(texts, ["one", "three"]);
        assert_eq!(kept.iter().filter(|e| matches!(e, Entry::Tool { .. })).count(), 1);
    }

    #[test]
    fn appends_and_reloads() {
        let dir = std::env::temp_dir().join(format!("pb-store-{}", uuid::Uuid::new_v4()));
        let s = Store::new(&dir, "abc");
        assert!(!s.exists());
        s.append(&Entry::Meta { cwd: "/p".into(), model: "m".into(), mode: "default".into(), minify: None, effort: None });
        s.append(&Entry::Msg { m: Message::user("hi") });
        let again = Store::new(&dir, "abc");
        let entries = again.load();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[1], Entry::Msg { m } if m.text() == "hi"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn req(error: &str) -> ReqRec {
        ReqRec {
            at: 1,
            kind: None,
            model: "claude-fable-5-1".into(),
            max_tokens: Some(64000),
            outcome: ReqOutcome::Error,
            stop: None,
            usage: None,
            first_ms: None,
            total_ms: 180_000,
            retries: 0,
            error: Some(error.into()),
            status: None,
        }
    }

    #[test]
    fn a_request_record_round_trips_and_stays_compact() {
        let mut rec = req("no data for 180s");
        rec.outcome = ReqOutcome::Ok;
        rec.error = None;
        rec.stop = Some(StopReason::MaxTokens);
        rec.usage = Some(Usage { input: 5, output: 7, ..Default::default() });
        rec.first_ms = Some(900);
        let line = serde_json::to_string(&Entry::Req(rec.clone())).unwrap();
        assert!(line.contains(r#""t":"req""#) && line.contains(r#""stop":"max_tokens""#), "{line}");
        // Unset fields stay out of the line.
        assert!(!line.contains("kind") && !line.contains("retries") && !line.contains("error"), "{line}");
        let back: Entry = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, Entry::Req(r) if r == rec));
    }

    #[test]
    fn a_build_without_request_records_skips_their_lines() {
        // What an older build's `Entry` does with a `req` line: it fails to parse and `load` drops it.
        #[derive(Deserialize)]
        #[serde(tag = "t", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum Old {
            Msg { m: Message },
        }
        let line = serde_json::to_string(&Entry::Req(req("boom"))).unwrap();
        assert!(serde_json::from_str::<Old>(&line).is_err());
    }

    #[test]
    fn request_records_never_reach_the_conversation_and_follow_a_rewind() {
        let msg = |t: &str| Entry::Msg { m: Message::user(t) };
        let entries = vec![
            msg("one"),
            Entry::Req(req("a")),
            msg("two"),
            Entry::Req(req("b")),
            Entry::Truncate { len: 1 },
            msg("three"),
        ];
        let kept = effective(&entries);
        assert_eq!(kept.iter().filter(|e| matches!(e, Entry::Req(_))).count(), 1, "the rewound call's record goes with it");
        let restored = crate::session::restore(&entries);
        assert_eq!(restored.messages.iter().map(|m| m.text()).collect::<Vec<_>>(), ["one", "three"]);
    }
}
