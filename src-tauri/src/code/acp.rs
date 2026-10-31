//! ACP client side: drives one coding agent child (Grok Build, Claude Code, OpenCode, …) per
//! Code session. `agents.rs` decides how to launch each one; everything here is plain ACP plus
//! the optional extensions listed in [`Extensions`].
//!
//! The ACP crate is `?Send`, so each agent lives on its own OS thread with a current-thread
//! runtime + `LocalSet`. Tauri talks to it through an mpsc command channel; agent output goes to
//! the UI as `code://event` events.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use agent_client_protocol::{self as acp, Agent as _};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::agents::{ApprovalSetup, Extensions, Launch};

pub const EVENT: &str = "code://event";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodeEvent {
    Status {
        state: String,
        message: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    SessionReady {
        acp_session_id: String,
        models: Value,
        modes: Value,
        loaded: bool,
        /// `agentCapabilities` from `initialize`, so the UI can gate features per agent.
        capabilities: Value,
        agent_info: Value,
        /// Session config options other than model and mode (e.g. reasoning effort).
        config_options: Value,
        extensions: Extensions,
        /// What this agent does without asking, whatever the session's approval mode.
        approval_gaps: &'static [&'static str],
    },
    /// The agent's session config changed (`config_option_update`, or after setting one).
    #[serde(rename_all = "camelCase")]
    Config {
        models: Value,
        modes: Value,
        config_options: Value,
    },
    Update {
        update: Value,
    },
    #[serde(rename_all = "camelCase")]
    Permission {
        request_id: String,
        request: Value,
    },
    #[serde(rename_all = "camelCase")]
    PermissionResolved {
        request_id: String,
    },
    /// A saved rule answered a permission request.
    #[serde(rename_all = "camelCase")]
    AutoApproved {
        tool_call_id: String,
        description: String,
    },
    /// Something worth showing in the transcript that isn't an error.
    Notice {
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    PromptDone {
        stop_reason: Value,
    },
    Error {
        message: String,
    },
    /// Grok Build's status-line payload (`session_status`); folded into `Usage` by the manager.
    SessionStatus {
        status: Value,
    },
    /// ACP `usage_update`: context fill and cumulative cost; folded into `Usage` by the manager.
    ContextUsage {
        update: Value,
    },
    /// ACP `PromptResponse.usage` for one turn; folded into `Usage` by the manager.
    TurnUsage {
        usage: Value,
    },
    /// The Claude plan's usage windows, from a `usage_update`; the app's usage guard reads it.
    PlanLimits {
        limits: super::plan_limits::PlanLimits,
    },
    /// Cumulative token usage and context fill for the session.
    Usage {
        usage: crate::usage::UsageView,
    },
    /// A prompt was sent to the agent (from `origin`, a mesh node), so every viewer shows it. One
    /// sent mid-turn is steered into the running turn or queued behind it; a queued one comes
    /// again as `Sent` when its turn starts.
    #[serde(rename_all = "camelCase")]
    UserPrompt {
        prompt_id: String,
        text: String,
        origin: String,
        attachments: Value,
        delivery: Delivery,
    },
    /// Queued prompts that won't be sent: the user removed one, or the turn was stopped or
    /// failed. `restore` asks the window that sent them to put them back in its composer.
    #[serde(rename_all = "camelCase")]
    Unqueued {
        prompt_ids: Vec<String>,
        restore: bool,
    },
    /// The session was renamed automatically (see `code::titles`).
    Title {
        title: String,
    },
    /// The agent wants the user to sign in (ACP `auth_required`): before it starts a session, or
    /// mid-session when a prompt is refused. It waits for `AgentCmd::Authenticate` with one of
    /// `methods`, or a restart after a terminal sign-in.
    AuthRequired {
        message: String,
        methods: Vec<AuthChoice>,
    },
    /// A sign-in asked for mid-session worked; the session goes on, and the user resends.
    AuthResolved,
    /// Something the agent sent that Patchbay can't show (an update type or content it doesn't
    /// know): a dim line in the transcript, with the raw payload.
    Unrecognized {
        message: String,
        raw: Value,
    },
}

/// A sign-in method the agent advertised in `initialize`, as the UI offers it.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthChoice {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// The method takes an API key from the user (`_meta["api-key"]` on the method).
    pub api_key: bool,
    /// The user signs in in a terminal running the agent's login command
    /// (`AgentCmd::TerminalAuth`), not through ACP `authenticate`.
    pub terminal: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub code_session_id: String,
    pub event: CodeEvent,
}

/// Delivers agent events to the UI (a Tauri `emit` in the app, a collector in tests).
pub type Sink = Arc<dyn Fn(Envelope) + Send + Sync>;

fn emit(sink: &Sink, code_session_id: &str, event: CodeEvent) {
    sink(Envelope {
        code_session_id: code_session_id.to_owned(),
        event,
    });
}

/// Permission requests awaiting a UI answer: request id -> (code session id, reply).
pub type PendingPermissions =
    Arc<Mutex<HashMap<String, (String, oneshot::Sender<Option<String>>)>>>;

/// How a prompt reached the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Delivery {
    /// As a new turn (`session/prompt`).
    Sent,
    /// Into the running turn, through the agent's steering extension.
    Steered,
    /// Held by Patchbay until the running turn ends: agents without steering cancel, refuse or
    /// garble a second `session/prompt` mid-turn.
    Queued,
}

/// A user prompt: the ACP blocks for the agent, and what the transcript shows for it.
#[derive(Debug, Clone)]
pub struct UserMessage {
    pub id: String,
    pub blocks: Vec<Value>,
    pub text: String,
    pub origin: String,
    pub attachments: Value,
}

impl UserMessage {
    fn event(&self, delivery: Delivery) -> CodeEvent {
        CodeEvent::UserPrompt {
            prompt_id: self.id.clone(),
            text: self.text.clone(),
            origin: self.origin.clone(),
            attachments: self.attachments.clone(),
            delivery,
        }
    }
}

pub enum AgentCmd {
    /// A new turn when the agent is idle. Mid-turn, steered into the running turn when the agent
    /// can take that, else queued until the turn ends. `force` cancels the running turn instead
    /// and sends the prompt next, ahead of anything queued.
    Prompt {
        msg: UserMessage,
        force: bool,
        reply: Option<oneshot::Sender<Result<Delivery>>>,
    },
    /// An inter-agent message: queue behind a running turn instead of steering it.
    AgentMessage {
        msg: UserMessage,
        reply: Option<oneshot::Sender<Result<Delivery>>>,
    },
    /// Drop a queued prompt.
    Unqueue { prompt_id: String },
    /// Send a queued prompt, or one steered into the running turn, now: cancel the running turn
    /// and send it next.
    SendNow { prompt_id: String },
    Cancel,
    /// Stop one running tool call; the turn goes on. Patchbay's own agent only.
    CancelTool { tool_call_id: String },
    /// Stop one background shell. Patchbay's own agent only.
    KillShell { shell_id: String },
    SetModel { model_id: String, reply: oneshot::Sender<Result<()>> },
    SetMode { mode_id: String, reply: oneshot::Sender<Result<()>> },
    SetConfigOption { config_id: String, value: String, reply: oneshot::Sender<Result<()>> },
    /// Drop the `ordinal`-th user prompt (0-based, as shown in the transcript) and everything
    /// after it. `revert_files` also restores files the agent changed since then.
    Rewind {
        ordinal: usize,
        preview: String,
        revert_files: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Sign in with one of the methods from `CodeEvent::AuthRequired`. Before the session
    /// starts, replies once it is up; mid-session, once the agent accepts the sign-in.
    Authenticate {
        method_id: String,
        api_key: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// The command a terminal sign-in method runs, for Patchbay's terminal to start.
    TerminalAuth {
        method_id: String,
        reply: oneshot::Sender<Result<crate::terminal::Program>>,
    },
    Shutdown,
}

#[cfg(test)]
impl AgentCmd {
    /// A text prompt nobody waits on.
    pub fn prompt(text: &str) -> Self {
        AgentCmd::Prompt {
            msg: UserMessage {
                id: crate::db::new_id(),
                blocks: vec![json!({ "type": "text", "text": text })],
                text: text.to_owned(),
                origin: String::new(),
                attachments: json!([]),
            },
            force: false,
            reply: None,
        }
    }
}

/// How an agent takes a message mid-turn.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Steer {
    /// ACP's `_session/steering` extension, advertised as `initialize` `_meta.steering`.
    Acp,
    /// Grok Build's `x.ai/interject`.
    Interject,
}

impl Steer {
    /// Only what the agent advertises counts, never a call that seems to work: some agents
    /// answer any `_` method with `null`.
    fn of(init: &Value, interject: bool) -> Option<Self> {
        if init["_meta"]["steering"]["supported"] == true {
            Some(Steer::Acp)
        } else if interject {
            Some(Steer::Interject)
        } else {
            None
        }
    }

    /// The extension method and params that steer `blocks` in, or `None` when they have to wait
    /// for the next turn.
    fn request(self, session_id: &str, blocks: &[Value]) -> Option<(&'static str, Value)> {
        match self {
            // `promptRequired`: if the turn just ended, Claude Code hands the message back instead
            // of starting a turn that no `session/prompt` reports the end of.
            Steer::Acp => Some((
                "session/steering",
                json!({
                    "sessionId": session_id,
                    "prompt": blocks,
                    "_meta": { "steering": { "idleBehavior": "promptRequired" } },
                }),
            )),
            // Grok takes one text and images: files and @-mentions wait for the next turn.
            Steer::Interject => {
                if blocks.iter().any(|b| !matches!(b["type"].as_str(), Some("text" | "image"))) {
                    return None;
                }
                let text = blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n\n");
                let content: Vec<Value> = std::iter::once(json!({ "type": "text", "text": text }))
                    .chain(blocks.iter().filter(|b| b["type"] == "image").cloned())
                    .collect();
                Some(("x.ai/interject", json!({ "sessionId": session_id, "text": text, "content": content })))
            }
        }
    }
}

enum Steered {
    Yes,
    /// The turn had already ended; send it as a new one.
    HandedBack,
    No,
}

fn steer_outcome(resp: &Value) -> Steered {
    // `_session/steering` answers `outcome`; Grok's `x.ai/interject` answers `status`. Codex
    // starts a turn itself when the running one just ended (`startedNewTurn`).
    match resp["outcome"].as_str().or(resp["status"].as_str()) {
        Some("injected" | "startedNewTurn" | "queued") => Steered::Yes,
        Some("promptRequired") => Steered::HandedBack,
        _ => Steered::No,
    }
}

fn answer(reply: Option<oneshot::Sender<Result<Delivery>>>, r: Result<Delivery>) {
    if let Some(reply) = reply {
        let _ = reply.send(r);
    }
}

/// What a turn needs to run beside the command loop.
#[derive(Clone)]
struct TurnCtx {
    conn: Rc<acp::ClientSideConnection>,
    sink: Sink,
    csid: String,
    session_id: String,
    init_v: Rc<Value>,
    auth_methods: Vec<AuthChoice>,
    auth_lost: Rc<std::cell::Cell<bool>>,
    login_hint: &'static str,
    /// Told when the turn ends, and whether queued prompts may follow (not after a stop or an
    /// error).
    done: mpsc::UnboundedSender<bool>,
}

/// Start a turn for `msg` with `session/prompt`.
fn start_turn(ctx: &TurnCtx, msg: UserMessage) -> Result<Delivery> {
    let req: acp::PromptRequest = from_json(json!({ "sessionId": ctx.session_id, "prompt": msg.blocks }))?;
    emit(&ctx.sink, &ctx.csid, msg.event(Delivery::Sent));
    let ctx = ctx.clone();
    tokio::task::spawn_local(async move {
        let (sink, csid) = (&ctx.sink, ctx.csid.as_str());
        let go_on = match ctx.conn.prompt(req).await {
            Ok(resp) => {
                let v = serde_json::to_value(&resp).unwrap_or_default();
                if v["usage"].is_object() {
                    emit(sink, csid, CodeEvent::TurnUsage { usage: v["usage"].clone() });
                }
                let go_on = v["stopReason"] != "cancelled";
                emit(sink, csid, CodeEvent::PromptDone { stop_reason: v["stopReason"].clone() });
                go_on
            }
            // Signed out mid-session (a logout, an expired token): offer a sign-in when there is
            // one Patchbay can run, else say how.
            Err(e) if is_auth_required(&e) => {
                let message = auth_message(&e, &ctx.init_v, ctx.login_hint, true);
                if ctx.auth_methods.is_empty() {
                    emit(sink, csid, CodeEvent::Error { message });
                } else {
                    ctx.auth_lost.set(true);
                    emit(sink, csid, CodeEvent::AuthRequired { message, methods: ctx.auth_methods.clone() });
                }
                emit(sink, csid, CodeEvent::PromptDone { stop_reason: json!("error") });
                false
            }
            Err(e) => {
                emit(sink, csid, CodeEvent::Error { message: e.to_string() });
                emit(sink, csid, CodeEvent::PromptDone { stop_reason: json!("error") });
                false
            }
        };
        let _ = ctx.done.send(go_on);
    });
    Ok(Delivery::Sent)
}

/// Where a held prompt waits: forced sends first, in the order they were sent, then steers that
/// a forced send cut off, then everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Forced,
    Rescued,
    Normal,
}

type Queue = std::collections::VecDeque<(Rank, UserMessage)>;

/// Put `msg` behind every prompt of its rank or better.
fn hold(queue: &mut Queue, rank: Rank, msg: UserMessage) {
    let i = queue.iter().position(|(r, _)| *r > rank).unwrap_or(queue.len());
    queue.insert(i, (rank, msg));
}

fn enqueue(ctx: &TurnCtx, queue: &mut Queue, rank: Rank, msg: UserMessage) -> Delivery {
    emit(&ctx.sink, &ctx.csid, msg.event(Delivery::Queued));
    hold(queue, rank, msg);
    Delivery::Queued
}

/// How long a forced send waits for the cancelled turn to end before saying the agent is slow
/// to stop. Agents usually stop within a few seconds, once a running tool is killed.
const FORCE_PATIENCE: std::time::Duration =
    if cfg!(test) { std::time::Duration::from_millis(300) } else { std::time::Duration::from_secs(30) };

/// Ask the agent to end the running turn; its `session/prompt` then returns `cancelled`.
fn cancel_turn(conn: &Rc<acp::ClientSideConnection>, session_id: &str) {
    if let Ok(n) = from_json::<acp::CancelNotification>(json!({ "sessionId": session_id })) {
        // A turn's `session/prompt` goes out from its own task. Spawning the cancel behind it
        // keeps it from overtaking a prompt that was just sent.
        let conn = conn.clone();
        tokio::task::spawn_local(async move {
            let _ = conn.cancel(n).await;
        });
    }
}

/// Ask the agent to stop one tool call or background shell. It answers by ending that call, so
/// the reply says nothing the UI needs.
fn stop_job(conn: &Rc<acp::ClientSideConnection>, method: &'static str, params: Value) {
    let conn = conn.clone();
    tokio::task::spawn_local(async move {
        let _ = ext(&conn, method, params).await;
    });
}

fn drop_queue(ctx: &TurnCtx, queue: &mut Queue) {
    if queue.is_empty() {
        return;
    }
    let prompt_ids = queue.drain(..).map(|(_, m)| m.id).collect();
    emit(&ctx.sink, &ctx.csid, CodeEvent::Unqueued { prompt_ids, restore: true });
}

/// Decides whether a permission request is pre-approved: (merged tool call, options) ->
/// (option id to select, description for the UI).
pub type AutoApprove = Arc<dyn Fn(&Value, &Value) -> Option<(String, String)> + Send + Sync>;

/// Builds the ACP `mcpServers` list once the agent's capabilities are known; also returns
/// names of servers left out because the agent doesn't support their transport.
pub type McpProvider = Box<dyn FnOnce(&Value) -> (Vec<Value>, Vec<String>) + Send>;

