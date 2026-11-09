//! Patchbay-owned MCP tool and its authenticated, local bridge into the running app.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
#[cfg(unix)]
use tokio::signal::unix::{signal as unix_signal, SignalKind};
#[cfg(unix)]
use std::time::Duration;


use crate::{AppState, api};

const TOOL_SEND: &str = "send_agent_message";
const TOOL_LIST_SESSIONS: &str = "list_agent_sessions";
const TOOL_LIST_QUEUED: &str = "list_queued_agent_messages";
const TOOL_ACCEPT_QUEUED: &str = "accept_queued_agent_message";
const TOOL_CREATE: &str = "create_agent_session";
/// Share an artifact the agent generated inside its session folder (pb-20261105-18).
const TOOL_SHARE: &str = "share_artifact";
const MAX_LINE: usize = 64 * 1024;

/// Env var carrying the child-side lifecycle log path (pb-20261109-03). Optional:
/// when absent the child behaves exactly as before and writes no log.
const ENV_LOG_PATH: &str = "PATCHBAY_MCP_LOG_PATH";
/// Host agent kind/name (e.g. opencode, claude) for diagnostics. Non-secret.
const ENV_HOST_AGENT: &str = "PATCHBAY_MCP_HOST_AGENT";
/// Max log file size before truncation (best-effort cap). ~512 KiB.
const MCP_LOG_MAX_BYTES: u64 = 512 * 1024;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mcp_log_path_from_env() -> Option<PathBuf> {
    let v = std::env::var_os(ENV_LOG_PATH)?;
    if v.is_empty() {
        return None;
    }
    Some(PathBuf::from(v))
}

/// Best-effort, size-capped append. Never fails the caller; never logs secrets.
/// `line` must already be safe (no token, no message bodies, no stderr content beyond error kind).
fn mcp_log_append(path: &Path, line: &str) {
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Ok(meta) = fs::metadata(path) {
            if meta.len() >= MCP_LOG_MAX_BYTES {
                // Truncate to keep a single size-capped file.
                let _ = fs::write(path, b"# truncated (size cap reached)\n");
            }
        }
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{line}")?;
        Ok(())
    })();
}

fn mcp_log_event(path: Option<&Path>, event: &str, fields: &[(&str, String)]) {
    let Some(path) = path else { return };
    let mut line = format!("ts={} event={event}", now_ms());
    for (k, v) in fields {
        // Values are caller-controlled diagnostics; never include secrets/bodies.
        // Sanitize newlines to keep one event per line.
        let v = v.replace(['\n', '\r'], " ");
        line.push(' ');
        line.push_str(k);
        line.push('=');
        line.push_str(&v);
    }
    mcp_log_append(path, &line);
}

fn mcp_host_agent_from_env() -> String {
    std::env::var(ENV_HOST_AGENT).unwrap_or_else(|_| "unknown".to_owned())
}

fn mcp_session_from_env() -> String {
    std::env::var("PATCHBAY_MESSAGE_SESSION").unwrap_or_else(|_| "unknown".to_owned())
}

fn mcp_process_fields() -> Vec<(&'static str, String)> {
    let ppid = parent_pid();
    vec![
        ("pid", std::process::id().to_string()),
        ("ppid", ppid),
        ("session", mcp_session_from_env()),
        ("host_agent", mcp_host_agent_from_env()),
    ]
}

#[cfg(unix)]
fn parent_pid() -> String {
    std::os::unix::process::parent_id().to_string()
}

#[cfg(not(unix))]
fn parent_pid() -> String {
    "unknown".to_owned()
}

/// Maps an error to a safe, named cause (no message body, no stderr content, no secrets).
/// Only allows known diagnostic kinds / env-var names; collapses anything else.
fn safe_error_cause(err: &anyhow::Error) -> String {
    let msg = format!("{err:#}");
    for known in [
        "stdin_eof",
        "stdin_read_error",
        "stdout_write_error",
        "stdout_flush_error",
        "setup_env_missing:PATCHBAY_MESSAGE_ADDR",
        "setup_env_missing:PATCHBAY_MESSAGE_TOKEN",
        "setup_env_missing:PATCHBAY_MESSAGE_SESSION",
        "mcp child panic",
    ] {
        if msg == known || msg.contains(known) {
            if known == "mcp child panic" {
                return "panic".to_owned();
            }
            return known.to_owned();
        }
    }
    if msg.contains("runtime init") || msg.contains("Tokio runtime") {
        return "runtime_init_error".to_owned();
    }
    "error".to_owned()
}

#[derive(Clone)]
pub struct AgentMessagingBridge {
    addr: SocketAddr,
    tokens: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    /// Optional child lifecycle log path passed to spawned MCP children (pb-20261109-03).
    mcp_log_path: Option<PathBuf>,
}

impl AgentMessagingBridge {
    /// Returns the std listener: converting to tokio needs a running runtime, which `serve` has.
    /// `mcp_log_path`: when set, passed to each MCP child via `PATCHBAY_MCP_LOG_PATH` so the
    /// child can record start/exit evidence. Pass `None` to preserve legacy behavior.
    pub fn bind(mcp_log_path: Option<PathBuf>) -> Result<(Self, StdTcpListener)> {
        let listener = StdTcpListener::bind(("127.0.0.1", 0))
            .context("binding local agent-messaging bridge")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        Ok((
            Self {
                addr,
                tokens: Arc::default(),
                mcp_log_path,
            },
            listener,
        ))
    }

