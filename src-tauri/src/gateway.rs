//! Requests from phones (remote members) that use this node as their gateway: the lists they
//! show, chats run here (or on the computer the phone picked) on their behalf, and Code commands
//! passed to each session's host. See
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
    "code.unqueue",
    "code.send_now",
    "code.set_model",
    "code.set_config_option",
    "code.authenticate",
    "code.set_mode",
    "code.set_permission_mode",
    "code.set_agent",
    "code.rewind",
    "code.permission",
    "code.files",
    "code.auto_title",
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
            if let Some(archived) = p["archived"].as_bool() {
                db.set_conversation_archived(&id, archived)?;
            }
            db.update_conversation(&id, p["title"].as_str(), p["modelRef"].as_str(), p["systemPrompt"].as_str())?;
            Value::Null
        }
        "remote.chat_send" | "remote.chat_regenerate" => {
            // The phone may pick the computer that makes the call; by default it's this one.
            match p["node"].as_str().filter(|n| !n.is_empty() && *n != state.mesh.node_id()) {
                Some(node) => {
                    if !state.mesh.connected(node) {
                        bail!("{} is offline", state.mesh.member_name(node));
                    }
                    let mut params = p.clone();
                    params["kind"] = json!(method.trim_start_matches("remote.chat_"));
                    params["phone"] = json!(from);
                    state.mesh.rpc(node, "chat.run", params).await?;
                }
                None => run_chat_local(state, from, method == "remote.chat_send", p).await?,
            }
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
            let id = s("id")?;
            if let Some(t) = p["title"].as_str() {
                db.set_code_session_title(&id, t, true)?;
            }
            if let Some(pinned) = p["pinned"].as_bool() {
                db.set_code_session_pinned(&id, pinned)?;
            }
            if let Some(archived) = p["archived"].as_bool() {
                crate::archive_code_session(state, &id, archived).await?;
            }
            Value::Null
        }
        // Starting a Code session on a chosen computer.
        "remote.new_session_options" => on_host(state, p, "code.new_options").await?,
        "remote.host_info" => on_host(state, p, "host.info").await?,
        "remote.list_dirs" => on_host(state, p, "fs.dirs").await?,
        "remote.create_code_session" => on_host(state, p, "code.create").await?,
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

/// Answer a request about the computer named in `host`: here, or passed to it.
pub(crate) async fn on_host(state: &AppState, p: &Value, method: &str) -> Result<Value> {
    let host = p["host"].as_str().ok_or_else(|| anyhow!("missing host"))?;
    if host == state.mesh.node_id() {
        return match method {
            "code.new_options" => new_session_options_local(state).await,
            "host.info" => Ok(host_info_local()),
            "code.create" => create_local(state, p).map(|s| json!(s)),
            "fs.dirs" => list_dirs_local(p["path"].as_str()),
            other => bail!("unknown request {other}"),
        };
    }
    if !state.mesh.connected(host) {
        bail!("{} is offline", state.mesh.member_name(host));
    }
    state.mesh.rpc(host, method, p.clone()).await
}

/// Most recent folders offered when starting a session.
const RECENT_FOLDERS: usize = 12;
/// Most folders one listing returns.
const MAX_DIRS: usize = 500;

/// What a phone needs to start a Code session on this computer: the agents installed here,
/// the default one, the home folder to browse from, and folders recent sessions here used.
pub(crate) async fn new_session_options_local(state: &AppState) -> Result<Value> {
    let prefs = prefs::load(&state.db);
    let paths = prefs.agent_paths.clone();
    let agents = tokio::task::spawn_blocking(move || crate::code::agents::detect_all(&paths)).await?;
    let me = state.mesh.node_id();
    let mut recent: Vec<String> = Vec::new();
    for s in state.db.list_code_sessions()? {
        let here = s.host_node.as_deref().is_none_or(|h| h == me);
        if here && !recent.contains(&s.cwd) && std::path::Path::new(&s.cwd).is_dir() {
            recent.push(s.cwd);
        }
        if recent.len() == RECENT_FOLDERS {
            break;
        }
    }
    Ok(json!({
        "agents": agents,
        "defaultAgent": prefs.default_agent,
        "home": crate::code::agents::home_dir().map(|h| h.to_string_lossy().into_owned()),
        "recent": recent,
    }))
}

