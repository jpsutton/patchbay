//! The ACP server: methods in, session updates and permission requests out.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use crate::host::Host;
use crate::mcp::McpSet;
use crate::rpc::{self, Incoming, Peer, RpcError};
use crate::session::{self, Mode, Session};
use crate::turn;

type Sessions = Arc<Mutex<HashMap<String, Arc<Session>>>>;

struct Agent {
    host: Arc<dyn Host>,
    peer: Peer,
    sessions: Sessions,
    /// The client keeps `session/prompt` open for background subagents.
    subagents_cap: std::sync::atomic::AtomicBool,
}

/// Serve ACP on `reader` and `writer` until the client hangs up.
pub async fn serve(
    host: Arc<dyn Host>,
    reader: impl AsyncRead + Unpin + Send + 'static,
    writer: impl AsyncWrite + Unpin + Send + 'static,
) {
    let (peer, mut incoming) = rpc::connect(reader, writer);
    let agent = Arc::new(Agent { host, peer, sessions: Sessions::default(), subagents_cap: Default::default() });
    while let Some(msg) = incoming.recv().await {
        match msg {
            Incoming::Request { method, params, reply } => {
                let agent = agent.clone();
                tokio::spawn(async move { reply.send(agent.handle(&method, params).await) });
            }
            Incoming::Notification { method, params } => {
                if method == "session/cancel"
                    && let Some(s) = params["sessionId"].as_str().and_then(|id| agent.session(id))
                {
                    if let Some(t) = s.cancel.lock().unwrap().as_ref() {
                        t.cancel();
                    }
                    s.agents.stop_background();
                }
            }
        }
    }
    // The client is gone: stop whatever still runs.
    for s in agent.sessions.lock().unwrap().values() {
        if let Some(t) = s.cancel.lock().unwrap().as_ref() {
            t.cancel();
        }
        s.shells.kill_all();
        s.agents.stop_background();
    }
}

fn param_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params[key].as_str().ok_or_else(|| RpcError::invalid_params(format!("{key} is required")))
}

impl Agent {
    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    fn need_session(&self, params: &Value) -> Result<Arc<Session>, RpcError> {
        let id = param_str(params, "sessionId")?;
        self.session(id).ok_or_else(|| RpcError::invalid_params(format!("unknown session {id}")))
    }