    /// `can_steer`: this session's agent takes mid-turn prompts (ACP steering or Grok interject).
    /// Non-steering agents get tools to list and accept queued peer messages mid-turn.
    pub fn server_definition(
        &self,
        session_id: &str,
        host_agent: &str,
        can_steer: bool,
    ) -> Result<Value> {
        let command = std::env::current_exe()?.to_string_lossy().into_owned();
        let token = hex::encode(rand::random::<[u8; 32]>());
        self.tokens
            .lock()
            .unwrap()
            .insert(session_id.to_owned(), token.clone());
        let mut env = vec![
            json!({ "name": "PATCHBAY_MESSAGE_ADDR", "value": self.addr.to_string() }),
            json!({ "name": "PATCHBAY_MESSAGE_TOKEN", "value": token }),
            json!({ "name": "PATCHBAY_MESSAGE_SESSION", "value": session_id }),
            json!({ "name": "PATCHBAY_MESSAGE_STEERING", "value": if can_steer { "1" } else { "0" } }),
            json!({ "name": ENV_HOST_AGENT, "value": host_agent }),
        ];
        if let Some(path) = &self.mcp_log_path {
            env.push(json!({ "name": ENV_LOG_PATH, "value": path.to_string_lossy().into_owned() }));
        }
        Ok(json!({
            "name": "patchbay-agent-messaging",
            "command": command,
            "args": ["--agent-messaging-mcp"],
            "env": env
        }))
    }

    pub fn serve(&self, listener: StdTcpListener, state: AppState) {
        let tokens = self.tokens.clone();
        api::spawn(async move {
            let listener = match TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("agent-messaging bridge cannot listen: {e}");
                    return;
                }
            };
            let limit = Arc::new(Semaphore::new(16));
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!("agent-messaging bridge accept failed: {e}");
                        continue;
                    }
                };
                let Ok(permit) = limit.clone().acquire_owned().await else {
                    break;
                };
                let state = state.clone();
                let tokens = tokens.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = handle_bridge(stream, &tokens, state).await {
                        tracing::debug!("agent-messaging bridge request failed: {e:#}");
                    }
                });
            }
        });
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeRequest {
    token: String,
    source_session_id: String,
    action: String,
    #[serde(default)]
    target_session_id: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    message_id: Option<String>,
    /// The tool's arguments, for actions that take more than the fields above (`create`).
    #[serde(default)]
    arguments: Option<Value>,
}

async fn handle_bridge(
    stream: TcpStream,
    tokens: &std::sync::Mutex<std::collections::HashMap<String, String>>,
    state: AppState,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read)
        .take(MAX_LINE as u64)
        .read_line(&mut line)
        .await?;
    let result: Result<Value> = async {
        let request: BridgeRequest = serde_json::from_str(&line)?;
        let expected = tokens
            .lock()
            .unwrap()
            .get(&request.source_session_id)
            .cloned();
        if !expected
            .as_deref()
            .is_some_and(|expected| constant_time_eq(request.token.as_bytes(), expected.as_bytes()))
        {
            anyhow::bail!("unauthorized");
        }
        let source = state
            .db
            .get_code_session(&request.source_session_id)?
            .ok_or_else(|| anyhow!("source Code session was not found"))?;
        if source.host_node.as_deref() != Some(state.mesh.node_id())
            || !state.code.is_running(&request.source_session_id)
        {
            anyhow::bail!("the sending agent session is no longer running here");
        }
        match request.action.as_str() {
            "list" => crate::list_agent_sessions(&state, &request.source_session_id),
            "send" => {
                let target = request
                    .target_session_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("targetSessionId is required"))?;
                let message = request
                    .message
                    .as_deref()
                    .ok_or_else(|| anyhow!("message is required"))?;
                check_send_limits(&request.source_session_id, target)?;
                crate::send_agent_message(
                    &state,
                    &state.mesh.node_id().to_owned(),
                    &request.source_session_id,
                    target,
                    message,
                )
                .await
            }
            "create" => {
                let args = request.arguments.as_ref().ok_or_else(|| anyhow!("arguments are required"))?;
                let text = |k: &str| args[k].as_str();
                crate::create_agent_session(
                    &state,
                    &request.source_session_id,
                    text("cwd").ok_or_else(|| anyhow!("cwd is required"))?,
                    text("prompt").ok_or_else(|| anyhow!("prompt is required"))?,
                    text("title"),
                    text("agent"),
                    text("model"),
                )
                .await
            }
            // Agent share tool (pb-20261105-18): confine to the source session's cwd, then reuse
            // the ordinary share registry (token/expiry/revoke) unchanged. `source` was already
            // loaded above; its `cwd` is the folder this agent runs in.
            "share" => {
                let args = request.arguments.as_ref().ok_or_else(|| anyhow!("arguments are required"))?;
                let path = args["path"].as_str().ok_or_else(|| anyhow!("path is required"))?;
                let canonical = confined_share_path(Path::new(&source.cwd), path)?;
                let info = state.shares.create(&canonical.to_string_lossy(), None, None)?;
                Ok(serde_json::to_value(info)?)
            }
            "list_queued" => crate::list_queued_agent_messages(&state, &request.source_session_id).await,
            "accept_queued" => {
                crate::accept_queued_agent_message(
                    &state,
                    &request.source_session_id,
                    request
                        .message_id
                        .as_deref()
                        .ok_or_else(|| anyhow!("messageId is required"))?,
                )
                .await
            }
            other => Err(anyhow!("unknown bridge action {other}")),
        }
    }
    .await;
    let response = match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": format!("{error:#}") }),
    };
    write.write_all(response.to_string().as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.shutdown().await?;
    Ok(())
}

