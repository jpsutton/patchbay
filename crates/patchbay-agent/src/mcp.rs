//! A small MCP client (stdio and streamable HTTP), enough to list and call tools.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use patchbay_llm::{Part, ToolSpec};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::rpc::{self, Peer, RpcError};
use crate::session::Session;
use crate::tools::{Tool, ToolCtx, ToolOutput};

const PROTOCOL: &str = "2025-06-18";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

enum Conn {
    Stdio { peer: Peer, _child: tokio::process::Child },
    Http { http: reqwest::Client, url: String, headers: Vec<(String, String)>, session: Mutex<Option<String>> },
}

pub struct McpServer {
    name: String,
    conn: Conn,
}

#[derive(Default)]
pub struct McpSet {
    servers: Vec<Arc<McpServer>>,
    tools: Vec<(Arc<McpServer>, Value)>,
    /// Servers that could not be used, with why.
    pub problems: Vec<String>,
}

fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["name"].as_str()?.to_owned(), p["value"].as_str()?.to_owned())))
        .collect()
}

impl McpServer {
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        match &self.conn {
            Conn::Stdio { peer, .. } => peer.request(method, params).await.map_err(|e| e.to_string()),
            Conn::Http { http, url, headers, session } => {
                http_request(http, url, headers, session, Some(method), params, true).await
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        match &self.conn {
            Conn::Stdio { peer, .. } => peer.notify(method, params),
            Conn::Http { http, url, headers, session } => {
                let _ = http_request(http, url, headers, session, Some(method), params, false).await;
            }
        }
    }

    async fn handshake(&self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "patchbay", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        self.notify("notifications/initialized", json!({})).await;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let params = cursor.as_ref().map(|c| json!({"cursor": c})).unwrap_or_else(|| json!({}));
            let r = self.request("tools/list", params).await?;
            out.extend(r["tools"].as_array().cloned().unwrap_or_default());
            match r["nextCursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_owned()),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// One JSON-RPC call over streamable HTTP. With `want_reply` false it is a notification.
async fn http_request(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    session: &Mutex<Option<String>>,
    method: Option<&str>,
    params: Value,
    want_reply: bool,
) -> Result<Value, String> {
    let id = 1u64 << 40 | (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0));
    let mut body = json!({"jsonrpc": "2.0", "method": method, "params": params});
    if want_reply {
        body["id"] = json!(id);
    }
    let mut rb = http
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL)
        .json(&body);
    for (k, v) in headers {
        rb = rb.header(k, v);
    }
    if let Some(s) = session.lock().await.clone() {
        rb = rb.header("mcp-session-id", s);
    }
    let resp = rb.send().await.map_err(|e| e.to_string())?;
    if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        *session.lock().await = Some(s.to_owned());
    }
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    if !want_reply {
        return Ok(Value::Null);
    }
    let is_sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("event-stream"));
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let pick = |v: Value| -> Option<Result<Value, String>> {
        if v["id"] != json!(id) {
            return None;
        }
        Some(match v.get("error") {
            Some(e) => Err(e["message"].as_str().unwrap_or("error").to_owned()),
            None => Ok(v["result"].clone()),
        })
    };
    if is_sse {
        for ev in text.split("\n\n") {
            let data: String = ev.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim_start).collect::<Vec<_>>().join("\n");
            if let Ok(v) = serde_json::from_str::<Value>(&data)
                && let Some(r) = pick(v)
            {
                return r;
            }
        }
        Err("the server sent no answer".into())
    } else {
        serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()).and_then(|v| pick(v).unwrap_or_else(|| Err("the server answered another request".into())))
    }
}

async fn connect_one(cfg: &Value) -> Result<McpServer, String> {
    let name = cfg["name"].as_str().ok_or("a server has no name")?.to_owned();
    let kind = cfg["type"].as_str().unwrap_or("stdio");
    let conn = match kind {
        "http" => Conn::Http {
            http: reqwest::Client::builder().timeout(CALL_TIMEOUT).build().map_err(|e| e.to_string())?,
            url: cfg["url"].as_str().ok_or("no url")?.to_owned(),
            headers: pairs(&cfg["headers"]),
            session: Mutex::new(None),
        },
        "sse" => return Err("the legacy SSE transport is not supported; use streamable HTTP".into()),
        _ => {
            let program = cfg["command"].as_str().ok_or("no command")?;
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(cfg["args"].as_array().into_iter().flatten().filter_map(Value::as_str))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            for (k, v) in pairs(&cfg["env"]) {
                cmd.env(k, v);
            }
            if let Some(dir) = cfg["cwd"].as_str().filter(|d| std::path::Path::new(d).is_dir()) {
                cmd.current_dir(dir);
            }
            let mut child = cmd.spawn().map_err(|e| format!("cannot start {program}: {e}"))?;
            let (stdin, stdout) = (child.stdin.take().ok_or("no stdin")?, child.stdout.take().ok_or("no stdout")?);
            let (peer, mut incoming) = rpc::connect(stdout, stdin);
            // Answer what the server asks of a client: nothing we support.
            tokio::spawn(async move {
                while let Some(m) = incoming.recv().await {
                    if let rpc::Incoming::Request { method, reply, .. } = m {
                        reply.send(if method == "ping" { Ok(json!({})) } else { Err(RpcError::new(-32601, "method not found")) });
                    }
                }
            });
            Conn::Stdio { peer, _child: child }
        }
    };
    let server = McpServer { name, conn };
    tokio::time::timeout(CONNECT_TIMEOUT, server.handshake()).await.map_err(|_| "timed out".to_owned())??;
    Ok(server)
}