/// Starts an agent that runs inside the app, given the two pipes its ACP connection uses: what
/// the client sends, and where the agent writes.
pub type NativeAgent =
    Arc<dyn Fn(tokio::io::DuplexStream, tokio::io::DuplexStream) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// The agent behind the connection: a child process, or a task in this process.
enum AgentProc {
    Child(tokio::process::Child),
    Native { done: tokio::sync::watch::Receiver<bool>, task: tokio::task::JoinHandle<()> },
}

impl AgentProc {
    /// Resolves, every time it is polled after the agent ended, with a description of how.
    async fn wait(&mut self) -> std::io::Result<String> {
        match self {
            AgentProc::Child(c) => c.wait().await.map(|s| s.to_string()),
            AgentProc::Native { done, .. } => {
                let _ = done.wait_for(|d| *d).await;
                Ok("the Patchbay agent stopped".to_owned())
            }
        }
    }

    fn start_kill(&mut self) {
        match self {
            AgentProc::Child(c) => {
                let _ = c.start_kill();
            }
            AgentProc::Native { task, .. } => task.abort(),
        }
    }
}

pub struct SpawnSpec {
    pub sink: Sink,
    pub code_session_id: String,
    /// The agent's id (`AgentKind`), for the wire log and diagnostics.
    pub agent: String,
    pub launch: Launch,
    pub cwd: PathBuf,
    /// Existing ACP session to `session/load`; `None` creates a new one.
    pub resume: Option<String>,
    pub pending: PendingPermissions,
    /// Called with the ACP session id once known.
    pub on_session: Box<dyn Fn(String) + Send>,
    pub auto_approve: AutoApprove,
    pub mcp: McpProvider,
}

struct UiClient {
    sink: Sink,
    code_session_id: String,
    pending: PendingPermissions,
    auto_approve: AutoApprove,
    /// Latest known fields per tool call; permission requests may carry only a partial update.
    tool_calls: RefCell<HashMap<String, Value>>,
    /// Which config options stand for model and mode, shared with the command loop.
    config_ids: Rc<RefCell<ConfigIds>>,
    approvals: ApprovalSetup,
    /// The command loop, to put the agent back in `approvals.select_mode`. Weak, so the loop
    /// still ends when its owner drops the sender.
    cmds: mpsc::WeakUnboundedSender<AgentCmd>,
    /// The ACP session this client shows, once known. Updates for other sessions are an agent's
    /// subagents streaming their own work, and stay out of this transcript.
    acp_session: Rc<RefCell<Option<String>>>,
}

/// Config option ids that carry the model and mode, when the agent uses `configOptions`
/// (`session/set_config_option`) instead of the older `session/set_model` / `set_mode`.
#[derive(Debug, Clone, Default, PartialEq)]
struct ConfigIds {
    model: Option<String>,
    mode: Option<String>,
}

impl ConfigIds {
    /// Take ids from a fresh option list, keeping known ones it leaves out.
    fn update(&mut self, next: ConfigIds) {
        if next.model.is_some() {
            self.model = next.model;
        }
        if next.mode.is_some() {
            self.mode = next.mode;
        }
    }
}

/// A session's models, modes and other config options in the shapes the UI reads. Agents that
/// only send `configOptions` (OpenCode, Kilo) get `models`/`modes` derived from them, so the UI
/// has one model picker and one mode switch whatever the agent.
#[derive(Debug, Clone, PartialEq)]
struct SessionConfig {
    models: Value,
    modes: Value,
    /// Options other than model and mode.
    extra: Value,
    ids: ConfigIds,
}

impl SessionConfig {
    /// From a `session/new` or `session/load` response.
    fn from_response(resp: &Value) -> Self {
        let mut cfg = Self::from_options(&resp["configOptions"]);
        if cfg.ids.model.is_none() {
            cfg.models = resp["models"].clone();
        }
        if cfg.ids.mode.is_none() {
            cfg.modes = resp["modes"].clone();
        }
        cfg
    }

    fn from_options(options: &Value) -> Self {
        let mut cfg = SessionConfig {
            models: Value::Null,
            modes: Value::Null,
            extra: json!([]),
            ids: ConfigIds::default(),
        };
        let Some(options) = options.as_array() else {
            return cfg;
        };
        let mut extra = Vec::new();
        for opt in options {
            let category = opt["category"].as_str().or(opt["id"].as_str());
            let id = opt["id"].as_str().map(str::to_owned);
            let is_select = opt["type"] == "select";
            match category {
                Some("model") if is_select && cfg.ids.model.is_none() => {
                    let available: Vec<Value> = select_values(&opt["options"])
                        .into_iter()
                        .map(|(value, name, description)| {
                            json!({ "modelId": value, "name": name, "description": description })
                        })
                        .collect();
                    cfg.models = json!({ "currentModelId": opt["currentValue"], "availableModels": available });
                    cfg.ids.model = id;
                }
                Some("mode") if is_select && cfg.ids.mode.is_none() => {
                    let available: Vec<Value> = select_values(&opt["options"])
                        .into_iter()
                        .map(|(value, name, description)| {
                            json!({ "id": value, "name": name, "description": description })
                        })
                        .collect();
                    cfg.modes = json!({ "currentModeId": opt["currentValue"], "availableModes": available });
                    cfg.ids.mode = id;
                }
                _ => extra.push(opt.clone()),
            }
        }
        cfg.extra = Value::Array(extra);
        cfg
    }

    fn ids(&self) -> ConfigIds {
        self.ids.clone()
    }

    fn event(&self) -> CodeEvent {
        CodeEvent::Config {
            models: self.models.clone(),
            modes: self.modes.clone(),
            config_options: self.extra.clone(),
        }
    }
}

/// (value, name, description) of a select option's choices, flattening option groups.
fn select_values(options: &Value) -> Vec<(Value, Value, Value)> {
    let mut out = Vec::new();
    for o in options.as_array().into_iter().flatten() {
        if let Some(group) = o["options"].as_array() {
            out.extend(select_values(&Value::Array(group.clone())));
        } else if !o["value"].is_null() {
            out.push((o["value"].clone(), o["name"].clone(), o["description"].clone()));
        }
    }
    out
}

/// Drop the agent's own approval-policy modes from what the UI offers.
fn hide_modes(modes: &mut Value, hide: &[&str]) {
    if let Some(list) = modes["availableModes"].as_array_mut() {
        list.retain(|m| !m["id"].as_str().is_some_and(|id| hide.contains(&id)));
    }
}

impl UiClient {
    /// Merge a tool call's fields into what's known about it, the newer fields winning, and
    /// return the result.
    fn merge_tool_call(&self, tc: &Value) -> Value {
        let Some(id) = tc["toolCallId"].as_str() else { return tc.clone() };
        let mut map = self.tool_calls.borrow_mut();
        let entry = map.entry(id.to_owned()).or_insert_with(|| json!({}));
        merge_fields(entry, tc);
        if let Some(o) = entry.as_object_mut() {
            o.remove("sessionUpdate");
        }
        entry.clone()
    }

    /// The agent switched itself into one of its own approval policies (Claude does when a plan
    /// is approved with "auto-accept edits"): put it back, so the session's mode still decides.
    fn keep_mode(&self, current: Option<&str>) {
        let (Some(current), Some(want)) = (current, self.approvals.select_mode.as_ref()) else { return };
        if !self.approvals.hide_modes.contains(&current) {
            return;
        }
        let Some(cmds) = self.cmds.upgrade() else { return };
        let (reply, _) = oneshot::channel();
        let _ = cmds.send(AgentCmd::SetMode { mode_id: want.clone(), reply });
        emit(
            &self.sink,
            &self.code_session_id,
            CodeEvent::Notice {
                message: format!(
                    "The agent switched to its \"{current}\" mode; Patchbay switched it back. This session's approval mode decides what runs without asking."
                ),
            },
        );
    }
}

/// Fill in the ACP defaults a `tool_call` announcement may leave out. The schema crate skips
/// default values when it serializes, so a call announced as `pending` / `other` arrives with
/// neither field. Only for announcements: in a `tool_call_update` an absent field means unchanged.
fn fill_tool_call_defaults(update: &mut Value) {
    if let Some(o) = update.as_object_mut() {
        for (k, v) in [("status", "pending"), ("kind", "other")] {
            if o.get(k).is_none_or(Value::is_null) {
                o.insert(k.into(), json!(v));
            }
        }
    }
}

