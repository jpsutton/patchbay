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
use crate::store::{Entry, Store, effective};
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
    /// The model the last request used, to tell the model when it changes.
    pub last_model: Option<String>,
    /// Model requests since TodoWrite was last used.
    pub since_todo_write: usize,
}

/// One user prompt, for rewinding: where it sits in the history and what files it changed.
pub struct PromptRec {
    /// Index of the prompt's message in `State::messages`.
    pub msg_index: usize,
    pub preview: String,
    /// File contents from before the prompt first changed them (`None`: it did not exist).
    pub files: HashMap<PathBuf, Option<Vec<u8>>>,
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
    /// Changes of files the agent read that it was already told about.
    pub notified: Mutex<HashMap<PathBuf, Stamp>>,
    pub prompts: Mutex<Vec<PromptRec>>,
    /// Tokens subagents used, added to the turn's total when it ends.
    pub extra_usage: Mutex<Totals>,
    pub shells: shell::Registry,
    pub mcp: tokio::sync::Mutex<Arc<McpSet>>,
    pub store: Store,
    /// Subagents this session started.
    pub agents: crate::subagent::Agents,
    /// Prefix of this session's tool-call ids on the client ("" for the main session), so a
    /// subagent's calls never collide with the main agent's.
    pub card_prefix: String,
    /// The client keeps `session/prompt` open for background subagents (`_meta.subagents`).
    pub keep_open: AtomicBool,
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
                last_model: None,
                since_todo_write: 0,
            }),
            cancel: Mutex::new(None),
            busy: AtomicBool::new(false),
            files: Mutex::default(),
            notified: Mutex::default(),
            prompts: Mutex::default(),
            extra_usage: Mutex::default(),
            shells: shell::Registry::default(),
            mcp: tokio::sync::Mutex::new(Arc::new(McpSet::default())),
            store,
            agents: Default::default(),
            card_prefix: String::new(),
            keep_open: AtomicBool::new(false),
        })
    }

    /// A subagent's own session: same client, model and MCP servers, but its own history, files,
    /// shells and working directory. `prefix` keeps its tool-call ids apart.
    pub async fn child(&self, cwd: PathBuf, prefix: String, messages: Vec<Message>, mode: Mode) -> Arc<Session> {
        let model_id = self.state.lock().await.model_id.clone();
        Arc::new(Session {
            id: self.id.clone(),
            cwd,
            host: self.host.clone(),
            peer: self.peer.clone(),
            state: tokio::sync::Mutex::new(State {
                messages,
                model_id,
                mode,
                todos: Vec::new(),
                context_tokens: 0,
                last_model: None,
                since_todo_write: 0,
            }),
            cancel: Mutex::new(None),
            busy: AtomicBool::new(false),
            files: Mutex::default(),
            notified: Mutex::default(),
            prompts: Mutex::default(),
            extra_usage: Mutex::default(),
            shells: shell::Registry::default(),
            mcp: tokio::sync::Mutex::new(self.mcp.lock().await.clone()),
            store: Store::disabled(),
            agents: Default::default(),
            card_prefix: prefix,
            keep_open: AtomicBool::new(false),
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

    /// Before a tool changes `path`: keep what it holds, once per prompt, so a rewind can restore it.
    pub fn snapshot_file(&self, path: &Path) {
        let mut prompts = self.prompts.lock().unwrap();
        if let Some(p) = prompts.last_mut() {
            p.files.entry(path.to_path_buf()).or_insert_with(|| std::fs::read(path).ok());
        }
    }

    /// Resolve a path the model gave against the session's directory.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { self.cwd.join(p) }
    }

    /// Show the stored conversation to the client: what `session/load` replays.
    pub fn replay(&self, entries: &[Entry]) {
        for e in &effective(entries) {
            match e {
                Entry::Msg { m } => self.replay_message(m),
                Entry::Tool { call } => self.update(call.clone()),
                Entry::Compacted { summary } => {
                    self.text_chunk("agent_message_chunk", &format!("— Earlier conversation summarized —\n{}", summary.text()))
                }
                Entry::Meta { .. } | Entry::Truncate { .. } => {}
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

/// A message the user wrote, as opposed to tool results and compaction summaries.
pub fn is_user_prompt(m: &Message) -> bool {
    m.role == Role::User
        && !m.parts.is_empty()
        && !m.parts.iter().any(|p| matches!(p, Part::ToolResult(_)))
        && !m.text().starts_with(crate::compact::SUMMARY_MARK)
}

/// The rewind points of a restored conversation (their file snapshots are gone).
pub fn prompts_of(messages: &[Message]) -> Vec<PromptRec> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_user_prompt(m))
        .map(|(i, m)| PromptRec {
            msg_index: i,
            preview: m.text().chars().take(80).collect(),
            files: HashMap::new(),
        })
        .collect()
}

pub fn restore(entries: &[Entry]) -> Restored {
    let mut r = Restored { cwd: None, model: None, mode: None, messages: Vec::new() };
    for e in &effective(entries) {
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
            Entry::Tool { .. } | Entry::Truncate { .. } => {}
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