impl McpSet {
    /// Connect to every server in an ACP `mcpServers` list; failures go to `problems`.
    pub async fn connect(servers: &Value) -> McpSet {
        let mut set = McpSet::default();
        let attempts = futures::future::join_all(servers.as_array().into_iter().flatten().map(|cfg| async move {
            let name = cfg["name"].as_str().unwrap_or("?").to_owned();
            let r = async {
                let s = connect_one(cfg).await?;
                let tools = tokio::time::timeout(CONNECT_TIMEOUT, s.list_tools()).await.map_err(|_| "timed out".to_owned())??;
                Ok::<_, String>((s, tools))
            }
            .await;
            (name, r)
        }))
        .await;
        for (name, r) in attempts {
            match r {
                Ok((server, tools)) => {
                    let server = Arc::new(server);
                    set.tools.extend(tools.into_iter().map(|t| (server.clone(), t)));
                    set.servers.push(server);
                }
                Err(e) => set.problems.push(format!("{name}: {e}")),
            }
        }
        set
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .iter()
            .filter_map(|(server, t)| {
                let tool = t["name"].as_str()?.to_owned();
                let mut schema = t.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object"}));
                if schema.get("type").is_none() {
                    schema["type"] = json!("object");
                }
                Some(Arc::new(McpTool {
                    full_name: tool_name(&server.name, &tool),
                    server: server.clone(),
                    tool,
                    description: t["description"].as_str().unwrap_or("").to_owned(),
                    schema,
                }) as Arc<dyn Tool>)
            })
            .collect()
    }
}

/// `mcp__server__tool`, restricted to what providers accept in tool names (64 characters).
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect::<String>();
    let mut name = format!("mcp__{}__{}", clean(server), clean(tool));
    name.truncate(64);
    name
}

struct McpTool {
    full_name: String,
    server: Arc<McpServer>,
    tool: String,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec { name: self.full_name.clone(), description: self.description.clone(), parameters: self.schema.clone() }
    }

    fn kind(&self) -> &'static str {
        "other"
    }

    fn title(&self, _: &Value) -> String {
        format!("{} ({})", self.tool, self.server.name)
    }

    fn needs_permission(&self, _: &Value, _: &Session) -> bool {
        true
    }

    async fn run(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let call = self.server.request("tools/call", json!({"name": self.tool, "arguments": input}));
        let r = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("Cancelled."),
            r = tokio::time::timeout(CALL_TIMEOUT, call) => match r {
                Ok(r) => r,
                Err(_) => return ToolOutput::error("The MCP server did not answer in time."),
            },
        };
        let r = match r {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(format!("MCP call failed: {e}")),
        };
        let mut content = Vec::new();
        for c in r["content"].as_array().into_iter().flatten() {
            match c["type"].as_str() {
                Some("text") => content.push(Part::Text(c["text"].as_str().unwrap_or("").to_owned())),
                Some("image") => content.push(Part::Image {
                    mime: c["mimeType"].as_str().unwrap_or("image/png").to_owned(),
                    data: c["data"].as_str().unwrap_or("").to_owned(),
                }),
                Some("resource") => content.push(Part::Text(
                    c["resource"]["text"].as_str().map(str::to_owned).unwrap_or_else(|| format!("[resource {}]", c["resource"]["uri"].as_str().unwrap_or(""))),
                )),
                _ => {}
            }
        }
        if content.is_empty() {
            content.push(Part::Text(
                r.get("structuredContent").map(|s| s.to_string()).unwrap_or_else(|| "(no output)".into()),
            ));
        }
        ToolOutput { content, ui: Vec::new(), raw: r.clone(), is_error: r["isError"].as_bool().unwrap_or(false), meta: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fit_provider_limits() {
        assert_eq!(tool_name("my server", "do.thing"), "mcp__my_server__do_thing");
        assert!(tool_name(&"s".repeat(40), &"t".repeat(40)).len() <= 64);
    }

    #[tokio::test]
    async fn talks_to_a_stdio_server() {
        // A tiny server in sh would be fragile; use python when present.
        let script = r#"
import sys, json
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    if m["method"] == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "1"}}
    elif m["method"] == "tools/list":
        r = {"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    else:
        r = {"content": [{"type": "text", "text": "echo: " + m["params"]["arguments"]["text"]}]}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let cfg = json!([{"name": "t", "command": "python3", "args": ["-c", script], "env": []}]);
        let set = McpSet::connect(&cfg).await;
        assert!(set.problems.is_empty(), "{:?}", set.problems);
        let tools = set.tools();
        assert_eq!(tools[0].spec().name, "mcp__t__echo");
    }
}