const RATE_MAX: usize = 20;
const RATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);
/// Caps a single conversation pair so overnight ping-pong cannot burn thousands of turns.
const PAIR_MAX: usize = 25;
const PAIR_WINDOW: std::time::Duration = std::time::Duration::from_secs(3600);
/// Identical body from the same source to the same target within this window is not re-prompted.
const DEDUP_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

/// Per-session and per-pair send budgets before the bridge calls into the mesh.
pub(crate) fn check_send_limits(source_session_id: &str, target_session_id: &str) -> Result<()> {
    check_rate(source_session_id)?;
    check_pair_rate(source_session_id, target_session_id)?;
    Ok(())
}

/// At most `RATE_MAX` sends per session per 10 minutes.
fn check_rate(session_id: &str) -> Result<()> {
    static SENDS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>,
    > = std::sync::LazyLock::new(Default::default);
    let now = std::time::Instant::now();
    let mut sends = SENDS.lock().unwrap();
    let recent = sends.entry(session_id.to_owned()).or_default();
    recent.retain(|t| now.duration_since(*t) < RATE_WINDOW);
    if recent.len() >= RATE_MAX {
        anyhow::bail!("rate limit: at most {RATE_MAX} agent messages per 10 minutes per session");
    }
    recent.push(now);
    Ok(())
}

/// At most `PAIR_MAX` sends from one session to another per hour (each direction counted alone).
fn check_pair_rate(source_session_id: &str, target_session_id: &str) -> Result<()> {
    static PAIRS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<(String, String), Vec<std::time::Instant>>>,
    > = std::sync::LazyLock::new(Default::default);
    let now = std::time::Instant::now();
    let key = (source_session_id.to_owned(), target_session_id.to_owned());
    let mut pairs = PAIRS.lock().unwrap();
    let recent = pairs.entry(key).or_default();
    recent.retain(|t| now.duration_since(*t) < PAIR_WINDOW);
    if recent.len() >= PAIR_MAX {
        anyhow::bail!(
            "rate limit: at most {PAIR_MAX} agent messages per hour to the same session (pause or send something new later)"
        );
    }
    recent.push(now);
    Ok(())
}

#[derive(Clone)]
struct RecentDelivery {
    at: std::time::Instant,
    message_id: String,
}

fn recent_deliveries() -> &'static std::sync::Mutex<std::collections::HashMap<String, RecentDelivery>> {
    static RECENT: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, RecentDelivery>>,
    > = std::sync::LazyLock::new(Default::default);
    &RECENT
}

fn delivery_key(source_session_id: &str, target_session_id: &str, message: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source_session_id.hash(&mut hasher);
    target_session_id.hash(&mut hasher);
    message.trim().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// If this exact body was already delivered from `source` to `target` within `DEDUP_WINDOW`,
/// return the prior `messageId` so the host can skip re-prompting.
pub(crate) fn take_duplicate(
    source_session_id: &str,
    target_session_id: &str,
    message: &str,
) -> Option<String> {
    let now = std::time::Instant::now();
    let key = delivery_key(source_session_id, target_session_id, message);
    let mut recent = recent_deliveries().lock().unwrap();
    recent.retain(|_, d| now.duration_since(d.at) < DEDUP_WINDOW);
    recent.get(&key).map(|d| d.message_id.clone())
}

/// Remember a successful delivery so an identical retry can be collapsed.
pub(crate) fn remember_delivery(
    source_session_id: &str,
    target_session_id: &str,
    message: &str,
    message_id: &str,
) {
    let now = std::time::Instant::now();
    let key = delivery_key(source_session_id, target_session_id, message);
    let mut recent = recent_deliveries().lock().unwrap();
    recent.retain(|_, d| now.duration_since(d.at) < DEDUP_WINDOW);
    recent.insert(
        key,
        RecentDelivery {
            at: now,
            message_id: message_id.to_owned(),
        },
    );
}

/// Short note for the MCP tool result so agents do not treat queued/steered as failure.
pub(crate) fn delivery_note(delivery: &str) -> &'static str {
    match delivery {
        "sent" => "accepted: delivered as a new turn",
        "steered" => "accepted: joined the running turn — do not resend",
        "queued" => "accepted: queued for the agent's next turn — do not resend",
        "duplicate" => "accepted earlier: identical message already delivered — do not resend",
        _ => "accepted",
    }
}

