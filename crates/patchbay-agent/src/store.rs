//! A session on disk: one JSON line per entry, appended as the conversation grows.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use patchbay_llm::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Entry {
    /// Session settings; the last one wins.
    Meta { cwd: String, model: String, mode: String },
    Msg { m: Message },
    /// The final `tool_call` update of a call, so a reload shows the same card.
    Tool { call: Value },
    /// The conversation so far was replaced by a summary: everything before is history only.
    Compacted { summary: Message },
    /// Rewind: keep only the first `len` messages since the last compaction.
    Truncate { len: usize },
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
    /// A store that keeps nothing (subagents live in memory).
    disabled: bool,
}

pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("agent").join("sessions")
}

impl Store {
    pub fn new(data_dir: &Path, id: &str) -> Store {
        Store { path: sessions_dir(data_dir).join(format!("{id}.jsonl")), file: Mutex::new(None), disabled: false }
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
            Entry::Meta { cwd: "/p".into(), model: "m".into(), mode: "default".into() },
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
        s.append(&Entry::Meta { cwd: "/p".into(), model: "m".into(), mode: "default".into() });
        s.append(&Entry::Msg { m: Message::user("hi") });
        let again = Store::new(&dir, "abc");
        let entries = again.load();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[1], Entry::Msg { m } if m.text() == "hi"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
