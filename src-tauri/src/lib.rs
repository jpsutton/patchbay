mod attachments;
mod chat;
mod code;
mod db;
mod desktop;
mod files;
mod mesh;
mod prefs;
mod providers;
mod terminal;
mod usage;
#[cfg(test)]
mod smoke_tests;

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

pub struct AppState {
    db: Arc<Db>,
    data_dir: PathBuf,
    chat: Arc<ChatManager>,
    code: Arc<CodeManager>,
    terminals: TerminalManager,
    mesh: Mesh,
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
) -> CmdResult<()> {
    if let Some(p) = pinned {
        state.db.set_conversation_pinned(&id, p)?;
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

fn spawn_chat(
    state: &State<'_, AppState>,
    conversation_id: String,
    model_ref: String,
    turn: chat::Turn,
    reasoning_effort: Option<String>,
    on_event: Channel<ChatEvent>,
) {
    let db = state.db.clone();
    let data_dir = state.data_dir.clone();
    let chat = state.chat.clone();
    let mesh = state.mesh.clone();
    // Progress goes to the asking UI and, live, to every other node.
    let sink: chat::ChatSink = {
        let mesh = mesh.clone();
        let conv = conversation_id.clone();
        Arc::new(move |ev: ChatEvent| {
            mesh.chat_event(&conv, serde_json::to_value(&ev).unwrap_or_default());
            let _ = on_event.send(ev);
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
    spawn_chat(&state, conversation_id, model_ref, turn, reasoning_effort, on_event);
    Ok(())
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
    spawn_chat(&state, conversation_id, model_ref, turn, reasoning_effort, on_event);
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
) -> CmdResult<CodeSession> {
    if !std::path::Path::new(&cwd).is_dir() {
        return Err(CmdError(format!("{cwd} is not a directory")));
    }
    Ok(state.db.create_code_session(&cwd, model_ref.as_deref())?)
}

#[tauri::command]
fn update_code_session(
    state: State<'_, AppState>,
    id: String,
    title: Option<String>,
    model_ref: Option<String>,
    pinned: Option<bool>,
    activity: Option<String>,
) -> CmdResult<()> {
    if let Some(p) = pinned {
        state.db.set_code_session_pinned(&id, p)?;
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

/// Agent events go to this window and, live, to every other node.
fn code_sink(app: &tauri::AppHandle, mesh: &Mesh) -> code::acp::Sink {
    use tauri::Emitter;
    let app = app.clone();
    let mesh = mesh.clone();
    Arc::new(move |env| {
        mesh.broadcast_event(code::acp::EVENT, serde_json::to_value(&env).unwrap_or_default());
        let _ = app.emit(code::acp::EVENT, env);
    })
}

async fn code_start_local(app: &tauri::AppHandle, state: &AppState, id: String) -> anyhow::Result<()> {
    ensure_local_host(state, &id)?;
    let sink = code_sink(app, &state.mesh);
    let (code, db, dir) = (state.code.clone(), state.db.clone(), state.data_dir.clone());
    // `start` may block briefly while a previous agent for this session shuts down.
    tokio::task::spawn_blocking(move || code.start(sink, db, &dir, &id)).await??;
    Ok(())
}

/// Start (or restart) the agent for a code session. Output arrives as `code://event`.
#[tauri::command]
async fn code_start(app: tauri::AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(&state, &id)? {
        state.mesh.rpc(&host, "code.start", serde_json::json!({ "id": id })).await?;
        return Ok(());
    }
    Ok(code_start_local(&app, &state, id).await?)
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
    app: &tauri::AppHandle,
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
    // Bumps updated_at so the session sorts to the top of the sidebar.
    state.db.update_code_session(&id, None, None, None)?;
    state.code.send(&id, AgentCmd::Prompt { blocks })?;
    // Every viewer shows the prompt; the window that sent it already has.
    code_sink(app, &state.mesh)(code::acp::Envelope {
        code_session_id: id,
        event: code::acp::CodeEvent::UserPrompt { text, origin, attachments: serde_json::to_value(&atts)? },
    });
    Ok(())
}

#[tauri::command]
async fn code_prompt(
    app: tauri::AppHandle,
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
    Ok(code_prompt_local(&app, &state, id, text, attachment_ids, mentions, me).await?)
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
    if let Some(r) = code::grok_config::model_ref_from_key(&model_id) {
        state.db.update_code_session(id, None, None, Some(r))?;
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

// ---------------- mesh ----------------

/// Serves other nodes' requests: remote control of sessions hosted here.
struct MeshRpc {
    app: tauri::AppHandle,
}

fn param<'a>(p: &'a serde_json::Value, k: &str) -> anyhow::Result<&'a str> {
    p[k].as_str().ok_or_else(|| anyhow::anyhow!("missing {k}"))
}

#[async_trait::async_trait]
impl mesh::RpcHandler for MeshRpc {
    async fn call(&self, from: String, method: String, p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        use serde_json::{Value, json};
        let state = self.app.state::<AppState>();
        let id = || param(&p, "id").map(str::to_owned);
        match method.as_str() {
            "code.start" => code_start_local(&self.app, &state, id()?).await.map(|_| Value::Null),
            "code.is_running" => Ok(json!(state.code.is_running(&id()?))),
            "code.stop" => {
                state.code.stop(&id()?);
                Ok(Value::Null)
            }
            "code.prompt" => {
                let atts: Vec<String> = serde_json::from_value(p["attachmentIds"].clone()).unwrap_or_default();
                let mentions: Vec<String> = serde_json::from_value(p["mentions"].clone()).unwrap_or_default();
                let text = p["text"].as_str().unwrap_or_default().to_owned();
                code_prompt_local(&self.app, &state, id()?, text, atts, mentions, from).await.map(|_| Value::Null)
            }
            "code.cancel" => {
                ensure_local_host(&state, &id()?)?;
                state.code.send(&id()?, AgentCmd::Cancel).map(|_| Value::Null)
            }
            "code.set_model" => code_set_model_local(&state, &id()?, param(&p, "modelId")?.to_owned()).await.map(|_| Value::Null),
            "code.set_mode" => {
                ensure_local_host(&state, &id()?)?;
                state.code.set_mode(&id()?, param(&p, "modeId")?.to_owned()).await.map(|_| Value::Null)
            }
            "code.rewind" => {
                ensure_local_host(&state, &id()?)?;
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
            other => anyhow::bail!("unknown request {other}"),
        }
    }

    fn synced(&self, applied: &mesh::sync::Applied) {
        if !applied.tables.contains("code_sessions") {
            return;
        }
        let state = self.app.state::<AppState>();
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

#[tauri::command]
fn term_open(
    state: State<'_, AppState>,
    id: String,
    cwd: String,
    cols: u16,
    rows: u16,
    on_event: Channel<TermEvent>,
) -> CmdResult<()> {
    let shell = prefs::load(&state.db).terminal_shell;
    Ok(state
        .terminals
        .open(&id, &cwd, Some(&shell), cols, rows, on_event)?)
}

#[tauri::command]
fn term_write(state: State<'_, AppState>, id: String, data: String) -> CmdResult<()> {
    Ok(state.terminals.write(&id, &data)?)
}

#[tauri::command]
fn term_resize(state: State<'_, AppState>, id: String, cols: u16, rows: u16) -> CmdResult<()> {
    Ok(state.terminals.resize(&id, cols, rows)?)
}

#[tauri::command]
fn term_close(state: State<'_, AppState>, id: String) {
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
            {
                use tauri::Emitter;
                let h = app.handle().clone();
                mesh.set_ui(Arc::new(move |name: &str, payload: serde_json::Value| {
                    let _ = h.emit(name, payload);
                }));
            }
            mesh.set_handler(Arc::new(MeshRpc { app: app.handle().clone() }));
            app.manage(AppState {
                db,
                data_dir,
                chat: Arc::default(),
                code: Arc::default(),
                terminals: TerminalManager::default(),
                mesh: mesh.clone(),
            });
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
