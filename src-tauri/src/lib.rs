mod attachments;
mod chat;
mod code;
mod db;
mod desktop;
mod files;
mod gateway;
mod handoff;
mod mesh;
mod prefs;
mod providers;
mod terminal;
mod usage;
#[cfg(test)]
mod agent_live_tests;
#[cfg(test)]
mod smoke_tests;
#[cfg(test)]
mod mesh_app_tests;

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{Manager, State};

use crate::chat::{ChatEvent, ChatManager};
use crate::code::CodeManager;
use crate::code::acp::AgentCmd;
use crate::db::{
    Attachment, ChatUsageRow, CodeSession, CodeUsageRow, Conversation, Db, McpServer, Message, Model,
    PermissionRule, Prompt, Provider, SearchHit, new_id,
};
use crate::mesh::Mesh;
use crate::terminal::{TermEvent, TerminalManager};

/// Everything commands share. Cheap to clone (all handles), so the mesh's request handler and
/// background tasks hold their own copy.
#[derive(Clone)]
pub struct AppState {
    db: Arc<Db>,
    data_dir: PathBuf,
    chat: Arc<ChatManager>,
    code: Arc<CodeManager>,
    terminals: Arc<TerminalManager>,
    mesh: Mesh,
    /// Sends an event to this node's window.
    emit: mesh::node::UiSink,
    /// Sessions whose checkpoint is being fetched right now.
    pulling: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Per running session: the events a newly attached viewer needs to catch up.
    live: Arc<std::sync::Mutex<std::collections::HashMap<String, LiveAgent>>>,
    /// Terminals of sessions on other nodes, shown here: terminal id -> output channel and
    /// an ordered queue of input for the host.
    remote_terms: Arc<std::sync::Mutex<std::collections::HashMap<String, RemoteTerm>>>,
}

impl AppState {
    fn new(db: Arc<Db>, data_dir: PathBuf, mesh: Mesh, emit: mesh::node::UiSink) -> Self {
        let state = AppState {
            db,
            data_dir,
            chat: Arc::default(),
            code: Arc::default(),
            terminals: Arc::default(),
            mesh: mesh.clone(),
            emit: emit.clone(),
            pulling: Arc::default(),
            live: Arc::default(),
            remote_terms: Arc::default(),
        };
        mesh.set_ui(emit);
        mesh.set_handler(Arc::new(MeshRpc { state: state.clone() }));
        state
    }
}

struct RemoteTerm {
    channel: Channel<TermEvent>,
    input: tokio::sync::mpsc::UnboundedSender<(String, serde_json::Value)>,
}

/// What a window opening an already-running session (here or on another node) must replay.
#[derive(Default)]
struct LiveAgent {
    ready: Option<serde_json::Value>,
    permissions: Vec<(String, serde_json::Value)>,
    usage: Option<serde_json::Value>,
}

/// Errors cross the IPC boundary as plain strings.
#[derive(Debug)]
pub struct CmdError(String);

impl Serialize for CmdError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<E: Into<anyhow::Error>> From<E> for CmdError {
    fn from(e: E) -> Self {
        CmdError(format!("{:#}", e.into()))
    }
}

type CmdResult<T> = Result<T, CmdError>;

// ---------------- providers & models ----------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderInput {
    id: Option<String>,
    name: String,
    kind: db::ProviderKind,
    base_url: String,
    /// `None` keeps the stored key; `Some("")` clears it.
    api_key: Option<String>,
}

#[tauri::command]
fn list_providers(state: State<'_, AppState>) -> CmdResult<Vec<Provider>> {
    Ok(providers::list_with_key_flag(&state.db)?)
}

#[tauri::command]
fn save_provider(state: State<'_, AppState>, input: ProviderInput) -> CmdResult<Provider> {
    let p = Provider {
        id: input.id.unwrap_or_else(new_id),
        name: input.name.trim().to_owned(),
        kind: input.kind,
        base_url: providers::normalize_base_url(&input.base_url),
        has_key: false,
    };
    if p.name.is_empty() || p.base_url.is_empty() {
        return Err(CmdError("name and base URL are required".into()));
    }
    state.db.upsert_provider(&p, None)?;
    match input.api_key.as_deref() {
        Some("") => {
            providers::delete_key(&p.id);
            state.db.clear_inline_key(&p.id)?;
        }
        Some(k) => providers::store_key(&state.db, &p, k.trim())?,
        None => {}
    }
    state.mesh.secrets_changed();
    let saved = providers::list_with_key_flag(&state.db)?
        .into_iter()
        .find(|x| x.id == p.id)
        .unwrap_or(p);
    Ok(saved)
}

#[tauri::command]
fn delete_provider(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    providers::delete_key(&id);
    state.db.delete_provider(&id)?;
    state.mesh.secrets_changed();
    Ok(())
}

#[tauri::command]
async fn discover_models(
    state: State<'_, AppState>,
    provider_id: String,
) -> CmdResult<Vec<providers::DiscoveredModel>> {
    let (p, inline) = state
        .db
        .get_provider(&provider_id)?
        .ok_or_else(|| CmdError("provider not found".into()))?;
    let key = providers::load_key(&p.id, inline.as_deref());
    Ok(providers::discover_models(&p.kind, &p.base_url, key.as_deref()).await?)
}

