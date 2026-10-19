pub mod api;
mod attachments;
mod chat;
mod claude_auth;
mod code;
mod db;
#[cfg(feature = "desktop")]
mod desktop;
mod files;
mod gateway;
mod handoff;
mod mesh;
mod prefs;
mod providers;
mod terminal;
mod usage;
#[cfg(feature = "web")]
pub mod web;
#[cfg(test)]
mod agent_live_tests;
#[cfg(test)]
mod smoke_tests;
#[cfg(test)]
mod mesh_app_tests;

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use crate::api::Chan;

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
    /// The native window's extras (quick-entry shortcut, title bar); `None` in the daemon.
    desktop: Arc<std::sync::OnceLock<Arc<dyn DesktopHooks>>>,
}

/// What only the desktop app can do. Preferences for these save everywhere, but apply only
/// where a native window runs.
pub trait DesktopHooks: Send + Sync {
    fn validate_shortcut(&self, accel: &str) -> Result<(), String>;
    fn set_quick_shortcut(&self, accel: &str) -> anyhow::Result<()>;
    fn apply_title_bar(&self, title_bar: &str);
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
            desktop: Arc::default(),
        };
        mesh.set_ui(emit);
        mesh.set_handler(Arc::new(MeshRpc { state: state.clone() }));
        state
    }
}

struct RemoteTerm {
    channel: Chan<TermEvent>,
    input: tokio::sync::mpsc::UnboundedSender<(String, serde_json::Value)>,
}

/// What a window opening an already-running session (here or on another node) must replay.
#[derive(Default)]
struct LiveAgent {
    ready: Option<serde_json::Value>,
    permissions: Vec<(String, serde_json::Value)>,
    usage: Option<serde_json::Value>,
    /// The agent's `authRequired` event while it waits for a sign-in.
    auth: Option<serde_json::Value>,
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

pub type CmdResult<T> = Result<T, CmdError>;

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
    #[serde(default)]
    auth_mode: db::AuthMode,
}

async fn list_providers(state: &AppState) -> CmdResult<Vec<Provider>> {
    Ok(providers::list_with_key_flag(&state.db)?)
}

async fn save_provider(state: &AppState, input: ProviderInput) -> CmdResult<Provider> {
    let auth_mode = match (&input.kind, input.auth_mode) {
        (db::ProviderKind::Anthropic, db::AuthMode::ClaudeLogin) => db::AuthMode::ClaudeLogin,
        _ => db::AuthMode::ApiKey,
    };
    let p = Provider {
        id: input.id.unwrap_or_else(new_id),
        name: input.name.trim().to_owned(),
        kind: input.kind,
        base_url: providers::normalize_base_url(&input.base_url),
        has_key: false,
        auth_mode,
    };
    if p.name.is_empty() || p.base_url.is_empty() {
        return Err(CmdError("name and base URL are required".into()));
    }
    state.db.upsert_provider(&p, None)?;
    if p.auth_mode == db::AuthMode::ClaudeLogin {
        providers::delete_key(&p.id);
        state.db.clear_inline_key(&p.id)?;
    } else {
        match input.api_key.as_deref() {
            Some("") => {
                providers::delete_key(&p.id);
                state.db.clear_inline_key(&p.id)?;
            }
            Some(k) => providers::store_key(&state.db, &p, k.trim())?,
            None => {}
        }
    }
    state.mesh.secrets_changed();
    let saved = providers::list_with_key_flag(&state.db)?
        .into_iter()
        .find(|x| x.id == p.id)
        .unwrap_or(p);
    Ok(saved)
}

async fn delete_provider(state: &AppState, id: String) -> CmdResult<()> {
    providers::delete_key(&id);
    state.db.delete_provider(&id)?;
    state.mesh.secrets_changed();
    Ok(())
}

async fn discover_models(
    state: &AppState,
    provider_id: String,
) -> CmdResult<Vec<providers::DiscoveredModel>> {
    let (p, inline) = state
        .db
        .get_provider(&provider_id)?
        .ok_or_else(|| CmdError("provider not found".into()))?;
    let auth = match p.auth_mode {
        db::AuthMode::ClaudeLogin => providers::Auth::ClaudeLogin,
        db::AuthMode::ApiKey => providers::Auth::Key,
    };
    let key = match auth {
        providers::Auth::ClaudeLogin => Some(claude_auth::access_token().await?),
        providers::Auth::Key => providers::load_key(&p.id, inline.as_deref()),
    };
    Ok(providers::discover_models(&p.kind, &p.base_url, key.as_deref(), auth).await?)
}

