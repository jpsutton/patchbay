//! One conversation: its history, settings and the handles tools need.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use patchbay_llm::{Effort, Message, Part, Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::host::{Host, ModelClient};
use crate::prompts::Prompts;
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
    /// This session's own choice about minified prompts; `None` follows the host's setting.
    pub minify: Mutex<Option<bool>>,
    /// This session's own reasoning effort; `None` follows the model's setting.
    pub effort: Mutex<Option<Effort>>,
    /// Messages the user sent into the running turn (`session/steering`).
    pub steer: Mutex<SteerInbox>,
    /// Woken when a message is steered in, for a turn that waits on background subagents.
    pub steered: tokio::sync::Notify,
    /// What the endpoint reported this session's requests cost so far, in USD (`None`: it
    /// reports no cost, and the host prices the tokens itself).
    pub cost_usd: Mutex<Option<f64>>,
    /// What this session loads from the user's Claude Code setup; reloaded with each prompt.
    pub claude: Mutex<Arc<crate::claude::Setup>>,
    /// Context the SessionStart hooks gave, for the first prompt.
    pub start_context: Mutex<Vec<String>>,
}

/// Messages steered into a running turn, waiting for the next step.
#[derive(Default)]
pub struct SteerInbox {
    /// A turn is running and will still read the inbox. Cleared, under the same lock, by the
    /// last look a turn takes before it ends, so no message is accepted and then dropped.
    pub open: bool,
    pub queued: Vec<Vec<Part>>,
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
            minify: Mutex::new(None),
            effort: Mutex::new(None),
            steer: Mutex::default(),
            steered: tokio::sync::Notify::new(),
            cost_usd: Mutex::new(None),
            claude: Mutex::default(),
            start_context: Mutex::default(),
        })
    }

    /// Whether prompts go out minified: the session's own choice, else the host's setting.
    pub fn minifying(&self) -> bool {
        (*self.minify.lock().unwrap()).unwrap_or_else(|| self.host.settings().minify_prompts)
    }

    /// `model_id` ready to call, with this session's own overrides applied.
    pub async fn model_client(&self, model_id: &str) -> Result<ModelClient, String> {
        let mut mc = self.host.model_client(model_id).await?;
        if let Some(e) = *self.effort.lock().unwrap() {
            mc.client.config.effort = Some(e);
        }
        Ok(mc)
    }

    /// Take the messages steered in so far, as parts for the next user message: each behind a
    /// note that says it arrived mid-turn. Empty when none came. `close`: the turn ends unless
    /// something came, so stop taking more.
    pub fn take_steered(&self, close: bool) -> Vec<Part> {
        let mut inbox = self.steer.lock().unwrap();
        let queued = std::mem::take(&mut inbox.queued);
        if close && queued.is_empty() {
            inbox.open = false;
        }
        drop(inbox);
        let note = self.prompts().msg("steered", &[]);
        queued
            .into_iter()
            .flat_map(|parts| std::iter::once(Part::Text(crate::prompt::reminder(&note))).chain(parts))
            .collect()
    }

    /// The Claude Code setup this session uses.
    pub fn claude(&self) -> Arc<crate::claude::Setup> {
        self.claude.lock().unwrap().clone()
    }

    /// Read the Claude Code setup again (it may have changed on disk or in Settings). Returns
    /// whether the slash commands changed.
    pub fn reload_claude(&self) -> bool {
        let cfg = self.host.settings().claude;
        let setup = Arc::new(crate::claude::Setup::load(&cfg, &self.cwd));
        let mut cur = self.claude.lock().unwrap();
        let changed = cur.commands != setup.commands || cur.skills != setup.skills;
        *cur = setup;
        changed
    }

    /// Tell the client the slash commands: `/compact`, the user's commands and skills.
    pub fn announce_commands(&self) {
        let setup = self.claude();
        let mut list = vec![json!({
            "name": "compact",
            "description": "Summarize the conversation to free up context",
            "input": {"hint": "what the summary should focus on"},
        })];
        for c in &setup.commands {
            let mut v = json!({"name": c.name, "description": c.description});
            if let Some(h) = &c.argument_hint {
                v["input"] = json!({"hint": h});
            }
            list.push(v);
        }
        for s in &setup.skills {
            list.push(json!({"name": s.name, "description": s.description}));
        }
        self.update(json!({"sessionUpdate": "available_commands_update", "availableCommands": list}));
    }

    /// Fields every hook gets on stdin.
    pub fn hook_input(&self, mode: Mode) -> Value {
        json!({
            "session_id": self.id,
            "transcript_path": "",
            "permission_mode": if mode == Mode::Plan { "plan" } else { "default" },
        })
    }

    /// Show hook notes (failures, `systemMessage`s) to the user.
    pub fn hook_notes(&self, notes: &[String]) {
        for n in notes {
            self.text_chunk("agent_thought_chunk", &format!("{n}\n"));
        }
    }

    /// Add a reported cost to the session's total; the total so far.
    pub fn add_cost(&self, cost: Option<f64>) -> Option<f64> {
        let mut total = self.cost_usd.lock().unwrap();
        if let Some(c) = cost {
            *total = Some(total.unwrap_or(0.0) + c);
        }
        *total
    }

    /// The prompt texts to use, in the form this session wants.
    pub fn prompts(&self) -> Prompts {
        Prompts::new(self.minifying())
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
            // A subagent speaks the way its parent does, whatever the setting becomes later.
            minify: Mutex::new(Some(self.minifying())),
            effort: Mutex::new(*self.effort.lock().unwrap()),
            steer: Mutex::default(),
            steered: tokio::sync::Notify::new(),
            cost_usd: Mutex::new(None),
            claude: Mutex::new(self.claude()),
            start_context: Mutex::default(),
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
            minify: *self.minify.lock().unwrap(),
            effort: *self.effort.lock().unwrap(),
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
                Entry::Meta { .. } | Entry::Truncate { .. } | Entry::Req(_) => {}
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
    pub minify: Option<bool>,
    pub effort: Option<Effort>,
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
    let mut r = Restored { minify: None, effort: None, cwd: None, model: None, mode: None, messages: Vec::new() };
    for e in &effective(entries) {
        match e {
            Entry::Meta { cwd, model, mode, minify, effort } => {
                r.minify = *minify;
                r.effort = *effort;
                r.cwd = Some(cwd.clone());
                r.model = Some(model.clone());
                r.mode = Mode::parse(mode);
            }
            Entry::Msg { m } => r.messages.push(m.clone()),
            Entry::Compacted { summary } => {
                r.messages = vec![summary.clone()];
            }
            Entry::Tool { .. } | Entry::Truncate { .. } | Entry::Req(_) => {}
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
            Entry::Meta { cwd: "/p".into(), model: "m".into(), mode: "plan".into(), minify: Some(true), effort: Some(Effort::High) },
            Entry::Msg { m: Message::user("one") },
            Entry::Msg { m: Message::assistant("two") },
            Entry::Compacted { summary: Message::user("summary") },
            Entry::Msg { m: Message::user("three") },
        ];
        let r = restore(&entries);
        assert_eq!(r.messages.len(), 2);
        assert_eq!(r.messages[0].text(), "summary");
        assert_eq!(r.mode, Some(Mode::Plan));
        assert_eq!(r.minify, Some(true));
        assert_eq!(r.effort, Some(Effort::High));
    }
}