/// Escape a filename for use as a markdown link label.
fn markdown_label(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '\\' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            '\r' | '\n' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// The share tool's result. An MCP tool result is only seen by the model (and, as plain text, in
/// the tool-call card), so it must hand the agent a ready-made markdown link and explicitly tell it
/// to put that link in its visible reply — otherwise the share would be silent (pb-20261105-18).
fn share_tool_result(ok: &Value) -> String {
    let label = markdown_label(ok["name"].as_str().unwrap_or("artifact"));
    let url = ok["url"].as_str().unwrap_or_default();
    format!(
        "Shared \"{label}\" as a read-only, time-limited link (revocable from the Share control).\n\
         A tool result is not shown in the chat. You MUST include this markdown link verbatim in your \
         visible reply to the user so the share is never silent:\n\
         [{label}]({url})\n\
         Do not report the share without the link."
    )
}

/// Resolve an agent-named file to a canonical absolute path INSIDE the session's cwd. Agent
/// shares are deliberately narrower than user shares (`create_share` accepts any explicit path):
/// `..` escapes, absolute paths outside the folder, directories, missing files and symlinks that
/// resolve outside the folder are all refused. The registry then reuses the normal token/expiry/
/// revoke semantics untouched.
fn confined_share_path(cwd: &Path, input: &str) -> Result<PathBuf> {
    let root = cwd.canonicalize().context("session folder does not exist")?;
    if !root.is_dir() {
        bail!("session folder is not a directory");
    }
    let p = Path::new(input);
    let joined = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    let canonical = joined.canonicalize().with_context(|| format!("cannot resolve {input}"))?;
    if !canonical.starts_with(&root) {
        bail!("only files inside the session folder can be shared");
    }
    if !canonical.is_file() {
        bail!("only a regular file inside the session folder can be shared");
    }
    Ok(canonical)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolReply {
    content: Vec<Value>,
    is_error: bool,
}

fn steering_enabled() -> bool {
    matches!(
        std::env::var("PATCHBAY_MESSAGE_STEERING").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn tools_list() -> Value {
    tools_list_for(steering_enabled())
}

fn tools_list_for(can_steer: bool) -> Value {
    let send_description = if can_steer {
        "Send a message to another Patchbay Code session in this mesh. When that session is mid-turn and can take mid-turn messages, it receives yours in the running turn; otherwise the message waits for its next turn. The recipient can reply by sending a message to your session. A result of sent, steered, queued, or duplicate all mean the message was accepted — do not resend because delivery was queued. Only send when you have new information."
    } else {
        "Send a message to another Patchbay Code session in this mesh. When that session is mid-turn and cannot take mid-turn messages, the message queues until its turn ends or it calls accept_queued_agent_message. This session also queues peer messages mid-turn: use list_queued_agent_messages and accept_queued_agent_message to read them as a tool step without ending your turn. A result of sent, steered, queued, or duplicate all mean the message was accepted — do not resend because delivery was queued. Only send when you have new information."
    };
    let mut tools = vec![
        json!({
            "name": TOOL_LIST_SESSIONS,
            "description": "List unarchived Patchbay Code sessions in this mesh so you can find a recipient by title and node. For sessions on the same computer as you, `activity` says what each is doing (running, waiting on a person, stalled = mid-turn with nothing from its agent for a while, idle, or stopped) and `lastActivityAt` when its agent last sent anything (Unix ms); both are null for sessions on other computers.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": TOOL_SEND,
            "description": send_description,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "targetSessionId": { "type": "string", "description": "The Patchbay Code session ID from list_agent_sessions." },
                    "message": { "type": "string", "description": "The message to deliver." }
                },
                "required": ["targetSessionId", "message"],
                "additionalProperties": false
            }
        }),
    ];
    tools.push(json!({
        "name": TOOL_CREATE,
        "description": "Start a new Patchbay Code session on this computer, in an existing folder, and send it a first prompt (a brief). It appears in Patchbay like any session and has your agent, model and approval mode unless you pass agent or model; it never gets looser approvals than yours. It returns its sessionId: use send_agent_message to follow up, and it can message you back. Limits: at most 5 sessions per creating session, and a session created this way can create only one level deeper. Create a session only when the user asked for one or the work clearly needs a separate session.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "description": "Absolute path of an existing folder for the new session." },
                "prompt": { "type": "string", "description": "The first message: a self-contained brief, as the new session sees nothing of this conversation." },
                "title": { "type": "string", "description": "Optional session title; defaults to one taken from the prompt." },
                "agent": { "type": "string", "description": "Optional coding agent id; defaults to your agent." },
                "model": { "type": "string", "description": "Optional model; defaults to your model." }
            },
            "required": ["cwd", "prompt"],
            "additionalProperties": false
        }
    }));
    tools.push(json!({
        "name": TOOL_SHARE,
        "description": "Share a file you generated INSIDE this session's folder so the user can open it as a read-only artifact. The path must resolve to a regular file inside the session's cwd; paths outside it, directories and symlinks that escape it are refused. The link is time-limited and revocable. A tool result is NOT shown in the chat: you MUST include the returned markdown link verbatim in your visible reply to the user, so a share you do not mention is never silent. Never share a file the user did not ask about.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path of the file to share, absolute or relative to this session's folder." }
            },
            "required": ["path"],
            "additionalProperties": false
        }
    }));
    if !can_steer {
        tools.push(json!({
            "name": TOOL_LIST_QUEUED,
            "description": "List peer agent messages waiting for this session while you are mid-turn. This agent cannot take mid-turn prompts automatically; call accept_queued_agent_message to read one as a tool result in the current turn (that does not end your turn).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }));
        tools.push(json!({
            "name": TOOL_ACCEPT_QUEUED,
            "description": "Take one queued peer agent message into this turn as the tool result. Removes it from the queue so it will not start a new turn when you finish. Pass messageId from list_queued_agent_messages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "messageId": { "type": "string", "description": "The messageId from list_queued_agent_messages." }
                },
                "required": ["messageId"],
                "additionalProperties": false
            }
        }));
    }
    json!({ "tools": tools })
}