#[tauri::command]
fn list_models(state: State<'_, AppState>) -> CmdResult<Vec<Model>> {
    Ok(state.db.list_models()?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelInput {
    id: Option<String>,
    provider_id: String,
    model_id: String,
    display_name: Option<String>,
    vision: Option<bool>,
    context_window: Option<i64>,
    /// `None` keeps the stored params.
    params: Option<db::ModelParams>,
}

#[tauri::command]
fn save_model(state: State<'_, AppState>, input: ModelInput) -> CmdResult<Model> {
    let model_id = input.model_id.trim().to_owned();
    if model_id.is_empty() {
        return Err(CmdError("model id is required".into()));
    }
    let models = state.db.list_models()?;
    let existing = input
        .id
        .as_deref()
        .and_then(|id| models.iter().find(|m| m.id == id))
        .or_else(|| {
            models
                .iter()
                .find(|m| m.provider_id == input.provider_id && m.model_id == model_id)
        })
        .cloned();
    let params = match input.params {
        Some(p) => {
            p.validate().map_err(CmdError)?;
            p
        }
        None => existing.as_ref().map(|e| e.params.clone()).unwrap_or_default(),
    };
    let m = Model {
        params,
        id: input
            .id
            .or_else(|| existing.map(|e| e.id))
            .unwrap_or_else(new_id),
        provider_id: input.provider_id,
        display_name: input
            .display_name
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| model_id.clone()),
        model_id,
        vision: input.vision.unwrap_or(true),
        context_window: input.context_window,
    };
    state.db.upsert_model(&m)?;
    Ok(m)
}

#[tauri::command]
fn delete_model(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    Ok(state.db.delete_model(&id)?)
}

#[tauri::command]
fn get_preferences(state: State<'_, AppState>) -> prefs::Preferences {
    prefs::load(&state.db)
}

#[tauri::command]
fn set_preferences(app: tauri::AppHandle, state: State<'_, AppState>, prefs: prefs::Preferences) -> CmdResult<()> {
    prefs.validate().map_err(CmdError)?;
    desktop::validate_shortcut(&prefs.quick_entry_shortcut).map_err(CmdError)?;
    let old = prefs::load(&state.db);
    if old.quick_entry_shortcut.trim() != prefs.quick_entry_shortcut.trim()
        && let Err(e) = desktop::set_quick_shortcut(&app, &prefs.quick_entry_shortcut)
    {
        // Put the previous one back so the saved setting matches what is registered.
        let _ = desktop::set_quick_shortcut(&app, &old.quick_entry_shortcut);
        return Err(e.into());
    }
    prefs::save(&state.db, &prefs)?;
    if old.title_bar != prefs.title_bar {
        desktop::apply_title_bar(&app, &prefs.title_bar);
    }
    Ok(())
}

#[tauri::command]
fn get_setting(state: State<'_, AppState>, key: String) -> CmdResult<Option<String>> {
    Ok(state.db.get_setting(&key)?)
}

#[tauri::command]
fn set_setting(state: State<'_, AppState>, key: String, value: String) -> CmdResult<()> {
    Ok(state.db.set_setting(&key, &value)?)
}

// ---------------- chat ----------------

#[tauri::command]
fn list_conversations(state: State<'_, AppState>) -> CmdResult<Vec<Conversation>> {
    Ok(state.db.list_conversations()?)
}

#[tauri::command]
fn create_conversation(
    state: State<'_, AppState>,
    model_ref: Option<String>,
) -> CmdResult<Conversation> {
    Ok(state.db.create_conversation(model_ref.as_deref())?)
}

#[tauri::command]
fn update_conversation(
    state: State<'_, AppState>,
    id: String,
    title: Option<String>,
    model_ref: Option<String>,
    system_prompt: Option<String>,
    pinned: Option<bool>,
    archived: Option<bool>,
) -> CmdResult<()> {
    if let Some(p) = pinned {
        state.db.set_conversation_pinned(&id, p)?;
    }
    if let Some(a) = archived {
        state.db.set_conversation_archived(&id, a)?;
    }
    Ok(state.db.update_conversation(
        &id,
        title.as_deref(),
        model_ref.as_deref(),
        system_prompt.as_deref(),
    )?)
}

#[tauri::command]
fn delete_conversation(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.chat.cancel(&id);
    Ok(state.db.delete_conversation(&id)?)
}

#[tauri::command]
fn list_messages(state: State<'_, AppState>, conversation_id: String) -> CmdResult<Vec<Message>> {
    Ok(state.db.list_messages(&conversation_id)?)
}

/// Make the newest branch through `message_id` the visible one.
#[tauri::command]
fn switch_branch(state: State<'_, AppState>, conversation_id: String, message_id: String) -> CmdResult<()> {
    let leaf = state.db.latest_leaf(&message_id)?;
    Ok(state.db.set_active_leaf(&conversation_id, &leaf)?)
}

#[tauri::command]
fn search_messages(state: State<'_, AppState>, query: String, limit: Option<usize>) -> CmdResult<Vec<SearchHit>> {
    Ok(state.db.search_messages(&query, limit.unwrap_or(30).min(200))?)
}

/// Run one chat turn; `local` gets its progress (the asking window, or this window on a phone's
/// behalf), and every other node gets it live through the mesh.
fn spawn_chat(
    state: &AppState,
    conversation_id: String,
    model_ref: String,
    turn: chat::Turn,
    reasoning_effort: Option<String>,
    local: chat::ChatSink,
) {
    let db = state.db.clone();
    let data_dir = state.data_dir.clone();
    let chat = state.chat.clone();
    let mesh = state.mesh.clone();
    let sink: chat::ChatSink = {
        let mesh = mesh.clone();
        let conv = conversation_id.clone();
        Arc::new(move |ev: ChatEvent| {
            mesh.chat_event(&conv, serde_json::to_value(&ev).unwrap_or_default());
            local(ev);
        })
    };
    let emit = chat::Emit(sink);
    let err_emit = emit.clone();
    // Run detached so the invoke resolves immediately; progress flows over the channel.
    tauri::async_runtime::spawn(async move {
        if let Err(e) =
            chat::send(db, data_dir, chat, conversation_id, model_ref, turn, reasoning_effort, emit, mesh).await
        {
            let _ = err_emit.send(ChatEvent::Error {
                message: format!("{e:#}"),
            });
        }
    });
}

/// `parent_id`: omitted = continue the visible branch; `""` = new first message; an id = branch
/// off under that message (used when editing).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn send_chat_message(
    state: State<'_, AppState>,
    conversation_id: String,
    model_ref: String,
    text: String,
    attachment_ids: Vec<String>,
    parent_id: Option<String>,
    reasoning_effort: Option<String>,
    on_event: Channel<ChatEvent>,
) -> CmdResult<()> {
    let parent = parent_id.map(|p| Some(p).filter(|p| !p.is_empty()));
    let turn = chat::Turn::Message { text, attachment_ids, parent };
    spawn_chat(&state, conversation_id, model_ref, turn, reasoning_effort, channel_sink(on_event));
    Ok(())
}

fn channel_sink(ch: Channel<ChatEvent>) -> chat::ChatSink {
    Arc::new(move |ev| {
        let _ = ch.send(ev);
    })
}

#[tauri::command]
fn regenerate_reply(
    state: State<'_, AppState>,
    conversation_id: String,
    model_ref: String,
    user_message_id: String,
    reasoning_effort: Option<String>,
    on_event: Channel<ChatEvent>,
) -> CmdResult<()> {
    let turn = chat::Turn::Regenerate { user_message_id };
    spawn_chat(&state, conversation_id, model_ref, turn, reasoning_effort, channel_sink(on_event));
    Ok(())
}

/// Stop a reply, wherever in the mesh it is streaming.
#[tauri::command]
async fn cancel_chat(state: State<'_, AppState>, conversation_id: String) -> CmdResult<()> {
    match state.mesh.streamer_of(&conversation_id).filter(|n| n != state.mesh.node_id()) {
        Some(node) => {
            state.mesh.rpc(&node, "chat.cancel", serde_json::json!({ "conversationId": conversation_id })).await?;
        }
        None => state.chat.cancel(&conversation_id),
    }
    Ok(())
}

// ---------------- attachments ----------------

#[tauri::command]
async fn add_attachment_path(state: State<'_, AppState>, path: String) -> CmdResult<Attachment> {
    let db = state.db.clone();
    let dir = state.data_dir.clone();
    Ok(tokio::task::spawn_blocking(move || {
        attachments::ingest_path(&db, &dir, std::path::Path::new(&path))
    })
    .await??)
}

#[tauri::command]
async fn add_attachment_bytes(
    state: State<'_, AppState>,
    name: String,
    data_base64: String,
) -> CmdResult<Attachment> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data_base64)?;
    let db = state.db.clone();
    let dir = state.data_dir.clone();
    Ok(
        tokio::task::spawn_blocking(move || attachments::ingest(&db, &dir, &name, &bytes))
            .await??,
    )
}

#[tauri::command]
async fn attachment_thumbnail(state: State<'_, AppState>, id: String) -> CmdResult<String> {
    let att = state
        .db
        .get_attachment(&id)?
        .ok_or_else(|| CmdError("attachment not found".into()))?;
    Ok(tokio::task::spawn_blocking(move || attachments::thumbnail(&att)).await??)
}

// ---------------- code ----------------
//
// A Code session runs on one node, its host. Commands for a session hosted elsewhere in the mesh
// go to that node as RPCs (`MeshRpc` serves them there); agent events come back as mesh events.

#[tauri::command]
fn list_code_sessions(state: State<'_, AppState>) -> CmdResult<Vec<CodeSession>> {
    Ok(state.db.list_code_sessions()?)
}

#[tauri::command]
fn create_code_session(
    state: State<'_, AppState>,
    cwd: String,
    model_ref: Option<String>,
    agent: Option<String>,
) -> CmdResult<CodeSession> {
    if !std::path::Path::new(&cwd).is_dir() {
        return Err(CmdError(format!("{cwd} is not a directory")));
    }
    let agent = agent.unwrap_or_else(|| prefs::load(&state.db).default_agent);
    if code::agents::AgentKind::parse(&agent).is_none() {
        return Err(CmdError(format!("unknown coding agent {agent}")));
    }
    Ok(state.db.create_code_session(&cwd, model_ref.as_deref(), &agent)?)
}

