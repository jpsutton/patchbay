//! Patchbay-owned MCP tool and its authenticated, local bridge into the running app.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::{AppState, api};

const TOOL_NAME: &str = "send_agent_message";
const MAX_LINE: usize = 64 * 1024;

#[derive(Clone)]
pub struct AgentMessagingBridge {
    addr: SocketAddr,
    tokens: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl AgentMessagingBridge {
    pub fn bind() -> Result<(Self, TcpListener)> {
        let listener = StdTcpListener::bind(("127.0.0.1", 0))
            .context("binding local agent-messaging bridge")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let listener = TcpListener::from_std(listener)?;
        Ok((
            Self {
                addr,
                tokens: Arc::default(),
            },
            listener,
        ))
    }

    pub fn server_definition(&self, session_id: &str) -> Result<Value> {
        let command = std::env::current_exe()?.to_string_lossy().into_owned();
        let token = hex::encode(rand::random::<[u8; 32]>());
        self.tokens
            .lock()
            .unwrap()
            .insert(session_id.to_owned(), token.clone());
        Ok(json!({
            "name": "Patchbay agent messaging",
            "command": command,
            "args": ["--agent-messaging-mcp"],
            "env": [
                { "name": "PATCHBAY_MESSAGE_ADDR", "value": self.addr.to_string() },
                { "name": "PATCHBAY_MESSAGE_TOKEN", "value": token },
                { "name": "PATCHBAY_MESSAGE_SESSION", "value": session_id },
            ]
        }))
    }

    pub fn serve(&self, listener: TcpListener, state: AppState) {
        let tokens = self.tokens.clone();
        api::spawn(async move {
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
                check_rate(&request.source_session_id)?;
                crate::send_agent_message(
                    &state,
                    &state.mesh.node_id().to_owned(),
                    &request.source_session_id,
                    request
                        .target_session_id
                        .as_deref()
                        .ok_or_else(|| anyhow!("targetSessionId is required"))?,
                    request
                        .message
                        .as_deref()
                        .ok_or_else(|| anyhow!("message is required"))?,
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

/// Stops two agents ping-ponging forever: at most `RATE_MAX` sends per session per window.
/// shortcut: counts sends only, not reply chains; add a hop count if a loop gets past this.
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolReply {
    content: Vec<Value>,
    is_error: bool,
}

pub fn run_stdio() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_stdio_async())
}

async fn run_stdio_async() -> Result<()> {
    let addr =
        std::env::var("PATCHBAY_MESSAGE_ADDR").context("missing Patchbay message address")?;
    let token =
        std::env::var("PATCHBAY_MESSAGE_TOKEN").context("missing Patchbay message token")?;
    let source_session_id =
        std::env::var("PATCHBAY_MESSAGE_SESSION").context("missing Patchbay source session")?;
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    while let Some(line) = input.next_line().await? {
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if request["method"]
            .as_str()
            .is_some_and(|m| m.starts_with("notifications/"))
        {
            continue;
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
            "tools/list" => Ok(json!({ "tools": [
                {
                    "name": "list_agent_sessions",
                    "description": "List unarchived Patchbay Code sessions in this mesh so you can find a recipient by title and node.",
                    "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
                },
                {
                    "name": TOOL_NAME,
                    "description": "Send a message to another Patchbay Code session in this mesh. The recipient can reply by sending a message to your session.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "targetSessionId": { "type": "string", "description": "The Patchbay Code session ID from list_agent_sessions." },
                            "message": { "type": "string", "description": "The message to deliver." }
                        },
                        "required": ["targetSessionId", "message"],
                        "additionalProperties": false
                    }
                }
            ] })),
            "tools/call" => {
                send_message_tool(&addr, &token, &source_session_id, &request["params"]).await
            }
            _ => Err(anyhow!("unknown MCP method {method}")),
        };
        let response = match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err(error) => json!({
                "jsonrpc": "2.0", "id": id,
                "result": ToolReply { content: vec![json!({ "type": "text", "text": format!("{error:#}") })], is_error: true }
            }),
        };
        output.write_all(response.to_string().as_bytes()).await?;
        output.write_all(b"\n").await?;
        output.flush().await?;
    }
    Ok(())
}

async fn send_message_tool(
    addr: &str,
    token: &str,
    source_session_id: &str,
    params: &Value,
) -> Result<Value> {
    let name = params["name"]
        .as_str()
        .ok_or_else(|| anyhow!("tool name is required"))?;
    if name != TOOL_NAME && name != "list_agent_sessions" {
        anyhow::bail!("unknown tool");
    }
    let (action, target_session_id, message) = if name == TOOL_NAME {
        (
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
        )
    } else {
        ("list", None, None)
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
    Ok(
        json!({ "content": [{ "type": "text", "text": response["ok"].to_string() }], "isError": false }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_trips_after_max_sends() {
        for _ in 0..RATE_MAX {
            check_rate("rate-test").unwrap();
        }
        assert!(check_rate("rate-test").is_err());
        assert!(check_rate("other-session").is_ok());
    }
}