pub fn run_stdio() -> Result<()> {
    let log_path = mcp_log_path_from_env();
    // START line before doing work: pid/ppid/session/host_agent/timestamp. No secrets.
    mcp_log_event(log_path.as_deref(), "start", &mcp_process_fields());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match runtime {
            Ok(rt) => rt.block_on(run_stdio_async(log_path.clone())),
            Err(e) => Err(anyhow::Error::new(e).context("tokio runtime init")),
        }
    }));

    match result {
        Ok(Ok(cause)) => {
            // cause is a named normal-exit reason (e.g. stdin_eof). No error payload.
            // Signal exits (unix) log and re-raise inside run_stdio_async, so they don't return here.
            let mut fields = mcp_process_fields();
            fields.push(("cause", cause));
            mcp_log_event(log_path.as_deref(), "exit", &fields);
            Ok(())
        }
        Ok(Err(e)) => {
            let mut fields = mcp_process_fields();
            // `e` carries our safe named cause via its display chain; extract kind only.
            fields.push(("cause", safe_error_cause(&e)));
            mcp_log_event(log_path.as_deref(), "exit", &fields);
            Err(e)
        }
        Err(_panic) => {
            let mut fields = mcp_process_fields();
            fields.push(("cause", "panic".to_owned()));
            mcp_log_event(log_path.as_deref(), "exit", &fields);
            // Preserve process behavior as much as possible; main.rs will see Err-like path
            // only if we return Err. A panic should still be visible, but we already logged.
            // Re-panic would prevent main's stderr+exit(1) formatting; return an error instead
            // so the existing failure path applies, without leaking panic payload.
            Err(anyhow!("mcp child panic"))
        }
    }
}

#[cfg(unix)]
const SIG_DFL: usize = 0;
#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "signal"]
    fn sys_signal(sig: i32, handler: usize) -> usize;
    #[link_name = "raise"]
    fn sys_raise(sig: i32) -> i32;
}
#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGINT: i32 = 2;
#[cfg(unix)]
const SIGHUP: i32 = 1;

#[cfg(unix)]
async fn log_signal_exit_and_reraise(log_path: Option<PathBuf>, cause: &'static str, sig: i32) {
    // Write-then-re-raise with a short bound. Failure to write must NEVER block exit.
    let path = log_path.clone();
    let fields = mcp_process_fields();
    let write = tokio::task::spawn_blocking(move || {
        let mut f = fields;
        f.push(("cause", cause.to_owned()));
        mcp_log_event(path.as_deref(), "exit", &f);
    });
    let _ = tokio::time::timeout(Duration::from_millis(200), write).await;
    unsafe {
        sys_signal(sig, SIG_DFL);
        sys_raise(sig);
    }
    // Safety net if raise returns (should not for TERM/INT/HUP with DFL).
    std::process::abort();
}

/// Returns Ok(named_cause) on normal end (stdin EOF), Err on failures.
/// Error messages are mapped to safe kinds by callers; this keeps causes explicit.
/// On unix, SIGTERM/SIGINT/SIGHUP are logged as signal_* then re-raised (host sees signal termination).
async fn run_stdio_async(log_path: Option<PathBuf>) -> Result<String> {
    let addr = std::env::var("PATCHBAY_MESSAGE_ADDR")
        .map_err(|_| anyhow!("setup_env_missing:PATCHBAY_MESSAGE_ADDR"))?;
    let token = std::env::var("PATCHBAY_MESSAGE_TOKEN")
        .map_err(|_| anyhow!("setup_env_missing:PATCHBAY_MESSAGE_TOKEN"))?;
    let source_session_id = std::env::var("PATCHBAY_MESSAGE_SESSION")
        .map_err(|_| anyhow!("setup_env_missing:PATCHBAY_MESSAGE_SESSION"))?;

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();

    #[cfg(unix)]
    let mut sigterm = unix_signal(SignalKind::terminate()).ok();
    #[cfg(unix)]
    let mut sigint = unix_signal(SignalKind::interrupt()).ok();
    #[cfg(unix)]
    let mut sighup = unix_signal(SignalKind::hangup()).ok();

    loop {
        #[cfg(unix)]
        {
            let recv_term = async {
                match sigterm.as_mut() {
                    Some(s) => s.recv().await,
                    None => std::future::pending::<Option<()>>().await,
                }
            };
            let recv_int = async {
                match sigint.as_mut() {
                    Some(s) => s.recv().await,
                    None => std::future::pending::<Option<()>>().await,
                }
            };
            let recv_hup = async {
                match sighup.as_mut() {
                    Some(s) => s.recv().await,
                    None => std::future::pending::<Option<()>>().await,
                }
            };
            tokio::select! {
                biased;
                _ = recv_term => { log_signal_exit_and_reraise(log_path.clone(), "signal_term", SIGTERM).await; }
                _ = recv_int => { log_signal_exit_and_reraise(log_path.clone(), "signal_int", SIGINT).await; }
                _ = recv_hup => { log_signal_exit_and_reraise(log_path.clone(), "signal_hup", SIGHUP).await; }
                line = input.next_line() => {
                    let line = match line {
                        Ok(Some(line)) => line,
                        Ok(None) => return Ok("stdin_eof".to_owned()),
                        Err(_) => return Err(anyhow!("stdin_read_error")),
                    };
                    handle_stdio_line(&addr, &token, &source_session_id, &mut output, line).await?;
                }
            }
        }
        #[cfg(not(unix))]
        {
            let line = match input.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => return Ok("stdin_eof".to_owned()),
                Err(_) => return Err(anyhow!("stdin_read_error")),
            };
            handle_stdio_line(&addr, &token, &source_session_id, &mut output, line).await?;
        }
    }
}