/// The coding agents Patchbay can drive, and whether each is installed.
#[tauri::command]
async fn list_agents(state: State<'_, AppState>) -> CmdResult<Vec<code::agents::AgentInfo>> {
    let paths = prefs::load(&state.db).agent_paths;
    Ok(tokio::task::spawn_blocking(move || code::agents::detect_all(&paths)).await?)
}

#[tauri::command]
async fn update_code_session(
    state: State<'_, AppState>,
    id: String,
    title: Option<String>,
    model_ref: Option<String>,
    pinned: Option<bool>,
    activity: Option<String>,
    archived: Option<bool>,
) -> CmdResult<()> {
    if let Some(p) = pinned {
        state.db.set_code_session_pinned(&id, p)?;
    }
    if let Some(a) = archived {
        archive_code_session(&state, &id, a).await?;
    }
    if let Some(a) = activity {
        state.db.set_code_session_activity(&id, &a)?;
    }
    if title.is_none() && model_ref.is_none() {
        return Ok(());
    }
    Ok(state
        .db
        .update_code_session(&id, None, title.as_deref(), model_ref.as_deref())?)
}

#[tauri::command]
fn delete_code_session(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.code.stop(&id);
    // Must match the id `TerminalPane` opens with.
    state.terminals.close(&format!("term-{id}"));
    Ok(state.db.delete_code_session(&id)?)
}

/// Archive or restore a Code session. Archiving ends its agent first, wherever it runs; a host
/// that's offline has nothing reachable to stop, and the archive still syncs to it.
async fn archive_code_session(state: &AppState, id: &str, archived: bool) -> anyhow::Result<()> {
    if archived {
        match remote_host(state, id).map_err(|e| anyhow::anyhow!(e.0))? {
            Some(host) if state.mesh.connected(&host) => {
                state.mesh.rpc(&host, "code.stop", serde_json::json!({ "id": id })).await?;
            }
            Some(_) => {}
            None => state.code.stop(id),
        }
    }
    state.db.set_code_session_archived(id, archived)
}

/// The node hosting session `id`, when that isn't this one.
fn remote_host(state: &AppState, id: &str) -> CmdResult<Option<String>> {
    let s = state
        .db
        .get_code_session(id)?
        .ok_or_else(|| CmdError("code session not found".into()))?;
    Ok(s.host_node.filter(|h| h != state.mesh.node_id()))
}

/// Refuse to act on a session another node now hosts (a fencing check for remote commands).
fn ensure_local_host(state: &AppState, id: &str) -> anyhow::Result<()> {
    if let Some(h) = remote_host(state, id).map_err(|e| anyhow::anyhow!(e.0))? {
        anyhow::bail!("this session now runs on {}", state.mesh.member_name(&h));
    }
    Ok(())
}

/// Agent events go to this window and, live, to every other node. A finished turn also tells
/// the other nodes to fetch a checkpoint, so the session survives this node going away.
fn code_sink(state: &AppState) -> code::acp::Sink {
    use code::acp::CodeEvent;
    let state = state.clone();
    Arc::new(move |env| {
        let mesh = &state.mesh;
        let turn_over = match &env.event {
            CodeEvent::PromptDone { .. } => true,
            CodeEvent::Status { state, .. } => state == "exited",
            _ => false,
        };
        if turn_over {
            state.code.set_busy(&env.code_session_id, false);
        }
        {
            let mut live = state.live.lock().unwrap();
            let id = env.code_session_id.clone();
            let ev = serde_json::to_value(&env.event).unwrap_or_default();
            match &env.event {
                CodeEvent::SessionReady { .. } => live.entry(id).or_default().ready = Some(ev),
                CodeEvent::Usage { .. } => live.entry(id).or_default().usage = Some(ev),
                CodeEvent::Permission { request_id, .. } => {
                    live.entry(id).or_default().permissions.push((request_id.clone(), ev))
                }
                CodeEvent::PermissionResolved { request_id } => {
                    if let Some(l) = live.get_mut(&id) {
                        l.permissions.retain(|(r, _)| r != request_id);
                    }
                }
                CodeEvent::Status { state, .. } if state == "exited" => {
                    live.remove(&id);
                }
                _ => {}
            }
        }
        if matches!(env.event, CodeEvent::PromptDone { .. }) {
            let p = serde_json::json!({ "id": env.code_session_id, "host": mesh.node_id() });
            mesh.broadcast_event("mesh://checkpoint", p);
        }
        // Stamped with its point, so phones and windows can resume from it.
        let v = mesh.broadcast_event(code::acp::EVENT, serde_json::to_value(&env).unwrap_or_default());
        (state.emit)(code::acp::EVENT, v);
    })
}

async fn code_start_local(state: &AppState, id: String) -> anyhow::Result<()> {
    ensure_local_host(state, &id)?;
    if state.code.is_moving(&id) {
        anyhow::bail!("this session is being moved to another computer");
    }
    let sink = code_sink(state);
    let (code, db, dir) = (state.code.clone(), state.db.clone(), state.data_dir.clone());
    // `start` may block briefly while a previous agent for this session shuts down.
    tokio::task::spawn_blocking(move || code.start(sink, db, &dir, &id)).await??;
    Ok(())
}

/// Start (or restart) the agent for a code session. Output arrives as `code://event`.
#[tauri::command]
async fn code_start(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        state.mesh.rpc(&host, "code.start", serde_json::json!({ "id": id })).await?;
        return Ok(());
    }
    Ok(code_start_local(&state, id).await?)
}

/// Catch-up for a window opening a running session: whether it runs and is mid-turn, plus the
/// events that set up its controls (session ready, open permission requests, usage).
fn code_attach_local(state: &AppState, id: &str) -> serde_json::Value {
    let running = state.code.is_running(id);
    let live = state.live.lock().unwrap();
    let mut events = Vec::new();
    if running && let Some(l) = live.get(id) {
        events.extend(l.ready.clone());
        events.extend(l.permissions.iter().map(|(_, e)| e.clone()));
        events.extend(l.usage.clone());
    }
    serde_json::json!({ "running": running, "busy": state.code.is_busy(id), "events": events })
}

#[tauri::command]
async fn code_attach(state: State<'_, AppState>, id: String) -> CmdResult<serde_json::Value> {
    if let Some(host) = remote_host(&state, &id)? {
        if !state.mesh.connected(&host) {
            return Ok(serde_json::json!({ "running": false, "busy": false, "events": [], "offline": true }));
        }
        return Ok(state.mesh.rpc(&host, "code.attach", serde_json::json!({ "id": id })).await?);
    }
    Ok(code_attach_local(&state, &id))
}

#[tauri::command]
async fn code_is_running(state: State<'_, AppState>, id: String) -> CmdResult<bool> {
    if let Some(host) = remote_host(&state, &id)? {
        if !state.mesh.connected(&host) {
            return Ok(false);
        }
        let v = state.mesh.rpc(&host, "code.is_running", serde_json::json!({ "id": id })).await?;
        return Ok(v.as_bool().unwrap_or(false));
    }
    Ok(state.code.is_running(&id))
}

#[tauri::command]
async fn code_stop(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        state.mesh.rpc(&host, "code.stop", serde_json::json!({ "id": id })).await?;
        return Ok(());
    }
    state.code.stop(&id);
    Ok(())
}