    async fn handle(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        // Extension methods may arrive with ACP's leading underscore.
        match method.trim_start_matches('_') {
            "initialize" => {
                self.subagents_cap.store(params["clientCapabilities"]["_meta"]["subagents"].as_bool().unwrap_or(false), Ordering::SeqCst);
                Ok(json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                    "mcpCapabilities": {"http": true, "sse": false},
                },
                "agentInfo": {"name": "patchbay", "title": "Patchbay", "version": env!("CARGO_PKG_VERSION")},
                "authMethods": [],
                }))
            }
            "authenticate" => Ok(json!({})),
            "session/new" => self.new_session(&params).await,
            "session/load" => self.load_session(&params).await,
            "session/prompt" => self.prompt(&params).await,
            "session/set_model" => {
                let s = self.need_session(&params)?;
                let id = param_str(&params, "modelId")?;
                if !self.host.models().iter().any(|m| m.id == id) {
                    return Err(RpcError::invalid_params(format!("unknown model {id}")));
                }
                let mode = {
                    let mut st = s.state.lock().await;
                    st.model_id = id.to_owned();
                    st.mode
                };
                s.save_meta(id, mode);
                Ok(json!({}))
            }
            "session/set_mode" => {
                let s = self.need_session(&params)?;
                let mode = Mode::parse(param_str(&params, "modeId")?).ok_or_else(|| RpcError::invalid_params("unknown mode"))?;
                let model = {
                    let mut st = s.state.lock().await;
                    st.mode = mode;
                    st.model_id.clone()
                };
                s.save_meta(&model, mode);
                s.update(json!({"sessionUpdate": "current_mode_update", "currentModeId": mode.id()}));
                Ok(json!({}))
            }
            "patchbay/rewind/points" => {
                let s = self.need_session(&params)?;
                let points: Vec<Value> = s
                    .prompts
                    .lock()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(i, p)| json!({"prompt_index": i, "prompt_preview": p.preview}))
                    .collect();
                Ok(json!({"rewind_points": points}))
            }
            "patchbay/rewind/execute" => self.rewind(&params).await,
            _ => Err(RpcError::new(-32601, format!("method not found: {method}"))),
        }
    }

    /// Drop a prompt and everything after it; with `mode: "all"` also put back the files they changed.
    async fn rewind(&self, params: &Value) -> Result<Value, RpcError> {
        let s = self.need_session(params)?;
        if s.busy.load(Ordering::SeqCst) {
            return Err(RpcError::invalid_params("a turn is running; stop it first"));
        }
        let target = params["targetPromptIndex"].as_u64().ok_or_else(|| RpcError::invalid_params("targetPromptIndex is required"))? as usize;
        let revert_files = params["mode"] == "all";
        let (msg_index, failed) = {
            let mut prompts = s.prompts.lock().unwrap();
            if target >= prompts.len() {
                return Err(RpcError::invalid_params("no rewind point for that prompt"));
            }
            let mut failed = Vec::new();
            if revert_files {
                for p in prompts[target..].iter().rev() {
                    for (path, old) in &p.files {
                        let r = match old {
                            Some(bytes) => std::fs::write(path, bytes),
                            None => match std::fs::remove_file(path) {
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                                r => r,
                            },
                        };
                        if let Err(e) = r {
                            failed.push(format!("{}: {e}", path.display()));
                        }
                    }
                }
            }
            let msg_index = prompts[target].msg_index;
            prompts.truncate(target);
            (msg_index, failed)
        };
        s.state.lock().await.messages.truncate(msg_index);
        s.store.append(&crate::store::Entry::Truncate { len: msg_index });
        s.files.lock().unwrap().clear();
        s.notified.lock().unwrap().clear();
        if failed.is_empty() {
            Ok(json!({}))
        } else {
            Err(RpcError::internal(format!("could not restore some files: {}", failed.join("; "))))
        }
    }

    fn announce_commands(&self, s: &Session) {
        s.update(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [{
                "name": "compact",
                "description": "Summarize the conversation to free up context",
                "input": {"hint": "what the summary should focus on"},
            }],
        }));
    }

    async fn session_info(&self, s: &Session) -> Value {
        let st = s.state.lock().await;
        json!({
            "models": {
                "currentModelId": st.model_id,
                "availableModels": self.host.models().iter().map(|m| json!({"modelId": m.id, "name": m.name})).collect::<Vec<_>>(),
            },
            "modes": {
                "currentModeId": st.mode.id(),
                "availableModes": [
                    {"id": "default", "name": "Default", "description": "Asks before changing files or running commands"},
                    {"id": "plan", "name": "Plan", "description": "Reads and plans; changes nothing until the plan is approved"},
                ],
            },
        })
    }

    async fn attach_mcp(&self, s: &Session, params: &Value) {
        if !params["mcpServers"].as_array().is_some_and(|a| !a.is_empty()) {
            return;
        }
        let set = McpSet::connect(&params["mcpServers"]).await;
        for p in &set.problems {
            s.text_chunk("agent_thought_chunk", &format!("MCP server unavailable: {p}\n"));
        }
        *s.mcp.lock().await = Arc::new(set);
    }

    async fn new_session(&self, params: &Value) -> Result<Value, RpcError> {
        let cwd = PathBuf::from(param_str(params, "cwd")?);
        let model = params["_meta"]["model"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| self.host.default_model())
            .ok_or_else(|| RpcError::internal("add a provider and model in Settings first"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let s = Session::new(id.clone(), cwd, self.host.clone(), self.peer.clone(), model.clone(), Mode::Default);
        s.save_meta(&model, Mode::Default);
        s.keep_open.store(self.subagents_cap.load(Ordering::SeqCst), Ordering::SeqCst);
        self.attach_mcp(&s, params).await;
        self.sessions.lock().unwrap().insert(id.clone(), s.clone());
        let mut info = self.session_info(&s).await;
        info["sessionId"] = json!(id);
        self.announce_commands(&s);
        Ok(info)
    }

    async fn load_session(&self, params: &Value) -> Result<Value, RpcError> {
        let id = param_str(params, "sessionId")?.to_owned();
        if let Some(s) = self.session(&id) {
            return Ok(self.session_info(&s).await);
        }
        let probe = crate::store::Store::new(&self.host.data_dir(), &id);
        if !probe.exists() {
            return Err(RpcError::invalid_params(format!("no stored session {id}")));
        }
        let entries = probe.load();
        let restored = session::restore(&entries);
        let cwd = params["cwd"].as_str().map(PathBuf::from).or_else(|| restored.cwd.clone().map(PathBuf::from));
        let Some(cwd) = cwd else { return Err(RpcError::invalid_params("cwd is required")) };
        let known = self.host.models();
        // A model that was removed since falls back to the default.
        let model = restored
            .model
            .filter(|m| known.iter().any(|k| &k.id == m))
            .or_else(|| self.host.default_model())
            .ok_or_else(|| RpcError::internal("add a provider and model in Settings first"))?;
        let mode = restored.mode.unwrap_or(Mode::Default);
        let s = Session::new(id.clone(), cwd, self.host.clone(), self.peer.clone(), model.clone(), mode);
        s.keep_open.store(self.subagents_cap.load(Ordering::SeqCst), Ordering::SeqCst);
        *s.prompts.lock().unwrap() = session::prompts_of(&restored.messages);
        s.state.lock().await.messages = restored.messages;
        s.replay(&entries);
        self.attach_mcp(&s, params).await;
        self.sessions.lock().unwrap().insert(id, s.clone());
        self.announce_commands(&s);
        Ok(self.session_info(&s).await)
    }

    async fn prompt(&self, params: &Value) -> Result<Value, RpcError> {
        let s = self.need_session(params)?;
        if s.busy.swap(true, Ordering::SeqCst) {
            return Err(RpcError::invalid_params("a turn is already running in this session"));
        }
        let token = CancellationToken::new();
        *s.cancel.lock().unwrap() = Some(token.clone());
        let result = turn::run_turn(s.clone(), &params["prompt"], token).await;
        *s.cancel.lock().unwrap() = None;
        s.busy.store(false, Ordering::SeqCst);
        match result {
            Ok(r) => Ok(json!({
                "stopReason": r.stop_reason,
                "usage": {
                    "inputTokens": r.usage.input,
                    "outputTokens": r.usage.output,
                    "thoughtTokens": r.usage.reasoning,
                    "cachedReadTokens": r.usage.cached_read,
                    "cachedWriteTokens": r.usage.cache_write,
                    "totalTokens": r.usage.input + r.usage.output,
                },
            })),
            Err(e) => Err(RpcError::internal(e)),
        }
    }
}