async fn handle_stdio_line(
    addr: &str,
    token: &str,
    source_session_id: &str,
    output: &mut tokio::io::Stdout,
    line: String,
) -> Result<()> {
    let request: Value = match serde_json::from_str(&line) {
        Ok(value) => value,
        Err(_) => return Ok(()),
    };
    if request["method"]
        .as_str()
        .is_some_and(|m| m.starts_with("notifications/"))
    {
        return Ok(());
    }
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": request["params"]["protocolVersion"].as_str().unwrap_or("2025-03-26"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "patchbay-agent-messaging", "version": env!("CARGO_PKG_VERSION") }
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        "tools/call" => call_tool(addr, token, source_session_id, &request["params"]).await,
        _ => Err(anyhow!("unknown MCP method {method}")),
    };
    let response = match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err(error) => json!({
            "jsonrpc": "2.0", "id": id,
            "result": ToolReply { content: vec![json!({ "type": "text", "text": format!("{error:#}") })], is_error: true }
        }),
    };
    if output
        .write_all(response.to_string().as_bytes())
        .await
        .is_err()
    {
        return Err(anyhow!("stdout_write_error"));
    }
    if output.write_all(b"\n").await.is_err() {
        return Err(anyhow!("stdout_write_error"));
    }
    if output.flush().await.is_err() {
        return Err(anyhow!("stdout_flush_error"));
    }
    Ok(())
}