async fn code_prompt_local(
    state: &AppState,
    id: String,
    text: String,
    attachment_ids: Vec<String>,
    mentions: Vec<String>,
    origin: String,
) -> anyhow::Result<()> {
    ensure_local_host(state, &id)?;
    let mut blocks = Vec::new();
    // @-mentioned project files go as standard ACP resource links; the agent reads them itself.
    for path in mentions {
        let p = std::path::Path::new(&path);
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
        blocks.push(serde_json::json!({ "type": "resource_link", "name": name, "uri": attachments::file_uri(p) }));
    }
    let mut atts = Vec::new();
    for aid in &attachment_ids {
        let att = state
            .db
            .get_attachment(aid)?
            .ok_or_else(|| anyhow::anyhow!("attachment not found"))?;
        if !std::path::Path::new(&att.path).exists() {
            state.mesh.ensure_file(&att.sha256, &att.name).await?;
        }
        let dir = state.data_dir.clone();
        let a = att.clone();
        blocks.extend(tokio::task::spawn_blocking(move || attachments::to_acp_blocks(&dir, &a)).await??);
        atts.push(att);
    }
    if !text.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": text }));
    }
    // A session that moved without its agent state gets the earlier conversation once.
    let note_key = format!("mesh.handoff.{id}");
    if let Some(note) = state.db.get_setting(&note_key)? {
        blocks.insert(0, serde_json::json!({ "type": "text", "text": note }));
    }
    // Bumps updated_at so the session sorts to the top of the sidebar.
    state.db.update_code_session(&id, None, None, None)?;
    state.code.send(&id, AgentCmd::Prompt { blocks })?;
    state.code.set_busy(&id, true);
    if state.db.get_setting(&note_key)?.is_some() {
        state.db.with_conn(|c| c.execute("DELETE FROM settings WHERE key = ?1", [&note_key]))?;
    }
    // Every viewer shows the prompt; the window that sent it already has.
    code_sink(state)(code::acp::Envelope {
        code_session_id: id,
        event: code::acp::CodeEvent::UserPrompt { text, origin, attachments: serde_json::to_value(&atts)? },
    });
    Ok(())
}

#[tauri::command]
async fn code_prompt(
    state: State<'_, AppState>,
    id: String,
    text: String,
    attachment_ids: Vec<String>,
    mentions: Option<Vec<String>>,
) -> CmdResult<()> {
    let mentions = mentions.unwrap_or_default();
    if let Some(host) = remote_host(&state, &id)? {
        let params = serde_json::json!({ "id": id, "text": text, "attachmentIds": attachment_ids, "mentions": mentions });
        state.mesh.rpc(&host, "code.prompt", params).await?;
        return Ok(());
    }
    let me = state.mesh.node_id().to_owned();
    Ok(code_prompt_local(&state, id, text, attachment_ids, mentions, me).await?)
}

#[tauri::command]
async fn code_cancel(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        state.mesh.rpc(&host, "code.cancel", serde_json::json!({ "id": id })).await?;
        return Ok(());
    }
    Ok(state.code.send(&id, AgentCmd::Cancel)?)
}

async fn code_set_model_local(state: &AppState, id: &str, model_id: String) -> anyhow::Result<()> {
    ensure_local_host(state, id)?;
    state.code.set_model(id, model_id.clone()).await?;
    let session = state.db.get_code_session(id)?;
    let patchbay_models = session
        .as_ref()
        .and_then(|s| code::agents::AgentKind::parse(&s.agent))
        .is_some_and(|k| k.uses_patchbay_models());
    if patchbay_models {
        if let Some(r) = code::grok_config::model_ref_from_key(&model_id) {
            state.db.update_code_session(id, None, None, Some(r))?;
        }
    } else {
        // Remembered so the next agent process for this session reselects it.
        state.db.set_code_session_agent_model(id, &model_id)?;
    }
    Ok(())
}

#[tauri::command]
async fn code_set_model(state: State<'_, AppState>, id: String, model_id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        state.mesh.rpc(&host, "code.set_model", serde_json::json!({ "id": id, "modelId": model_id })).await?;
        return Ok(());
    }
    Ok(code_set_model_local(&state, &id, model_id).await?)
}

#[tauri::command]
async fn code_set_config_option(
    state: State<'_, AppState>,
    id: String,
    config_id: String,
    value: String,
) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        let params = serde_json::json!({ "id": id, "configId": config_id, "value": value });
        state.mesh.rpc(&host, "code.set_config_option", params).await?;
        return Ok(());
    }
    Ok(state.code.set_config_option(&id, config_id, value).await?)
}

/// Switch a session that hasn't started yet to another agent. The agent here stops; whoever asked
/// starts the new one, as when opening the session.
fn code_set_agent_local(state: &AppState, id: &str, agent: &str, model_ref: Option<&str>) -> anyhow::Result<()> {
    ensure_local_host(state, id)?;
    if code::agents::AgentKind::parse(agent).is_none() {
        anyhow::bail!("unknown coding agent {agent}");
    }
    let session = state.db.get_code_session(id)?.ok_or_else(|| anyhow::anyhow!("code session not found"))?;
    // The transcript is the UI's (JSON entries); any user turn means the old agent has the history.
    let started = state.db.code_transcript(id)?.is_some_and(|t| t.contains(r#""kind":"user""#)) || state.code.is_busy(id);
    if started {
        anyhow::bail!("this session already has messages; start a new session to use another agent");
    }
    state.code.stop(id);
    state.db.set_code_session_agent(id, agent)?;
    // Grok runs on Patchbay's models: a session made for another agent may have none yet.
    if session.model_ref.is_none()
        && let Some(m) = model_ref
    {
        state.db.update_code_session(id, None, None, Some(m))?;
    }
    Ok(())
}

#[tauri::command]
async fn code_set_agent(state: State<'_, AppState>, id: String, agent: String, model_ref: Option<String>) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        let p = serde_json::json!({ "id": id, "agent": agent, "modelRef": model_ref });
        state.mesh.rpc(&host, "code.set_agent", p).await?;
        return Ok(());
    }
    Ok(code_set_agent_local(&state, &id, &agent, model_ref.as_deref())?)
}

#[tauri::command]
async fn code_set_mode(state: State<'_, AppState>, id: String, mode_id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        state.mesh.rpc(&host, "code.set_mode", serde_json::json!({ "id": id, "modeId": mode_id })).await?;
        return Ok(());
    }
    Ok(state.code.set_mode(&id, mode_id).await?)
}

#[tauri::command]
async fn code_rewind(
    state: State<'_, AppState>,
    id: String,
    ordinal: usize,
    preview: String,
    revert_files: bool,
) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        let params = serde_json::json!({ "id": id, "ordinal": ordinal, "preview": preview, "revertFiles": revert_files });
        state.mesh.rpc(&host, "code.rewind", params).await?;
        return Ok(());
    }
    Ok(state.code.rewind(&id, ordinal, preview, revert_files).await?)
}

#[tauri::command]
fn code_usage(state: State<'_, AppState>, id: String) -> CmdResult<usage::UsageView> {
    Ok(state.db.code_usage(&id)?.view())
}

/// Answer a permission request; `id` (the session) routes it to the node running the agent.
#[tauri::command]
async fn code_permission_respond(
    state: State<'_, AppState>,
    request_id: String,
    option_id: Option<String>,
    id: Option<String>,
) -> CmdResult<()> {
    if let Some(id) = id
        && let Some(host) = remote_host(&state, &id)?
    {
        let params = serde_json::json!({ "requestId": request_id, "optionId": option_id });
        state.mesh.rpc(&host, "code.permission", params).await?;
        return Ok(());
    }
    state.code.respond_permission(&request_id, option_id);
    Ok(())
}


// ---------------- moving sessions between nodes ----------------

fn grok_home(state: &AppState) -> PathBuf {
    state.data_dir.join("grok")
}

