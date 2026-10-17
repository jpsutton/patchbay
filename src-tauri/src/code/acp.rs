//! ACP client side: drives one Grok Build `agent stdio` child per Code session.
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
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub grok_home: PathBuf,
    pub env: Vec<(String, String)>,
    pub model_key: Option<String>,
    /// Existing ACP session to `session/load`; `None` creates a new one.
    pub resume: Option<String>,
    /// Extra instructions appended to the agent's system prompt (`_meta.rules`).
    pub rules: Option<String>,
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
}

impl UiClient {
    fn merged_tool_call(&self, tc: &Value) -> Value {
        let id = tc["toolCallId"].as_str().unwrap_or_default();
        let mut out = self.tool_calls.borrow().get(id).cloned().unwrap_or_else(|| json!({}));
        merge_fields(&mut out, tc);
        out
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
        emit(&self.sink, &self.code_session_id, CodeEvent::Update { update });
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
            let result = local.block_on(&rt, run(spec, rx));
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

async fn run(spec: SpawnSpec, mut rx: mpsc::UnboundedReceiver<AgentCmd>) -> Result<()> {
    let SpawnSpec {
        sink,
        code_session_id: csid,
        binary,
        cwd,
        grok_home,
        env,
        model_key,
        resume,
        rules,
        pending,
        on_session,
        auto_approve,
        mcp,
    } = spec;
    let session_meta = match rules.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => json!({ "rules": r }),
        None => json!({}),
    };
    emit(&sink, &csid, CodeEvent::Status { state: "starting".into(), message: None });

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.arg("agent").arg("--no-leader");
    if let Some(m) = &model_key {
        cmd.arg("-m").arg(m);
    }
    cmd.arg("stdio")
        .current_dir(&cwd)
        .env("GROK_HOME", &grok_home)
        .env("GROK_TELEMETRY_ENABLED", "false")
        .env("GROK_TELEMETRY_MIXPANEL_ENABLED", "false")
        .env("GROK_TELEMETRY_TRACE_UPLOAD", "false")
        .env("GROK_DISABLE_AUTOUPDATER", "1")
        .env_remove("XAI_API_KEY")
        .env_remove("GROK_CODE_XAI_API_KEY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("start agent {}", binary.display()))?;
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
                tracing::debug!(target: "grok_sidecar", "{line}");
                let mut t = tail.borrow_mut();
                t.push(line);
                if t.len() > 40 {
                    t.remove(0);
                }
            }
        });
    }

    let client = UiClient {
        sink: sink.clone(),
        code_session_id: csid.clone(),
        pending: pending.clone(),
        auto_approve,
        tool_calls: RefCell::default(),
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

    let init: acp::InitializeRequest = from_json(json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
            // Ask for `session_status` updates (context fill, session token totals).
            "_meta": { "x.ai/statusLine": true }
        },
        "clientInfo": { "name": "patchbay", "version": env!("CARGO_PKG_VERSION") }
    }))?;
    let init_resp = conn
        .initialize(init)
        .await
        .map_err(|e| anyhow!("initialize failed: {e}"))?;
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
    let (session_id, models, modes, loaded) = match resume.filter(|_| can_load) {
        Some(sid) => {
            let req: acp::LoadSessionRequest = from_json(
                json!({ "sessionId": sid, "cwd": cwd_str, "mcpServers": mcp_servers, "_meta": session_meta }),
            )?;
            match conn.load_session(req).await {
                Ok(resp) => {
                    let v = serde_json::to_value(&resp).unwrap_or_default();
                    (sid, v["models"].clone(), v["modes"].clone(), true)
                }
                Err(e) => {
                    emit(
                        &sink,
                        &csid,
                        CodeEvent::Error {
                            message: format!("could not resume previous session ({e}); started a new one"),
                        },
                    );
                    new_session(&conn, &cwd_str, &mcp_servers, &session_meta).await?
                }
            }
        }
        None => new_session(&conn, &cwd_str, &mcp_servers, &session_meta).await?,
    };
    on_session(session_id.clone());
    emit(
        &sink,
        &csid,
        CodeEvent::SessionReady {
            acp_session_id: session_id.clone(),
            models,
            modes,
            loaded,
            capabilities,
            agent_info: init_v["agentInfo"].clone(),
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

    loop {
        tokio::select! {
            status = child.wait() => {
                let tail = stderr_tail.borrow().join("\n");
                let code = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                return Err(anyhow!("agent process exited ({code})\n{tail}"));
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
                                Ok(resp) => emit(&sink, &csid, CodeEvent::PromptDone {
                                    stop_reason: serde_json::to_value(resp.stop_reason).unwrap_or_default(),
                                }),
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
                        let r = match from_json::<acp::SetSessionModelRequest>(
                            json!({ "sessionId": session_id, "modelId": model_id }),
                        ) {
                            Ok(req) => conn.set_session_model(req).await.map(|_| ()).map_err(|e| anyhow!("{e}")),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(r);
                    }
                    AgentCmd::SetMode { mode_id, reply } => {
                        let r = match from_json::<acp::SetSessionModeRequest>(
                            json!({ "sessionId": session_id, "modeId": mode_id }),
                        ) {
                            Ok(req) => conn.set_session_mode(req).await.map(|_| ()).map_err(|e| anyhow!("{e}")),
                            Err(e) => Err(e),
                        };
                        let _ = reply.send(r);
                    }
                    AgentCmd::Rewind { ordinal, preview, revert_files, reply } => {
                        let r = rewind(&conn, &session_id, ordinal, &preview, revert_files).await;
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
) -> Result<(String, Value, Value, bool)> {
    let req: acp::NewSessionRequest =
        from_json(json!({ "cwd": cwd, "mcpServers": mcp_servers, "_meta": meta }))?;
    let resp = conn
        .new_session(req)
        .await
        .map_err(|e| anyhow!("session/new failed: {e}"))?;
    let v = serde_json::to_value(&resp).unwrap_or_default();
    Ok((
        resp.session_id.to_string(),
        v["models"].clone(),
        v["modes"].clone(),
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::{RewindPoint, pick_rewind_point};

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
