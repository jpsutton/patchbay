//! The ACP server: methods in, session updates and permission requests out.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use patchbay_llm::Effort;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use crate::host::Host;
use crate::mcp::McpSet;
use crate::rpc::{self, Incoming, Peer, RpcError};
use crate::session::{self, Mode, Session};
use crate::turn;

type Sessions = Arc<Mutex<HashMap<String, Arc<Session>>>>;

/// The ACP config option that turns minified prompts on and off.
const MINIFY_OPTION: &str = "minify_prompts";
/// The ACP config option that sets the session's reasoning effort.
const EFFORT_OPTION: &str = "effort";

const EFFORTS: [(Effort, &str, &str); 7] = [
    (Effort::None, "Off", "No extended thinking"),
    (Effort::Minimal, "Minimal", "Barely any thinking"),
    (Effort::Low, "Low", "Quick answers"),
    (Effort::Medium, "Medium", "Balanced"),
    (Effort::High, "High", "Thinks longer on hard problems"),
    (Effort::Xhigh, "Extra high", "Thinks longer still"),
    (Effort::Max, "Max", "As much thinking as the model allows"),
];

fn effort_name(e: Effort) -> &'static str {
    EFFORTS.iter().find(|(x, ..)| *x == e).map_or("", |(_, n, _)| n)
}

/// The session's options. `model_default` is the effort the current model is set up with.
fn config_options(s: &Session, model_default: Option<Effort>) -> Value {
    let default_name = match model_default {
        Some(e) => format!("Model default ({})", effort_name(e)),
        None => "Model default".to_owned(),
    };
    let mut efforts = vec![json!({"value": "default", "name": default_name, "description": "The effort set for this model in Settings"})];
    efforts.extend(EFFORTS.iter().map(|(e, name, desc)| json!({"value": e.as_str(), "name": name, "description": desc})));
    let effort = *s.effort.lock().unwrap();
    json!([{
        "id": EFFORT_OPTION,
        "name": "Thinking",
        "description": "How hard the model reasons before it answers. Models without reasoning ignore it.",
        "category": "thought_level",
        "type": "select",
        "currentValue": effort.map_or("default", Effort::as_str),
        "options": efforts,
    }, {
        "id": MINIFY_OPTION,
        "name": "Compact prompts",
        "description": "Send the agent's own prompts in a shortened form to save input tokens, and ask it to reply tersely, like caveman.",
        "type": "select",
        "currentValue": if s.minifying() { "on" } else { "off" },
        "options": [
            {"value": "off", "name": "Off", "description": "Full prompts"},
            {"value": "on", "name": "On", "description": "Terse prompts and terse replies"},
        ],
    }])
}

impl Agent {
    /// The effort `model_id` is set up with.
    fn model_effort(&self, model_id: &str) -> Option<Effort> {
        self.host.models().into_iter().find(|m| m.id == model_id).and_then(|m| m.effort)
    }