async fn list_models(state: &AppState) -> CmdResult<Vec<Model>> {
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

async fn save_model(state: &AppState, input: ModelInput) -> CmdResult<Model> {
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

async fn delete_model(state: &AppState, id: String) -> CmdResult<()> {
    Ok(state.db.delete_model(&id)?)
}

async fn get_preferences(state: &AppState) -> CmdResult<prefs::Preferences> {
    Ok(prefs::load(&state.db))
}

async fn set_preferences(state: &AppState, prefs: prefs::Preferences) -> CmdResult<()> {
    prefs.validate().map_err(CmdError)?;
    let desktop = state.desktop.get().map(|d| &**d);
    if let Some(d) = desktop {
        d.validate_shortcut(&prefs.quick_entry_shortcut).map_err(CmdError)?;
    }
    let old = prefs::load(&state.db);
    if let Some(d) = desktop
        && old.quick_entry_shortcut.trim() != prefs.quick_entry_shortcut.trim()
        && let Err(e) = d.set_quick_shortcut(&prefs.quick_entry_shortcut)
    {
        // Put the previous one back so the saved setting matches what is registered.
        let _ = d.set_quick_shortcut(&old.quick_entry_shortcut);
        return Err(e.into());
    }
    prefs::save(&state.db, &prefs)?;
    if let Some(d) = desktop
        && old.title_bar != prefs.title_bar
    {
        d.apply_title_bar(&prefs.title_bar);
    }
    Ok(())
}

async fn get_setting(state: &AppState, key: String) -> CmdResult<Option<String>> {
    Ok(state.db.get_setting(&key)?)
}

async fn set_setting(state: &AppState, key: String, value: String) -> CmdResult<()> {
    Ok(state.db.set_setting(&key, &value)?)
}

// ---------------- chat ----------------

async fn list_conversations(state: &AppState) -> CmdResult<Vec<Conversation>> {
    Ok(state.db.list_conversations()?)
}

async fn create_conversation(
    state: &AppState,
    model_ref: Option<String>,
) -> CmdResult<Conversation> {
    Ok(state.db.create_conversation(model_ref.as_deref())?)
}

async fn update_conversation(
    state: &AppState,
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

async fn delete_conversation(state: &AppState, id: String) -> CmdResult<()> {
    state.chat.cancel(&id);
    Ok(state.db.delete_conversation(&id)?)
}

async fn list_messages(state: &AppState, conversation_id: String) -> CmdResult<Vec<Message>> {
    Ok(state.db.list_messages(&conversation_id)?)
}

/// Make the newest branch through `message_id` the visible one.
async fn switch_branch(state: &AppState, conversation_id: String, message_id: String) -> CmdResult<()> {
    let leaf = state.db.latest_leaf(&message_id)?;
    Ok(state.db.set_active_leaf(&conversation_id, &leaf)?)
}

async fn search_messages(state: &AppState, query: String, limit: Option<usize>) -> CmdResult<Vec<SearchHit>> {
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
    api::spawn(async move {
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
#[allow(clippy::too_many_arguments)]
async fn send_chat_message(
    state: &AppState,
    conversation_id: String,
    model_ref: String,
    text: String,
    attachment_ids: Vec<String>,
    parent_id: Option<String>,
    reasoning_effort: Option<String>,
    on_event: Chan<ChatEvent>,
) -> CmdResult<()> {
    let parent = parent_id.map(|p| Some(p).filter(|p| !p.is_empty()));
    let turn = chat::Turn::Message { text, attachment_ids, parent };
    spawn_chat(state, conversation_id, model_ref, turn, reasoning_effort, channel_sink(on_event));
    Ok(())
}

fn channel_sink(ch: Chan<ChatEvent>) -> chat::ChatSink {
    Arc::new(move |ev| {
        let _ = ch.send(ev);
    })
}

async fn regenerate_reply(
    state: &AppState,
    conversation_id: String,
    model_ref: String,
    user_message_id: String,
    reasoning_effort: Option<String>,
    on_event: Chan<ChatEvent>,
) -> CmdResult<()> {
    let turn = chat::Turn::Regenerate { user_message_id };
    spawn_chat(state, conversation_id, model_ref, turn, reasoning_effort, channel_sink(on_event));
    Ok(())
}

/// Stop a reply, wherever in the mesh it is streaming.
async fn cancel_chat(state: &AppState, conversation_id: String) -> CmdResult<()> {
    match state.mesh.streamer_of(&conversation_id).filter(|n| n != state.mesh.node_id()) {
        Some(node) => {
            state.mesh.rpc(&node, "chat.cancel", serde_json::json!({ "conversationId": conversation_id })).await?;
        }
        None => state.chat.cancel(&conversation_id),
    }
    Ok(())
}

// ---------------- attachments ----------------

async fn add_attachment_path(state: &AppState, path: String) -> CmdResult<Attachment> {
    let db = state.db.clone();
    let dir = state.data_dir.clone();
    Ok(tokio::task::spawn_blocking(move || {
        attachments::ingest_path(&db, &dir, std::path::Path::new(&path))
    })
    .await??)
}

async fn add_attachment_bytes(
    state: &AppState,
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

async fn attachment_thumbnail(state: &AppState, id: String) -> CmdResult<String> {
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

async fn list_code_sessions(state: &AppState) -> CmdResult<Vec<CodeSession>> {
    Ok(state.db.list_code_sessions()?)
}

async fn create_code_session(
    state: &AppState,
    cwd: String,
    model_ref: Option<String>,
    agent: Option<String>,
) -> CmdResult<CodeSession> {
    Ok(create_code_session_local(state, &cwd, model_ref.as_deref(), agent)?)
}

/// A new Code session hosted here, in `cwd` on this computer.
fn create_code_session_local(
    state: &AppState,
    cwd: &str,
    model_ref: Option<&str>,
    agent: Option<String>,
) -> anyhow::Result<CodeSession> {
    if !std::path::Path::new(cwd).is_dir() {
        anyhow::bail!("{cwd} is not a directory");
    }
    let prefs = prefs::load(&state.db);
    let agent = agent.unwrap_or_else(|| prefs.default_agent.clone());
    if code::agents::AgentKind::parse(&agent).is_none() {
        anyhow::bail!("unknown coding agent {agent}");
    }
    state.db.create_code_session(cwd, model_ref, &agent, &prefs.permission_mode)
}

/// The coding agents Patchbay can drive, and whether each is installed here or, with `host`, on
/// that computer.
async fn list_agents(state: &AppState, host: Option<String>) -> CmdResult<serde_json::Value> {
    if let Some(h) = host.filter(|h| h != state.mesh.node_id()) {
        let options = state.mesh.rpc(&h, "code.new_options", serde_json::json!({})).await?;
        return Ok(options["agents"].clone());
    }
    let paths = prefs::load(&state.db).agent_paths;
    let agents = tokio::task::spawn_blocking(move || code::agents::detect_all(&paths)).await?;
    Ok(serde_json::to_value(agents)?)
}

async fn update_code_session(
    state: &AppState,
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
        archive_code_session(state, &id, a).await?;
    }
    if let Some(a) = activity {
        state.db.set_code_session_activity(&id, &a)?;
    }
    if let Some(t) = &title {
        state.db.set_code_session_title(&id, t, true)?;
    }
    if let Some(m) = &model_ref {
        state.db.update_code_session(&id, None, None, Some(m))?;
    }
    Ok(())
}

async fn delete_code_session(state: &AppState, id: String) -> CmdResult<()> {
    state.code.stop(&id);
    state.code.titles.forget(&id);
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
    let state = state.clone();
    Arc::new(move |env| on_code_event(&state, env))
}

fn on_code_event(state: &AppState, env: code::acp::Envelope) {
    use code::acp::CodeEvent;
    track_titles(state, &env);
    {
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
                CodeEvent::SessionReady { .. } => {
                    let l = live.entry(id).or_default();
                    l.ready = Some(ev);
                    l.auth = None;
                }
                CodeEvent::AuthRequired { .. } => live.entry(id).or_default().auth = Some(ev),
                CodeEvent::AuthResolved => {
                    if let Some(l) = live.get_mut(&id) {
                        l.auth = None;
                    }
                }
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
    }
}

/// Feeds a session's automatic title from its agent's events (see `code::titles`).
fn track_titles(state: &AppState, env: &code::acp::Envelope) {
    use code::acp::CodeEvent;
    let id = &env.code_session_id;
    match &env.event {
        CodeEvent::Update { update } => match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                if let Some(t) = update["content"]["text"].as_str() {
                    state.code.titles.on_chunk(id, t);
                }
            }
            // Agents that name their sessions (Grok Build does) send it as session info.
            Some("session_info_update") => {
                if let Some(t) = update["title"].as_str().map(str::trim).filter(|t| !t.is_empty())
                    && state.code.titles.on_agent_title(id)
                {
                    set_auto_title(state, id, t);
                }
            }
            _ => {}
        },
        CodeEvent::PromptDone { stop_reason } => {
            let completed = !matches!(stop_reason.as_str(), Some("cancelled" | "error" | "refusal"));
            if let Some(convo) = state.code.titles.on_turn_end(id, completed)
                && prefs::load(&state.db).generate_titles
            {
                let (state, id) = (state.clone(), id.clone());
                api::spawn(async move {
                    // An agent that names sessions itself often does it just after the turn.
                    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                    if state.code.titles.agent_titled(&id) {
                        return;
                    }
                    let Ok(Some(session)) = state.db.get_code_session(&id) else { return };
                    if session.title_manual {
                        return;
                    }
                    match code::titles::generate(&state.db, &session, convo).await {
                        Ok(t) => set_auto_title(&state, &id, &t),
                        Err(e) => tracing::warn!("session title: {e:#}"),
                    }
                });
            }
        }
        _ => {}
    }
}

/// Name a session automatically, unless someone named it, and show the name everywhere.
fn set_auto_title(state: &AppState, id: &str, title: &str) {
    let title = chat::clip(title, 80);
    match state.db.set_code_session_title(id, &title, false) {
        Ok(true) => on_code_event(
            state,
            code::acp::Envelope { code_session_id: id.to_owned(), event: code::acp::CodeEvent::Title { title } },
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!("session title: {e:#}"),
    }
}

/// Name a session from its whole conversation, unpinning a title someone typed.
async fn code_auto_title_local(state: &AppState, id: &str) -> anyhow::Result<String> {
    let session = state.db.get_code_session(id)?.ok_or_else(|| anyhow::anyhow!("code session not found"))?;
    let convo = code::titles::transcript_context(state.db.code_transcript(id)?.as_deref())
        .ok_or_else(|| anyhow::anyhow!("nothing to name it from yet: send it a message first"))?;
    let title = code::titles::generate(&state.db, &session, convo).await?;
    state.db.unpin_code_session_title(id)?;
    set_auto_title(state, id, &title);
    Ok(chat::clip(&title, 80))
}

async fn code_auto_title(state: &AppState, id: String) -> CmdResult<String> {
    // The host names it, so the new title reaches viewers in the session's own event stream.
    if let Some(host) = remote_host(state, &id)? {
        let t = state.mesh.rpc(&host, "code.auto_title", serde_json::json!({ "id": id })).await?;
        return Ok(t.as_str().unwrap_or_default().to_owned());
    }
    Ok(code_auto_title_local(state, &id).await?)
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
async fn code_start(state: &AppState, id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        state.mesh.rpc(&host, "code.start", serde_json::json!({ "id": id })).await?;
        return Ok(());
    }
    Ok(code_start_local(state, id).await?)
}

/// Catch-up for a window opening a running session: whether it runs and is mid-turn, plus the
/// events that set up its controls (session ready, open permission requests, usage).
fn code_attach_local(state: &AppState, id: &str) -> serde_json::Value {
    let running = state.code.is_running(id);
    let live = state.live.lock().unwrap();
    let mut events = Vec::new();
    if running && let Some(l) = live.get(id) {
        events.extend(l.ready.clone());
        events.extend(l.auth.clone());
        events.extend(l.permissions.iter().map(|(_, e)| e.clone()));
        events.extend(l.usage.clone());
    }
    serde_json::json!({ "running": running, "busy": state.code.is_busy(id), "events": events })
}

async fn code_attach(state: &AppState, id: String) -> CmdResult<serde_json::Value> {
    if let Some(host) = remote_host(state, &id)? {
        if !state.mesh.connected(&host) {
            return Ok(serde_json::json!({ "running": false, "busy": false, "events": [], "offline": true }));
        }
        return Ok(state.mesh.rpc(&host, "code.attach", serde_json::json!({ "id": id })).await?);
    }
    Ok(code_attach_local(state, &id))
}

async fn code_is_running(state: &AppState, id: String) -> CmdResult<bool> {
    if let Some(host) = remote_host(state, &id)? {
        if !state.mesh.connected(&host) {
            return Ok(false);
        }
        let v = state.mesh.rpc(&host, "code.is_running", serde_json::json!({ "id": id })).await?;
        return Ok(v.as_bool().unwrap_or(false));
    }
    Ok(state.code.is_running(&id))
}

async fn code_stop(state: &AppState, id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
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
    let note = state.db.get_setting(&note_key)?;
    if let Some(note) = &note {
        blocks.insert(0, serde_json::json!({ "type": "text", "text": note }));
    }
    // Bumps updated_at so the session sorts to the top of the sidebar.
    state.db.update_code_session(&id, None, None, None)?;
    if let Some(s) = state.db.get_code_session(&id)?
        && let Some(t) = code::titles::first_line_title(&s, if text.trim().is_empty() { atts.first().map_or("", |a| a.name.as_str()) } else { &text })
    {
        set_auto_title(state, &id, &t);
    }
    state.code.titles.on_prompt(&id, &text, || state.db.code_transcript(&id).ok().flatten());
    if note.is_some() {
        state.code.titles.on_handoff(&id);
    }
    state.code.send(&id, AgentCmd::Prompt { blocks })?;
    state.code.set_busy(&id, true);
    if note.is_some() {
        state.db.with_conn(|c| c.execute("DELETE FROM settings WHERE key = ?1", [&note_key]))?;
    }
    // Every viewer shows the prompt; the window that sent it already has.
    code_sink(state)(code::acp::Envelope {
        code_session_id: id,
        event: code::acp::CodeEvent::UserPrompt { text, origin, attachments: serde_json::to_value(&atts)? },
    });
    Ok(())
}

async fn code_prompt(
    state: &AppState,
    id: String,
    text: String,
    attachment_ids: Vec<String>,
    mentions: Option<Vec<String>>,
) -> CmdResult<()> {
    let mentions = mentions.unwrap_or_default();
    if let Some(host) = remote_host(state, &id)? {
        let params = serde_json::json!({ "id": id, "text": text, "attachmentIds": attachment_ids, "mentions": mentions });
        state.mesh.rpc(&host, "code.prompt", params).await?;
        return Ok(());
    }
    let me = state.mesh.node_id().to_owned();
    Ok(code_prompt_local(state, id, text, attachment_ids, mentions, me).await?)
}

async fn code_cancel(state: &AppState, id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
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

async fn code_set_model(state: &AppState, id: String, model_id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        state.mesh.rpc(&host, "code.set_model", serde_json::json!({ "id": id, "modelId": model_id })).await?;
        return Ok(());
    }
    Ok(code_set_model_local(state, &id, model_id).await?)
}

async fn code_set_config_option(
    state: &AppState,
    id: String,
    config_id: String,
    value: String,
) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        let params = serde_json::json!({ "id": id, "configId": config_id, "value": value });
        state.mesh.rpc(&host, "code.set_config_option", params).await?;
        return Ok(());
    }
    Ok(state.code.set_config_option(&id, config_id, value).await?)
}

/// Sign the session's agent in with a method from its `authRequired` event. A browser sign-in
/// opens on the computer that runs the agent.
async fn code_authenticate(
    state: &AppState,
    id: String,
    method_id: String,
    api_key: Option<String>,
) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        let params = serde_json::json!({ "id": id, "methodId": method_id, "apiKey": api_key });
        state.mesh.rpc_long(&host, "code.authenticate", params).await?;
        return Ok(());
    }
    Ok(state.code.authenticate(&id, method_id, api_key).await?)
}

/// Switch a session that hasn't started yet to another agent. The agent here stops; whoever asked
/// starts the new one, as when opening the session.
fn code_set_agent_local(state: &AppState, id: &str, agent: &str, model_ref: Option<&str>) -> anyhow::Result<()> {
    ensure_local_host(state, id)?;
    let kind = code::agents::AgentKind::parse(agent).ok_or_else(|| anyhow::anyhow!("unknown coding agent {agent}"))?;
    let session = state.db.get_code_session(id)?.ok_or_else(|| anyhow::anyhow!("code session not found"))?;
    if session.agent == agent {
        return Ok(());
    }
    if state.code.is_busy(id) {
        anyhow::bail!("the agent is working; wait for it to finish, or stop it, before switching agents");
    }
    // The transcript is the UI's (JSON entries); any user turn means the old agent has the history.
    let transcript = state.db.code_transcript(id)?;
    if transcript.as_deref().is_some_and(|t| t.contains(r#""kind":"user""#)) {
        // The new agent can't load the old one's session. Its first prompt carries the
        // conversation so far, and the transcript keeps the old turns as earlier history.
        let from = code::agents::AgentKind::parse(&session.agent).map_or(session.agent.as_str(), |k| k.label());
        let divider = format!("Switched from {from} to {}. It got the conversation so far as context.", kind.label());
        let folded = handoff::fold_for_switch(transcript.as_deref(), &divider);
        state.db.set_setting(&format!("mesh.handoff.{id}"), &handoff::switch_prompt(Some(&folded), from))?;
        state.db.set_code_transcript(id, &folded)?;
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

async fn code_set_agent(state: &AppState, id: String, agent: String, model_ref: Option<String>) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        let p = serde_json::json!({ "id": id, "agent": agent, "modelRef": model_ref });
        state.mesh.rpc(&host, "code.set_agent", p).await?;
        return Ok(());
    }
    Ok(code_set_agent_local(state, &id, &agent, model_ref.as_deref())?)
}

async fn code_set_mode(state: &AppState, id: String, mode_id: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        state.mesh.rpc(&host, "code.set_mode", serde_json::json!({ "id": id, "modeId": mode_id })).await?;
        return Ok(());
    }
    Ok(state.code.set_mode(&id, mode_id).await?)
}

/// Patchbay's Ask / Auto / Always-approve policy for this session's ACP permission prompts.
async fn code_set_permission_mode(state: &AppState, id: String, mode: String) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        state
            .mesh
            .rpc(&host, "code.set_permission_mode", serde_json::json!({ "id": id, "mode": mode }))
            .await?;
        return Ok(());
    }
    Ok(state.db.set_code_session_permission_mode(&id, &mode)?)
}