/// Commits the target already has at `cwd`, so the bundle can leave them out.
fn local_commits(cwd: &str) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "for-each-ref", "--count=100", "--format=%(objectname)", "refs/heads", "refs/remotes"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).lines().map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// Host side: package a session for another node (`kind` = handoff | checkpoint).
async fn package_local(state: &AppState, id: &str, kind: &str, have: Vec<String>) -> anyhow::Result<serde_json::Value> {
    ensure_local_host(state, id)?;
    if kind == "handoff" {
        if state.code.is_busy(id) {
            anyhow::bail!("the agent is in the middle of a turn; move the session once it finishes");
        }
        state.code.set_moving(id, true);
        // Let the agent flush its session files before they're copied.
        let code = state.code.clone();
        let sid = id.to_owned();
        tokio::task::spawn_blocking(move || code.stop_and_wait(&sid, std::time::Duration::from_secs(5))).await?;
    }
    let s = state.db.get_code_session(id)?.ok_or_else(|| anyhow::anyhow!("no such session"))?;
    let (dir, home, kind2) = (state.data_dir.clone(), grok_home(state), kind.to_owned());
    let res = tokio::task::spawn_blocking(move || handoff::package(&dir, &home, &s, &kind2, &have)).await?;
    match res {
        Ok((out, meta)) => {
            let token = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(serde_json::json!({ "token": token, "meta": meta }))
        }
        Err(e) => {
            state.code.set_moving(id, false);
            Err(e)
        }
    }
}

/// Copy a package's files from `host` into a local inbox directory.
async fn fetch_package(state: &AppState, host: &str, token: &str, meta: &handoff::Meta) -> anyhow::Result<PathBuf> {
    use base64::Engine;
    let dir = handoff::inbox(&state.data_dir).join(token);
    tokio::fs::create_dir_all(&dir).await?;
    for name in &meta.files {
        let mut f = tokio::fs::File::create(dir.join(name)).await?;
        let mut offset = 0u64;
        loop {
            let r = state
                .mesh
                .rpc(host, "code.fetch", serde_json::json!({ "token": token, "name": name, "offset": offset }))
                .await?;
            let chunk = base64::engine::general_purpose::STANDARD.decode(r["data"].as_str().unwrap_or_default())?;
            tokio::io::AsyncWriteExt::write_all(&mut f, &chunk).await?;
            offset += chunk.len() as u64;
            if r["eof"].as_bool().unwrap_or(true) || chunk.is_empty() {
                break;
            }
        }
    }
    Ok(dir)
}

/// Target side: pull session `id` from its host into `cwd` here and take over the lease.
async fn import_local(state: &AppState, id: &str, cwd: &str) -> anyhow::Result<Option<String>> {
    let s = state.db.get_code_session(id)?.ok_or_else(|| anyhow::anyhow!("no such session"))?;
    let me = state.mesh.node_id().to_owned();
    let host = s.host_node.clone().filter(|h| h != &me).ok_or_else(|| anyhow::anyhow!("this session already runs here"))?;
    let host_name = state.mesh.member_name(&host);
    if !state.mesh.connected(&host) {
        anyhow::bail!("{host_name} is offline; use Take over to continue from its last checkpoint");
    }
    let r = state
        .mesh
        .rpc_long(&host, "code.package", serde_json::json!({ "id": id, "kind": "handoff", "have": local_commits(cwd) }))
        .await?;
    let token = r["token"].as_str().unwrap_or_default().to_owned();
    let meta: handoff::Meta = serde_json::from_value(r["meta"].clone())?;
    let result: anyhow::Result<Option<String>> = async {
        let dir = fetch_package(state, &host, &token, &meta).await?;
        let (home, target, from) = (grok_home(state), PathBuf::from(cwd), host_name.clone());
        let m = meta.clone();
        let d = dir.clone();
        let (restored, note) = tokio::task::spawn_blocking(move || handoff::unpack(&d, &m, &home, &target, &from)).await??;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        finish_takeover(state, &s, cwd, restored, &host_name)?;
        Ok(note)
    }
    .await;
    let moved = result.is_ok();
    let _ = state.mesh.rpc(&host, "code.package_done", serde_json::json!({ "id": id, "token": token, "moved": moved })).await;
    result
}

/// Record that this node now hosts the session; without agent state, queue the hand-off note.
fn finish_takeover(state: &AppState, s: &CodeSession, cwd: &str, agent_restored: bool, from_name: &str) -> anyhow::Result<()> {
    if !agent_restored {
        let transcript = state.db.code_transcript(&s.id)?;
        state.db.set_setting(&format!("mesh.handoff.{}", s.id), &handoff::handoff_prompt(transcript.as_deref(), from_name))?;
    }
    let acp = if agent_restored { s.acp_session_id.as_deref() } else { None };
    state.db.set_code_session_host(&s.id, state.mesh.node_id(), cwd, acp)?;
    Ok(())
}

/// Move a session to `target` (any node, this one included), into folder `cwd` there.
#[tauri::command]
async fn code_move(state: State<'_, AppState>, id: String, target: String, cwd: String) -> CmdResult<Option<String>> {
    let cwd = cwd.trim().to_owned();
    if cwd.is_empty() {
        return Err(CmdError("choose a folder on the target computer".into()));
    }
    if target == state.mesh.node_id() {
        return Ok(import_local(&state, &id, &cwd).await?);
    }
    let r = state.mesh.rpc_long(&target, "code.import", serde_json::json!({ "id": id, "cwd": cwd })).await?;
    Ok(r.as_str().map(str::to_owned))
}

/// Continue a session here from the last checkpoint this node holds, when its host is gone.
async fn take_over_local(state: &AppState, id: &str, cwd: &str) -> anyhow::Result<Option<String>> {
    let s = state.db.get_code_session(id)?.ok_or_else(|| anyhow::anyhow!("no such session"))?;
    let me = state.mesh.node_id().to_owned();
    let host = s.host_node.clone().filter(|h| h != &me).ok_or_else(|| anyhow::anyhow!("this session already runs here"))?;
    let host_name = state.mesh.member_name(&host);
    if state.mesh.connected(&host) {
        anyhow::bail!("{host_name} is online; move the session instead");
    }
    let (dir, home, target) = (state.data_dir.clone(), grok_home(state), PathBuf::from(cwd));
    let (sid, from) = (id.to_owned(), host_name.clone());
    let (restored, note) = if handoff::checkpoint(&state.data_dir, id).is_some() {
        tokio::task::spawn_blocking(move || handoff::restore_checkpoint(&dir, &home, &sid, &target, &from)).await??
    } else if target.is_dir() {
        (false, Some(format!("No checkpoint from {host_name} reached this computer, so only the conversation came along.")))
    } else {
        anyhow::bail!("no checkpoint from {host_name} is here and {} doesn't exist", target.display());
    };
    finish_takeover(state, &s, cwd, restored, &host_name)?;
    Ok(note)
}

#[tauri::command]
async fn code_take_over(state: State<'_, AppState>, id: String, cwd: String) -> CmdResult<Option<String>> {
    Ok(take_over_local(&state, &id, cwd.trim()).await?)
}

/// Checkpoints of other nodes' sessions this node holds: session id -> taken at (ms).
#[tauri::command]
fn code_checkpoints(state: State<'_, AppState>) -> CmdResult<std::collections::HashMap<String, i64>> {
    let mut out = std::collections::HashMap::new();
    for s in state.db.list_code_sessions()? {
        if let Some(m) = handoff::checkpoint(&state.data_dir, &s.id) {
            out.insert(s.id, m.created_at);
        }
    }
    Ok(out)
}