pub(crate) async fn call_tool(
    addr: &str,
    token: &str,
    source_session_id: &str,
    params: &Value,
) -> Result<Value> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| anyhow!("tool name is required"))?;
    let can_steer = steering_enabled();
    if can_steer && matches!(name, TOOL_LIST_QUEUED | TOOL_ACCEPT_QUEUED) {
        anyhow::bail!("this agent takes mid-turn messages automatically; queued-message tools are not available");
    }
    let (action, target_session_id, message, message_id) = match name {
        TOOL_SEND => (
            "send",
            Some(
                params["arguments"]["targetSessionId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("targetSessionId is required"))?,
            ),
            Some(
                params["arguments"]["message"]
                    .as_str()
                    .ok_or_else(|| anyhow!("message is required"))?,
            ),
            None,
        ),
        TOOL_CREATE => ("create", None, None, None),
        TOOL_SHARE => ("share", None, None, None),
        TOOL_LIST_SESSIONS => ("list", None, None, None),
        TOOL_LIST_QUEUED => ("list_queued", None, None, None),
        TOOL_ACCEPT_QUEUED => (
            "accept_queued",
            None,
            None,
            Some(
                params["arguments"]["messageId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("messageId is required"))?,
            ),
        ),
        _ => anyhow::bail!("unknown tool"),
    };
    let stream = TcpStream::connect(addr)
        .await
        .context("Patchbay is no longer available")?;
    let (read, mut write) = stream.into_split();
    let request = json!({
        "token": token,
        "sourceSessionId": source_session_id,
        "action": action,
        "targetSessionId": target_session_id,
        "message": message,
        "messageId": message_id,
        "arguments": matches!(name, TOOL_CREATE | TOOL_SHARE).then(|| params["arguments"].clone()),
    });
    write.write_all(request.to_string().as_bytes()).await?;
    write.write_all(b"\n").await?;
    write.shutdown().await?;
    let mut line = String::new();
    BufReader::new(read)
        .take(MAX_LINE as u64)
        .read_line(&mut line)
        .await?;
    let response: Value = serde_json::from_str(&line)?;
    if let Some(error) = response["error"].as_str() {
        return Ok(json!({ "content": [{ "type": "text", "text": error }], "isError": true }));
    }
    let text = match name {
        TOOL_ACCEPT_QUEUED => response["ok"]["text"].as_str().unwrap_or_default().to_owned(),
        TOOL_SHARE => share_tool_result(&response["ok"]),
        _ => {
            let ok = &response["ok"];
            let delivery = ok["delivery"].as_str().unwrap_or("");
            if delivery.is_empty() {
                ok.to_string()
            } else {
                format!("{ok}\n{}", delivery_note(delivery))
            }
        }
    };
    Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": false }))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_definition_name_matches_acp_pattern() {
        let (bridge, listener) = AgentMessagingBridge::bind(None).unwrap();
        let def = bridge.server_definition("session-1", "opencode", false).unwrap();
        drop(listener);
        let name = def["name"].as_str().unwrap();
        assert_eq!(name, "patchbay-agent-messaging");
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        let env = def["env"].as_array().unwrap();
        let steering = env
            .iter()
            .find(|e| e["name"] == "PATCHBAY_MESSAGE_STEERING")
            .unwrap();
        assert_eq!(steering["value"], "0");
        let host = env.iter().find(|e| e["name"] == ENV_HOST_AGENT).unwrap();
        assert_eq!(host["value"], "opencode");
        assert!(env.iter().all(|e| e["name"] != ENV_LOG_PATH));
    }

    #[test]
    fn tools_list_adds_queue_tools_when_not_steering() {
        let names = |can_steer| {
            tools_list_for(can_steer)["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        };
        assert!(names(false).iter().any(|n| n == TOOL_LIST_QUEUED));
        assert!(names(false).iter().any(|n| n == TOOL_ACCEPT_QUEUED));
        assert!(!names(true).iter().any(|n| n == TOOL_LIST_QUEUED));
        assert!(!names(true).iter().any(|n| n == TOOL_ACCEPT_QUEUED));
    }

    #[test]
    fn tools_list_offers_create_agent_session_in_both_modes() {
        for can_steer in [true, false] {
            let list = tools_list_for(can_steer);
            let tool = list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == TOOL_CREATE)
                .expect("create tool listed");
            assert_eq!(tool["inputSchema"]["required"], json!(["cwd", "prompt"]));
            for k in ["cwd", "prompt", "title", "agent", "model"] {
                assert!(tool["inputSchema"]["properties"][k].is_object(), "{k}");
            }
        }
    }

    #[test]
    fn rate_limit_trips_after_max_sends() {
        for _ in 0..RATE_MAX {
            check_rate("rate-test-session").unwrap();
        }
        assert!(check_rate("rate-test-session").is_err());
        assert!(check_rate("other-session").is_ok());
    }

    #[test]
    fn pair_rate_trips_after_max_to_same_target() {
        let src = "pair-src";
        let dst = "pair-dst";
        for _ in 0..PAIR_MAX {
            check_pair_rate(src, dst).unwrap();
        }
        assert!(check_pair_rate(src, dst).is_err());
        assert!(check_pair_rate(src, "other-dst").is_ok(), "other target still allowed");
        assert!(check_pair_rate(dst, src).is_ok(), "reply direction is a separate pair");
    }

    #[test]
    fn identical_deliveries_dedup_within_window() {
        let src = "dedup-src";
        let dst = "dedup-dst";
        assert!(take_duplicate(src, dst, "same body").is_none());
        remember_delivery(src, dst, "same body", "msg-1");
        assert_eq!(take_duplicate(src, dst, "same body").as_deref(), Some("msg-1"));
        assert_eq!(take_duplicate(src, dst, " same body ").as_deref(), Some("msg-1"));
        assert!(take_duplicate(src, dst, "different").is_none());
        assert!(take_duplicate(src, "other", "same body").is_none());
    }

    #[test]
    fn delivery_notes_discourage_resend_on_queued() {
        assert!(delivery_note("queued").contains("do not resend"));
        assert!(delivery_note("duplicate").contains("do not resend"));
        assert!(delivery_note("sent").contains("accepted"));
    }

    #[test]
    fn tools_list_offers_share_artifact_in_both_modes() {
        for can_steer in [true, false] {
            let list = tools_list_for(can_steer);
            let tool = list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == TOOL_SHARE)
                .expect("share tool listed");
            assert_eq!(tool["inputSchema"]["required"], json!(["path"]));
            assert!(tool["inputSchema"]["properties"]["path"].is_object());
        }
    }

    #[test]
    fn share_result_carries_link_and_visibility_instruction() {
        let ok = json!({ "name": "report.html", "url": "http://host/share/node/token" });
        let text = share_tool_result(&ok);
        assert!(text.contains("[report.html](http://host/share/node/token)"), "{text}");
        assert!(text.contains("visible reply"), "{text}");
        assert!(text.contains("never silent"), "{text}");
        // The label is escaped so a filename with markdown punctuation can't break the link.
        let tricky = json!({ "name": "a[b].html", "url": "http://host/x" });
        assert!(share_tool_result(&tricky).contains("[a\\[b\\].html](http://host/x)"));
    }

    #[test]
    fn markdown_label_escapes_punctuation() {
        assert_eq!(markdown_label("a[b]c"), "a\\[b\\]c");
        assert_eq!(markdown_label("a\\b"), "a\\\\b");
        assert_eq!(markdown_label("a\nb"), "a b");
    }

    #[test]
    fn confined_share_path_stays_inside_cwd() {
        let dir = crate::test_support::short_dir("share-mcp");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        let file = dir.join("nested/report.html");
        std::fs::write(&file, b"hi").unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();

        // A relative path and an absolute path inside cwd both work.
        assert_eq!(confined_share_path(&dir, "nested/report.html").unwrap(), canonical);
        assert_eq!(confined_share_path(&dir, file.to_str().unwrap()).unwrap(), canonical);
        // `..` escape, an absolute path outside, a directory, a missing file, and cwd itself fail.
        assert!(confined_share_path(&dir, "../outside.html").is_err());
        assert!(confined_share_path(&dir, "/etc/hosts").is_err());
        assert!(confined_share_path(&dir, "nested").is_err());
        assert!(confined_share_path(&dir, "missing.html").is_err());
        assert!(confined_share_path(&dir, ".").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn confined_share_path_rejects_symlink_escape() {
        let dir = crate::test_support::short_dir("share-mcp-sym");
        let outside = crate::test_support::short_dir("share-mcp-outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.html");
        std::fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("link.html")).unwrap();
        assert!(confined_share_path(&dir, "link.html").is_err(), "symlink escape refused");
        // A symlink pointing to a file INSIDE cwd is fine (canonical target is inside root).
        let inner = dir.join("inner.html");
        std::fs::write(&inner, b"ok").unwrap();
        std::os::unix::fs::symlink(&inner, dir.join("inner-link.html")).unwrap();
        assert_eq!(
            confined_share_path(&dir, "inner-link.html").unwrap(),
            std::fs::canonicalize(&inner).unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(outside);
    }


    #[test]
    fn server_definition_includes_log_path_when_configured() {
        let log = std::env::temp_dir().join(format!("pb-mcp-log-{}.log", std::process::id()));
        let (bridge, listener) = AgentMessagingBridge::bind(Some(log.clone())).unwrap();
        let def = bridge.server_definition("session-1", "claude", true).unwrap();
        drop(listener);
        let env = def["env"].as_array().unwrap();
        let lp = env.iter().find(|e| e["name"] == ENV_LOG_PATH).unwrap();
        assert_eq!(lp["value"].as_str().unwrap(), log.to_string_lossy().as_ref());
        // Token is required for auth - present but must never appear in child lifecycle log content.
        assert!(env.iter().any(|e| e["name"] == "PATCHBAY_MESSAGE_TOKEN"));
    }

    #[test]
    fn mcp_log_failure_does_not_panic_and_writes_no_secrets() {
        // A path whose parent cannot be created as a normal file write target: use a directory
        // as the log path so append fails. Best-effort must swallow it.
        let dir = std::env::temp_dir().join(format!("pb-mcp-logdir-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Make parent a file so create_dir_all/open fail on most platforms.
        let _ = std::fs::create_dir_all(dir.join("blocked"));
        let _ = std::fs::write(dir.join("blocked").join("log.txt"), b"x");
        // Point at a path under a file - append should fail; must not panic.
        let impossible = dir.join("blocked").join("log.txt").join("nope.log");
        mcp_log_event(Some(&impossible), "start", &[("pid", "1".into())]);
        mcp_log_event(None, "start", &[("pid", "1".into())]); // absent path: no-op

        // Successful write: verify content has diagnostics and no secrets.
        let ok_path = std::env::temp_dir().join(format!("pb-mcp-log-ok-{}-{}.log", std::process::id(), now_ms()));
        let token = "SUPER_SECRET_TOKEN_VALUE";
        let body = "private message body";
        let fields = vec![
            ("pid", "123".to_owned()),
            ("ppid", "45".to_owned()),
            ("session", "sess-1".to_owned()),
            ("host_agent", "opencode".to_owned()),
            ("cause", "stdin_eof".to_owned()),
        ];
        mcp_log_event(Some(&ok_path), "exit", &fields);
        let content = std::fs::read_to_string(&ok_path).unwrap();
        assert!(content.contains("event=exit"), "{content}");
        assert!(content.contains("cause=stdin_eof"), "{content}");
        assert!(content.contains("pid=123"), "{content}");
        assert!(content.contains("ppid=45"), "{content}");
        assert!(content.contains("session=sess-1"), "{content}");
        assert!(content.contains("host_agent=opencode"), "{content}");
        assert!(!content.contains(token), "token leaked: {content}");
        assert!(!content.contains(body), "body leaked: {content}");
        assert!(!content.contains("PATCHBAY_MESSAGE_TOKEN"), "token env name value context must not imply secret: {content}");
        let _ = std::fs::remove_file(&ok_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_log_is_size_capped() {
        let path = std::env::temp_dir().join(format!("pb-mcp-log-cap-{}-{}.log", std::process::id(), now_ms()));
        // Pre-fill beyond cap.
        let big = vec![b'x'; (MCP_LOG_MAX_BYTES as usize) + 64];
        std::fs::write(&path, &big).unwrap();
        mcp_log_event(Some(&path), "start", &[("pid", "1".into())]);
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() < MCP_LOG_MAX_BYTES + 1024, "log not capped: {}", meta.len());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("truncated") || content.contains("event=start"), "{content}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn safe_cause_never_echoes_arbitrary_content() {
        let e = anyhow!("stdout_write_error");
        assert_eq!(safe_error_cause(&e), "stdout_write_error");
        let e = anyhow!("setup_env_missing:PATCHBAY_MESSAGE_ADDR");
        assert_eq!(safe_error_cause(&e), "setup_env_missing:PATCHBAY_MESSAGE_ADDR");
        // Unknown/internal errors collapse to a kind, not full payload.
        let e = anyhow!("weird {} {}", "SECRET=abc", "body=hello");
        let c = safe_error_cause(&e);
        assert!(!c.contains("SECRET"), "{c}");
        assert!(!c.contains("body=hello"), "{c}");
    }
}