/// Which build runs on this computer, for a session's info dialog: the Patchbay version, the
/// operating system (`std::env::consts::OS`: "macos", "linux", "windows") and the CPU architecture.
pub(crate) fn host_info_local() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    })
}

/// The folders in `path` (default: home), for picking where a session runs. Only folders, only
/// inside the home folder, and no hidden ones: a phone gets no more of the disk than it needs.
pub(crate) fn list_dirs_local(path: Option<&str>) -> Result<Value> {
    let home = crate::code::agents::home_dir()
        .ok_or_else(|| anyhow!("no home folder"))?
        .canonicalize()?;
    let dir = match path.filter(|p| !p.is_empty()) {
        Some(p) => std::path::Path::new(p).canonicalize()?,
        None => home.clone(),
    };
    if !dir.starts_with(&home) {
        bail!("only folders inside your home folder can be listed");
    }
    let mut dirs: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.metadata().is_ok_and(|m| m.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    dirs.sort_by_key(|n| n.to_lowercase());
    let truncated = dirs.len() > MAX_DIRS;
    dirs.truncate(MAX_DIRS);
    let parent = (dir != home).then(|| dir.parent().map(|p| p.to_string_lossy().into_owned())).flatten();
    Ok(json!({
        "path": dir.to_string_lossy(),
        "parent": parent,
        "home": home.to_string_lossy(),
        "dirs": dirs,
        "truncated": truncated,
    }))
}

/// A new Code session hosted on this computer, for a phone.
pub(crate) fn create_local(state: &AppState, p: &Value) -> Result<crate::db::CodeSession> {
    let cwd = p["cwd"].as_str().ok_or_else(|| anyhow!("missing cwd"))?;
    let agent = p["agent"].as_str().map(str::to_owned);
    crate::create_code_session_local(state, cwd, p["modelRef"].as_str(), agent)
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

/// Longest a chat turn passed from a gateway waits for what it needs to sync here.
const SYNC_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run a phone's chat turn here (`send`: a new message, else a regenerate). A turn passed from
/// its gateway may arrive before the new conversation or attachments do, so it waits for them.
pub(crate) async fn run_chat_local(state: &AppState, phone: &str, send: bool, p: &Value) -> Result<()> {
    let s = |k: &str| p[k].as_str().map(str::to_owned).ok_or_else(|| anyhow!("missing {k}"));
    let conv = s("conversationId")?;
    let attachment_ids: Vec<String> = serde_json::from_value(p["attachmentIds"].clone()).unwrap_or_default();
    let user_message = if send { None } else { Some(s("userMessageId")?) };
    let db = &state.db;
    let ready = || -> Result<bool> {
        Ok(db.get_conversation(&conv)?.is_some()
            && attachment_ids.iter().all(|a| db.get_attachment(a).ok().flatten().is_some())
            && user_message.as_ref().is_none_or(|m| db.path_to(&conv, m).is_ok_and(|path| !path.is_empty())))
    };
    let deadline = std::time::Instant::now() + SYNC_WAIT;
    while !ready()? && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let turn = match user_message {
        None => {
            let parent = p.get("parentId").filter(|v| !v.is_null()).map(|v| v.as_str().filter(|x| !x.is_empty()).map(str::to_owned));
            chat::Turn::Message { text: s("text")?, attachment_ids, parent }
        }
        Some(user_message_id) => chat::Turn::Regenerate { user_message_id },
    };
    let effort = p["reasoningEffort"].as_str().map(str::to_owned);
    spawn_chat(state, conv.clone(), s("modelRef")?, turn, effort, show_here(state, phone, &conv));
    Ok(())
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
            // A browser sign-in waits for the user.
            if method == "code.authenticate" {
                return state.mesh.rpc_long(&host, method, params).await;
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
