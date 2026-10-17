//! Requests from phones (remote members) that use this node as their gateway: the lists they
//! show, chats run here on their behalf, and Code commands passed to each session's host. See
//! docs/design/mobile.md, section 2.

use std::io::Write;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::chat::{self, ChatEvent};
use crate::mesh::remote::{EV_CHAT, Live};
use crate::{AppState, MeshRpc, attachments, code_attach_local, prefs, providers, remote_host, spawn_chat};

/// Code commands a phone may send, each naming its session in `id`.
const CODE_METHODS: &[&str] = &[
    "code.start",
    "code.is_running",
    "code.attach",
    "code.stop",
    "code.prompt",
    "code.cancel",
    "code.set_model",
    "code.set_config_option",
    "code.set_mode",
    "code.rewind",
    "code.permission",
    "code.files",
];
/// Largest attachment a phone may upload.
const MAX_UPLOAD: u64 = 200 * 1024 * 1024;

pub(crate) async fn call(state: &AppState, from: &str, method: &str, p: &Value) -> Result<Value> {
    let s = |k: &str| p[k].as_str().map(str::to_owned).ok_or_else(|| anyhow!("missing {k}"));
    let db = &state.db;
    Ok(match method {
        "remote.index" => index(state)?,
        "remote.catalog" => json!({
            "providers": providers::list_with_key_flag(db)?,
            "models": db.list_models()?,
            "defaultModel": db.get_setting("default_model")?,
            "defaultSystemPrompt": prefs::load(db).default_system_prompt,
        }),
        "remote.messages" => json!(db.list_messages(&s("conversationId")?)?),
        "remote.switch_branch" => {
            let leaf = db.latest_leaf(&s("messageId")?)?;
            db.set_active_leaf(&s("conversationId")?, &leaf)?;
            Value::Null
        }
        "remote.create_conversation" => json!(db.create_conversation(p["modelRef"].as_str())?),
        "remote.update_conversation" => {
            let id = s("id")?;
            if let Some(pinned) = p["pinned"].as_bool() {
                db.set_conversation_pinned(&id, pinned)?;
            }
            db.update_conversation(&id, p["title"].as_str(), p["modelRef"].as_str(), p["systemPrompt"].as_str())?;
            Value::Null
        }
        "remote.chat_send" | "remote.chat_regenerate" => {
            let conv = s("conversationId")?;
            let turn = if method == "remote.chat_send" {
                let parent = p.get("parentId").filter(|v| !v.is_null()).map(|v| v.as_str().filter(|x| !x.is_empty()).map(str::to_owned));
                chat::Turn::Message {
                    text: s("text")?,
                    attachment_ids: serde_json::from_value(p["attachmentIds"].clone()).unwrap_or_default(),
                    parent,
                }
            } else {
                chat::Turn::Regenerate { user_message_id: s("userMessageId")? }
            };
            let effort = p["reasoningEffort"].as_str().map(str::to_owned);
            spawn_chat(state, conv.clone(), s("modelRef")?, turn, effort, show_here(state, from, &conv));
            Value::Null
        }
        "remote.chat_cancel" => {
            let conv = s("conversationId")?;
            match state.mesh.streamer_of(&conv).filter(|n| n != state.mesh.node_id()) {
                Some(node) => {
                    state.mesh.rpc(&node, "chat.cancel", json!({ "conversationId": conv })).await?;
                }
                None => state.chat.cancel(&conv),
            }
            Value::Null
        }
        "remote.streams" => json!(state.mesh.all_streams()),
        "remote.transcript" => json!(db.code_transcript(&s("id")?)?),
        "remote.snapshot" => {
            let id = s("id")?;
            let has = db.code_transcript(&id)?.is_some_and(|t| !t.is_empty());
            json!({ "attach": attach_any(state, &id).await, "hasTranscript": has })
        }
        "remote.code" => forward_code(state, from, p).await?,
        "remote.usage" => json!(db.code_usage(&s("id")?)?.view()),
        "remote.update_code_session" => {
            if let Some(t) = p["title"].as_str() {
                db.update_code_session(&s("id")?, None, Some(t), None)?;
            }
            Value::Null
        }
        "remote.attach_put" => upload(state, p).await?,
        "remote.thumbnail" => {
            let att = db.get_attachment(&s("id")?)?.ok_or_else(|| anyhow!("attachment not found"))?;
            if !std::path::Path::new(&att.path).exists() {
                state.mesh.ensure_file(&att.sha256, &att.name).await?;
            }
            json!(tokio::task::spawn_blocking(move || attachments::thumbnail(&att)).await??)
        }
        other => bail!("unknown request {other}"),
    })
}

