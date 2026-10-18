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
    /// Cumulative token usage and context fill for the session.
    Usage {
        usage: crate::usage::UsageView,
    },
    /// A prompt was sent to the agent (from `origin`, a mesh node), so every viewer shows it.
    #[serde(rename_all = "camelCase")]
    UserPrompt {
        text: String,
        origin: String,
        attachments: Value,
    },
    /// The session was renamed automatically (see `code::titles`).
    Title {
        title: String,
    },
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

pub enum AgentCmd {
    Prompt { blocks: Vec<Value> },
    Cancel,
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
    Shutdown,
}

/// Decides whether a permission request is pre-approved: (merged tool call, options) ->
/// (option id to select, description for the UI).
pub type AutoApprove = Arc<dyn Fn(&Value, &Value) -> Option<(String, String)> + Send + Sync>;

/// Builds the ACP `mcpServers` list once the agent's capabilities are known; also returns
/// names of servers left out because the agent doesn't support their transport.
pub type McpProvider = Box<dyn FnOnce(&Value) -> (Vec<Value>, Vec<String>) + Send>;

pub struct SpawnSpec {
    pub sink: Sink,
    pub code_session_id: String,
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
    fn merged_tool_call(&self, tc: &Value) -> Value {
        let id = tc["toolCallId"].as_str().unwrap_or_default();
        let mut out = self.tool_calls.borrow().get(id).cloned().unwrap_or_else(|| json!({}));
        merge_fields(&mut out, tc);
        out
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

fn merge_fields(into: &mut Value, from: &Value) {
    if let (Some(dst), Some(src)) = (into.as_object_mut(), from.as_object()) {
        for (k, v) in src {
            if !v.is_null() {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for UiClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let raw = serde_json::to_value(&args).unwrap_or(Value::Null);
        let tool_call = self.merged_tool_call(&raw["toolCall"]);
        if let Some((option_id, description)) = (self.auto_approve)(&tool_call, &raw["options"]) {
            emit(
                &self.sink,
                &self.code_session_id,
                CodeEvent::AutoApproved {
                    tool_call_id: tool_call["toolCallId"].as_str().unwrap_or_default().to_owned(),
                    description,
                },
            );
            return serde_json::from_value(json!({ "outcome": { "outcome": "selected", "optionId": option_id } }))
                .map_err(|_| acp::Error::internal_error());
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
                request: serde_json::to_value(&args).unwrap_or(Value::Null),
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
        let resp: acp::RequestPermissionResponse =
            serde_json::from_value(json!({ "outcome": outcome }))
                .map_err(|_| acp::Error::internal_error())?;
        Ok(resp)
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let update = serde_json::to_value(&args.update).unwrap_or(Value::Null);
        if matches!(update["sessionUpdate"].as_str(), Some("tool_call" | "tool_call_update"))
            && let Some(id) = update["toolCallId"].as_str()
        {
            let mut map = self.tool_calls.borrow_mut();
            let entry = map.entry(id.to_owned()).or_insert_with(|| json!({}));
            merge_fields(entry, &update);
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
        Ok(())
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        // `x.ai/session_notification` carries Grok Build's own session updates.
        if args.method.trim_start_matches('_') == "x.ai/session_notification"
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
        ext: extensions,
        session_meta,
        initial_model,
        login_hint,
        approvals,
    } = launch;
    emit(&sink, &csid, CodeEvent::Status { state: "starting".into(), message: None });

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

    // Keep the last stderr lines to explain crashes.
    let stderr_tail: Rc<RefCell<Vec<String>>> = Rc::default();
    {
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
    }

    let config_ids: Rc<RefCell<ConfigIds>> = Rc::default();
    let client = UiClient {
        sink: sink.clone(),
        code_session_id: csid.clone(),
        pending: pending.clone(),
        auto_approve,
        tool_calls: RefCell::default(),
        config_ids: config_ids.clone(),
        approvals: approvals.clone(),
        cmds,
    };
    let (conn, io) = acp::ClientSideConnection::new(
        client,
        stdin.compat_write(),
        stdout.compat(),
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );
    let conn = Rc::new(conn);
    let io_task = tokio::task::spawn_local(io);

    // `terminal_output` (the convention Zed uses): command output comes as `_meta.terminal_output`
    // and `terminal_exit` on the tool call. An agent that backgrounds a command sends its
    // `terminal_exit` only when the command ends, which is how the UI knows it still runs.
    let mut client_caps = json!({
        "fs": { "readTextFile": false, "writeTextFile": false },
        "terminal": false,
        "_meta": { "terminal_output": true },
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
    let init_v = serde_json::to_value(&init_resp).unwrap_or_default();
    // Grok advertises its builtin slash commands here; other agents only send updates.
    let init_commands = init_v["_meta"]["availableCommands"].clone();
    let capabilities = init_v["agentCapabilities"].clone();
    let can_load = capabilities["loadSession"].as_bool().unwrap_or(false);
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
    let auth_hint = |e: &acp::Error| auth_message(e, &init_v, login_hint);
    let (session_id, resp, loaded) = match resume.filter(|_| can_load) {
        Some(sid) => {
            let req: acp::LoadSessionRequest = from_json(
                json!({ "sessionId": sid, "cwd": cwd_str, "mcpServers": mcp_servers, "_meta": session_meta }),
            )?;
            match conn.load_session(req).await {
                Ok(resp) => (sid, serde_json::to_value(&resp).unwrap_or_default(), true),
                Err(e) => {
                    if let Some(msg) = auth_hint(&e) {
                        return Err(anyhow!(msg));
                    }
                    // Not an error: some agents (Cursor, Claude Code) can't load a session that
                    // never got a prompt, and the transcript cache still shows earlier turns.
                    emit(
                        &sink,
                        &csid,
                        CodeEvent::Notice {
                            message: format!("Couldn't resume the agent's previous session, so it started a new one ({e})."),
                        },
                    );
                    new_session(&conn, &cwd_str, &mcp_servers, &session_meta, &init_v, login_hint).await?
                }
            }
        }
        None => new_session(&conn, &cwd_str, &mcp_servers, &session_meta, &init_v, login_hint).await?,
    };
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

    loop {
        tokio::select! {
            status = child.wait() => {
                return Err(exited(status, &stderr_tail));
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    AgentCmd::Prompt { blocks } => {
                        let req: acp::PromptRequest =
                            match from_json(json!({ "sessionId": session_id, "prompt": blocks })) {
                                Ok(r) => r,
                                Err(e) => {
                                    emit(&sink, &csid, CodeEvent::Error { message: format!("{e:#}") });
                                    continue;
                                }
                            };
                        let conn = conn.clone();
                        let sink = sink.clone();
                        let csid = csid.clone();
                        tokio::task::spawn_local(async move {
                            match conn.prompt(req).await {
                                Ok(resp) => {
                                    let v = serde_json::to_value(&resp).unwrap_or_default();
                                    if v["usage"].is_object() {
                                        emit(&sink, &csid, CodeEvent::TurnUsage { usage: v["usage"].clone() });
                                    }
                                    emit(&sink, &csid, CodeEvent::PromptDone { stop_reason: v["stopReason"].clone() });
                                }
                                Err(e) => {
                                    emit(&sink, &csid, CodeEvent::Error { message: e.to_string() });
                                    emit(&sink, &csid, CodeEvent::PromptDone { stop_reason: json!("error") });
                                }
                            }
                        });
                    }
                    AgentCmd::Cancel => {
                        cancel_pending(&pending);
                        if let Ok(n) = from_json::<acp::CancelNotification>(json!({ "sessionId": session_id })) {
                            let _ = conn.cancel(n).await;
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
                            rewind(&conn, &session_id, ordinal, &preview, revert_files).await
                        } else {
                            Err(anyhow!("this agent can't rewind a session"))
                        };
                        let _ = reply.send(r);
                    }
                    AgentCmd::Shutdown => break,
                }
            }
        }
    }
    cancel_pending(&pending);
    // Close stdin (by dropping the connection) so the agent can flush session state, then
    // give it a moment before killing.
    io_task.abort();
    drop(conn);
    if tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    Ok(())
}

fn exited(
    status: std::io::Result<std::process::ExitStatus>,
    stderr_tail: &Rc<RefCell<Vec<String>>>,
) -> anyhow::Error {
    let tail = stderr_tail.borrow().join("\n");
    let code = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
    anyhow!("agent process exited ({code})\n{tail}")
}

/// For ACP's `auth_required` error: how to sign in. Patchbay's own hint for the agent comes
/// first, then whatever auth methods the agent advertises.
fn auth_message(e: &acp::Error, init: &Value, login_hint: &str) -> Option<String> {
    if i32::from(e.code) != -32000 {
        return None;
    }
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
    Some(if how.is_empty() {
        format!("{name} needs you to sign in first ({e}).")
    } else {
        format!("{name} needs you to sign in first. {how}")
    })
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
    session_id: &str,
    ordinal: usize,
    preview: &str,
    revert_files: bool,
) -> Result<()> {
    let points = ext(conn, "x.ai/rewind/points", json!({ "sessionId": session_id })).await?;
    let mut points: Vec<RewindPoint> =
        serde_json::from_value(points["rewind_points"].clone()).unwrap_or_default();
    points.sort_by_key(|p| p.prompt_index);
    let target = pick_rewind_point(&points, ordinal, preview)
        .ok_or_else(|| anyhow!("no rewind point for that message"))?;
    ext(
        conn,
        "x.ai/rewind/execute",
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

async fn new_session(
    conn: &acp::ClientSideConnection,
    cwd: &str,
    mcp_servers: &[Value],
    meta: &Value,
    init: &Value,
    login_hint: &str,
) -> Result<(String, Value, bool)> {
    let req: acp::NewSessionRequest =
        from_json(json!({ "cwd": cwd, "mcpServers": mcp_servers, "_meta": meta }))?;
    let resp = conn.new_session(req).await.map_err(|e| match auth_message(&e, init, login_hint) {
        Some(msg) => anyhow!(msg),
        None => anyhow!("session/new failed: {e}"),
    })?;
    let v = serde_json::to_value(&resp).unwrap_or_default();
    Ok((resp.session_id.to_string(), v, false))
}

#[cfg(test)]
mod tests {
    use super::{ConfigIds, RewindPoint, SessionConfig, merge_config, pick_rewind_point};
    use serde_json::json;

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
}