    async fn options_of(&self, s: &Session) -> Value {
        let model = s.state.lock().await.model_id.clone();
        config_options(s, self.model_effort(&model))
    }
}

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
                "_meta": {"steering": {"supported": true}},
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
                // The "model default" effort follows the model.
                s.update(json!({"sessionUpdate": "config_option_update", "configOptions": self.options_of(&s).await}));
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
            "session/set_config_option" => {
                let s = self.need_session(&params)?;
                match param_str(&params, "configId")? {
                    MINIFY_OPTION => {
                        let on = match param_str(&params, "value")? {
                            "on" | "true" => true,
                            "off" | "false" => false,
                            other => return Err(RpcError::invalid_params(format!("{other:?} is not on or off"))),
                        };
                        *s.minify.lock().unwrap() = Some(on);
                    }
                    EFFORT_OPTION => {
                        let effort = match param_str(&params, "value")? {
                            "default" => None,
                            v => Some(Effort::parse(v).ok_or_else(|| RpcError::invalid_params(format!("unknown effort {v:?}")))?),
                        };
                        *s.effort.lock().unwrap() = effort;
                    }
                    other => return Err(RpcError::invalid_params(format!("unknown config option {other}"))),
                }
                let (model, mode) = {
                    let st = s.state.lock().await;
                    (st.model_id.clone(), st.mode)
                };
                s.save_meta(&model, mode);
                Ok(json!({"configOptions": self.options_of(&s).await}))
            }
            "session/steering" => self.steer(&params),
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
            // Stop one running tool call; the turn goes on and the model sees it was stopped.
            "patchbay/cancel_tool" => {
                let s = self.need_session(&params)?;
                let id = param_str(&params, "toolCallId")?;
                let token = s.calls.lock().unwrap().get(id).cloned();
                if let Some(t) = &token {
                    t.cancel();
                }
                Ok(json!({"cancelled": token.is_some()}))
            }
            // Stop one background shell started with Bash `run_in_background`.
            "patchbay/kill_shell" => {
                let s = self.need_session(&params)?;
                Ok(json!({"killed": s.shells.kill(param_str(&params, "shellId")?)}))
            }
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

    /// `session/steering`: a message for the running turn, read before its next model request.
    fn steer(&self, params: &Value) -> Result<Value, RpcError> {
        let s = self.need_session(params)?;
        let parts = turn::blocks_to_parts(&params["prompt"]);
        if parts.is_empty() {
            return Err(RpcError::invalid_params("the message is empty"));
        }
        let mut inbox = s.steer.lock().unwrap();
        if !inbox.open {
            // No turn will read it: the client sends it as a prompt of its own.
            return Ok(json!({"outcome": "promptRequired"}));
        }
        inbox.queued.push(parts);
        drop(inbox);
        s.steered.notify_one();
        Ok(json!({"outcome": "injected"}))
    }

    /// Load the session's Claude Code setup, run its SessionStart hooks (`source`: startup or
    /// resume) and announce its slash commands.
    async fn start_claude(&self, s: &Session, source: &str) {
        s.reload_claude();
        let setup = s.claude();
        if setup.has_hooks("SessionStart") {
            let mode = s.state.lock().await.mode;
            let mut input = s.hook_input(mode);
            input["source"] = json!(source);
            let r = crate::claude::run_hooks(&setup.hooks, "SessionStart", None, input, &s.cwd).await;
            s.hook_notes(&r.notes);
            s.start_context.lock().unwrap().extend(r.context);
        }
        s.announce_commands();
    }

    async fn session_info(&self, s: &Session) -> Value {
        let st = s.state.lock().await;
        json!({
            "configOptions": config_options(s, self.model_effort(&st.model_id)),
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

    /// Connect the MCP servers the client passed and the ones in the user's Claude Code setup
    /// (the client's win on a name clash). Call after `start_claude`.
    async fn attach_mcp(&self, s: &Session, params: &Value) {
        let mut servers: Vec<Value> = params["mcpServers"].as_array().cloned().unwrap_or_default();
        for e in &s.claude().mcp {
            if !servers.iter().any(|v| v["name"] == e.name.as_str()) {
                servers.push(e.config.clone());
            }
        }
        if servers.is_empty() {
            return;
        }
        let set = McpSet::connect(&Value::Array(servers)).await;
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
        if let Some(on) = params["_meta"]["minifyPrompts"].as_bool() {
            *s.minify.lock().unwrap() = Some(on);
        }
        s.save_meta(&model, Mode::Default);
        s.keep_open.store(self.subagents_cap.load(Ordering::SeqCst), Ordering::SeqCst);
        self.start_claude(&s, "startup").await;
        self.attach_mcp(&s, params).await;
        self.sessions.lock().unwrap().insert(id.clone(), s.clone());
        let mut info = self.session_info(&s).await;
        info["sessionId"] = json!(id);
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
        // The session's own choice is remembered; a caller may set it again on load.
        *s.minify.lock().unwrap() = params["_meta"]["minifyPrompts"].as_bool().or(restored.minify);
        *s.effort.lock().unwrap() = restored.effort;
        *s.prompts.lock().unwrap() = session::prompts_of(&restored.messages);
        s.state.lock().await.messages = restored.messages;
        s.replay(&entries);
        self.start_claude(&s, "resume").await;
        self.attach_mcp(&s, params).await;
        self.sessions.lock().unwrap().insert(id, s.clone());
        Ok(self.session_info(&s).await)
    }

    async fn prompt(&self, params: &Value) -> Result<Value, RpcError> {
        let s = self.need_session(params)?;
        if s.busy.swap(true, Ordering::SeqCst) {
            return Err(RpcError::invalid_params("a turn is already running in this session"));
        }
        let token = CancellationToken::new();
        *s.cancel.lock().unwrap() = Some(token.clone());
        s.steer.lock().unwrap().open = true;
        let result = turn::run_turn(s.clone(), &params["prompt"], token).await;
        // A message steered in after the turn's last step (it stopped early, failed or was
        // cancelled) still goes into the history, so the next turn sees what the user saw sent.
        let left = s.take_steered(true);
        s.steer.lock().unwrap().open = false;
        if !left.is_empty() {
            let msg = patchbay_llm::Message::with_parts(patchbay_llm::Role::User, left);
            s.store.append(&crate::store::Entry::Msg { m: msg.clone() });
            s.state.lock().await.messages.push(msg);
        }
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