fn merge_fields(into: &mut Value, from: &Value) {
    if let (Some(dst), Some(src)) = (into.as_object_mut(), from.as_object()) {
        for (k, v) in src {
            if !v.is_null() {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

#[cfg(test)]
mod tool_call_tests {
    use super::*;
    use acp::Client as _;

    /// A client whose sink records events and answers each permission request with "allow".
    fn client() -> (UiClient, Arc<Mutex<Vec<CodeEvent>>>, mpsc::UnboundedSender<AgentCmd>) {
        let events: Arc<Mutex<Vec<CodeEvent>>> = Arc::default();
        let pending: PendingPermissions = Arc::default();
        let (seen, answers) = (events.clone(), pending.clone());
        let sink: Sink = Arc::new(move |env: Envelope| {
            if let CodeEvent::Permission { request_id, .. } = &env.event
                && let Some((_, tx)) = answers.lock().unwrap().remove(request_id)
            {
                let _ = tx.send(Some("allow".into()));
            }
            seen.lock().unwrap().push(env.event);
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = UiClient {
            sink,
            code_session_id: "c".into(),
            pending,
            auto_approve: Arc::new(|_, _| None),
            tool_calls: RefCell::default(),
            config_ids: Rc::default(),
            approvals: ApprovalSetup::default(),
            cmds: tx.downgrade(),
            acp_session: Rc::default(),
        };
        (client, events, tx)
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    fn notify(c: &UiClient, update: Value) {
        let n = serde_json::from_value(json!({ "sessionId": "s", "update": update })).unwrap();
        block_on(c.session_notification(n)).unwrap();
    }

    fn last_update(events: &Mutex<Vec<CodeEvent>>) -> Value {
        match events.lock().unwrap().last() {
            Some(CodeEvent::Update { update }) => update.clone(),
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn announced_tool_call_gets_spec_defaults() {
        let (c, events, _tx) = client();
        notify(&c, json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run", "status": "pending", "kind": "other" }));
        let u = last_update(&events);
        assert_eq!((u["status"].clone(), u["kind"].clone()), (json!("pending"), json!("other")));

        notify(&c, json!({ "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "Edit", "kind": "edit", "status": "in_progress" }));
        let u = last_update(&events);
        assert_eq!((u["status"].clone(), u["kind"].clone()), (json!("in_progress"), json!("edit")));

        // An update leaves out what didn't change.
        notify(&c, json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t2", "title": "Edit a.rs" }));
        let u = last_update(&events);
        assert!(u.get("status").is_none() && u.get("kind").is_none(), "{u}");
        assert_eq!(c.tool_calls.borrow()["t2"]["status"], "in_progress");
    }

    #[test]
    fn reannounced_tool_call_keeps_its_status_and_kind() {
        let (c, events, _tx) = client();
        notify(&c, json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit", "kind": "edit", "status": "completed" }));
        notify(&c, json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit a.rs" }));
        let u = last_update(&events);
        assert!(u.get("status").is_none() && u.get("kind").is_none(), "{u}");
        let stored = &c.tool_calls.borrow()["t1"];
        assert_eq!((stored["status"].clone(), stored["kind"].clone()), (json!("completed"), json!("edit")));
        assert_eq!(stored["title"], "Edit a.rs");
    }

    #[test]
    fn permission_request_fields_win_and_are_kept() {
        let (c, events, _tx) = client();
        notify(&c, json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Edit", "kind": "edit", "rawInput": { "old": 1 } }));
        let req = serde_json::from_value(json!({
            "sessionId": "s",
            "toolCall": {
                "toolCallId": "t1",
                "title": "Edit `a.rs`",
                "rawInput": { "file_path": "a.rs" },
                "content": [{ "type": "diff", "path": "/w/a.rs", "oldText": "a", "newText": "b" }]
            },
            "options": [{ "optionId": "allow", "name": "Yes", "kind": "allow_once" }]
        }))
        .unwrap();
        block_on(c.request_permission(req)).unwrap();

        let sent = events.lock().unwrap().iter().find_map(|e| match e {
            CodeEvent::Permission { request, .. } => Some(request["toolCall"].clone()),
            _ => None,
        });
        let tc = sent.expect("a permission event");
        assert_eq!(tc["title"], "Edit `a.rs`");
        assert_eq!(tc["rawInput"], json!({ "file_path": "a.rs" }));
        assert_eq!(tc["kind"], "edit");
        assert_eq!(tc["status"], "pending");
        assert_eq!(tc["content"][0]["type"], "diff");
        assert!(tc.get("sessionUpdate").is_none());
        // The stored call keeps the request's fields once the request is answered, and the UI
        // gets them as an update too.
        assert_eq!(c.tool_calls.borrow()["t1"], tc);
        let evs = events.lock().unwrap();
        let upd = evs.iter().find_map(|e| match e {
            CodeEvent::Update { update } if update["sessionUpdate"] == "tool_call_update" => Some(update.clone()),
            _ => None,
        });
        assert_eq!(upd.expect("a tool_call_update")["content"][0]["newText"], "b");
    }
}

impl UiClient {
    /// Ask the UI (or a saved rule) about a permission request; returns the ACP response.
    async fn permission(&self, mut raw: Value) -> Value {
        // The request may carry fields no notification did (a diff, rawInput): keep them with the
        // call, and show the UI the merged call, the request's fields winning. A call the
        // transcript already shows also gets them as an update, so they outlast the request (or
        // reach the UI at all when a rule answers it).
        let known = raw["toolCall"]["toolCallId"]
            .as_str()
            .is_some_and(|id| self.tool_calls.borrow().contains_key(id));
        let tool_call = self.merge_tool_call(&raw["toolCall"]);
        if known {
            let mut update = raw["toolCall"].clone();
            update["sessionUpdate"] = json!("tool_call_update");
            emit(&self.sink, &self.code_session_id, CodeEvent::Update { update });
        }
        raw["toolCall"] = tool_call.clone();
        if let Some((option_id, description)) = (self.auto_approve)(&tool_call, &raw["options"]) {
            emit(
                &self.sink,
                &self.code_session_id,
                CodeEvent::AutoApproved {
                    tool_call_id: tool_call["toolCallId"].as_str().unwrap_or_default().to_owned(),
                    description,
                },
            );
            return json!({ "outcome": { "outcome": "selected", "optionId": option_id } });
        }
        let request_id = crate::db::new_id();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(request_id.clone(), (self.code_session_id.clone(), tx));
        emit(
            &self.sink,
            &self.code_session_id,
            CodeEvent::Permission {
                request_id: request_id.clone(),
                request: raw,
            },
        );
        let choice = rx.await.ok().flatten();
        self.pending.lock().unwrap().remove(&request_id);
        emit(
            &self.sink,
            &self.code_session_id,
            CodeEvent::PermissionResolved { request_id },
        );
        let outcome = match choice {
            Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
            None => json!({ "outcome": "cancelled" }),
        };
        json!({ "outcome": outcome })
    }

    fn update(&self, session_id: &str, mut update: Value) {
        if self.acp_session.borrow().as_deref().is_some_and(|sid| session_id != sid) {
            return;
        }
        // Defaults only for a new call: re-announcing a known one leaves out what didn't change.
        if update["sessionUpdate"] == "tool_call"
            && update["toolCallId"].as_str().is_some_and(|id| !self.tool_calls.borrow().contains_key(id))
        {
            fill_tool_call_defaults(&mut update);
        }
        if matches!(update["sessionUpdate"].as_str(), Some("tool_call" | "tool_call_update")) {
            self.merge_tool_call(&update);
        }
        match update["sessionUpdate"].as_str() {
            Some("config_option_update") => {
                if update["configOptions"].is_array() {
                    let mut cfg = SessionConfig::from_options(&update["configOptions"]);
                    self.config_ids.borrow_mut().update(cfg.ids());
                    self.keep_mode(cfg.modes["currentModeId"].as_str());
                    hide_modes(&mut cfg.modes, self.approvals.hide_modes);
                    emit(&self.sink, &self.code_session_id, cfg.event());
                }
            }
            Some("current_mode_update") => {
                self.keep_mode(update["currentModeId"].as_str());
                emit(&self.sink, &self.code_session_id, CodeEvent::Update { update });
            }
            Some("usage_update") => {
                emit(&self.sink, &self.code_session_id, CodeEvent::ContextUsage { update });
            }
            _ => emit(&self.sink, &self.code_session_id, CodeEvent::Update { update }),
        }
    }

    /// A `session/update` the ACP types can't decode (see `wire::reroute`). The UI takes the
    /// kinds it knows as they are, unknown values included; the rest show as a dim line.
    fn raw_update(&self, params: Value) {
        let Some(session_id) = params["sessionId"].as_str() else {
            // No session to check it against: show it in this one rather than drop it unseen.
            tracing::warn!("the agent sent a session/update without a session id");
            let message = "The agent sent an update without a session id".to_owned();
            emit(&self.sink, &self.code_session_id, CodeEvent::Unrecognized { message, raw: params });
            return;
        };
        if self.acp_session.borrow().as_deref().is_some_and(|sid| session_id != sid) {
            return;
        }
        let update = params["update"].clone();
        match unknown_update(&update) {
            None => self.update(session_id, update),
            Some(message) => emit(&self.sink, &self.code_session_id, CodeEvent::Unrecognized { message, raw: update }),
        }
    }
}

/// Why the UI can't show `update` (one ACP couldn't decode), or `None` when it can as is.
fn unknown_update(update: &Value) -> Option<String> {
    let kind = update["sessionUpdate"].as_str().unwrap_or("?");
    match kind {
        "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
            let ty = update["content"]["type"].as_str().unwrap_or("?");
            let shown = ty == "text" || (ty == "image" && kind == "agent_message_chunk");
            (!shown).then(|| format!("The agent sent content Patchbay can't show ({kind}, type \"{ty}\")"))
        }
        "tool_call" | "tool_call_update" if !update["toolCallId"].is_string() => {
            Some(format!("The agent sent a tool call Patchbay can't show ({kind} without an id)"))
        }
        "plan" if !update["entries"].is_array() => Some("The agent sent a plan Patchbay can't show".into()),
        "tool_call" | "tool_call_update" | "plan" | "current_mode_update" | "available_commands_update"
        | "config_option_update" | "usage_update" | "session_info_update" => None,
        // Patchbay's own agent: how its subagents are doing (`patchbay-agent`'s `publish_agents`).
        "subagents_update" if update["agents"].is_array() => None,
        "subagents_update" => Some("The agent sent a subagent list Patchbay can't show".into()),
        _ => Some(format!("The agent sent an update Patchbay doesn't know (\"{kind}\")")),
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for UiClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let raw = serde_json::to_value(&args).unwrap_or(Value::Null);
        serde_json::from_value(self.permission(raw).await).map_err(|_| acp::Error::internal_error())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        self.update(&args.session_id.0, serde_json::to_value(&args.update).unwrap_or(Value::Null));
        Ok(())
    }

    async fn ext_method(&self, args: acp::ExtRequest) -> acp::Result<acp::ExtResponse> {
        if &*args.method != super::wire::RAW_PERMISSION {
            return Err(acp::Error::method_not_found());
        }
        // A permission request the ACP types can't decode (an unknown option kind, say). The UI
        // shows unknown values as they are (an unknown kind as a plain button); without a tool
        // call or usable options there is nothing to show or choose.
        let raw: Value = serde_json::from_str(args.params.get()).unwrap_or_default();
        let usable = raw["toolCall"]["toolCallId"].is_string()
            && raw["options"].as_array().is_some_and(|o| {
                !o.is_empty() && o.iter().all(|o| o["optionId"].is_string() && o["name"].is_string())
            });
        let resp = if usable {
            self.permission(raw).await
        } else {
            emit(
                &self.sink,
                &self.code_session_id,
                CodeEvent::Unrecognized {
                    message: "The agent asked for a permission Patchbay can't show; it was declined".into(),
                    raw,
                },
            );
            json!({ "outcome": { "outcome": "cancelled" } })
        };
        let resp = serde_json::value::to_raw_value(&resp).map_err(|_| acp::Error::internal_error())?;
        Ok(acp::ExtResponse::new(resp.into()))
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        let method = args.method.trim_start_matches('_');
        if method == super::wire::RAW_UPDATE {
            if let Ok(v) = serde_json::from_str::<Value>(args.params.get()) {
                self.raw_update(v);
            }
        // `x.ai/session_notification` carries Grok Build's own session updates.
        } else if method == "x.ai/session_notification"
            && let Ok(v) = serde_json::from_str::<Value>(args.params.get())
            && v["update"]["sessionUpdate"] == "session_status"
        {
            emit(
                &self.sink,
                &self.code_session_id,
                CodeEvent::SessionStatus {
                    status: v["update"].clone(),
                },
            );
        }
        Ok(())
    }
}

fn from_json<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).context("build ACP request")
}

/// Start the agent thread. Returns the command sender and the thread handle.
pub fn spawn(spec: SpawnSpec) -> (mpsc::UnboundedSender<AgentCmd>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let cmds = tx.downgrade();
    let sink = spec.sink.clone();
    let csid = spec.code_session_id.clone();
    let thread = std::thread::Builder::new()
        .name(format!("acp-{csid}"))
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    emit(&sink, &csid, CodeEvent::Error { message: e.to_string() });
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            let result = local.block_on(&rt, run(spec, rx, cmds));
            if let Err(e) = result {
                emit(&sink, &csid, CodeEvent::Error { message: format!("{e:#}") });
            }
            emit(
                &sink,
                &csid,
                CodeEvent::Status { state: "exited".into(), message: None },
            );
        })
        .expect("spawn ACP thread");
    (tx, thread)
}

async fn run(
    spec: SpawnSpec,
    mut rx: mpsc::UnboundedReceiver<AgentCmd>,
    cmds: mpsc::WeakUnboundedSender<AgentCmd>,
) -> Result<()> {
    let SpawnSpec {
        sink,
        code_session_id: csid,
        agent,
        launch,
        cwd,
        resume,
        pending,
        on_session,
        auto_approve,
        mcp,
    } = spec;
    let Launch {
        program,
        args,
        env,
        env_remove,
        ext: mut extensions,
        session_meta,
        initial_model,
        login_hint,
        approvals,
        native,
    } = launch;
    emit(&sink, &csid, CodeEvent::Status { state: "starting".into(), message: None });

    // Patchbay's own agent and Grok Build each name their rewind methods in their own namespace.
    let rewind_ns = if native.is_some() { "patchbay" } else { "x.ai" };
    // Keep the last stderr lines to explain crashes.
    let stderr_tail: Rc<RefCell<Vec<String>>> = Rc::default();
    let (stdin, stdout, mut child): (
        Box<dyn tokio::io::AsyncWrite + Unpin>,
        Box<dyn tokio::io::AsyncRead + Unpin>,
        AgentProc,
    ) = match native {
        Some(start) => {
            let (to_agent, agent_in) = tokio::io::duplex(1 << 20);
            let (agent_out, from_agent) = tokio::io::duplex(1 << 20);
            let (done_tx, done) = tokio::sync::watch::channel(false);
            let agent = start(agent_in, agent_out);
            let task = tokio::task::spawn_local(async move {
                agent.await;
                let _ = done_tx.send(true);
            });
            (Box::new(to_agent), Box::new(from_agent), AgentProc::Native { done, task })
        }
        None => {
            let mut cmd = tokio::process::Command::new(&program);
            cmd.args(&args)
                .current_dir(&cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            for k in &env_remove {
                cmd.env_remove(k);
            }
            for (k, v) in &env {
                cmd.env(k, v);
            }
            let mut child = cmd
                .spawn()
                .with_context(|| format!("start agent {}", program.display()))?;
            let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
            let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
            let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
            let tail = stderr_tail.clone();
            tokio::task::spawn_local(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "agent_stderr", "{line}");
                    let mut t = tail.borrow_mut();
                    t.push(line);
                    if t.len() > 40 {
                        t.remove(0);
                    }
                }
            });
            (Box::new(stdin), Box::new(stdout), AgentProc::Child(child))
        }
    };

    let config_ids: Rc<RefCell<ConfigIds>> = Rc::default();
    let acp_session: Rc<RefCell<Option<String>>> = Rc::default();
    let client = UiClient {
        sink: sink.clone(),
        code_session_id: csid.clone(),
        pending: pending.clone(),
        auto_approve,
        tool_calls: RefCell::default(),
        config_ids: config_ids.clone(),
        approvals: approvals.clone(),
        cmds,
        acp_session: acp_session.clone(),
    };
    // Through the wire tap: it logs frames when asked, and passes on raw the ones ACP can't decode.
    let (to_agent, from_agent) = super::wire::tap(super::wire::Tap { session: csid.clone(), agent }, stdin, stdout);
    let (conn, io) = acp::ClientSideConnection::new(
        client,
        to_agent.compat_write(),
        from_agent.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    let conn = Rc::new(conn);
    let io_task = tokio::task::spawn_local(io);

    // `terminal_output` (the convention Zed uses): command output comes as `_meta.terminal_output`
    // and `terminal_exit` on the tool call. An agent that backgrounds a command sends its
    // `terminal_exit` only when the command ends, which is how the UI knows it still runs.
    // `auth.terminal` (ACP, unstable) and `_meta["terminal-auth"]` (the older convention): the
    // agent may offer sign-in methods that run its login command in a terminal, which Patchbay
    // opens in its terminal pane.
    // `_meta.subagents`: Cursor CLI delivers a background subagent's result only to clients that
    // advertise it, by keeping `session/prompt` open and running a follow-up turn when the
    // subagent ends. Without it the result is queued and never read. It then also streams each
    // subagent under its own session id, which `session_notification` drops. Claude's adapter
    // reads only the top-level `subagents`, so it is unaffected.
    let mut client_caps = json!({
        "fs": { "readTextFile": false, "writeTextFile": false },
        "terminal": false,
        "auth": { "terminal": true },
        "_meta": { "terminal_output": true, "terminal-auth": true, "subagents": true },
    });
    if extensions.status_line {
        // Ask Grok Build for `session_status` updates (context fill, session token totals).
        client_caps["_meta"]["x.ai/statusLine"] = json!(true);
    }
    let init: acp::InitializeRequest = from_json(json!({
        "protocolVersion": 1,
        "clientCapabilities": client_caps,
        "clientInfo": { "name": "patchbay", "version": env!("CARGO_PKG_VERSION") }
    }))?;
    let init_resp = tokio::select! {
        r = conn.initialize(init) => r.map_err(|e| anyhow!("initialize failed: {e}"))?,
        status = child.wait() => return Err(exited(status, &stderr_tail)),
    };
    let init_v = Rc::new(serde_json::to_value(&init_resp).unwrap_or_default());
    let auth_methods = auth_choices(&init_v);
    // A terminal sign-in runs this agent's own login command, built from how it was launched.
    let login_command = |method_id: &str| terminal_auth_command(&init_v, method_id, &program, &args, &env, &env_remove);
    // Grok advertises its builtin slash commands here; other agents only send updates.
    let init_commands = init_v["_meta"]["availableCommands"].clone();
    let capabilities = init_v["agentCapabilities"].clone();
    let can_load = capabilities["loadSession"].as_bool().unwrap_or(false);
    let steer = Steer::of(&init_v, extensions.interject);
    extensions.steering = steer.is_some();
    extensions.force_send = true;
    let (mcp_servers, mcp_skipped) = mcp(&init_v);
    if !mcp_skipped.is_empty() {
        emit(
            &sink,
            &csid,
            CodeEvent::Notice {
                message: format!(
                    "This agent doesn't support these MCP servers' transport: {}",
                    mcp_skipped.join(", ")
                ),
            },
        );
    }

    let cwd_str = cwd.to_string_lossy().into_owned();
    let resume = resume.filter(|_| can_load);
    // A sign-in waiting for the session it was for, so it can say whether it worked.
    let mut signing_in: Option<oneshot::Sender<Result<()>>> = None;
    let (session_id, resp, loaded) = loop {
        let opened = open_session(
            &conn,
            &sink,
            &csid,
            resume.as_deref(),
            &cwd_str,
            &mcp_servers,
            &session_meta,
        )
        .await;
        let e = match opened {
            Ok(opened) => {
                if let Some(reply) = signing_in.take() {
                    let _ = reply.send(Ok(()));
                }
                break opened;
            }
            Err(OpenError::Other(e)) => {
                if let Some(reply) = signing_in.take() {
                    let _ = reply.send(Err(anyhow!("{e:#}")));
                }
                return Err(e);
            }
            Err(OpenError::Auth(e)) => e,
        };
        let message = auth_message(&e, &init_v, login_hint, false);
        if auth_methods.is_empty() {
            return Err(anyhow!(message));
        }
        if let Some(reply) = signing_in.take() {
            // The agent accepted the sign-in but still refuses the session.
            let _ = reply.send(Err(anyhow!(message.clone())));
        }
        emit(&sink, &csid, CodeEvent::AuthRequired { message, methods: auth_methods.clone() });
        match wait_for_sign_in(&conn, &mut rx, &mut child, &stderr_tail, &login_command).await? {
            Some(reply) => signing_in = Some(reply),
            None => {
                close(io_task, conn, &mut child).await;
                return Ok(());
            }
        }
    };
    *acp_session.borrow_mut() = Some(session_id.clone());
    on_session(session_id.clone());
    let mut cfg = SessionConfig::from_response(&resp);
    *config_ids.borrow_mut() = cfg.ids();

    // Agents that keep their own model list: reselect the model this session last used.
    if let Some(want) = initial_model.filter(|m| {
        cfg.models["currentModelId"].as_str() != Some(m.as_str())
            && cfg.models["availableModels"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x["modelId"].as_str() == Some(m.as_str())))
    }) {
        match set_model(&conn, &session_id, &cfg.ids(), &want).await {
            Ok(Some(updated)) => cfg = merge_config(cfg, updated),
            Ok(None) => cfg.models["currentModelId"] = json!(want),
            Err(e) => tracing::warn!("could not restore model {want}: {e:#}"),
        }
        *config_ids.borrow_mut() = cfg.ids();
    }

    // Agents with approval policies of their own: start in the one that asks.
    if let Some(want) = approvals.select_mode.as_ref().filter(|m| {
        cfg.modes["currentModeId"].as_str() != Some(m.as_str())
            && cfg.modes["availableModes"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x["id"].as_str() == Some(m.as_str())))
    }) {
        match set_mode(&conn, &session_id, &cfg.ids(), want).await {
            Ok(Some(updated)) => cfg = merge_config(cfg, updated),
            Ok(None) => cfg.modes["currentModeId"] = json!(want),
            Err(e) => tracing::warn!("could not select mode {want}: {e:#}"),
        }
        *config_ids.borrow_mut() = cfg.ids();
    }
    hide_modes(&mut cfg.modes, approvals.hide_modes);

    emit(
        &sink,
        &csid,
        CodeEvent::SessionReady {
            acp_session_id: session_id.clone(),
            models: cfg.models.clone(),
            modes: cfg.modes.clone(),
            loaded,
            capabilities,
            agent_info: init_v["agentInfo"].clone(),
            config_options: cfg.extra.clone(),
            extensions,
            approval_gaps: approvals.gaps,
        },
    );
    if init_commands.is_array() {
        emit(
            &sink,
            &csid,
            CodeEvent::Update {
                update: json!({ "sessionUpdate": "available_commands_update", "availableCommands": init_commands }),
            },
        );
    }
    emit(&sink, &csid, CodeEvent::Status { state: "ready".into(), message: None });

    let cancel_pending = |pending: &PendingPermissions| {
        let mut map = pending.lock().unwrap();
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, (s, _))| *s == csid)
            .map(|(k, _)| k.clone())
            .collect();
        for id in ids {
            if let Some((_, tx)) = map.remove(&id) {
                let _ = tx.send(None);
            }
        }
    };
    // After `set_config_option`, the agent returns every option; show the new state.
    let apply_config = |options: Value| {
        if !options.is_array() {
            return;
        }
        let mut cfg = SessionConfig::from_options(&options);
        config_ids.borrow_mut().update(cfg.ids());
        hide_modes(&mut cfg.modes, approvals.hide_modes);
        emit(&sink, &csid, cfg.event());
    };
    // A prompt was refused for want of a sign-in (`AuthRequired` sent), and whether a sign-in
    // for it is running now.
    let auth_lost: Rc<std::cell::Cell<bool>> = Rc::default();
    let signing_in: Rc<std::cell::Cell<bool>> = Rc::default();

    // One turn at a time: a prompt sent mid-turn is steered in or queued here, never sent as a
    // second `session/prompt` (agents cancel, refuse or garble those).
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<bool>();
    // The steered message, its reply, the agent's answer, and `forces` when it was sent.
    type SteerReply = (UserMessage, Option<oneshot::Sender<Result<Delivery>>>, Result<Value>, u64);
    let (steered_tx, mut steered_rx) = mpsc::unbounded_channel::<SteerReply>();
    let turns = TurnCtx {
        conn: conn.clone(),
        sink: sink.clone(),
        csid: csid.clone(),
        session_id: session_id.clone(),
        init_v: init_v.clone(),
        auth_methods: auth_methods.clone(),
        auth_lost: auth_lost.clone(),
        login_hint,
        done: done_tx,
    };
    let mut running = false;
    let mut queue = Queue::new();
    // A forced send cancelled the running turn: the front of the queue goes when it returns.
    let mut forcing = false;
    // How many turns a forced send has cancelled. A steer sent before the latest one went into
    // a cancelled turn, whatever the agent answers.
    let mut forces: u64 = 0;
    // Messages steered into the running turn, for "Send now" on one the agent hasn't read yet.
    // Cleared when the turn ends.
    let mut steered_in: std::collections::HashMap<String, UserMessage> = Default::default();
    // Steered messages "Send now" has since moved to the queue: their late replies don't count.
    let mut sent_now: std::collections::HashSet<String> = Default::default();
    // Armed while a forced send waits on its cancel: an agent that ignores the cancel gets a
    // notice instead of a silent wait. The message still goes when the turn ends.
    let force_wait = tokio::time::sleep(FORCE_PATIENCE);
    tokio::pin!(force_wait);
    let mut force_armed = false;

    loop {
        tokio::select! {
            // A turn that has ended is seen before any command: a forced send then starts a turn
            // instead of cancelling one that is already over.
            biased;
            status = child.wait() => {
                return Err(exited(status, &stderr_tail));
            }
            Some(go_on) = done_rx.recv() => {
                running = false;
                force_armed = false;
                steered_in.clear();
                let forced = std::mem::take(&mut forcing) && !auth_lost.get();
                if !go_on && !forced {
                    drop_queue(&turns, &mut queue);
                } else if let Some((_, next)) = queue.pop_front() {
                    match start_turn(&turns, next) {
                        Ok(_) => running = true,
                        Err(e) => emit(&sink, &csid, CodeEvent::Error { message: format!("{e:#}") }),
                    }
                }
            }
            Some((msg, reply, resp, sent_at)) = steered_rx.recv() => {
                // "Send now" took it from the turn: it is queued, whatever the agent answers.
                if sent_now.remove(&msg.id) {
                    answer(reply, Ok(Delivery::Queued));
                    continue;
                }
                let outcome = resp.map(|v| steer_outcome(&v)).unwrap_or_else(|e| {
                    tracing::warn!("steering failed, queueing the message instead: {e:#}");
                    Steered::No
                });
                // A forced send cancelled the turn it went into: it goes again, right after the
                // forced prompts.
                let cut_off = forcing || sent_at != forces;
                if cut_off || !matches!(outcome, Steered::Yes) {
                    steered_in.remove(&msg.id);
                }
                // Shown as steered already; a queued or sent event moves it where it went.
                let r = match outcome {
                    Steered::Yes if !cut_off => Ok(Delivery::Steered),
                    _ if running && cut_off => Ok(enqueue(&turns, &mut queue, Rank::Rescued, msg)),
                    _ if running => Ok(enqueue(&turns, &mut queue, Rank::Normal, msg)),
                    _ => start_turn(&turns, msg).inspect(|_| running = true),
                };
                answer(reply, r);
            }
            () = &mut force_wait, if force_armed => {
                force_armed = false;
                let secs = FORCE_PATIENCE.as_secs();
                let message = format!(
                    "The agent hasn't stopped its turn after {secs} seconds. Your message goes when it does; Stop drops it."
                );
                emit(&sink, &csid, CodeEvent::Notice { message });
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                // A forced send steers instead of cancelling where steering moves a running shell
                // command to the background and `session/cancel` would kill it (`force_steers`).
                let force_steers = extensions.force_steers && steer == Some(Steer::Acp) && running && !forcing;
                let background_tools = |conn: &Rc<acp::ClientSideConnection>| {
                    if force_steers && extensions.background_tools {
                        stop_job(conn, "patchbay/background_tools", json!({ "sessionId": session_id }));
                    }
                };
                let cmd = match cmd {
                    AgentCmd::Prompt { msg, force: true, reply } if force_steers => {
                        background_tools(&conn);
                        AgentCmd::Prompt { msg, force: false, reply }
                    }
                    other => other,
                };
                match cmd {
                    AgentCmd::Prompt { msg, force: true, reply } => {
                        if !running {
                            answer(reply, start_turn(&turns, msg).inspect(|_| running = true));
                        } else {
                            // Never steered: the user wants this turn over. The cancelled turn's
                            // output stays in the transcript. Forced sends go in the order sent.
                            answer(reply, Ok(enqueue(&turns, &mut queue, Rank::Forced, msg)));
                            if !forcing {
                                cancel_pending(&pending);
                                cancel_turn(&conn, &session_id);
                                forcing = true;
                                forces += 1;
                                force_wait.as_mut().reset(tokio::time::Instant::now() + FORCE_PATIENCE);
                                force_armed = true;
                            }
                        }
                    }
                    AgentCmd::Prompt { msg, force: false, reply } => {
                        if !running {
                            answer(reply, start_turn(&turns, msg).inspect(|_| running = true));
                        } else if let Some((method, params)) =
                            steer.filter(|_| !forcing).and_then(|s| s.request(&session_id, &msg.blocks))
                        {
                            // Shown at once, ahead of the agent's answer to it. The agent's reply
                            // comes on `steered_rx`, so the loop goes on meanwhile. Not while a
                            // forced send is cancelling the turn: it would go down with it.
                            emit(&sink, &csid, msg.event(Delivery::Steered));
                            steered_in.insert(msg.id.clone(), msg.clone());
                            let (conn, steered, sent_at) = (conn.clone(), steered_tx.clone(), forces);
                            tokio::task::spawn_local(async move {
                                let resp = ext(&conn, method, params).await;
                                let _ = steered.send((msg, reply, resp, sent_at));
                            });
                        } else {
                            answer(reply, Ok(enqueue(&turns, &mut queue, Rank::Normal, msg)));
                        }
                    }
                    AgentCmd::AgentMessage { msg, reply } => {
                        let r = if running {
                            Ok(enqueue(&turns, &mut queue, Rank::Normal, msg))
                        } else {
                            start_turn(&turns, msg).inspect(|_| running = true)
                        };
                        answer(reply, r);
                    }
                    AgentCmd::Unqueue { prompt_id } => {
                        if let Some(i) = queue.iter().position(|(_, m)| m.id == prompt_id) {
                            queue.remove(i);
                            emit(&sink, &csid, CodeEvent::Unqueued { prompt_ids: vec![prompt_id], restore: false });
                        }
                    }
                    AgentCmd::SendNow { prompt_id } => {
                        if force_steers {
                            // Nothing is cancelled: a steered message stays steered, once the
                            // shell command it waits on is in the background. A queued one is
                            // steered in now.
                            background_tools(&conn);
                            let Some(i) = queue.iter().position(|(_, m)| m.id == prompt_id) else { continue };
                            let (rank, msg) = queue.remove(i).expect("found above");
                            let Some((method, params)) = steer.and_then(|s| s.request(&session_id, &msg.blocks)) else {
                                queue.insert(i, (rank, msg));
                                continue;
                            };
                            emit(&sink, &csid, msg.event(Delivery::Steered));
                            steered_in.insert(msg.id.clone(), msg.clone());
                            let (conn, steered, sent_at) = (conn.clone(), steered_tx.clone(), forces);
                            tokio::task::spawn_local(async move {
                                let resp = ext(&conn, method, params).await;
                                let _ = steered.send((msg, None, resp, sent_at));
                            });
                            continue;
                        }
                        let (rank, i, msg) = if let Some(i) = queue.iter().position(|(_, m)| m.id == prompt_id) {
                            let (rank, msg) = queue.remove(i).expect("found above");
                            (rank, i, msg)
                        } else if let Some(msg) = steered_in.remove(&prompt_id).filter(|_| running) {
                            // Steered into the running turn and not read yet: it moves to the
                            // queue, and the agent's reply to the steer no longer counts.
                            sent_now.insert(prompt_id);
                            emit(&sink, &csid, msg.event(Delivery::Queued));
                            (Rank::Normal, 0, msg)
                        } else {
                            continue;
                        };
                        if running {
                            // Already forced: it keeps its place among the others.
                            match rank {
                                Rank::Forced => queue.insert(i, (rank, msg)),
                                _ => hold(&mut queue, Rank::Forced, msg),
                            }
                            if !forcing {
                                cancel_pending(&pending);
                                cancel_turn(&conn, &session_id);
                                forcing = true;
                                forces += 1;
                                force_wait.as_mut().reset(tokio::time::Instant::now() + FORCE_PATIENCE);
                                force_armed = true;
                            }
                        } else {
                            match start_turn(&turns, msg) {
                                Ok(_) => running = true,
                                Err(e) => emit(&sink, &csid, CodeEvent::Error { message: format!("{e:#}") }),
                            }
                        }
                    }
                    AgentCmd::Cancel => {
                        cancel_pending(&pending);
                        drop_queue(&turns, &mut queue);
                        forcing = false;
                        force_armed = false;
                        cancel_turn(&conn, &session_id);
                    }
                    AgentCmd::CancelTool { tool_call_id } => {
                        if extensions.cancel_tool {
                            stop_job(&conn, "patchbay/cancel_tool", json!({ "sessionId": session_id, "toolCallId": tool_call_id }));
                        }
                    }
                    AgentCmd::KillShell { shell_id } => {
                        if extensions.cancel_tool {
                            stop_job(&conn, "patchbay/kill_shell", json!({ "sessionId": session_id, "shellId": shell_id }));
                        }
                    }
                    AgentCmd::SetModel { model_id, reply } => {
                        let ids = config_ids.borrow().clone();
                        let r = set_model(&conn, &session_id, &ids, &model_id).await;
                        let _ = reply.send(r.map(|updated| updated.map(apply_config).unwrap_or(())));
                    }
                    AgentCmd::SetMode { mode_id, reply } => {
                        let ids = config_ids.borrow().clone();
                        let r = set_mode(&conn, &session_id, &ids, &mode_id).await;
                        let _ = reply.send(r.map(|updated| updated.map(apply_config).unwrap_or(())));
                    }
                    AgentCmd::SetConfigOption { config_id, value, reply } => {
                        let r = set_config_option(&conn, &session_id, &config_id, &value)
                            .await
                            .map(apply_config);
                        let _ = reply.send(r);
                    }
                    AgentCmd::Rewind { ordinal, preview, revert_files, reply } => {
                        let r = if extensions.rewind {
                            rewind(&conn, rewind_ns, &session_id, ordinal, &preview, revert_files).await
                        } else {
                            Err(anyhow!("this agent can't rewind a session"))
                        };
                        let _ = reply.send(r);
                    }
                    AgentCmd::Authenticate { reply, .. } if !auth_lost.get() => {
                        let _ = reply.send(Err(anyhow!("the agent is already signed in")));
                    }
                    AgentCmd::Authenticate { reply, .. } if signing_in.get() => {
                        let _ = reply.send(Err(anyhow!("a sign-in is already in progress")));
                    }
                    // The session already exists, so a sign-in only has to succeed. A browser
                    // sign-in can take minutes, so it runs beside the loop.
                    AgentCmd::Authenticate { method_id, api_key, reply } => {
                        signing_in.set(true);
                        let (conn, sink, csid) = (conn.clone(), sink.clone(), csid.clone());
                        let (auth_lost, signing_in) = (auth_lost.clone(), signing_in.clone());
                        tokio::task::spawn_local(async move {
                            let r = authenticate(&conn, &method_id, api_key.as_deref()).await;
                            signing_in.set(false);
                            if r.is_ok() {
                                auth_lost.set(false);
                                emit(&sink, &csid, CodeEvent::AuthResolved);
                                emit(&sink, &csid, CodeEvent::Notice { message: "Signed in. Send your message again.".into() });
                            }
                            let _ = reply.send(r);
                        });
                    }
                    AgentCmd::TerminalAuth { method_id, reply } => {
                        let _ = reply.send(login_command(&method_id));
                    }
                    AgentCmd::Shutdown => break,
                }
            }
        }
    }
    cancel_pending(&pending);
    close(io_task, conn, &mut child).await;
    Ok(())
}

/// Close stdin (by dropping the connection) so the agent can flush session state, then give it
/// a moment before killing.
async fn close(
    io_task: tokio::task::JoinHandle<acp::Result<()>>,
    conn: Rc<acp::ClientSideConnection>,
    child: &mut AgentProc,
) {
    io_task.abort();
    drop(conn);
    if tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        child.start_kill();
        let _ = child.wait().await;
    }
}

/// Wait, after `CodeEvent::AuthRequired`, for a sign-in the agent accepts. Returns its reply
/// channel, to answer once the session is up, or `None` when Patchbay stops the agent instead.
/// Other commands are refused until then.
async fn wait_for_sign_in(
    conn: &acp::ClientSideConnection,
    rx: &mut mpsc::UnboundedReceiver<AgentCmd>,
    child: &mut AgentProc,
    stderr_tail: &Rc<RefCell<Vec<String>>>,
    login_command: &dyn Fn(&str) -> Result<crate::terminal::Program>,
) -> Result<Option<oneshot::Sender<Result<()>>>> {
    loop {
        let (method_id, api_key, reply) = tokio::select! {
            status = child.wait() => return Err(exited(status, stderr_tail)),
            cmd = rx.recv() => match cmd {
                None | Some(AgentCmd::Shutdown) => return Ok(None),
                Some(AgentCmd::Authenticate { method_id, api_key, reply }) => (method_id, api_key, reply),
                // A terminal sign-in ends with Patchbay restarting the agent.
                Some(AgentCmd::TerminalAuth { method_id, reply }) => {
                    let _ = reply.send(login_command(&method_id));
                    continue;
                }
                Some(other) => {
                    refuse_until_signed_in(other);
                    continue;
                }
            },
        };
        // A browser sign-in can take minutes: keep answering commands meanwhile.
        let signed_in = authenticate(conn, &method_id, api_key.as_deref());
        tokio::pin!(signed_in);
        let result = loop {
            tokio::select! {
                r = &mut signed_in => break r,
                status = child.wait() => {
                    let e = exited(status, stderr_tail);
                    let _ = reply.send(Err(anyhow!("{e:#}")));
                    return Err(e);
                }
                cmd = rx.recv() => match cmd {
                    None | Some(AgentCmd::Shutdown) => {
                        let _ = reply.send(Err(anyhow!("the agent stopped")));
                        return Ok(None);
                    }
                    Some(AgentCmd::TerminalAuth { method_id, reply }) => {
                        let _ = reply.send(login_command(&method_id));
                    }
                    Some(other) => refuse_until_signed_in(other),
                },
            }
        };
        match result {
            Ok(()) => return Ok(Some(reply)),
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }
}

fn refuse_until_signed_in(cmd: AgentCmd) {
    match cmd {
        AgentCmd::SetModel { reply, .. }
        | AgentCmd::SetMode { reply, .. }
        | AgentCmd::SetConfigOption { reply, .. }
        | AgentCmd::Rewind { reply, .. } => {
            let _ = reply.send(Err(anyhow!("the agent isn't signed in yet")));
        }
        AgentCmd::Authenticate { reply, .. } => {
            let _ = reply.send(Err(anyhow!("a sign-in is already in progress")));
        }
        AgentCmd::TerminalAuth { reply, .. } => {
            let _ = reply.send(Err(anyhow!("a sign-in is already in progress")));
        }
        AgentCmd::Prompt { reply, .. } => answer(reply, Err(anyhow!("the agent isn't signed in yet"))),
        AgentCmd::AgentMessage { reply, .. } => answer(reply, Err(anyhow!("the agent isn't signed in yet"))),
        AgentCmd::Unqueue { .. }
        | AgentCmd::SendNow { .. }
        | AgentCmd::Cancel
        | AgentCmd::CancelTool { .. }
        | AgentCmd::KillShell { .. }
        | AgentCmd::Shutdown => {}
    }
}

/// The advertised auth methods Patchbay can run: ones the agent carries out itself (no `type`,
/// or `agent`), optionally with an API key the user types, and terminal methods (`terminal`, or
/// the older `_meta["terminal-auth"]`), whose login command runs in Patchbay's terminal.
/// Methods that need the client to set environment variables, and custom gateways that need a
/// URL, are left out.
fn auth_choices(init: &Value) -> Vec<AuthChoice> {
    init["authMethods"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| matches!(m["type"].as_str(), None | Some("agent") | Some("terminal")))
        .filter(|m| m["_meta"]["gateway"].is_null())
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_owned();
            let terminal = is_terminal_method(m);
            Some(AuthChoice {
                name: m["name"].as_str().unwrap_or(&id).to_owned(),
                description: m["description"].as_str().map(str::to_owned),
                api_key: !terminal && !m["_meta"]["api-key"].is_null(),
                terminal,
                id,
            })
        })
        .collect()
}

fn is_terminal_method(m: &Value) -> bool {
    m["type"] == "terminal" || m["_meta"]["terminal-auth"].is_object()
}

/// The command for terminal sign-in method `method_id`. ACP's `terminal` method names arguments
/// (and environment) to add to the agent's own command line, so it is the agent's launch
/// command plus those. `_meta["terminal-auth"]`, when the agent sends it, gives the exact
/// command instead. Either way the agent's launch environment applies.
fn terminal_auth_command(
    init: &Value,
    method_id: &str,
    program: &std::path::Path,
    launch_args: &[String],
    env: &[(String, String)],
    env_remove: &[String],
) -> Result<crate::terminal::Program> {
    let method = init["authMethods"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["id"] == method_id && is_terminal_method(m))
        .ok_or_else(|| anyhow!("the agent offers no terminal sign-in \"{method_id}\""))?;
    let strings = |v: &Value| -> Vec<String> {
        v.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_owned)).collect()
    };
    let meta = &method["_meta"]["terminal-auth"];
    let (program, args) = match meta["command"].as_str().filter(|c| !c.is_empty()) {
        Some(command) => (command.to_owned(), strings(&meta["args"])),
        None => {
            let mut args = launch_args.to_vec();
            args.extend(strings(&method["args"]));
            (program.to_string_lossy().into_owned(), args)
        }
    };
    let mut env = env.to_vec();
    for source in [&method["env"], &meta["env"]] {
        for (k, v) in source.as_object().into_iter().flatten() {
            if let Some(v) = v.as_str() {
                env.push((k.clone(), v.to_owned()));
            }
        }
    }
    Ok(crate::terminal::Program { program, args, env, env_remove: env_remove.to_vec() })
}

/// `_meta` values to try, in order, for ACP `authenticate`. A key goes in `_meta["api-key"]`,
/// which agents read in two shapes: a string, or codex-acp `{ "apiKey": … }`. Neither
/// rejects the other's shape outright; each falls back to its environment and fails there.
fn auth_metas(api_key: Option<&str>) -> Vec<Option<Value>> {
    match api_key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => vec![Some(json!({ "api-key": k })), Some(json!({ "api-key": { "apiKey": k } }))],
        None => vec![None],
    }
}

async fn authenticate(conn: &acp::ClientSideConnection, method_id: &str, api_key: Option<&str>) -> Result<()> {
    let mut last = None;
    for meta in auth_metas(api_key) {
        let mut req = json!({ "methodId": method_id });
        if let Some(meta) = meta {
            req["_meta"] = meta;
        }
        let req: acp::AuthenticateRequest = from_json(req)?;
        match conn.authenticate(req).await {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) => anyhow!("Sign-in failed: {}", acp_error_text(&e)),
        None => anyhow!("Sign-in failed"),
    })
}

/// An ACP error's message, plus its `data.message` when the agent explains further.
fn acp_error_text(e: &acp::Error) -> String {
    let detail = e
        .data
        .as_ref()
        .and_then(|d| d["message"].as_str().or(d.as_str()))
        .filter(|d| !d.is_empty() && *d != e.message);
    match detail {
        Some(d) => format!("{} ({d})", e.message),
        None => e.message.clone(),
    }
}

fn exited(status: std::io::Result<String>, stderr_tail: &Rc<RefCell<Vec<String>>>) -> anyhow::Error {
    let tail = stderr_tail.borrow().join("\n");
    let code = status.unwrap_or_else(|e| e.to_string());
    anyhow!("agent process exited ({code})\n{tail}")
}

/// `data.errorKind` values with which claude-agent-acp reports a sign-in that failed mid-session.
/// It sends them as a plain internal error (`-32603`) and keeps `-32000` for "Please run /login".
const AUTH_ERROR_KINDS: [&str; 2] = ["authentication_failed", "oauth_org_not_allowed"];

fn is_auth_required(e: &acp::Error) -> bool {
    match i32::from(e.code) {
        -32000 => true,
        -32603 => e
            .data
            .as_ref()
            .and_then(|d| d["errorKind"].as_str())
            .is_some_and(|k| AUTH_ERROR_KINDS.contains(&k)),
        _ => false,
    }
}

/// An internal error's message without JSON-RPC's generic "Internal error:" prefix, or `None`
/// for other errors.
fn internal_error_reason(e: &acp::Error) -> Option<&str> {
    if i32::from(e.code) != -32603 {
        return None;
    }
    let m = e
        .message
        .strip_prefix("Internal error:")
        .unwrap_or(&e.message)
        .trim();
    (!m.is_empty()).then_some(m)
}

/// For ACP's `auth_required` error: how to sign in. Patchbay's own hint for the agent comes
/// first, then whatever auth methods the agent advertises. `again`: the agent was signed in
/// when the session started.
fn auth_message(e: &acp::Error, init: &Value, login_hint: &str, again: bool) -> String {
    let how = if login_hint.is_empty() {
        init["authMethods"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["description"].as_str().or(m["name"].as_str()))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        login_hint.to_owned()
    };
    let name = init["agentInfo"]["title"]
        .as_str()
        .or(init["agentInfo"]["name"].as_str())
        .unwrap_or("The agent");
    let when = if again { "again" } else { "first" };
    // An internal error carries the agent's reason (an expired login, say) and nothing else
    // does, so show it even next to the hint.
    match (internal_error_reason(e), how.is_empty()) {
        (Some(why), true) => format!("{name} needs you to sign in {when} ({why})."),
        (Some(why), false) => format!("{name} needs you to sign in {when} ({why}). {how}"),
        (None, true) => format!("{name} needs you to sign in {when} ({e})."),
        (None, false) => format!("{name} needs you to sign in {when}. {how}"),
    }
}

/// Select a model through the config option when the agent has one, else `session/set_model`.
/// Returns the agent's full option list when it sent one back.
async fn set_model(
    conn: &acp::ClientSideConnection,
    session_id: &str,
    ids: &ConfigIds,
    model_id: &str,
) -> Result<Option<Value>> {
    if let Some(config_id) = &ids.model {
        return set_config_option(conn, session_id, config_id, model_id).await.map(Some);
    }
    let req: acp::SetSessionModelRequest =
        from_json(json!({ "sessionId": session_id, "modelId": model_id }))?;
    conn.set_session_model(req).await.map_err(|e| anyhow!("{e}"))?;
    Ok(None)
}

/// Select a mode through the config option when the agent has one, else `session/set_mode`.
/// Returns the agent's full option list when it sent one back.
async fn set_mode(
    conn: &acp::ClientSideConnection,
    session_id: &str,
    ids: &ConfigIds,
    mode_id: &str,
) -> Result<Option<Value>> {
    if let Some(config_id) = &ids.mode {
        return set_config_option(conn, session_id, config_id, mode_id).await.map(Some);
    }
    let req: acp::SetSessionModeRequest = from_json(json!({ "sessionId": session_id, "modeId": mode_id }))?;
    conn.set_session_mode(req).await.map_err(|e| anyhow!("{e}"))?;
    Ok(None)
}

async fn set_config_option(
    conn: &acp::ClientSideConnection,
    session_id: &str,
    config_id: &str,
    value: &str,
) -> Result<Value> {
    let req: acp::SetSessionConfigOptionRequest =
        from_json(json!({ "sessionId": session_id, "configId": config_id, "value": value }))?;
    let resp = conn
        .set_session_config_option(req)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(serde_json::to_value(&resp).unwrap_or_default()["configOptions"].clone())
}

/// Fold a fresh option list into `cfg`, keeping legacy models/modes the options don't cover.
fn merge_config(cfg: SessionConfig, options: Value) -> SessionConfig {
    let mut next = SessionConfig::from_options(&options);
    if next.ids.model.is_none() {
        next.models = cfg.models;
    }
    if next.ids.mode.is_none() {
        next.modes = cfg.modes;
    }
    next
}

async fn ext(conn: &acp::ClientSideConnection, method: &str, params: Value) -> Result<Value> {
    let raw = serde_json::value::to_raw_value(&params)?;
    let resp = conn
        .ext_method(acp::ExtRequest::new(method, raw.into()))
        .await
        .map_err(|e| anyhow!("{method} failed: {e}"))?;
    Ok(serde_json::from_str(resp.0.get()).unwrap_or(Value::Null))
}

#[derive(serde::Deserialize)]
struct RewindPoint {
    prompt_index: usize,
    #[serde(default)]
    prompt_preview: Option<String>,
}

/// Map a transcript position to Grok Build's prompt index (via `x.ai/rewind/points`, so
/// synthetic turns the UI never shows don't shift it) and rewind there.
async fn rewind(
    conn: &acp::ClientSideConnection,
    ns: &str,
    session_id: &str,
    ordinal: usize,
    preview: &str,
    revert_files: bool,
) -> Result<()> {
    let points = ext(conn, &format!("{ns}/rewind/points"), json!({ "sessionId": session_id })).await?;
    let mut points: Vec<RewindPoint> =
        serde_json::from_value(points["rewind_points"].clone()).unwrap_or_default();
    points.sort_by_key(|p| p.prompt_index);
    let target = pick_rewind_point(&points, ordinal, preview)
        .ok_or_else(|| anyhow!("no rewind point for that message"))?;
    ext(
        conn,
        &format!("{ns}/rewind/execute"),
        json!({
            "sessionId": session_id,
            "targetPromptIndex": target,
            "mode": if revert_files { "all" } else { "conversation_only" },
            "force": true,
        }),
    )
    .await?;
    Ok(())
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Prefer the point at `ordinal` when its preview agrees with the message text; otherwise the
/// only point whose preview matches; otherwise fall back to `ordinal`.
fn pick_rewind_point(points: &[RewindPoint], ordinal: usize, text: &str) -> Option<usize> {
    let text = norm(text);
    let matches = |p: &RewindPoint| {
        p.prompt_preview.as_deref().is_some_and(|pv| {
            let pv = norm(pv.trim_end_matches(['…', '.']));
            !pv.is_empty() && (text.starts_with(&pv) || pv.starts_with(&text) || pv.contains(&text))
        })
    };
    if let Some(p) = points.get(ordinal)
        && (p.prompt_preview.is_none() || matches(p))
    {
        return Some(p.prompt_index);
    }
    let found: Vec<&RewindPoint> = points.iter().filter(|p| matches(p)).collect();
    if found.len() == 1 {
        return Some(found[0].prompt_index);
    }
    points.get(ordinal).map(|p| p.prompt_index)
}

enum OpenError {
    /// ACP `auth_required`: the agent wants a sign-in first.
    Auth(acp::Error),
    Other(anyhow::Error),
}

/// `session/load` the session to resume, falling back to `session/new`.
async fn open_session(
    conn: &acp::ClientSideConnection,
    sink: &Sink,
    csid: &str,
    resume: Option<&str>,
    cwd: &str,
    mcp_servers: &[Value],
    meta: &Value,
) -> std::result::Result<(String, Value, bool), OpenError> {
    if let Some(sid) = resume {
        let req: acp::LoadSessionRequest = from_json(
            json!({ "sessionId": sid, "cwd": cwd, "mcpServers": mcp_servers, "_meta": meta }),
        )
        .map_err(OpenError::Other)?;
        match conn.load_session(req).await {
            Ok(resp) => return Ok((sid.to_owned(), serde_json::to_value(&resp).unwrap_or_default(), true)),
            Err(e) if is_auth_required(&e) => return Err(OpenError::Auth(e)),
            Err(e) => {
                // Not an error: some agents (Cursor, Claude Code) can't load a session that
                // never got a prompt, and the transcript cache still shows earlier turns.
                emit(
                    sink,
                    csid,
                    CodeEvent::Notice {
                        message: format!("Couldn't resume the agent's previous session, so it started a new one ({e})."),
                    },
                );
            }
        }
    }
    let req: acp::NewSessionRequest =
        from_json(json!({ "cwd": cwd, "mcpServers": mcp_servers, "_meta": meta })).map_err(OpenError::Other)?;
    match conn.new_session(req).await {
        Ok(resp) => {
            let v = serde_json::to_value(&resp).unwrap_or_default();
            Ok((resp.session_id.to_string(), v, false))
        }
        Err(e) if is_auth_required(&e) => Err(OpenError::Auth(e)),
        Err(e) => Err(OpenError::Other(anyhow!("session/new failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentCmd, AuthChoice, CodeEvent, ConfigIds, Delivery, Envelope, RewindPoint, SessionConfig, SpawnSpec, Steer,
        Steered, UserMessage, auth_choices, auth_metas, merge_config, pick_rewind_point, steer_outcome,
        terminal_auth_command,
    };
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn opencode_like() -> serde_json::Value {
        json!({
            "sessionId": "s",
            "configOptions": [
                { "id": "model", "name": "Model", "category": "model", "type": "select",
                  "currentValue": "a/x",
                  "options": [
                    { "value": "a/x", "name": "X" },
                    { "group": "b", "name": "B", "options": [{ "value": "b/y", "name": "Y", "description": "why" }] }
                  ] },
                { "id": "effort", "name": "Effort", "category": "thought_level", "type": "select",
                  "currentValue": "default", "options": [{ "value": "low", "name": "Low" }, { "value": "default", "name": "Default" }] },
                { "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                  "currentValue": "build", "options": [{ "value": "build", "name": "Build" }, { "value": "plan", "name": "Plan" }] }
            ]
        })
    }

    #[test]
    fn config_options_become_models_and_modes() {
        let cfg = SessionConfig::from_response(&opencode_like());
        assert_eq!(cfg.ids, ConfigIds { model: Some("model".into()), mode: Some("mode".into()) });
        assert_eq!(cfg.models["currentModelId"], "a/x");
        let ids: Vec<&str> = cfg.models["availableModels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["modelId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a/x", "b/y"], "groups are flattened");
        assert_eq!(cfg.modes["currentModeId"], "build");
        assert_eq!(cfg.modes["availableModes"][1]["id"], "plan");
        let extra = cfg.extra.as_array().unwrap();
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0]["id"], "effort");
    }

    #[test]
    fn legacy_models_and_modes_pass_through() {
        let resp = json!({
            "models": { "currentModelId": "m1", "availableModels": [{ "modelId": "m1", "name": "M1" }] },
            "modes": { "currentModeId": "agent", "availableModes": [{ "id": "agent", "name": "Agent" }] }
        });
        let cfg = SessionConfig::from_response(&resp);
        assert_eq!(cfg.ids, ConfigIds::default());
        assert_eq!(cfg.models, resp["models"]);
        assert_eq!(cfg.modes, resp["modes"]);
        assert_eq!(cfg.extra, json!([]));
    }

    #[test]
    fn mixed_agents_prefer_config_options() {
        // Claude's adapter sends legacy modes and a model config option.
        let resp = json!({
            "modes": { "currentModeId": "default", "availableModes": [{ "id": "default", "name": "Manual" }] },
            "configOptions": [{ "id": "model", "name": "Model", "category": "model", "type": "select",
                                "currentValue": "opus", "options": [{ "value": "opus", "name": "Opus" }] }]
        });
        let cfg = SessionConfig::from_response(&resp);
        assert_eq!(cfg.ids.model.as_deref(), Some("model"));
        assert_eq!(cfg.ids.mode, None);
        assert_eq!(cfg.modes, resp["modes"]);
        // A later option list without modes keeps the legacy ones.
        let next = merge_config(cfg, resp["configOptions"].clone());
        assert_eq!(next.modes, resp["modes"]);
        assert_eq!(next.models["currentModelId"], "opus");
    }

    fn pt(i: usize, pv: Option<&str>) -> RewindPoint {
        RewindPoint {
            prompt_index: i,
            prompt_preview: pv.map(str::to_owned),
        }
    }

    #[test]
    fn picks_by_ordinal_when_preview_agrees() {
        let pts = [pt(0, Some("first")), pt(1, Some("second prompt"))];
        assert_eq!(pick_rewind_point(&pts, 1, "second prompt"), Some(1));
    }

    #[test]
    fn skips_hidden_synthetic_prompts() {
        // A synthetic turn at index 1 is not shown in the transcript.
        let pts = [pt(0, Some("first")), pt(1, Some("[auto wake]")), pt(2, Some("second"))];
        assert_eq!(pick_rewind_point(&pts, 1, "second"), Some(2));
    }

    #[test]
    fn falls_back_to_ordinal() {
        let pts = [pt(0, None), pt(1, None)];
        assert_eq!(pick_rewind_point(&pts, 1, "anything"), Some(1));
        assert_eq!(pick_rewind_point(&pts, 5, "anything"), None);
    }

    #[test]
    fn auth_choices_keep_methods_patchbay_can_run() {
        // Methods as codex-acp and others advertise them, plus kinds Patchbay can't run.
        let init = json!({ "authMethods": [
            { "id": "oauth-personal", "name": "Log in with Google", "description": "Log in with your Google account" },
            { "id": "provider-api-key", "name": "Provider API key", "_meta": { "api-key": { "provider": "google" } } },
            { "id": "gateway", "name": "AI API Gateway", "_meta": { "gateway": { "protocol": "google" } } },
            { "type": "env_var", "id": "env", "name": "Env", "vars": [{ "name": "X_KEY" }] },
            // Claude's adapter: ACP's terminal type, and the older `_meta` convention alone.
            { "type": "terminal", "id": "claude-ai-login", "name": "Claude Subscription",
              "args": ["--cli", "auth", "login", "--claudeai"] },
            { "id": "claude-login", "name": "Log in with Claude",
              "_meta": { "terminal-auth": { "command": "node", "args": ["acp.js", "--cli"] } } },
        ] });
        let choice = |id: &str, name: &str, description: Option<&str>, api_key: bool, terminal: bool| AuthChoice {
            id: id.into(),
            name: name.into(),
            description: description.map(Into::into),
            api_key,
            terminal,
        };
        assert_eq!(
            auth_choices(&init),
            [
                choice("oauth-personal", "Log in with Google", Some("Log in with your Google account"), false, false),
                choice("provider-api-key", "Provider API key", None, true, false),
                choice("claude-ai-login", "Claude Subscription", None, false, true),
                choice("claude-login", "Log in with Claude", None, false, true),
            ]
        );
        assert!(auth_choices(&json!({})).is_empty());
    }

    #[test]
    fn api_keys_go_in_both_meta_shapes() {
        assert_eq!(auth_metas(None), [None]);
        assert_eq!(auth_metas(Some("  ")), [None], "a blank key is no key");
        assert_eq!(
            auth_metas(Some(" sk-1 ")),
            [Some(json!({ "api-key": "sk-1" })), Some(json!({ "api-key": { "apiKey": "sk-1" } }))]
        );
    }

    #[test]
    fn auth_required_event_serializes_for_the_ui() {
        let ev = CodeEvent::AuthRequired {
            message: "Sign in".into(),
            methods: vec![AuthChoice { id: "k".into(), name: "Key".into(), description: None, api_key: true, terminal: false }],
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({ "type": "authRequired", "message": "Sign in",
                    "methods": [{ "id": "k", "name": "Key", "description": null, "apiKey": true, "terminal": false }] })
        );
        assert_eq!(serde_json::to_value(CodeEvent::AuthResolved).unwrap(), json!({ "type": "authResolved" }));
    }

    #[test]
    fn terminal_sign_in_runs_the_agent_login_command() {
        let init = json!({ "authMethods": [
            { "id": "browser", "name": "Log in" },
            { "type": "terminal", "id": "tui", "name": "Terminal", "args": ["--cli", "login"], "env": { "MODE": "tui" } },
            { "type": "terminal", "id": "meta", "name": "Meta",
              "_meta": { "terminal-auth": { "command": "/usr/bin/node", "args": ["/x/acp.js", "--cli"], "label": "Login" } } },
        ] });
        let launch_args = ["-y".to_owned(), "adapter@1".to_owned()];
        let env = [("HOME".to_owned(), "/h".to_owned())];
        let remove = ["SECRET".to_owned()];
        let run = |id: &str| terminal_auth_command(&init, id, "npx".as_ref(), &launch_args, &env, &remove);

        // ACP's form: the agent's own launch command, plus the method's arguments and environment.
        let p = run("tui").unwrap();
        assert_eq!(p.program, "npx");
        assert_eq!(p.args, ["-y", "adapter@1", "--cli", "login"]);
        assert_eq!(p.env, [("HOME".to_owned(), "/h".to_owned()), ("MODE".to_owned(), "tui".to_owned())]);
        assert_eq!(p.env_remove, ["SECRET"]);
        // `_meta["terminal-auth"]` names the exact command.
        let p = run("meta").unwrap();
        assert_eq!(p.program, "/usr/bin/node");
        assert_eq!(p.args, ["/x/acp.js", "--cli"]);
        // Only advertised terminal methods run.
        assert!(run("browser").is_err());
        assert!(run("rm -rf").is_err());
    }

    /// Claude's adapter (0.93), as it answers Patchbay's `initialize`, survives the ACP crate's
    /// types on the way in.
    #[test]
    fn claude_terminal_methods_survive_the_acp_types() {
        let wire = json!({ "protocolVersion": 1, "agentCapabilities": {}, "authMethods": [{
            "description": "Use Claude subscription ", "name": "Claude Subscription", "id": "claude-ai-login",
            "type": "terminal", "args": ["--cli", "auth", "login", "--claudeai"],
            "_meta": { "terminal-auth": { "command": "/usr/bin/node",
                "args": ["/npx/claude-agent-acp/dist/index.js", "--cli", "auth", "login", "--claudeai"], "label": "Claude Login" } },
        }] });
        let resp: agent_client_protocol::InitializeResponse = serde_json::from_value(wire).unwrap();
        let init = serde_json::to_value(&resp).unwrap();
        assert_eq!(auth_choices(&init).iter().map(|m| (m.id.as_str(), m.terminal)).collect::<Vec<_>>(), [("claude-ai-login", true)]);
        let p = terminal_auth_command(&init, "claude-ai-login", "npx".as_ref(), &[], &[], &[]).unwrap();
        assert_eq!(p.program, "/usr/bin/node");
        assert_eq!(p.args, ["/npx/claude-agent-acp/dist/index.js", "--cli", "auth", "login", "--claudeai"]);

        // And the capability that makes it list them goes out on the wire.
        let req: agent_client_protocol::InitializeRequest = serde_json::from_value(json!({
            "protocolVersion": 1,
            "clientCapabilities": { "auth": { "terminal": true }, "_meta": { "terminal-auth": true } },
        }))
        .unwrap();
        let sent = serde_json::to_value(&req).unwrap();
        assert_eq!(sent["clientCapabilities"]["auth"]["terminal"], true);
        assert_eq!(sent["clientCapabilities"]["_meta"]["terminal-auth"], true);
    }

    /// An ACP agent that refuses `session/new` until `authenticate` gets method `api-key` with
    /// key `sk-test` in codex-acp's shape, so the string shape Patchbay tries first is refused.
    /// The prompt `expire` signs it out again, and prompts fail with `auth_required` until the
    /// next sign-in. `expire-oauth` does the same with claude-agent-acp's `-32603` and
    /// `errorKind`, and `crash` fails with a `-32603` that isn't about signing in. To clients
    /// that run terminal sign-ins it also offers `tui`, whose command
    /// (this script with `--login`) leaves a `signed-in` file that a new process starts with.
    /// `--no-methods` starts it signed in, with no auth methods.
    ///
    /// Turns: a prompt `hold` runs until a `release` steer or a cancel, and `brief` for half a
    /// second. `subagent` streams `child:work` under another session id, as a subagent would,
    /// then says whether `initialize` advertised `_meta.subagents`. A second prompt mid-turn is
    /// refused. `--steering` advertises `_session/steering` (a `refuse` steer fails, a `slow` one
    /// is answered 0.3s late), and every prompt or steer is echoed as agent text
    /// (`prompt:<text>`, `steer:<text>`). `--slow-cancel` says `cancel` for each cancel and ends
    /// the turn 0.3s later; `--late-cancel` ends it 1s later, past a forced send's patience.
    const FAKE_AGENT: &str = r#"
import json, os, sys, threading
if "--login" in sys.argv:
    if os.environ.get("FAKE_LOGIN") != "1":
        sys.exit(3)
    open("signed-in", "w").close()
    sys.exit(0)
no_methods = "--no-methods" in sys.argv
authed = no_methods or os.path.exists("signed-in")
lock = threading.Lock()
held = None
def send(m):
    with lock:
        sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
def err(i, msg, data=None):
    e = {"code": -32000, "message": msg}
    if data: e["data"] = {"message": data}
    send({"jsonrpc": "2.0", "id": i, "error": e})
def say(text):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1",
          "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
def finish(reason="end_turn"):
    global held
    i, held = held, None
    if i is not None:
        send({"jsonrpc": "2.0", "id": i, "result": {"stopReason": reason}})
def text_of(p):
    return " ".join(b.get("text", "") for b in p.get("prompt") or [] if b.get("type") == "text")
for line in sys.stdin:
    m = json.loads(line)
    if m.get("method") == "session/cancel":
        if "--slow-cancel" in sys.argv or "--late-cancel" in sys.argv:
            say("cancel")
            threading.Timer(1.0 if "--late-cancel" in sys.argv else 0.3, finish, ["cancelled"]).start()
        else:
            finish("cancelled")
        continue
    if "id" not in m or "method" not in m:
        continue
    i, meth, p = m["id"], m["method"], m.get("params") or {}
    if meth == "initialize":
        subagents = (((p.get("clientCapabilities") or {}).get("_meta") or {}).get("subagents")) is True
        methods = [{"id": "browser", "name": "Log in", "description": "Opens a browser"},
                   {"id": "api-key", "name": "API key", "_meta": {"api-key": {"provider": "x"}}}]
        if ((p.get("clientCapabilities") or {}).get("auth") or {}).get("terminal"):
            methods.append({"id": "tui", "name": "Terminal", "type": "terminal", "args": ["--login"],
                            "env": {"FAKE_LOGIN": "1"}})
        meta = {"steering": {"supported": True}} if "--steering" in sys.argv else {}
        send({"jsonrpc": "2.0", "id": i, "result": {"protocolVersion": 1, "agentCapabilities": {},
              "agentInfo": {"name": "fake", "title": "Fake Agent", "version": "1"},
              "authMethods": [] if no_methods else methods, "_meta": meta}})
    elif meth == "authenticate":
        key = (p.get("_meta") or {}).get("api-key")
        if p["methodId"] == "api-key" and isinstance(key, dict) and key.get("apiKey") == "sk-test":
            authed = True
            send({"jsonrpc": "2.0", "id": i, "result": {}})
        else:
            err(i, "Authentication required", "bad key")
    elif meth == "session/new":
        if authed:
            send({"jsonrpc": "2.0", "id": i, "result": {"sessionId": "s1"}})
        else:
            err(i, "Authentication required")
    elif meth == "session/prompt":
        t = text_of(p)
        if held is not None:
            err(i, "a turn is already running")
            continue
        if t in ("expire", "expire-oauth"):
            authed = False
        if t == "crash":
            send({"jsonrpc": "2.0", "id": i, "error": {"code": -32603, "message": "Internal error: boom",
                  "data": {"errorKind": "unknown"}}})
            continue
        if not authed and t == "expire-oauth":
            send({"jsonrpc": "2.0", "id": i, "error": {"code": -32603,
                  "message": "Internal error: Failed to authenticate: OAuth session expired",
                  "data": {"errorKind": "authentication_failed"}}})
            continue
        if not authed:
            err(i, "Authentication required")
            continue
        say("prompt:" + t)
        if t == "subagent":
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1-child",
                  "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "child:work"}}}})
            say("subagents:" + str(subagents))
        if t == "odd":
            def upd(u):
                send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": u}})
            upd({"sessionUpdate": "brand_new", "x": 1})
            upd({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run", "status": "cancelled"})
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"update": {"sessionUpdate": "plan", "entries": []}}})
            send({"jsonrpc": "2.0", "id": "p1", "method": "session/request_permission", "params": {"sessionId": "s1",
                  "toolCall": {"toolCallId": "t1"}, "options": [{"optionId": "a", "name": "Allow", "kind": "allow_for_session"}]}})
            send({"jsonrpc": "2.0", "id": "p2", "method": "session/request_permission", "params": {"sessionId": "s1", "options": []}})
        if t in ("hold", "brief"):
            held = i
            if t == "brief":
                threading.Timer(0.5, finish).start()
        else:
            send({"jsonrpc": "2.0", "id": i, "result": {"stopReason": "end_turn"}})
    elif meth == "_x.ai/interject" and "--interject" in sys.argv:
        say("interject:" + p["text"])
        send({"jsonrpc": "2.0", "id": i, "result": {"status": "queued"}})
    elif meth == "_session/steering" and "--steering" in sys.argv:
        t = text_of(p)
        if held is None:
            idle = ((p.get("_meta") or {}).get("steering") or {}).get("idleBehavior")
            send({"jsonrpc": "2.0", "id": i, "result": {"outcome": idle or "startedNewTurn"}})
        elif t == "refuse":
            send({"jsonrpc": "2.0", "id": i, "result": {"outcome": "failed"}})
        elif t == "slow":
            say("steer:" + t)
            threading.Timer(0.3, send, [{"jsonrpc": "2.0", "id": i, "result": {"outcome": "injected"}}]).start()
        else:
            say("steer:" + t)
            send({"jsonrpc": "2.0", "id": i, "result": {"outcome": "injected"}})
            if t == "release":
                finish()
    else:
        send({"jsonrpc": "2.0", "id": i, "error": {"code": -32601, "message": "unsupported"}})
"#;

    type Events = Arc<Mutex<Vec<CodeEvent>>>;

    type Fake = (Events, tokio::sync::mpsc::UnboundedSender<AgentCmd>, std::thread::JoinHandle<()>);

    /// Start `FAKE_AGENT` under Patchbay's ACP client, or `None` without a Python to run it.
    fn spawn_fake_agent(dir: &std::path::Path) -> Option<Fake> {
        spawn_fake_agent_with(dir, &[], "")
    }

    fn spawn_fake_agent_with(dir: &std::path::Path, flags: &[&str], login_hint: &'static str) -> Option<Fake> {
        spawn_fake_agent_ext(dir, flags, login_hint, Default::default())
    }

    fn spawn_fake_agent_ext(
        dir: &std::path::Path,
        flags: &[&str],
        login_hint: &'static str,
        ext: crate::code::agents::Extensions,
    ) -> Option<Fake> {
        let python = ["python3", "python"].into_iter().find(|p| {
            std::process::Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success())
        })?;
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join("agent.py");
        std::fs::write(&script, FAKE_AGENT).unwrap();
        let events: Events = Arc::default();
        let ev = events.clone();
        let (tx, thread) = super::spawn(SpawnSpec {
            sink: Arc::new(move |e: Envelope| ev.lock().unwrap().push(e.event)),
            code_session_id: "cs".into(),
            agent: "fake".into(),
            launch: crate::code::agents::Launch {
                program: python.into(),
                args: std::iter::once(script.to_string_lossy().into_owned())
                    .chain(flags.iter().map(|f| f.to_string()))
                    .collect(),
                env: Vec::new(),
                env_remove: Vec::new(),
                ext,
                session_meta: json!({}),
                initial_model: None,
                login_hint,
                approvals: Default::default(),
                native: None,
            },
            cwd: dir.to_owned(),
            resume: None,
            pending: Arc::default(),
            on_session: Box::new(|_| {}),
            auto_approve: Arc::new(|_, _| None),
            mcp: Box::new(|_| (Vec::new(), Vec::new())),
        });
        Some((events, tx, thread))
    }

    fn wait_for(events: &Events, what: &str, f: impl Fn(&[CodeEvent]) -> bool) {
        let t0 = std::time::Instant::now();
        while !f(&events.lock().unwrap()) {
            assert!(t0.elapsed() < Duration::from_secs(20), "timed out waiting for {what}: {:?}", events.lock().unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn auth_events(evs: &[CodeEvent]) -> usize {
        evs.iter().filter(|e| matches!(e, CodeEvent::AuthRequired { .. })).count()
    }

    fn count(events: &Events, f: impl Fn(&CodeEvent) -> bool) -> usize {
        events.lock().unwrap().iter().filter(|e| f(e)).count()
    }

    fn prompt_done(e: &CodeEvent) -> bool {
        matches!(e, CodeEvent::PromptDone { .. })
    }

    fn send_prompt(tx: &tokio::sync::mpsc::UnboundedSender<AgentCmd>, text: &str) {
        tx.send(AgentCmd::prompt(text)).unwrap();
    }

    fn authenticate(
        tx: &tokio::sync::mpsc::UnboundedSender<AgentCmd>,
        method: &str,
        key: Option<&str>,
    ) -> anyhow::Result<()> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let cmd = AgentCmd::Authenticate { method_id: method.into(), api_key: key.map(str::to_owned), reply };
        tx.send(cmd).unwrap();
        rx.blocking_recv().unwrap()
    }

    fn last_auth(events: &Events) -> (String, Vec<AuthChoice>) {
        let evs = events.lock().unwrap();
        match evs.iter().rev().find(|e| matches!(e, CodeEvent::AuthRequired { .. })) {
            Some(CodeEvent::AuthRequired { message, methods }) => (message.clone(), methods.clone()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn sign_in_then_start_the_session() {
        let dir = std::env::temp_dir().join(format!("pb-auth-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent(&dir) else {
            eprintln!("skipped: no python");
            return;
        };
        let authenticate = |method: &str, key: Option<&str>| authenticate(&tx, method, key);

        wait_for(&events, "authRequired", |evs| auth_events(evs) == 1);
        {
            let (message, methods) = last_auth(&events);
            assert!(message.starts_with("Fake Agent needs you to sign in first."), "{message}");
            let ids: Vec<(&str, bool, bool)> = methods.iter().map(|m| (m.id.as_str(), m.api_key, m.terminal)).collect();
            // `tui` shows that Patchbay told the agent it runs terminal sign-ins.
            assert_eq!(ids, [("browser", false, false), ("api-key", true, false), ("tui", false, true)]);
        }

        // While it waits, other commands are refused.
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(AgentCmd::SetModel { model_id: "m".into(), reply }).unwrap();
        let err = rx.blocking_recv().unwrap().unwrap_err();
        assert!(err.to_string().contains("isn't signed in"), "{err}");

        // A refused sign-in reports the agent's reason, and the agent keeps waiting.
        let err = authenticate("api-key", Some("wrong")).unwrap_err();
        assert!(err.to_string().contains("bad key"), "{err}");
        // The right key, in the second shape Patchbay tries, signs in and starts the session.
        authenticate("api-key", Some("sk-test")).unwrap();
        wait_for(&events, "sessionReady", |evs| evs.iter().any(|e| matches!(e, CodeEvent::SessionReady { .. })));
        assert_eq!(auth_events(&events.lock().unwrap()), 1);

        let err = authenticate("api-key", Some("sk-test")).unwrap_err();
        assert!(err.to_string().contains("already signed in"), "{err}");
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sign_in_again_when_a_prompt_is_refused() {
        let dir = std::env::temp_dir().join(format!("pb-auth-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent(&dir) else {
            eprintln!("skipped: no python");
            return;
        };
        wait_for(&events, "authRequired", |evs| auth_events(evs) == 1);
        authenticate(&tx, "api-key", Some("sk-test")).unwrap();
        wait_for(&events, "sessionReady", |evs| evs.iter().any(|e| matches!(e, CodeEvent::SessionReady { .. })));

        // The agent signs out mid-session: the refused prompt asks for a sign-in, not an error.
        send_prompt(&tx, "expire");
        wait_for(&events, "promptDone", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 1);
        assert_eq!(auth_events(&events.lock().unwrap()), 2);
        let (message, methods) = last_auth(&events);
        assert!(message.starts_with("Fake Agent needs you to sign in again."), "{message}");
        assert_eq!(methods.len(), 3);
        assert_eq!(count(&events, |e| matches!(e, CodeEvent::Error { .. })), 0);
        assert!(matches!(events.lock().unwrap().last(), Some(CodeEvent::PromptDone { stop_reason }) if stop_reason == "error"));

        // The sign-in runs in the main loop: a refused one leaves the card up.
        let err = authenticate(&tx, "api-key", Some("wrong")).unwrap_err();
        assert!(err.to_string().contains("bad key"), "{err}");
        assert_eq!(count(&events, |e| matches!(e, CodeEvent::AuthResolved)), 0);
        // A good one clears it without a new session; the user resends.
        authenticate(&tx, "api-key", Some("sk-test")).unwrap();
        wait_for(&events, "authResolved", |evs| evs.iter().any(|e| matches!(e, CodeEvent::AuthResolved)));
        assert_eq!(count(&events, |e| matches!(e, CodeEvent::SessionReady { .. })), 1);
        let resend = |e: &CodeEvent| matches!(e, CodeEvent::Notice { message } if message.contains("Send your message again"));
        assert_eq!(count(&events, resend), 1);
        let err = authenticate(&tx, "api-key", Some("sk-test")).unwrap_err();
        assert!(err.to_string().contains("already signed in"), "{err}");

        send_prompt(&tx, "hello");
        wait_for(&events, "promptDone", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);
        // The agent's text for the turn may land after its end: notifications run as tasks.
        let last_done = events.lock().unwrap().iter().rev().find(|e| prompt_done(e)).cloned();
        assert!(matches!(last_done, Some(CodeEvent::PromptDone { stop_reason }) if stop_reason == "end_turn"));
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refused_prompt_without_methods_shows_the_login_hint() {
        let dir = std::env::temp_dir().join(format!("pb-auth-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods"], "Run `fake login`.") else {
            eprintln!("skipped: no python");
            return;
        };
        wait_for(&events, "sessionReady", |evs| evs.iter().any(|e| matches!(e, CodeEvent::SessionReady { .. })));
        send_prompt(&tx, "expire");
        wait_for(&events, "promptDone", |evs| evs.iter().any(prompt_done));
        assert_eq!(auth_events(&events.lock().unwrap()), 0);
        let errors: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                CodeEvent::Error { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(errors, ["Fake Agent needs you to sign in again. Run `fake login`."]);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// claude-agent-acp reports an expired login as an internal error tagged `errorKind`. That
    /// asks for a sign-in, with the reason minus "Internal error:"; other internal errors don't.
    #[test]
    fn internal_error_with_an_auth_error_kind_asks_for_a_sign_in() {
        let dir = std::env::temp_dir().join(format!("pb-auth-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent(&dir) else {
            eprintln!("skipped: no python");
            return;
        };
        wait_for(&events, "authRequired", |evs| auth_events(evs) == 1);
        authenticate(&tx, "api-key", Some("sk-test")).unwrap();
        wait_for(&events, "sessionReady", |evs| {
            evs.iter()
                .any(|e| matches!(e, CodeEvent::SessionReady { .. }))
        });

        send_prompt(&tx, "crash");
        wait_for(&events, "promptDone", |evs| {
            evs.iter().filter(|e| prompt_done(e)).count() == 1
        });
        assert_eq!(auth_events(&events.lock().unwrap()), 1);
        assert_eq!(count(&events, |e| matches!(e, CodeEvent::Error { .. })), 1);

        send_prompt(&tx, "expire-oauth");
        wait_for(&events, "promptDone", |evs| {
            evs.iter().filter(|e| prompt_done(e)).count() == 2
        });
        assert_eq!(auth_events(&events.lock().unwrap()), 2);
        assert_eq!(count(&events, |e| matches!(e, CodeEvent::Error { .. })), 1);
        let (message, _) = last_auth(&events);
        let want = "Fake Agent needs you to sign in again (Failed to authenticate: OAuth session expired). Opens";
        assert!(message.starts_with(want), "{message}");
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The terminal method's command, run as Patchbay's terminal would, signs the agent in; the
    /// restarted agent starts its session without asking.
    #[test]
    fn terminal_sign_in_then_restart() {
        let dir = std::env::temp_dir().join(format!("pb-auth-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent(&dir) else {
            eprintln!("skipped: no python");
            return;
        };
        wait_for(&events, "authRequired", |evs| auth_events(evs) == 1);
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(AgentCmd::TerminalAuth { method_id: "tui".into(), reply }).unwrap();
        let program = rx.blocking_recv().unwrap().unwrap();
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(AgentCmd::TerminalAuth { method_id: "browser".into(), reply }).unwrap();
        assert!(rx.blocking_recv().unwrap().is_err(), "not a terminal method");

        let status = std::process::Command::new(&program.program)
            .args(&program.args)
            .envs(program.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success(), "{program:?}: {status}");
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();

        let (events, tx, thread) = spawn_fake_agent(&dir).unwrap();
        wait_for(&events, "sessionReady", |evs| evs.iter().any(|e| matches!(e, CodeEvent::SessionReady { .. })));
        assert_eq!(auth_events(&events.lock().unwrap()), 0);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stopping_while_waiting_for_sign_in_exits_cleanly() {
        let dir = std::env::temp_dir().join(format!("pb-auth-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent(&dir) else {
            eprintln!("skipped: no python");
            return;
        };
        wait_for(&events, "authRequired", |evs| auth_events(evs) == 1);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let evs = events.lock().unwrap();
        assert!(!evs.iter().any(|e| matches!(e, CodeEvent::Error { .. })), "{evs:?}");
        assert!(matches!(evs.last(), Some(CodeEvent::Status { state, .. }) if state == "exited"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A prompt whose id is its text, sent with `blocks` (or just the text).
    fn prompt_with(
        tx: &tokio::sync::mpsc::UnboundedSender<AgentCmd>,
        text: &str,
        blocks: Option<Vec<serde_json::Value>>,
    ) -> Delivery {
        deliver(tx, text, blocks, false)
    }

    /// A prompt sent with Ctrl+Enter: it cancels the running turn to go next.
    fn force(tx: &tokio::sync::mpsc::UnboundedSender<AgentCmd>, text: &str) -> Delivery {
        deliver(tx, text, None, true)
    }

    fn deliver(
        tx: &tokio::sync::mpsc::UnboundedSender<AgentCmd>,
        text: &str,
        blocks: Option<Vec<serde_json::Value>>,
        force: bool,
    ) -> Delivery {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let msg = UserMessage {
            id: text.into(),
            blocks: blocks.unwrap_or_else(|| vec![json!({ "type": "text", "text": text })]),
            text: text.into(),
            origin: "me".into(),
            attachments: json!([]),
        };
        tx.send(AgentCmd::Prompt { msg, force, reply: Some(reply) }).unwrap();
        rx.blocking_recv().unwrap().unwrap()
    }

    fn prompt(tx: &tokio::sync::mpsc::UnboundedSender<AgentCmd>, text: &str) -> Delivery {
        prompt_with(tx, text, None)
    }

    /// What the fake agent said it got, in order (`prompt:x`, `steer:x`, `interject:x`).
    fn heard(evs: &[CodeEvent]) -> Vec<String> {
        evs.iter()
            .filter_map(|e| match e {
                CodeEvent::Update { update } => update["content"]["text"].as_str().map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    /// Once the agent has said `n` things: its text may trail the turn's end.
    fn agent_heard(events: &Events, n: usize) -> Vec<String> {
        wait_for(events, "agent text", |evs| heard(evs).len() >= n);
        heard(&events.lock().unwrap())
    }

    /// How each prompt went, as the transcript heard it: (id, delivery).
    fn deliveries(events: &Events) -> Vec<(String, Delivery)> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                CodeEvent::UserPrompt { prompt_id, delivery, .. } => Some((prompt_id.clone(), *delivery)),
                _ => None,
            })
            .collect()
    }

    fn steering_advertised(events: &Events) -> bool {
        events.lock().unwrap().iter().any(|e| matches!(e, CodeEvent::SessionReady { extensions, .. } if extensions.steering))
    }

    fn ready(events: &Events) {
        wait_for(events, "sessionReady", |evs| evs.iter().any(|e| matches!(e, CodeEvent::SessionReady { .. })));
    }

    fn no_errors(events: &Events) {
        let evs = events.lock().unwrap();
        assert!(!evs.iter().any(|e| matches!(e, CodeEvent::Error { .. })), "{evs:?}");
    }

    #[test]
    fn steering_joins_the_running_turn() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods", "--steering"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert!(steering_advertised(&events));

        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        assert_eq!(prompt(&tx, "also this"), Delivery::Steered);
        assert_eq!(prompt(&tx, "release"), Delivery::Steered);
        wait_for(&events, "promptDone", |evs| evs.iter().any(prompt_done));
        // Idle again: the next one is a turn of its own.
        assert_eq!(prompt(&tx, "after"), Delivery::Sent);
        wait_for(&events, "second promptDone", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);

        assert_eq!(agent_heard(&events, 4), ["prompt:hold", "steer:also this", "steer:release", "prompt:after"]);
        assert_eq!(
            deliveries(&events),
            [
                ("hold".into(), Delivery::Sent),
                ("also this".into(), Delivery::Steered),
                ("release".into(), Delivery::Steered),
                ("after".into(), Delivery::Sent),
            ]
        );
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_steer_waits_for_the_next_turn() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods", "--steering"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "brief"), Delivery::Sent);
        assert_eq!(prompt(&tx, "refuse"), Delivery::Queued);
        wait_for(&events, "two turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);
        assert_eq!(agent_heard(&events, 2), ["prompt:brief", "prompt:refuse"]);
        assert_eq!(
            deliveries(&events),
            [
                ("brief".into(), Delivery::Sent),
                ("refuse".into(), Delivery::Steered),
                ("refuse".into(), Delivery::Queued),
                ("refuse".into(), Delivery::Sent),
            ]
        );
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prompts_mid_turn_wait_without_steering() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert!(!steering_advertised(&events));

        assert_eq!(prompt(&tx, "brief"), Delivery::Sent);
        assert_eq!(prompt(&tx, "second"), Delivery::Queued);
        assert_eq!(prompt(&tx, "third"), Delivery::Queued);
        tx.send(AgentCmd::Unqueue { prompt_id: "third".into() }).unwrap();
        // The queued one goes out once the running turn ends, as a turn of its own.
        wait_for(&events, "two turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);

        assert_eq!(agent_heard(&events, 2), ["prompt:brief", "prompt:second"]);
        assert_eq!(
            deliveries(&events),
            [
                ("brief".into(), Delivery::Sent),
                ("second".into(), Delivery::Queued),
                ("third".into(), Delivery::Queued),
                ("second".into(), Delivery::Sent),
            ]
        );
        let unqueued = |e: &CodeEvent| matches!(e, CodeEvent::Unqueued { prompt_ids, restore: false } if prompt_ids == &["third"]);
        assert_eq!(count(&events, unqueued), 1);
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stopping_hands_back_the_queue() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        assert_eq!(prompt(&tx, "next"), Delivery::Queued);
        assert_eq!(prompt(&tx, "and next"), Delivery::Queued);
        tx.send(AgentCmd::Cancel).unwrap();
        wait_for(&events, "promptDone", |evs| evs.iter().any(prompt_done));
        // Nothing follows a stopped turn; the queued text goes back to whoever wrote it.
        assert_eq!(prompt(&tx, "fresh"), Delivery::Sent);
        wait_for(&events, "second promptDone", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);

        assert_eq!(agent_heard(&events, 2), ["prompt:hold", "prompt:fresh"]);
        let handed_back =
            |e: &CodeEvent| matches!(e, CodeEvent::Unqueued { prompt_ids, restore: true } if prompt_ids == &["next", "and next"]);
        assert_eq!(count(&events, handed_back), 1);
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn stop_reasons(events: &Events) -> Vec<serde_json::Value> {
        let evs = events.lock().unwrap();
        evs.iter()
            .filter_map(|e| match e {
                CodeEvent::PromptDone { stop_reason } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn force_send_cancels_the_turn_instead_of_steering() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods", "--steering"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        // Idle, a forced send is just a send.
        assert_eq!(force(&tx, "hold"), Delivery::Sent);
        assert_eq!(prompt(&tx, "also this"), Delivery::Steered);
        assert_eq!(force(&tx, "urgent"), Delivery::Queued);
        wait_for(&events, "two turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);

        // The cancelled turn's output stays; the forced prompt is the next turn.
        assert_eq!(agent_heard(&events, 3), ["prompt:hold", "steer:also this", "prompt:urgent"]);
        assert_eq!(stop_reasons(&events), [json!("cancelled"), json!("end_turn")]);
        assert_eq!(
            deliveries(&events),
            [
                ("hold".into(), Delivery::Sent),
                ("also this".into(), Delivery::Steered),
                ("urgent".into(), Delivery::Queued),
                ("urgent".into(), Delivery::Sent),
            ]
        );
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Claude Code and Patchbay's agent move a running shell command to the background on a steer,
    /// where `session/cancel` kills it: a forced send steers there and the turn is not cancelled.
    #[test]
    fn force_send_steers_where_that_backgrounds_tools() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let ext = crate::code::agents::Extensions { force_steers: true, ..Default::default() };
        let Some((events, tx, thread)) = spawn_fake_agent_ext(&dir, &["--no-methods", "--steering"], "", ext) else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        assert_eq!(force(&tx, "urgent"), Delivery::Steered);
        assert_eq!(prompt(&tx, "release"), Delivery::Steered);
        wait_for(&events, "promptDone", |evs| evs.iter().any(prompt_done));

        assert_eq!(agent_heard(&events, 3), ["prompt:hold", "steer:urgent", "steer:release"]);
        assert_eq!(stop_reasons(&events), [json!("end_turn")]);
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn send_now_jumps_the_queue() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        assert_eq!(prompt(&tx, "first"), Delivery::Queued);
        assert_eq!(prompt(&tx, "second"), Delivery::Queued);
        tx.send(AgentCmd::SendNow { prompt_id: "second".into() }).unwrap();
        // The rest of the queue still follows, after the one sent now.
        wait_for(&events, "three turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 3);

        assert_eq!(agent_heard(&events, 3), ["prompt:hold", "prompt:second", "prompt:first"]);
        assert_eq!(stop_reasons(&events), [json!("cancelled"), json!("end_turn"), json!("end_turn")]);
        assert_eq!(count(&events, |e| matches!(e, CodeEvent::Unqueued { .. })), 0);
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The agent takes a steer, then a forced send cancels that turn before the agent's answer
    /// comes back: the steered message went down with the turn, so it goes again.
    #[test]
    fn a_steer_cut_off_by_a_forced_send_goes_again() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods", "--steering"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        // Answered 0.3s late, by when `brief` (0.5s) is running.
        let (reply, slow) = tokio::sync::oneshot::channel();
        let msg = UserMessage {
            id: "slow".into(),
            blocks: vec![json!({ "type": "text", "text": "slow" })],
            text: "slow".into(),
            origin: "me".into(),
            attachments: json!([]),
        };
        tx.send(AgentCmd::Prompt { msg, force: false, reply: Some(reply) }).unwrap();
        assert_eq!(force(&tx, "brief"), Delivery::Queued);
        assert_eq!(slow.blocking_recv().unwrap().unwrap(), Delivery::Queued);
        wait_for(&events, "three turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 3);

        assert_eq!(agent_heard(&events, 4), ["prompt:hold", "steer:slow", "prompt:brief", "prompt:slow"]);
        assert_eq!(stop_reasons(&events), [json!("cancelled"), json!("end_turn"), json!("end_turn")]);
        assert_eq!(
            deliveries(&events),
            [
                ("hold".into(), Delivery::Sent),
                ("slow".into(), Delivery::Steered),
                ("brief".into(), Delivery::Queued),
                ("brief".into(), Delivery::Sent),
                ("slow".into(), Delivery::Queued),
                ("slow".into(), Delivery::Sent),
            ]
        );
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// "Send now" on a message steered into the running turn: the turn is cancelled and the
    /// message goes next, as a prompt of its own.
    #[test]
    fn send_now_takes_a_steered_message_out_of_the_turn() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods", "--steering"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        assert_eq!(prompt(&tx, "also this"), Delivery::Steered);
        tx.send(AgentCmd::SendNow { prompt_id: "also this".into() }).unwrap();
        wait_for(&events, "two turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);

        assert_eq!(agent_heard(&events, 3), ["prompt:hold", "steer:also this", "prompt:also this"]);
        assert_eq!(stop_reasons(&events), [json!("cancelled"), json!("end_turn")]);
        assert_eq!(
            deliveries(&events),
            [
                ("hold".into(), Delivery::Sent),
                ("also this".into(), Delivery::Steered),
                ("also this".into(), Delivery::Queued),
                ("also this".into(), Delivery::Sent),
            ]
        );
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// While a forced send's cancel is in flight, the turn is still running but on its way out:
    /// a plain prompt waits behind the forced one instead of steering into it.
    #[test]
    fn a_prompt_during_a_forced_cancel_waits() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let flags = ["--no-methods", "--steering", "--slow-cancel"];
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &flags, "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        assert_eq!(force(&tx, "urgent"), Delivery::Queued);
        assert_eq!(prompt(&tx, "later"), Delivery::Queued);
        wait_for(&events, "three turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 3);

        assert_eq!(agent_heard(&events, 4), ["prompt:hold", "cancel", "prompt:urgent", "prompt:later"]);
        assert_eq!(stop_reasons(&events), [json!("cancelled"), json!("end_turn"), json!("end_turn")]);
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Forced sends and "Send now" go in the order they were made, ahead of the plain queue, and
    /// the turn gets one cancel however many pile up behind it.
    #[test]
    fn forced_sends_keep_their_order() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods", "--slow-cancel"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        assert_eq!(prompt(&tx, "normal"), Delivery::Queued);
        assert_eq!(force(&tx, "first"), Delivery::Queued);
        assert_eq!(prompt(&tx, "third"), Delivery::Queued);
        tx.send(AgentCmd::SendNow { prompt_id: "third".into() }).unwrap();
        assert_eq!(force(&tx, "second"), Delivery::Queued);
        wait_for(&events, "five turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 5);

        assert_eq!(
            agent_heard(&events, 6),
            ["prompt:hold", "cancel", "prompt:first", "prompt:third", "prompt:second", "prompt:normal"]
        );
        assert_eq!(stop_reasons(&events)[0], json!("cancelled"));
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An agent slow to stop a cancelled turn gets a notice, and the forced message still goes
    /// once the turn ends.
    #[test]
    fn a_slow_cancel_gets_a_notice_and_the_forced_send_still_goes() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods", "--late-cancel"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert_eq!(prompt(&tx, "hold"), Delivery::Sent);
        assert_eq!(force(&tx, "urgent"), Delivery::Queued);
        wait_for(&events, "two turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);

        let notices: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                CodeEvent::Notice { message } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("hasn't stopped its turn"));
        assert_eq!(agent_heard(&events, 3), ["prompt:hold", "cancel", "prompt:urgent"]);
        assert_eq!(stop_reasons(&events), [json!("cancelled"), json!("end_turn")]);
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn grok_interjects_text_and_queues_files() {
        let dir = std::env::temp_dir().join(format!("pb-steer-{}", crate::db::new_id()));
        let ext = crate::code::agents::Extensions { interject: true, ..Default::default() };
        let Some((events, tx, thread)) = spawn_fake_agent_ext(&dir, &["--no-methods", "--interject"], "", ext) else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        assert!(steering_advertised(&events));

        assert_eq!(prompt(&tx, "brief"), Delivery::Sent);
        assert_eq!(prompt(&tx, "look left"), Delivery::Steered);
        let with_file = vec![
            json!({ "type": "resource_link", "name": "a.rs", "uri": "file:///a.rs" }),
            json!({ "type": "text", "text": "see this" }),
        ];
        assert_eq!(prompt_with(&tx, "see this", Some(with_file)), Delivery::Queued);
        wait_for(&events, "two turns", |evs| evs.iter().filter(|e| prompt_done(e)).count() == 2);
        assert_eq!(agent_heard(&events, 3), ["prompt:brief", "interject:look left", "prompt:see this"]);
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn subagents_advertised_and_kept_out_of_the_transcript() {
        let dir = std::env::temp_dir().join(format!("pb-subagent-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        send_prompt(&tx, "subagent");
        wait_for(&events, "promptDone", |evs| evs.iter().any(prompt_done));
        assert_eq!(agent_heard(&events, 2), ["prompt:subagent", "subagents:True"]);
        no_errors(&events);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn undecodable_frames_reach_the_ui() {
        let dir = std::env::temp_dir().join(format!("pb-odd-{}", crate::db::new_id()));
        let Some((events, tx, thread)) = spawn_fake_agent_with(&dir, &["--no-methods"], "") else {
            eprintln!("skipped: no python");
            return;
        };
        ready(&events);
        send_prompt(&tx, "odd");
        wait_for(&events, "promptDone", |evs| evs.iter().any(prompt_done));
        wait_for(&events, "both permissions", |evs| {
            let n = |f: fn(&CodeEvent) -> bool| evs.iter().filter(|e| f(e)).count();
            n(|e| matches!(e, CodeEvent::Permission { .. })) == 1 && n(|e| matches!(e, CodeEvent::Unrecognized { .. })) == 3
        });
        let evs = events.lock().unwrap();
        let unknown: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                CodeEvent::Unrecognized { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(unknown[0].contains("\"brand_new\""), "{unknown:?}");
        assert!(unknown[1].contains("without a session id"), "{unknown:?}");
        assert!(unknown[2].contains("permission"), "{unknown:?}");
        // Unknown values the UI can show go through as they are.
        assert!(evs.iter().any(|e| matches!(e, CodeEvent::Update { update } if update["status"] == "cancelled")));
        assert!(evs.iter().any(
            |e| matches!(e, CodeEvent::Permission { request, .. } if request["options"][0]["kind"] == "allow_for_session")
        ));
        drop(evs);
        tx.send(AgentCmd::Shutdown).unwrap();
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn steering_needs_advertising() {
        assert_eq!(Steer::of(&json!({ "_meta": { "steering": { "supported": true } } }), false), Some(Steer::Acp));
        // An agent that answers any `_` method (Antigravity) but advertises nothing doesn't steer.
        assert_eq!(Steer::of(&json!({ "_meta": {} }), false), None);
        assert_eq!(Steer::of(&json!({ "_meta": { "steering": { "supported": "yes" } } }), false), None);
        assert_eq!(Steer::of(&json!({}), true), Some(Steer::Interject));
    }

    #[test]
    fn steering_requests_and_answers() {
        let blocks = [json!({ "type": "text", "text": "a" }), json!({ "type": "image", "data": "x", "mimeType": "image/png" })];
        let (method, params) = Steer::Acp.request("s", &blocks).unwrap();
        assert_eq!(method, "session/steering");
        assert_eq!(params["prompt"].as_array().unwrap().len(), 2);
        assert_eq!(params["_meta"]["steering"]["idleBehavior"], "promptRequired");

        let (method, params) = Steer::Interject.request("s", &blocks).unwrap();
        assert_eq!(method, "x.ai/interject");
        assert_eq!(params["text"], "a");
        assert_eq!(params["content"][1]["type"], "image");
        let note_and_text = [json!({ "type": "text", "text": "note" }), json!({ "type": "text", "text": "ask" })];
        assert_eq!(Steer::Interject.request("s", &note_and_text).unwrap().1["text"], "note\n\nask");
        assert!(Steer::Interject.request("s", &[json!({ "type": "resource", "resource": {} })]).is_none());

        assert!(matches!(steer_outcome(&json!({ "outcome": "injected" })), Steered::Yes));
        assert!(matches!(steer_outcome(&json!({ "outcome": "startedNewTurn" })), Steered::Yes));
        assert!(matches!(steer_outcome(&json!({ "status": "queued" })), Steered::Yes));
        assert!(matches!(steer_outcome(&json!({ "outcome": "promptRequired" })), Steered::HandedBack));
        assert!(matches!(steer_outcome(&json!({ "outcome": "failed" })), Steered::No));
        assert!(matches!(steer_outcome(&json!(null)), Steered::No));
    }
}