/// Fetch the latest checkpoint of a session hosted on `host`.
async fn pull_checkpoint(state: AppState, id: String, host: String) {
    if !state.pulling.lock().unwrap().insert(id.clone()) {
        return;
    }
    let mut have = handoff::checkpoint_have(&state.data_dir, &id);
    let mut result = Err(anyhow::anyhow!("not attempted"));
    for _ in 0..2 {
        result = async {
            let r = state
                .mesh
                .rpc_long(&host, "code.package", serde_json::json!({ "id": id, "kind": "checkpoint", "have": have }))
                .await?;
            let token = r["token"].as_str().unwrap_or_default().to_owned();
            let meta: handoff::Meta = serde_json::from_value(r["meta"].clone())?;
            let dir = fetch_package(&state, &host, &token, &meta).await;
            let _ = state.mesh.rpc(&host, "code.package_done", serde_json::json!({ "id": id, "token": token, "moved": false })).await;
            let dir = dir?;
            let data = state.data_dir.clone();
            let d = dir.clone();
            tokio::task::spawn_blocking(move || handoff::store_checkpoint(&data, &d, &meta)).await??;
            let _ = tokio::fs::remove_dir_all(&dir).await;
            anyhow::Ok(())
        }
        .await;
        // The host may have pruned what we had: retry once with a full copy.
        if result.is_ok() || have.is_empty() {
            break;
        }
        have.clear();
    }
    if let Err(e) = result {
        tracing::warn!("mesh: checkpoint of {id}: {e:#}");
    }
    state.pulling.lock().unwrap().remove(&id);
}

// ---------------- mesh ----------------

/// Serves other nodes' requests: remote control of sessions hosted here.
struct MeshRpc {
    state: AppState,
}

fn param<'a>(p: &'a serde_json::Value, k: &str) -> anyhow::Result<&'a str> {
    p[k].as_str().ok_or_else(|| anyhow::anyhow!("missing {k}"))
}

#[async_trait::async_trait]
impl mesh::RpcHandler for MeshRpc {
    async fn call(&self, from: String, method: String, p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        use serde_json::{Value, json};
        let state = &self.state;
        let id = || param(&p, "id").map(str::to_owned);
        match method.as_str() {
            "code.start" => code_start_local(state, id()?).await.map(|_| Value::Null),
            "code.is_running" => Ok(json!(state.code.is_running(&id()?))),
            "code.attach" => Ok(code_attach_local(state, &id()?)),
            "code.stop" => {
                state.code.stop(&id()?);
                Ok(Value::Null)
            }
            "code.prompt" => {
                let atts: Vec<String> = serde_json::from_value(p["attachmentIds"].clone()).unwrap_or_default();
                let mentions: Vec<String> = serde_json::from_value(p["mentions"].clone()).unwrap_or_default();
                let text = p["text"].as_str().unwrap_or_default().to_owned();
                // A gateway passing on a phone's prompt names the phone.
                let origin = p["origin"].as_str().map(str::to_owned).unwrap_or(from);
                code_prompt_local(state, id()?, text, atts, mentions, origin).await.map(|_| Value::Null)
            }
            "code.cancel" => {
                ensure_local_host(state, &id()?)?;
                state.code.send(&id()?, AgentCmd::Cancel).map(|_| Value::Null)
            }
            "code.set_model" => code_set_model_local(state, &id()?, param(&p, "modelId")?.to_owned()).await.map(|_| Value::Null),
            "code.set_config_option" => {
                ensure_local_host(state, &id()?)?;
                let (cid, value) = (param(&p, "configId")?.to_owned(), param(&p, "value")?.to_owned());
                state.code.set_config_option(&id()?, cid, value).await.map(|_| Value::Null)
            }
            "code.set_agent" => {
                code_set_agent_local(state, &id()?, param(&p, "agent")?, p["modelRef"].as_str()).map(|_| Value::Null)
            }
            "code.set_mode" => {
                ensure_local_host(state, &id()?)?;
                state.code.set_mode(&id()?, param(&p, "modeId")?.to_owned()).await.map(|_| Value::Null)
            }
            "code.rewind" => {
                ensure_local_host(state, &id()?)?;
                let ordinal = p["ordinal"].as_u64().unwrap_or(0) as usize;
                let preview = p["preview"].as_str().unwrap_or_default().to_owned();
                let revert = p["revertFiles"].as_bool().unwrap_or(false);
                state.code.rewind(&id()?, ordinal, preview, revert).await.map(|_| Value::Null)
            }
            "code.permission" => {
                let opt = p["optionId"].as_str().map(str::to_owned);
                state.code.respond_permission(param(&p, "requestId")?, opt);
                Ok(Value::Null)
            }
            "code.files" => {
                let s = state.db.get_code_session(&id()?)?.ok_or_else(|| anyhow::anyhow!("no such session"))?;
                let files = tokio::task::spawn_blocking(move || files::list(std::path::Path::new(&s.cwd))).await??;
                Ok(json!(files))
            }
            "chat.cancel" => {
                state.chat.cancel(param(&p, "conversationId")?);
                Ok(Value::Null)
            }
            "code.package" => {
                let have: Vec<String> = serde_json::from_value(p["have"].clone()).unwrap_or_default();
                package_local(state, &id()?, param(&p, "kind")?, have).await
            }
            "code.fetch" => {
                let (token, name) = (param(&p, "token")?, param(&p, "name")?);
                if [token, name].iter().any(|x| x.contains(['/', '\\']) || x.starts_with('.')) {
                    anyhow::bail!("bad name");
                }
                let path = handoff::outbox(&state.data_dir).join(token).join(name);
                mesh::node::read_chunk(path, p["offset"].as_u64().unwrap_or(0)).await
            }
            "code.package_done" => {
                let token = param(&p, "token")?;
                if !token.contains(['/', '\\']) && !token.starts_with('.') {
                    let _ = tokio::fs::remove_dir_all(handoff::outbox(&state.data_dir).join(token)).await;
                }
                if p["moved"].as_bool() == Some(false) && token.starts_with("handoff-") {
                    state.code.set_moving(&id()?, false);
                }
                Ok(Value::Null)
            }
            "term.open" => {
                let sid = param(&p, "sessionId")?;
                ensure_local_host(state, sid)?;
                let s = state.db.get_code_session(sid)?.ok_or_else(|| anyhow::anyhow!("no such session"))?;
                // Keyed by requester too, so it never collides with this node's own terminal.
                let key = format!("{}@{from}", param(&p, "termId")?);
                let (mesh, to, term_id) = (state.mesh.clone(), from.clone(), param(&p, "termId")?.to_owned());
                let sink: terminal::TermSink = Arc::new(move |ev| {
                    let payload = json!({ "termId": term_id, "event": ev });
                    mesh.send_event(&to, "mesh://term", payload)
                });
                let shell = prefs::load(&state.db).terminal_shell;
                let (cols, rows) = (p["cols"].as_u64().unwrap_or(80) as u16, p["rows"].as_u64().unwrap_or(24) as u16);
                state.terminals.open(&key, &s.cwd, Some(&shell), cols, rows, sink)?;
                Ok(Value::Null)
            }
            "term.write" => {
                let key = format!("{}@{from}", param(&p, "termId")?);
                state.terminals.write(&key, param(&p, "data")?).map(|_| Value::Null)
            }
            "term.resize" => {
                let key = format!("{}@{from}", param(&p, "termId")?);
                let (cols, rows) = (p["cols"].as_u64().unwrap_or(80) as u16, p["rows"].as_u64().unwrap_or(24) as u16);
                state.terminals.resize(&key, cols, rows).map(|_| Value::Null)
            }
            "term.close" => {
                state.terminals.close(&format!("{}@{from}", param(&p, "termId")?));
                Ok(Value::Null)
            }
            "code.import" => {
                let note = import_local(state, &id()?, param(&p, "cwd")?).await?;
                Ok(json!(note))
            }
            m if m.starts_with("remote.") => gateway::call(state, &from, m, &p).await,
            other => anyhow::bail!("unknown request {other}"),
        }
    }

