//! One conversation: its history, settings and the handles tools need.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use patchbay_llm::{Message, Part, Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::host::Host;
use crate::mcp::McpSet;
use crate::rpc::Peer;
use crate::store::{Entry, Store};
use crate::tools::shell;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
    #[serde(default, rename = "activeForm")]
    pub active_form: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Default,
    /// Read-only: explore and write a plan, then ask to leave.
    Plan,
}

impl Mode {
    pub fn id(self) -> &'static str {
        match self {
            Mode::Default => "default",
            Mode::Plan => "plan",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "default" => Some(Mode::Default),
            "plan" => Some(Mode::Plan),
            _ => None,
        }
    }
}

/// Token totals over a session.
#[derive(Debug, Clone, Copy, Default)]
pub struct Totals {
    pub input: u64,
    pub cached_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

pub struct State {
    pub messages: Vec<Message>,
    pub model_id: String,
    pub mode: Mode,
    pub todos: Vec<Todo>,
    /// Tokens the last request put in the context window.
    pub context_tokens: u64,
}

/// What a file looked like when the agent last read or wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub mtime: SystemTime,
    pub len: u64,
}

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub host: Arc<dyn Host>,
    pub peer: Peer,
    pub state: tokio::sync::Mutex<State>,
    /// Cancels the running turn.
    pub cancel: Mutex<Option<CancellationToken>>,
    pub busy: AtomicBool,
    pub files: Mutex<HashMap<PathBuf, Stamp>>,
    pub shells: shell::Registry,
    pub mcp: tokio::sync::Mutex<Arc<McpSet>>,
    pub store: Store,
}

pub fn stamp_of(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp { mtime: m.modified().ok()?, len: m.len() })
}

impl Session {
    pub fn new(
        id: String,
        cwd: PathBuf,
        host: Arc<dyn Host>,
        peer: Peer,
        model_id: String,
        mode: Mode,
    ) -> Arc<Session> {
        let store = Store::new(&host.data_dir(), &id);
        Arc::new(Session {
            id,
            cwd,
            host,
            peer,
            state: tokio::sync::Mutex::new(State {
                messages: Vec::new(),
                model_id,
                mode,
                todos: Vec::new(),
                context_tokens: 0,
            }),
            cancel: Mutex::new(None),
            busy: AtomicBool::new(false),
            files: Mutex::default(),
            shells: shell::Registry::default(),
            mcp: tokio::sync::Mutex::new(Arc::new(McpSet::default())),
            store,
        })
    }

    /// Send a `session/update`.
    pub fn update(&self, update: Value) {
        self.peer.notify("session/update", json!({"sessionId": self.id, "update": update}));
    }

    pub fn text_chunk(&self, kind: &str, text: &str) {
        self.update(json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}));
    }

    pub fn save_meta(&self, model: &str, mode: Mode) {
        self.store.append(&Entry::Meta {
            cwd: self.cwd.to_string_lossy().into_owned(),
            model: model.to_owned(),
            mode: mode.id().to_owned(),
        });
    }

    /// Resolve a path the model gave against the session's directory.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { self.cwd.join(p) }
    }

    /// Show the stored conversation to the client: what `session/load` replays.
    pub fn replay(&self, entries: &[Entry]) {
        for e in entries {
            match e {
                Entry::Msg { m } => self.replay_message(m),
                Entry::Tool { call } => self.update(call.clone()),
                Entry::Compacted { summary } => {
                    self.text_chunk("agent_message_chunk", &format!("— Earlier conversation summarized —\n{}", summary.text()))
                }
                Entry::Meta { .. } => {}
            }
        }
    }

    fn replay_message(&self, m: &Message) {
        for p in &m.parts {
            match (m.role, p) {
                (Role::User, Part::Text(t)) if !crate::prompt::is_reminder(t) => {
                    self.text_chunk("user_message_chunk", t)
                }
                (Role::Assistant, Part::Text(t)) => self.text_chunk("agent_message_chunk", t),
                (Role::Assistant, Part::Reasoning { text, .. }) if !text.is_empty() => {
                    self.text_chunk("agent_thought_chunk", text)
                }
                _ => {}
            }
        }
    }
}

/// The state a stored session ends in.
pub struct Restored {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub mode: Option<Mode>,
    pub messages: Vec<Message>,
}

pub fn restore(entries: &[Entry]) -> Restored {
    let mut r = Restored { cwd: None, model: None, mode: None, messages: Vec::new() };
    for e in entries {
        match e {
            Entry::Meta { cwd, model, mode } => {
                r.cwd = Some(cwd.clone());
                r.model = Some(model.clone());
                r.mode = Mode::parse(mode);
            }
            Entry::Msg { m } => r.messages.push(m.clone()),
            Entry::Compacted { summary } => {
                r.messages = vec![summary.clone()];
            }
            Entry::Tool { .. } => {}
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_replaces_history_on_restore() {
        let entries = vec![
            Entry::Meta { cwd: "/p".into(), model: "m".into(), mode: "plan".into() },
            Entry::Msg { m: Message::user("one") },
            Entry::Msg { m: Message::assistant("two") },
            Entry::Compacted { summary: Message::user("summary") },
            Entry::Msg { m: Message::user("three") },
        ];
        let r = restore(&entries);
        assert_eq!(r.messages.len(), 2);
        assert_eq!(r.messages[0].text(), "summary");
        assert_eq!(r.mode, Some(Mode::Plan));
    }
}