async fn code_rewind(
    state: &AppState,
    id: String,
    ordinal: usize,
    preview: String,
    revert_files: bool,
) -> CmdResult<()> {
    if let Some(host) = remote_host(state, &id)? {
        let params = serde_json::json!({ "id": id, "ordinal": ordinal, "preview": preview, "revertFiles": revert_files });
        state.mesh.rpc(&host, "code.rewind", params).await?;
        return Ok(());
    }
    Ok(state.code.rewind(&id, ordinal, preview, revert_files).await?)
}

async fn code_usage(state: &AppState, id: String) -> CmdResult<usage::UsageView> {
    Ok(state.db.code_usage(&id)?.view())
}

/// Answer a permission request; `id` (the session) routes it to the node running the agent.
async fn code_permission_respond(
    state: &AppState,
    request_id: String,
    option_id: Option<String>,
    id: Option<String>,
) -> CmdResult<()> {
    if let Some(id) = id
        && let Some(host) = remote_host(state, &id)?
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
async fn code_move(state: &AppState, id: String, target: String, cwd: String) -> CmdResult<Option<String>> {
    let cwd = cwd.trim().to_owned();
    if cwd.is_empty() {
        return Err(CmdError("choose a folder on the target computer".into()));
    }
    if target == state.mesh.node_id() {
        return Ok(import_local(state, &id, &cwd).await?);
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

async fn code_take_over(state: &AppState, id: String, cwd: String) -> CmdResult<Option<String>> {
    Ok(take_over_local(state, &id, cwd.trim()).await?)
}

/// Checkpoints of other nodes' sessions this node holds: session id -> taken at (ms).
async fn code_checkpoints(state: &AppState) -> CmdResult<std::collections::HashMap<String, i64>> {
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
            "code.authenticate" => {
                ensure_local_host(state, &id()?)?;
                let method = param(&p, "methodId")?.to_owned();
                let key = p["apiKey"].as_str().map(str::to_owned);
                state.code.authenticate(&id()?, method, key).await.map(|_| Value::Null)
            }
            "code.set_agent" => {
                code_set_agent_local(state, &id()?, param(&p, "agent")?, p["modelRef"].as_str()).map(|_| Value::Null)
            }
            "code.set_mode" => {
                ensure_local_host(state, &id()?)?;
                state.code.set_mode(&id()?, param(&p, "modeId")?.to_owned()).await.map(|_| Value::Null)
            }
            "code.set_permission_mode" => {
                ensure_local_host(state, &id()?)?;
                state.db.set_code_session_permission_mode(&id()?, param(&p, "mode")?).map(|_| Value::Null)
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
            "code.auto_title" => Ok(json!(code_auto_title_local(state, &id()?).await?)),
            "code.files" => {
                let s = state.db.get_code_session(&id()?)?.ok_or_else(|| anyhow::anyhow!("no such session"))?;
                let files = tokio::task::spawn_blocking(move || files::list(std::path::Path::new(&s.cwd))).await??;
                Ok(json!(files))
            }
            "chat.cancel" => {
                state.chat.cancel(param(&p, "conversationId")?);
                Ok(Value::Null)
            }
            // A phone's chat turn, passed on by its gateway because the phone picked this computer.
            "chat.run" => {
                let phone = param(&p, "phone")?;
                gateway::run_chat_local(state, phone, param(&p, "kind")? == "send", &p).await?;
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
                let (cols, rows) = (p["cols"].as_u64().unwrap_or(80) as u16, p["rows"].as_u64().unwrap_or(24) as u16);
                match p["authMethod"].as_str() {
                    Some(method) => {
                        let program = state.code.terminal_auth(sid, method.to_owned()).await?;
                        state.terminals.open_program(&key, &s.cwd, &program, cols, rows, sink)?;
                    }
                    None => {
                        let shell = prefs::load(&state.db).terminal_shell;
                        state.terminals.open(&key, &s.cwd, Some(&shell), cols, rows, sink)?;
                    }
                }
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
            // Starting a session here for a phone, through its gateway.
            "code.new_options" => gateway::new_session_options_local(state).await,
            "code.create" => gateway::create_local(state, &p).map(|s| json!(s)),
            "fs.dirs" => gateway::list_dirs_local(p["path"].as_str()),
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

async fn mesh_status(state: &AppState) -> CmdResult<serde_json::Value> {
    Ok(state.mesh.status())
}

async fn mesh_create(state: &AppState, passphrase: String, name: String) -> CmdResult<()> {
    Ok(state.mesh.create(&passphrase, &name)?)
}

async fn mesh_join(state: &AppState, url: String, passphrase: String, name: String) -> CmdResult<()> {
    Ok(state.mesh.join(&url, &passphrase, &name)?)
}

async fn mesh_cancel_join(state: &AppState) -> CmdResult<()> {
    state.mesh.cancel_join();
    Ok(())
}

async fn mesh_approve(state: &AppState, node_id: String) -> CmdResult<()> {
    Ok(state.mesh.approve(&node_id)?)
}

async fn mesh_reject(state: &AppState, node_id: String) -> CmdResult<()> {
    state.mesh.reject(&node_id);
    Ok(())
}

async fn mesh_remove(state: &AppState, node_id: String) -> CmdResult<()> {
    Ok(state.mesh.remove(&node_id)?)
}

async fn mesh_leave(state: &AppState) -> CmdResult<()> {
    Ok(state.mesh.leave().await?)
}

async fn mesh_set_config(
    state: &AppState,
    name: Option<String>,
    listen: Option<String>,
    advertise: Option<Vec<String>>,
) -> CmdResult<()> {
    Ok(state.mesh.set_config(name.as_deref(), listen.as_deref(), advertise)?)
}

async fn mesh_set_passphrase(state: &AppState, passphrase: String) -> CmdResult<()> {
    Ok(state.mesh.set_passphrase(&passphrase)?)
}

async fn mesh_add_address(state: &AppState, url: String) -> CmdResult<()> {
    Ok(state.mesh.add_address(&url)?)
}

async fn mesh_chat_streams(state: &AppState) -> CmdResult<Vec<mesh::node::Stream>> {
    Ok(state.mesh.remote_streams())
}

// ---------------- terminal ----------------
//
// With `session_id`, the terminal runs on the node hosting that session: output arrives as
// `mesh://term` events, and input goes there in order through a per-terminal queue. With
// `auth_method` too, it runs that session's agent's terminal sign-in command, which the agent
// thread builds from what the agent advertised, instead of the user's shell.

async fn term_open(
    state: &AppState,
    id: String,
    cwd: String,
    cols: u16,
    rows: u16,
    on_event: Chan<TermEvent>,
    session_id: Option<String>,
    auth_method: Option<String>,
) -> CmdResult<()> {
    if let Some(sid) = &session_id
        && let Some(host) = remote_host(state, sid)?
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, serde_json::Value)>();
        let mesh = state.mesh.clone();
        let h = host.clone();
        api::spawn(async move {
            while let Some((method, params)) = rx.recv().await {
                if let Err(e) = mesh.rpc(&h, &method, params).await {
                    tracing::debug!("remote terminal: {e:#}");
                }
            }
        });
        state.remote_terms.lock().unwrap().insert(id.clone(), RemoteTerm { channel: on_event, input: tx });
        let params = serde_json::json!({
            "termId": id, "sessionId": sid, "cols": cols, "rows": rows, "authMethod": auth_method,
        });
        state.mesh.rpc(&host, "term.open", params).await?;
        return Ok(());
    }
    let sink: terminal::TermSink = Arc::new(move |ev| on_event.send(ev));
    if let Some(method) = auth_method {
        let sid = session_id.ok_or_else(|| CmdError("a sign-in terminal needs its session".into()))?;
        let program = state.code.terminal_auth(&sid, method).await?;
        return Ok(state.terminals.open_program(&id, &cwd, &program, cols, rows, sink)?);
    }
    let shell = prefs::load(&state.db).terminal_shell;
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

async fn term_write(state: &AppState, id: String, data: String) -> CmdResult<()> {
    if remote_term(state, &id, "term.write", serde_json::json!({ "termId": id, "data": data })) {
        return Ok(());
    }
    Ok(state.terminals.write(&id, &data)?)
}

async fn term_resize(state: &AppState, id: String, cols: u16, rows: u16) -> CmdResult<()> {
    if remote_term(state, &id, "term.resize", serde_json::json!({ "termId": id, "cols": cols, "rows": rows })) {
        return Ok(());
    }
    Ok(state.terminals.resize(&id, cols, rows)?)
}

async fn term_close(state: &AppState, id: String) -> CmdResult<()> {
    if remote_term(state, &id, "term.close", serde_json::json!({ "termId": id })) {
        state.remote_terms.lock().unwrap().remove(&id);
        return Ok(());
    }
    state.terminals.close(&id);
    Ok(())
}

// ---------------- code session extras ----------------

async fn get_code_transcript(state: &AppState, id: String) -> CmdResult<Option<String>> {
    Ok(state.db.code_transcript(&id)?)
}

/// Cache of the rendered transcript, so a session shows its history before (or without) the
/// agent replaying it.
async fn save_code_transcript(state: &AppState, id: String, json: String) -> CmdResult<()> {
    const MAX: usize = 4 * 1024 * 1024;
    if json.len() > MAX {
        return Err(CmdError("transcript too large to cache".into()));
    }
    Ok(state.db.set_code_transcript(&id, &json)?)
}

/// Files under `cwd` for @-mentions; with `session_id`, on whichever node hosts that session.
async fn list_project_files(state: &AppState, cwd: String, session_id: Option<String>) -> CmdResult<Vec<String>> {
    if let Some(id) = session_id
        && let Some(host) = remote_host(state, &id)?
    {
        let v = state.mesh.rpc(&host, "code.files", serde_json::json!({ "id": id })).await?;
        return Ok(serde_json::from_value(v)?);
    }
    Ok(tokio::task::spawn_blocking(move || files::list(std::path::Path::new(&cwd))).await??)
}

async fn list_mcp_servers(state: &AppState) -> CmdResult<Vec<McpServer>> {
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

async fn save_mcp_server(state: &AppState, input: McpServerInput) -> CmdResult<McpServer> {
    let saved = code::mcp::save(&state.db, input.id, &input.name, input.enabled, input.config)?;
    state.mesh.secrets_changed();
    Ok(saved)
}

async fn delete_mcp_server(state: &AppState, id: String) -> CmdResult<()> {
    code::mcp::delete(&state.db, &id)?;
    state.mesh.secrets_changed();
    Ok(())
}

async fn list_permission_rules(state: &AppState) -> CmdResult<Vec<PermissionRule>> {
    Ok(state.db.list_permission_rules()?)
}

async fn add_permission_rule(
    state: &AppState,
    cwd: String,
    tool_kind: String,
    pattern: String,
) -> CmdResult<PermissionRule> {
    if tool_kind.trim().is_empty() {
        return Err(CmdError("tool kind is required".into()));
    }
    Ok(state.db.add_permission_rule(cwd.trim(), tool_kind.trim(), pattern.trim())?)
}

async fn delete_permission_rule(state: &AppState, id: String) -> CmdResult<()> {
    Ok(state.db.delete_permission_rule(&id)?)
}

// ---------------- prompts ----------------

async fn list_prompts(state: &AppState) -> CmdResult<Vec<Prompt>> {
    Ok(state.db.list_prompts()?)
}

async fn save_prompt(state: &AppState, id: Option<String>, name: String, content: String) -> CmdResult<Prompt> {
    if name.trim().is_empty() {
        return Err(CmdError("name is required".into()));
    }
    Ok(state.db.save_prompt(id.as_deref(), name.trim(), &content)?)
}

async fn delete_prompt(state: &AppState, id: String) -> CmdResult<()> {
    Ok(state.db.delete_prompt(&id)?)
}

// ---------------- usage ----------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageReport {
    chat: Vec<ChatUsageRow>,
    code: Vec<CodeUsageRow>,
}

async fn usage_report(state: &AppState, days: u32) -> CmdResult<UsageReport> {
    let since = db::now_ms() - i64::from(days.clamp(1, 3650)) * 86_400_000;
    Ok(UsageReport {
        chat: state.db.chat_usage_since(since)?,
        code: state.db.code_usage_rows()?,
    })
}

/// Where the desktop app keeps its data (Tauri's `app_data_dir` for `dev.patchbay.app`), so the
/// daemon can take over a computer's existing setup. Its mesh secrets use the keyring's unscoped
/// names (see `OsKeyStore`).
pub fn default_data_dir() -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    const ID: &str = "dev.patchbay.app";
    let home = || std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set");
    if cfg!(target_os = "macos") {
        return Ok(home()?.join("Library/Application Support").join(ID));
    }
    if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA").context("APPDATA is not set")?;
        return Ok(PathBuf::from(appdata).join(ID));
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map_or_else(|| home().map(|h| h.join(".local/share")), Ok)?;
    Ok(base.join(ID))
}

/// Opens the database and mesh under `data_dir` and wires them into a fresh `AppState`. Both
/// the desktop app and the daemon start here; `emit` reaches their windows or web clients.
pub fn open_state(data_dir: PathBuf, emit: mesh::node::UiSink) -> anyhow::Result<AppState> {
    std::fs::create_dir_all(&data_dir)?;
    // One process per data folder: two would share a mesh identity and fight over sessions.
    let lock = std::fs::File::create(data_dir.join("instance.lock"))?;
    if lock.try_lock().is_err() {
        anyhow::bail!("another Patchbay (the app or patchbayd) is already using {}", data_dir.display());
    }
    // Held until the process exits.
    std::mem::forget(lock);
    let db = Arc::new(Db::open(&data_dir.join("patchbay.sqlite"))?);
    let keys = Arc::new(mesh::keys::OsKeyStore::new(&data_dir));
    let mesh = Mesh::new(db.clone(), data_dir.clone(), keys)?;
    let state = AppState::new(db, data_dir, mesh.clone(), emit);
    if mesh.is_member() {
        api::spawn(async move { mesh.start() });
    }
    Ok(state)
}

impl AppState {
    /// Stop everything this node runs: agents, terminals and the mesh.
    pub fn shutdown(&self) {
        self.code.stop_all();
        self.terminals.close_all();
        self.mesh.stop();
    }
}

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PATCHBAY_LOG").unwrap_or_else(|_| "info".into()),
        )
        .init();
}

#[cfg(feature = "desktop")]
pub use desktop::run;

pub use mesh::node::UiSink;