    fn event(&self, name: &str, payload: &serde_json::Value) {
        if name == "mesh://term" {
            let terms = self.state.remote_terms.lock().unwrap();
            if let Some(t) = payload["termId"].as_str().and_then(|id| terms.get(id))
                && let Ok(ev) = serde_json::from_value::<TermEvent>(payload["event"].clone())
            {
                let _ = t.channel.send(ev);
            }
            return;
        }
        if name != "mesh://checkpoint" {
            return;
        }
        let (Some(id), Some(host)) = (payload["id"].as_str(), payload["host"].as_str()) else { return };
        tokio::spawn(pull_checkpoint(self.state.clone(), id.to_owned(), host.to_owned()));
    }

    fn synced(&self, applied: &mesh::sync::Applied) {
        if !applied.tables.contains("code_sessions") {
            return;
        }
        let state = &self.state;
        let me = state.mesh.node_id().to_owned();
        for (id, prev, prev_cwd) in &applied.host_changes {
            if prev.as_deref() != Some(me.as_str()) {
                continue;
            }
            if state.code.is_moving(id) {
                // We handed it over ourselves.
                state.code.set_moving(id, false);
                continue;
            }
            // Taken over elsewhere while we were away: keep whatever we did since, in the
            // folder the session had here (the synced row now names the new host's folder).
            let Some(cwd) = prev_cwd.clone() else { continue };
            let (emit, id) = (state.emit.clone(), id.clone());
            tokio::spawn(async move {
                let sid = id.clone();
                let cwd = PathBuf::from(cwd);
                let kept = tokio::task::spawn_blocking(move || handoff::keep_orphaned_work(&cwd, &sid)).await;
                if let Ok(Ok(Some(r))) = kept {
                    let msg = format!("This session continued on another computer. Work done here since is saved as git ref {r}.");
                    let env = code::acp::Envelope { code_session_id: id, event: code::acp::CodeEvent::Notice { message: msg } };
                    emit(code::acp::EVENT, serde_json::to_value(env).unwrap_or_default());
                }
            });
        }
        // A session that moved away or was deleted elsewhere stops running here.
        for (t, pk) in &applied.rows {
            if t != "code_sessions" || !state.code.is_running(pk) {
                continue;
            }
            let here = state
                .db
                .get_code_session(pk)
                .ok()
                .flatten()
                .is_some_and(|s| s.host_node.as_deref().is_none_or(|h| h == state.mesh.node_id()));
            if !here {
                state.code.stop(pk);
            }
        }
    }
}

#[tauri::command]
fn mesh_status(state: State<'_, AppState>) -> serde_json::Value {
    state.mesh.status()
}

#[tauri::command]
async fn mesh_create(state: State<'_, AppState>, passphrase: String, name: String) -> CmdResult<()> {
    Ok(state.mesh.create(&passphrase, &name)?)
}

#[tauri::command]
async fn mesh_join(state: State<'_, AppState>, url: String, passphrase: String, name: String) -> CmdResult<()> {
    Ok(state.mesh.join(&url, &passphrase, &name)?)
}

#[tauri::command]
async fn mesh_cancel_join(state: State<'_, AppState>) -> CmdResult<()> {
    state.mesh.cancel_join();
    Ok(())
}

#[tauri::command]
async fn mesh_approve(state: State<'_, AppState>, node_id: String) -> CmdResult<()> {
    Ok(state.mesh.approve(&node_id)?)
}

#[tauri::command]
async fn mesh_reject(state: State<'_, AppState>, node_id: String) -> CmdResult<()> {
    state.mesh.reject(&node_id);
    Ok(())
}

#[tauri::command]
async fn mesh_remove(state: State<'_, AppState>, node_id: String) -> CmdResult<()> {
    Ok(state.mesh.remove(&node_id)?)
}

#[tauri::command]
async fn mesh_leave(state: State<'_, AppState>) -> CmdResult<()> {
    Ok(state.mesh.leave().await?)
}

#[tauri::command]
async fn mesh_set_config(
    state: State<'_, AppState>,
    name: Option<String>,
    listen: Option<String>,
    advertise: Option<Vec<String>>,
) -> CmdResult<()> {
    Ok(state.mesh.set_config(name.as_deref(), listen.as_deref(), advertise)?)
}

#[tauri::command]
fn mesh_add_address(state: State<'_, AppState>, url: String) -> CmdResult<()> {
    Ok(state.mesh.add_address(&url)?)
}

#[tauri::command]
fn mesh_chat_streams(state: State<'_, AppState>) -> Vec<mesh::node::Stream> {
    state.mesh.remote_streams()
}

// ---------------- terminal ----------------
//
// With `session_id`, the terminal runs on the node hosting that session: output arrives as
// `mesh://term` events, and input goes there in order through a per-terminal queue.

#[tauri::command]
async fn term_open(
    state: State<'_, AppState>,
    id: String,
    cwd: String,
    cols: u16,
    rows: u16,
    on_event: Channel<TermEvent>,
    session_id: Option<String>,
) -> CmdResult<()> {
    if let Some(sid) = session_id
        && let Some(host) = remote_host(&state, &sid)?
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, serde_json::Value)>();
        let mesh = state.mesh.clone();
        let h = host.clone();
        tauri::async_runtime::spawn(async move {
            while let Some((method, params)) = rx.recv().await {
                if let Err(e) = mesh.rpc(&h, &method, params).await {
                    tracing::debug!("remote terminal: {e:#}");
                }
            }
        });
        state.remote_terms.lock().unwrap().insert(id.clone(), RemoteTerm { channel: on_event, input: tx });
        let params = serde_json::json!({ "termId": id, "sessionId": sid, "cols": cols, "rows": rows });
        state.mesh.rpc(&host, "term.open", params).await?;
        return Ok(());
    }
    let shell = prefs::load(&state.db).terminal_shell;
    let sink: terminal::TermSink = Arc::new(move |ev| on_event.send(ev).is_ok());
    Ok(state.terminals.open(&id, &cwd, Some(&shell), cols, rows, sink)?)
}

/// Queue input for a remote terminal; false when `id` is local.
fn remote_term(state: &AppState, id: &str, method: &str, params: serde_json::Value) -> bool {
    match state.remote_terms.lock().unwrap().get(id) {
        Some(t) => {
            let _ = t.input.send((method.to_owned(), params));
            true
        }
        None => false,
    }
}

#[tauri::command]
fn term_write(state: State<'_, AppState>, id: String, data: String) -> CmdResult<()> {
    if remote_term(&state, &id, "term.write", serde_json::json!({ "termId": id, "data": data })) {
        return Ok(());
    }
    Ok(state.terminals.write(&id, &data)?)
}

#[tauri::command]
fn term_resize(state: State<'_, AppState>, id: String, cols: u16, rows: u16) -> CmdResult<()> {
    if remote_term(&state, &id, "term.resize", serde_json::json!({ "termId": id, "cols": cols, "rows": rows })) {
        return Ok(());
    }
    Ok(state.terminals.resize(&id, cols, rows)?)
}

#[tauri::command]
fn term_close(state: State<'_, AppState>, id: String) {
    if remote_term(&state, &id, "term.close", serde_json::json!({ "termId": id })) {
        state.remote_terms.lock().unwrap().remove(&id);
        return;
    }
    state.terminals.close(&id);
}

// ---------------- code session extras ----------------

#[tauri::command]
fn get_code_transcript(state: State<'_, AppState>, id: String) -> CmdResult<Option<String>> {
    Ok(state.db.code_transcript(&id)?)
}

