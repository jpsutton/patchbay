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
    /// The agent wants the user to sign in (ACP `auth_required`): before it starts a session, or
    /// mid-session when a prompt is refused. It waits for `AgentCmd::Authenticate` with one of
    /// `methods`, or a restart after a terminal sign-in.
    AuthRequired {
        message: String,
        methods: Vec<AuthChoice>,
    },
    /// A sign-in asked for mid-session worked; the session goes on, and the user resends.
    AuthResolved,
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
    // `auth.terminal` (ACP, unstable) and `_meta["terminal-auth"]` (the older convention): the
    // agent may offer sign-in methods that run its login command in a terminal, which Patchbay
    // opens in its terminal pane.
    let mut client_caps = json!({
        "fs": { "readTextFile": false, "writeTextFile": false },
        "terminal": false,
        "auth": { "terminal": true },
        "_meta": { "terminal_output": true, "terminal-auth": true },
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
                        let (init_v, methods, auth_lost) = (init_v.clone(), auth_methods.clone(), auth_lost.clone());
                        tokio::task::spawn_local(async move {
                            match conn.prompt(req).await {
                                Ok(resp) => {
                                    let v = serde_json::to_value(&resp).unwrap_or_default();
                                    if v["usage"].is_object() {
                                        emit(&sink, &csid, CodeEvent::TurnUsage { usage: v["usage"].clone() });
                                    }
                                    emit(&sink, &csid, CodeEvent::PromptDone { stop_reason: v["stopReason"].clone() });
                                }
                                // Signed out mid-session (a logout, an expired token): offer a
                                // sign-in when there is one Patchbay can run, else say how.
                                Err(e) if is_auth_required(&e) => {
                                    let message = auth_message(&e, &init_v, login_hint, true);
                                    if methods.is_empty() {
                                        emit(&sink, &csid, CodeEvent::Error { message });
                                    } else {
                                        auth_lost.set(true);
                                        emit(&sink, &csid, CodeEvent::AuthRequired { message, methods });
                                    }
                                    emit(&sink, &csid, CodeEvent::PromptDone { stop_reason: json!("error") });
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
    child: &mut tokio::process::Child,
) {
    io_task.abort();
    drop(conn);
    if tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// Wait, after `CodeEvent::AuthRequired`, for a sign-in the agent accepts. Returns its reply
/// channel, to answer once the session is up, or `None` when Patchbay stops the agent instead.
/// Other commands are refused until then.
async fn wait_for_sign_in(
    conn: &acp::ClientSideConnection,
    rx: &mut mpsc::UnboundedReceiver<AgentCmd>,
    child: &mut tokio::process::Child,
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
        AgentCmd::Prompt { .. } | AgentCmd::Cancel | AgentCmd::Shutdown => {}
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
/// which agents read in two shapes: Gemini CLI a string, codex-acp `{ "apiKey": … }`. Neither
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

fn exited(
    status: std::io::Result<std::process::ExitStatus>,
    stderr_tail: &Rc<RefCell<Vec<String>>>,
) -> anyhow::Error {
    let tail = stderr_tail.borrow().join("\n");
    let code = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
    anyhow!("agent process exited ({code})\n{tail}")
}

fn is_auth_required(e: &acp::Error) -> bool {
    i32::from(e.code) == -32000
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
    if how.is_empty() {
        format!("{name} needs you to sign in {when} ({e}).")
    } else {
        format!("{name} needs you to sign in {when}. {how}")
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
        AgentCmd, AuthChoice, CodeEvent, ConfigIds, Envelope, RewindPoint, SessionConfig, SpawnSpec, auth_choices,
        auth_metas, merge_config, pick_rewind_point, terminal_auth_command,
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
        // Methods as Gemini CLI and codex-acp advertise them, plus kinds Patchbay can't run.
        let init = json!({ "authMethods": [
            { "id": "oauth-personal", "name": "Log in with Google", "description": "Log in with your Google account" },
            { "id": "gemini-api-key", "name": "Gemini API key", "_meta": { "api-key": { "provider": "google" } } },
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
                choice("gemini-api-key", "Gemini API key", None, true, false),
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
    /// next sign-in. To clients that run terminal sign-ins it also offers `tui`, whose command
    /// (this script with `--login`) leaves a `signed-in` file that a new process starts with.
    /// `--no-methods` starts it signed in, with no auth methods.
    const FAKE_AGENT: &str = r#"
import json, os, sys
if "--login" in sys.argv:
    if os.environ.get("FAKE_LOGIN") != "1":
        sys.exit(3)
    open("signed-in", "w").close()
    sys.exit(0)
no_methods = "--no-methods" in sys.argv
authed = no_methods or os.path.exists("signed-in")
def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
def err(i, msg, data=None):
    e = {"code": -32000, "message": msg}
    if data: e["data"] = {"message": data}
    send({"jsonrpc": "2.0", "id": i, "error": e})
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m or "method" not in m:
        continue
    i, meth, p = m["id"], m["method"], m.get("params") or {}
    if meth == "initialize":
        methods = [{"id": "browser", "name": "Log in", "description": "Opens a browser"},
                   {"id": "api-key", "name": "API key", "_meta": {"api-key": {"provider": "x"}}}]
        if ((p.get("clientCapabilities") or {}).get("auth") or {}).get("terminal"):
            methods.append({"id": "tui", "name": "Terminal", "type": "terminal", "args": ["--login"],
                            "env": {"FAKE_LOGIN": "1"}})
        send({"jsonrpc": "2.0", "id": i, "result": {"protocolVersion": 1, "agentCapabilities": {},
              "agentInfo": {"name": "fake", "title": "Fake Agent", "version": "1"},
              "authMethods": [] if no_methods else methods}})
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
        if p["prompt"][0].get("text") == "expire":
            authed = False
        if authed:
            send({"jsonrpc": "2.0", "id": i, "result": {"stopReason": "end_turn"}})
        else:
            err(i, "Authentication required")
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
            launch: crate::code::agents::Launch {
                program: python.into(),
                args: std::iter::once(script.to_string_lossy().into_owned())
                    .chain(flags.iter().map(|f| f.to_string()))
                    .collect(),
                env: Vec::new(),
                env_remove: Vec::new(),
                ext: Default::default(),
                session_meta: json!({}),
                initial_model: None,
                login_hint,
                approvals: Default::default(),
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
        tx.send(AgentCmd::Prompt { blocks: vec![json!({ "type": "text", "text": text })] }).unwrap();
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
        assert!(matches!(events.lock().unwrap().last(), Some(CodeEvent::PromptDone { stop_reason }) if stop_reason == "end_turn"));
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
}