/// Conversations, Code sessions with what each is doing now, and the replies streaming.
fn index(state: &AppState) -> Result<Value> {
    let sessions = state.db.list_code_sessions()?;
    let mut live = state.mesh.live();
    // Sessions hosted here: the agent manager knows better than the events seen so far.
    let me = state.mesh.node_id().to_owned();
    {
        let agents = state.live.lock().unwrap();
        for s in sessions.iter().filter(|s| s.host_node.as_deref().is_none_or(|h| h == me)) {
            let running = state.code.is_running(&s.id);
            let l = live.entry(s.id.clone()).or_insert_with(Live::default);
            if !running {
                *l = Live { status: "exited".into(), last: l.last, ..Live::default() };
                continue;
            }
            if l.status.is_empty() || l.status == "exited" {
                l.status = "ready".into();
            }
            l.busy = state.code.is_busy(&s.id);
            l.permissions = agents.get(&s.id).map(|a| a.permissions.iter().map(|(r, _)| r.clone()).collect()).unwrap_or_default();
        }
    }
    Ok(json!({
        "conversations": state.db.list_conversations()?,
        "codeSessions": sessions,
        "live": live,
        "streams": state.mesh.all_streams(),
    }))
}

/// A reply run here for a phone also shows in this node's own window, credited to the phone.
fn show_here(state: &AppState, phone: &str, conv: &str) -> chat::ChatSink {
    let (emit, phone, conv) = (state.emit.clone(), phone.to_owned(), conv.to_owned());
    let label = format!("this computer, for {}", state.mesh.member_name(&phone));
    Arc::new(move |ev: ChatEvent| {
        let payload = json!({ "conversationId": conv, "streamer": phone, "streamerName": label, "event": ev });
        emit(EV_CHAT, payload);
    })
}

/// What a window opening the session needs, from wherever it runs.
async fn attach_any(state: &AppState, id: &str) -> Value {
    let offline = json!({ "running": false, "busy": false, "events": [], "offline": true });
    match remote_host(state, id) {
        Ok(None) => code_attach_local(state, id),
        Ok(Some(host)) if state.mesh.connected(&host) => {
            state.mesh.rpc(&host, "code.attach", json!({ "id": id })).await.unwrap_or(offline)
        }
        _ => offline,
    }
}

/// Run a Code command here when this node hosts the session, else pass it to the host.
async fn forward_code(state: &AppState, from: &str, p: &Value) -> Result<Value> {
    let method = p["method"].as_str().ok_or_else(|| anyhow!("missing method"))?;
    if !CODE_METHODS.contains(&method) {
        bail!("{method} isn't available to a phone");
    }
    let mut params = p["params"].clone();
    let id = params["id"].as_str().ok_or_else(|| anyhow!("missing id"))?.to_owned();
    // The phone shows its own prompts; every other window shows them as the phone's.
    params["origin"] = json!(from);
    match remote_host(state, &id).map_err(|e| anyhow!(e.0))? {
        Some(host) => {
            if method == "code.attach" && !state.mesh.connected(&host) {
                return Ok(json!({ "running": false, "busy": false, "events": [], "offline": true }));
            }
            state.mesh.rpc(&host, method, params).await
        }
        None => {
            use crate::mesh::RpcHandler;
            MeshRpc { state: state.clone() }.call(from.to_owned(), method.to_owned(), params).await
        }
    }
}

/// One chunk of an attachment from a phone; the last one stores it like a picked file.
async fn upload(state: &AppState, p: &Value) -> Result<Value> {
    let id = p["uploadId"].as_str().unwrap_or_default();
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        bail!("bad upload id");
    }
    let name = p["name"].as_str().unwrap_or("attachment").to_owned();
    let offset = p["offset"].as_u64().unwrap_or(0);
    let data = base64::engine::general_purpose::STANDARD.decode(p["data"].as_str().unwrap_or_default())?;
    let dir = state.data_dir.join("uploads");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(id);
    let have = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if have != offset {
        bail!("upload chunk out of order (have {have} bytes, chunk starts at {offset})");
    }
    if offset + data.len() as u64 > MAX_UPLOAD {
        let _ = std::fs::remove_file(&path);
        bail!("attachment too large");
    }
    std::fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(&data)?;
    if !p["done"].as_bool().unwrap_or(false) {
        return Ok(Value::Null);
    }
    let bytes = std::fs::read(&path)?;
    let _ = std::fs::remove_file(&path);
    let (db, data_dir) = (state.db.clone(), state.data_dir.clone());
    let att = tokio::task::spawn_blocking(move || attachments::ingest(&db, &data_dir, &name, &bytes)).await??;
    Ok(json!(att))
}