/// Cache of the rendered transcript, so a session shows its history before (or without) the
/// agent replaying it.
#[tauri::command]
fn save_code_transcript(state: State<'_, AppState>, id: String, json: String) -> CmdResult<()> {
    const MAX: usize = 4 * 1024 * 1024;
    if json.len() > MAX {
        return Err(CmdError("transcript too large to cache".into()));
    }
    Ok(state.db.set_code_transcript(&id, &json)?)
}

/// Files under `cwd` for @-mentions; with `session_id`, on whichever node hosts that session.
#[tauri::command]
async fn list_project_files(state: State<'_, AppState>, cwd: String, session_id: Option<String>) -> CmdResult<Vec<String>> {
    if let Some(id) = session_id
        && let Some(host) = remote_host(&state, &id)?
    {
        let v = state.mesh.rpc(&host, "code.files", serde_json::json!({ "id": id })).await?;
        return Ok(serde_json::from_value(v)?);
    }
    Ok(tokio::task::spawn_blocking(move || files::list(std::path::Path::new(&cwd))).await??)
}

#[tauri::command]
fn list_mcp_servers(state: State<'_, AppState>) -> CmdResult<Vec<McpServer>> {
    Ok(code::mcp::list_redacted(&state.db)?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpServerInput {
    id: Option<String>,
    name: String,
    enabled: bool,
    config: code::mcp::McpConfig,
}

#[tauri::command]
fn save_mcp_server(state: State<'_, AppState>, input: McpServerInput) -> CmdResult<McpServer> {
    let saved = code::mcp::save(&state.db, input.id, &input.name, input.enabled, input.config)?;
    state.mesh.secrets_changed();
    Ok(saved)
}

#[tauri::command]
fn delete_mcp_server(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    code::mcp::delete(&state.db, &id)?;
    state.mesh.secrets_changed();
    Ok(())
}

#[tauri::command]
fn list_permission_rules(state: State<'_, AppState>) -> CmdResult<Vec<PermissionRule>> {
    Ok(state.db.list_permission_rules()?)
}

#[tauri::command]
fn add_permission_rule(
    state: State<'_, AppState>,
    cwd: String,
    tool_kind: String,
    pattern: String,
) -> CmdResult<PermissionRule> {
    if tool_kind.trim().is_empty() {
        return Err(CmdError("tool kind is required".into()));
    }
    Ok(state.db.add_permission_rule(cwd.trim(), tool_kind.trim(), pattern.trim())?)
}

#[tauri::command]
fn delete_permission_rule(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    Ok(state.db.delete_permission_rule(&id)?)
}

// ---------------- prompts ----------------

#[tauri::command]
fn list_prompts(state: State<'_, AppState>) -> CmdResult<Vec<Prompt>> {
    Ok(state.db.list_prompts()?)
}

#[tauri::command]
fn save_prompt(state: State<'_, AppState>, id: Option<String>, name: String, content: String) -> CmdResult<Prompt> {
    if name.trim().is_empty() {
        return Err(CmdError("name is required".into()));
    }
    Ok(state.db.save_prompt(id.as_deref(), name.trim(), &content)?)
}

#[tauri::command]
fn delete_prompt(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    Ok(state.db.delete_prompt(&id)?)
}

// ---------------- usage ----------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageReport {
    chat: Vec<ChatUsageRow>,
    code: Vec<CodeUsageRow>,
}

#[tauri::command]
fn usage_report(state: State<'_, AppState>, days: u32) -> CmdResult<UsageReport> {
    let since = db::now_ms() - i64::from(days.clamp(1, 3650)) * 86_400_000;
    Ok(UsageReport {
        chat: state.db.chat_usage_since(since)?,
        code: state.db.code_usage_rows()?,
    })
}

// ---------------- quick entry ----------------

/// From the quick-entry window: hand the text to the main window as a new chat.
#[tauri::command]
fn quick_submit(app: tauri::AppHandle, text: String) -> CmdResult<()> {
    use tauri::Emitter;
    desktop::hide_quick(&app);
    desktop::show_main(&app);
    app.emit_to("main", desktop::EV_QUICK_PROMPT, text)?;
    Ok(())
}

#[tauri::command]
fn quick_dismiss(app: tauri::AppHandle) {
    desktop::hide_quick(&app);
}

// ---------------- files ----------------

/// Write a user-chosen export (the path comes from the native save dialog).
#[tauri::command]
fn write_text_file(path: String, contents: String) -> CmdResult<()> {
    Ok(std::fs::write(&path, contents)?)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PATCHBAY_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = Arc::new(Db::open(&data_dir.join("patchbay.sqlite"))?);
            let keys = Arc::new(mesh::keys::OsKeyStore::new(&data_dir));
            let mesh = Mesh::new(db.clone(), data_dir.clone(), keys)?;
            let emit: mesh::node::UiSink = {
                use tauri::Emitter;
                let h = app.handle().clone();
                Arc::new(move |name: &str, payload: serde_json::Value| {
                    let _ = h.emit(name, payload);
                })
            };
            app.manage(AppState::new(db, data_dir, mesh.clone(), emit));
            if mesh.is_member() {
                tauri::async_runtime::spawn(async move { mesh.start() });
            }
            let handle = app.handle().clone();
            // A tray needs a StatusNotifier host on Linux; never let its absence stop the app.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| desktop::build_tray(&handle))) {
                Ok(Err(e)) => tracing::warn!("tray icon unavailable: {e:#}"),
                Err(_) => tracing::warn!("tray icon unavailable"),
                Ok(Ok(())) => {}
            }
            let p = prefs::load(&app.state::<AppState>().db);
            if p.title_bar != "system" {
                desktop::apply_title_bar(&handle, &p.title_bar);
            }
            if let Err(e) = desktop::set_quick_shortcut(&handle, &p.quick_entry_shortcut) {
                tracing::warn!("quick entry shortcut: {e:#}");
            }
            Ok(())
        })
        .on_window_event(|w, ev| {
            // The hidden quick-entry window must not keep the app alive after the main one closes.
            if w.label() == "main" && matches!(ev, tauri::WindowEvent::Destroyed) {
                w.app_handle().exit(0);
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_providers,
            save_provider,
            delete_provider,
            discover_models,
            list_models,
            save_model,
            delete_model,
            get_setting,
            set_setting,
            get_preferences,
            set_preferences,
            list_conversations,
            create_conversation,
            update_conversation,
            delete_conversation,
            list_messages,
            switch_branch,
            search_messages,
            send_chat_message,
            regenerate_reply,
            cancel_chat,
            add_attachment_path,
            add_attachment_bytes,
            attachment_thumbnail,
            list_code_sessions,
            create_code_session,
            update_code_session,
            delete_code_session,
            code_start,
            code_is_running,
            code_stop,
            code_prompt,
            code_cancel,
            code_set_model,
            code_set_mode,
            code_set_agent,
            code_set_config_option,
            list_agents,
            code_permission_respond,
            code_rewind,
            code_usage,
            term_open,
            term_write,
            term_resize,
            term_close,
            write_text_file,
            get_code_transcript,
            save_code_transcript,
            list_project_files,
            list_mcp_servers,
            save_mcp_server,
            delete_mcp_server,
            list_permission_rules,
            add_permission_rule,
            delete_permission_rule,
            list_prompts,
            save_prompt,
            delete_prompt,
            usage_report,
            quick_submit,
            quick_dismiss,
            mesh_status,
            mesh_create,
            mesh_join,
            mesh_cancel_join,
            mesh_approve,
            mesh_reject,
            mesh_remove,
            mesh_leave,
            mesh_set_config,
            mesh_add_address,
            mesh_chat_streams,
            code_move,
            code_take_over,
            code_attach,
            code_checkpoints,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Patchbay")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event
                && let Some(state) = app.try_state::<AppState>()
            {
                state.code.stop_all();
                state.terminals.close_all();
                state.mesh.stop();
            }
        });
}
